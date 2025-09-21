use std::{
    collections::{BTreeMap, HashMap},
    fmt::Display,
    marker::PhantomData,
    rc::Rc,
    sync::mpsc::{sync_channel, Receiver, RecvError, SyncSender},
    thread::{self},
    time,
};

use error_stack::{Context, Result};
use jack::{Client, ClientStatus, Control, MidiOut, ProcessScope, RawMidi};
use klavier_core::{bar::RepeatSet, have_start_tick::HaveStartTick, play_start_tick::ToAccumTickError};
use klavier_core::channel::Channel;
use klavier_core::play_start_tick::PlayStartTick;
use klavier_core::{
    bar::Bar,
    ctrl_chg::CtrlChg,
    duration::Duration,
    global_repeat::RenderRegionWarning,
    key::Key,
    note::Note,
    octave::Octave,
    pitch::Pitch,
    project::{tempo_at, ModelChangeMetadata},
    repeat::{render_region, Chunk, RenderRegionError, AccumTick},
    repeat_set,
    rhythm::Rhythm,
    sharp_flat::SharpFlat,
    solfa::Solfa,
    tempo::{Tempo, TempoValue},
    velocity::Velocity,
};
use klavier_helper::{bag_store::BagStore, sliding, store::{self, Store}};

#[cfg(not(test))]
use jack::{AsyncClient, ClosureProcessHandler, MidiWriter};

pub struct Player {
    // Frequency (ex. 48kHz => 48000)
    pub sampling_rate: usize,
    cmd_channel: SyncSender<Cmd>,
    resp_channel: Option<Receiver<Resp>>,
    closer: Option<Box<dyn Send + 'static + FnOnce() -> Option<jack::Error>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum MidiSrc {
    NoteOn {
        channel: Channel,
        pitch: Pitch,
        velocity: Velocity,
    },
    NoteOff {
        channel: Channel,
        pitch: Pitch,
    },
    CtrlChg {
        channel: Channel,
        number: u8,
        velocity: Velocity,
    },
}

impl MidiSrc {
    fn render_to(self, buf: &mut Vec<u8>) {
        match self {
            MidiSrc::NoteOn {
                channel,
                pitch,
                velocity,
            } => {
                buf.push(0b10010000 | channel.as_u8());
                buf.push(pitch.value() as u8);
                buf.push(velocity.as_u8());
            }
            MidiSrc::NoteOff { channel, pitch } => {
                buf.push(0b10010000 | channel.as_u8());
                buf.push(pitch.value() as u8);
                buf.push(0);
            }
            MidiSrc::CtrlChg {
                channel,
                number,
                velocity,
            } => {
                buf.push(0b10110000 | channel.as_u8());
                buf.push(number);
                buf.push(velocity.as_u8());
            }
        }
    }
}

#[derive(Clone)]
pub struct PlayData {
    midi_data: Store<u64, Vec<Vec<u8>>, ()>,
    // Key: cycle, Value: tick
    table_for_tracking: Store<u64, (AccumTick, TempoValue), ()>,
    chunks: Store<AccumTick, Chunk, ()>,
}

impl PlayData {
    pub fn cycle_to_tick(&self, cycle: u64, sampling_rate: u32) -> AccumTick {
        let mut finder = self.table_for_tracking.finder();
        match finder.just_before(cycle) {
            Some((c, (tick, tempo))) => {
                tick + ((cycle - c) * tempo.as_u16() as u64 * Duration::TICK_RESOLUTION as u64
                    / sampling_rate as u64
                    / 60) as u32
            }
            None => {
                (cycle * TempoValue::default().as_u16() as u64 * Duration::TICK_RESOLUTION as u64
                    / sampling_rate as u64
                    / 60) as u32
            }
        }
    }

    pub fn accum_tick_to_tick(&self, tick: AccumTick) -> u32 {
        match self.chunks.finder().just_before(tick) {
            Some((at_tick, chunk)) => chunk.start_tick() + (tick - at_tick),
            None => tick,
        }
    }
}

struct MidiReporter {
    key: Key,
    accidental: HashMap<(Solfa, Octave), SharpFlat>,
}

impl MidiReporter {
    fn new(key: Key) -> Self {
        Self {
            key,
            accidental: HashMap::new(),
        }
    }

    fn on_note(&mut self, note: Note) -> Vec<(u32, MidiSrc)> {
        let base_pitch = note.pitch;
        let sharp_flat = base_pitch.sharp_flat();

        fn create_event(note: &Note, pitch: Pitch) -> Vec<(u32, MidiSrc)> {
            let mut ret = Vec::with_capacity(2);

            if !note.tied {
                ret.push((
                    note.start_tick(),
                    MidiSrc::NoteOn {
                        pitch,
                        velocity: note.velocity(),
                        channel: note.channel,
                    },
                ));
            }

            if !note.tie {
                ret.push((
                    note.start_tick() + note.tick_len(),
                    MidiSrc::NoteOff {
                        pitch,
                        channel: note.channel,
                    },
                ));
            }

            ret
        }

        if sharp_flat == SharpFlat::Null {
            let pitch = match self
                .accidental
                .get(&(base_pitch.solfa(), base_pitch.octave()))
            {
                None => base_pitch.apply_key(self.key).unwrap_or(base_pitch),
                Some(sharp_flat) => {
                    Pitch::value_of(base_pitch.solfa(), base_pitch.octave(), *sharp_flat)
                        .unwrap_or(base_pitch)
                }
            };
            create_event(&note, pitch)
        } else {
            self.accidental.insert(
                (base_pitch.solfa(), base_pitch.octave()),
                base_pitch.sharp_flat(),
            );
            create_event(&note, base_pitch)
        }
    }

    fn on_dumper(&mut self, dumper: CtrlChg) -> Vec<(u32, MidiSrc)> {
        vec![(
            dumper.start_tick,
            MidiSrc::CtrlChg {
                channel: dumper.channel,
                number: 64,
                velocity: dumper.velocity,
            },
        )]
    }

    fn on_soft(&mut self, dumper: CtrlChg) -> Vec<(u32, MidiSrc)> {
        vec![(
            dumper.start_tick,
            MidiSrc::CtrlChg {
                channel: dumper.channel,
                number: 67,
                velocity: dumper.velocity,
            },
        )]
    }
}

#[derive(Clone)]
struct MidiEvents {
    events: BTreeMap<AccumTick, Vec<MidiSrc>>,
    tempo_table: Store<AccumTick, TempoValue, ()>,
    chunks: Store<AccumTick, Chunk, ()>,
}

impl MidiEvents {
    fn new(chunks: &[Chunk]) -> Self {
        Self {
            events: BTreeMap::new(),
            tempo_table: Store::new(false),
            chunks: Chunk::by_accum_tick(chunks),
        }
    }

    fn add_midi_event(&mut self, tick: AccumTick, m: MidiSrc) {
        match self.events.get_mut(&tick) {
            Some(found) => {
                found.push(m);
            }
            None => {
                self.events.insert(tick, vec![m]);
            }
        };
    }

    fn add_tempo(&mut self, tick: AccumTick, tempo: TempoValue) {
        self.tempo_table.add(tick, tempo, ());
    }

    fn cycles_by_accum_tick(
        &self,
        sampling_rate: usize,
        ticks_per_quarter: u32,
    ) -> Store<AccumTick, (TempoValue, u64), ()> {
        let mut cycles: u64 = 0;
        let mut buf = Store::with_capacity(self.tempo_table.len(), false);
        let mut prev_tick: AccumTick = 0;
        let mut prev_tempo: u16 = TempoValue::default().as_u16();

        for (tick, tempo) in self.tempo_table.iter() {
            cycles += (tick - prev_tick) as u64 * sampling_rate as u64 * 60
                / prev_tempo as u64
                / ticks_per_quarter as u64;
            buf.add(*tick, (*tempo, cycles), ());
            prev_tick = *tick;
            prev_tempo = tempo.as_u16();
        }

        buf
    }

    fn accum_tick_to_cycle(finder: &mut store::Finder<'_, u32, (TempoValue, u64), ()>, tick: AccumTick, sampling_rate: usize, ticks_per_quarter: u32) -> u64 {
        match finder.just_before(tick) {
            Some((t, (tempo, cycles))) => 
                *cycles + Self::tick_to_cycle(tick - *t, sampling_rate, tempo.as_u16(), ticks_per_quarter),
            None => 
                Self::tick_to_cycle(tick, sampling_rate, TempoValue::default().as_u16(), ticks_per_quarter),
        }
    }

    fn tick_to_cycle(tick: u32, sampling_rate: usize, tempo: u16, ticks_per_quarter: u32) -> u64 {
        tick as u64 * sampling_rate as u64 * 60
            / tempo as u64
            / ticks_per_quarter as u64
    }

    fn to_play_data(self, cycles_by_tick: Store<AccumTick, (TempoValue, u64), ()>, sampling_rate: usize, ticks_per_quarter: u32) -> PlayData {
        let mut cycles_by_tick = cycles_by_tick.finder();
        let mut midi_data = Store::new(false);
        let mut table_for_tracking = Store::new(false);

        for (tick, events) in self.events.iter() {
            let c = Self::accum_tick_to_cycle(&mut cycles_by_tick, *tick, sampling_rate, ticks_per_quarter);
            for e in events.iter() {
                let mut midi = vec![];
                e.render_to(&mut midi);

                midi_data.replace_mut(&c, (), |found: Option<&mut Vec<Vec<u8>>>| match found {
                    Some(current_midi_data) => {
                        current_midi_data.push(midi);
                        None
                    }
                    None => Some(vec![midi]),
                });
            }
        }

        for (tick, tempo) in self.tempo_table.iter() {
            let c = Self::accum_tick_to_cycle(&mut cycles_by_tick, *tick, sampling_rate, ticks_per_quarter);
            table_for_tracking.add(c, (*tick, tempo.clone()), ());
        }

        PlayData {
            midi_data,
            table_for_tracking,
            chunks: self.chunks,
        }
    }

//    fn play_start_cycle(&self, play_start_tick: PlayStartTick) -> u64 {
//
//    }
}

#[derive(Clone)]
pub enum Cmd {
    Play {
        seq: usize,
        play_data: PlayData,
        start_cycle: u64,
    },
    Stop {
        seq: usize,
    },
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum CmdError {
    AlreadyPlaying,
    MidiWriteError(String),
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum CmdInfo {
    PlayingEnded,
    CurrentLoc {
        seq: usize,
        tick: u32,
        accum_tick: AccumTick,
    },
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Resp {
    Err { seq: Option<usize>, error: CmdError },
    Info { seq: Option<usize>, info: CmdInfo },
    Ok { seq: usize },
}

pub enum PlayState {
    Init,
    Playing {
        seq: usize,
        current_loc: u64,
        play_data: PlayData,
    },
    Stopping,
}

#[cfg(not(test))]
pub struct JackClientProxy {
    client: Option<Client>,
    status: ClientStatus,
}

pub struct TestMidiWriter<'a> {
    _phantom: PhantomData<&'a ()>,
}

impl<'a> TestMidiWriter<'a> {
    pub fn write(&mut self, _message: &RawMidi) -> Result<(), jack::Error> {
        Ok(())
    }
}

#[cfg(not(test))]
impl JackClientProxy {
    pub fn new(app_name: &str, options: jack::ClientOptions) -> Result<Self, jack::Error> {
        let (client, status) = jack::Client::new(app_name, options)?;
        Ok(Self {
            client: Some(client),
            status,
        })
    }

    pub fn buffer_size(&self) -> u32 {
        self.client.as_ref().map(|c| c.buffer_size()).unwrap()
    }

    pub fn sampling_rate(&self) -> usize {
        self.client.as_ref().map(|c| c.sample_rate()).unwrap()
    }

    pub fn midi_out_port(&self) -> Result<Port<MidiOut>, jack::Error> {
        Ok(self
            .client
            .as_ref()
            .map(|c| c.register_port("MIDI OUT", jack::MidiOut::default()))
            .unwrap()?)
    }

    pub fn closer<F>(
        &mut self,
        callback: F,
    ) -> Result<Option<Box<dyn Send + 'static + FnOnce() -> Option<jack::Error>>>, jack::Error>
    where
        F: 'static + Send + FnMut(&Client, &ProcessScope) -> Control,
    {
        let active_client: AsyncClient<(), ClosureProcessHandler<_>> = {
            let client = self.client.take().unwrap();
            client.activate_async((), jack::ClosureProcessHandler::new(callback))?
        };
        let closer = move || active_client.deactivate().err();
        Ok(Some(Box::new(closer)))
    }
}

pub struct TestPort<T> {
    _phantom: PhantomData<T>,
}

impl<T> TestPort<T> {
    pub fn writer<'a>(&'a mut self, _ps: &'a ProcessScope) -> TestMidiWriter<'a> {
        TestMidiWriter {
            _phantom: PhantomData,
        }
    }
}

pub struct TestJackClientProxy {
    pub status: ClientStatus,

    pub app_name: String,
    pub buffer_size: u32,
    pub sampling_rate: usize,
}

#[cfg(not(test))]
use jack::Port;

impl TestJackClientProxy {
    pub fn new(_app_name: &str, _options: jack::ClientOptions) -> Result<Self, jack::Error> {
        panic!("You need to pass client_factory to Player::open() for testing.");
    }

    pub fn new_test(
        app_name: &str,
        _options: jack::ClientOptions,
        status: ClientStatus,
        buffer_size: u32,
        sampling_rate: usize,
    ) -> Result<Self, jack::Error> {
        Ok(Self {
            status,
            app_name: app_name.to_owned(),
            buffer_size,
            sampling_rate,
        })
    }

    pub fn buffer_size(&self) -> u32 {
        self.buffer_size
    }

    pub fn sampling_rate(&self) -> usize {
        self.sampling_rate
    }

    pub fn midi_out_port(&self) -> Result<TestPort<MidiOut>, jack::Error> {
        Ok(TestPort {
            _phantom: PhantomData,
        })
    }

    pub fn closer<F>(
        &mut self,
        _callback: F,
    ) -> Result<Option<Box<dyn Send + 'static + FnOnce() -> Option<jack::Error>>>, jack::Error>
    where
        F: 'static + Send + FnMut(&Client, &ProcessScope) -> Control,
    {
        thread::spawn(move || {
            let _cb = _callback;
            // Keep callback instance until test is finished.
            thread::sleep(time::Duration::from_secs(60));
        });

        Ok(None)
    }
}

#[cfg(test)]
use TestJackClientProxy as JCProxy;

#[cfg(not(test))]
use JackClientProxy as JCProxy;

#[cfg(test)]
use TestMidiWriter as MW;

#[cfg(not(test))]
use MidiWriter as MW;

impl Player {
    fn resp(resp_sender: &SyncSender<Resp>, resp: Resp) {
        if let Err(e) = resp_sender.send(resp) {
            println!("Cannot send response: {:?}.", e);
        }
    }

    fn perform_cmd(
        cmd_receiver: &Receiver<Cmd>,
        pstate: &mut PlayState,
        resp_sender: &SyncSender<Resp>,
    ) -> jack::Control {
        match cmd_receiver.try_recv() {
            Ok(cmd) => match cmd {
                Cmd::Play {
                    seq,
                    play_data,
                    start_cycle: start_loc,
                } => match pstate {
                    PlayState::Init => {
                        *pstate = PlayState::Playing {
                            seq,
                            current_loc: start_loc,
                            play_data,
                        };
                        Self::resp(&resp_sender, Resp::Ok { seq });
                    }
                    PlayState::Playing {
                        seq: _seq,
                        current_loc: _,
                        play_data: _,
                    } => {
                        Self::resp(
                            &resp_sender,
                            Resp::Err {
                                seq: Some(seq),
                                error: CmdError::AlreadyPlaying,
                            },
                        );
                    },
                    PlayState::Stopping => {
                        Self::resp(
                            &resp_sender,
                            Resp::Err {
                                seq: Some(seq),
                                error: CmdError::AlreadyPlaying,
                            },
                        );
                    },
                },
                Cmd::Stop { seq } => match pstate {
                    PlayState::Init => {
                        Self::resp(&resp_sender, Resp::Ok { seq });
                    }
                    PlayState::Playing {
                        seq: _seq,
                        current_loc: _,
                        play_data: _,
                    } => {
                        *pstate = PlayState::Stopping;
                        Self::resp(
                            &resp_sender,
                            Resp::Info {
                                seq: Some(seq),
                                info: CmdInfo::PlayingEnded,
                            },
                        );
                    },
                    PlayState::Stopping => {
                        Self::resp(&resp_sender, Resp::Ok { seq });
                    },
                },
            },
            Err(err) => {
                if err == std::sync::mpsc::TryRecvError::Disconnected {
                    return jack::Control::Quit;
                }
            }
        }

        jack::Control::Continue
    }

    fn perform_state<'a>(
        pstate: &mut PlayState,
        midi_writer: &mut MW<'a>,
        resp_sender: &SyncSender<Resp>,
        buffer_size: u32,
        sampling_rate: usize,
    ) {
        match pstate {
            PlayState::Init => {}
            PlayState::Playing {
                seq: playing_seq,
                current_loc,
                play_data,
            } => {
                let (_idx, data) = play_data
                    .midi_data
                    .range(*current_loc..(*current_loc + buffer_size as u64));
                for (cycle, midi) in data.iter() {
                    let offset = (cycle - *current_loc) as u32;

                    for bytes in midi.iter() {
                        if let Err(e) = midi_writer.write(&jack::RawMidi {
                            time: offset,
                            bytes,
                        }) {
                            Self::resp(
                                &resp_sender,
                                Resp::Err {
                                    seq: None,
                                    error: CmdError::MidiWriteError(e.to_string()),
                                },
                            )
                        }
                    }
                }

                let new_loc = *current_loc + buffer_size as u64;
                let ended = play_data
                    .midi_data
                    .peek_last()
                    .map(|(cycle, _midi)| *cycle < new_loc)
                    .unwrap_or(false);
                if ended {
                    Self::resp(
                        &resp_sender,
                        Resp::Info {
                            seq: Some(*playing_seq),
                            info: CmdInfo::PlayingEnded,
                        },
                    );
                    *pstate = PlayState::Init;
                } else {
                    let accum_tick = play_data.cycle_to_tick(*current_loc, sampling_rate as u32);
                    let tick = play_data.accum_tick_to_tick(accum_tick);
                    Self::resp(
                        &resp_sender,
                        Resp::Info {
                            seq: Some(*playing_seq),
                            info: CmdInfo::CurrentLoc {
                                seq: *playing_seq,
                                tick,
                                accum_tick,
                            },
                        },
                    );
                    *current_loc = new_loc;
                }
            },
            PlayState::Stopping => {
                let mut bytes: Vec<u8> = Vec::with_capacity(3 * 16);
                for ch in 0..16 {
                    bytes.push(0xB0 + ch); // Soft off
                    bytes.push(67);
                    bytes.push(0);
                }
                if let Err(e) = midi_writer.write(&jack::RawMidi { time: 0, bytes: &bytes }) {
                    Self::resp(
                        &resp_sender,
                        Resp::Err {
                            seq: None,
                            error: CmdError::MidiWriteError(e.to_string()),
                        },
                    )
                }

                let mut bytes: Vec<u8> = Vec::with_capacity(3 * 16);
                for ch in 0..16 {
                    bytes.push(0xB0 + ch); // All sound off
                    bytes.push(120);
                    bytes.push(0);
                }
                if let Err(e) = midi_writer.write(&jack::RawMidi { time: 0, bytes: &bytes }) {
                    Self::resp(
                        &resp_sender,
                        Resp::Err {
                            seq: None,
                            error: CmdError::MidiWriteError(e.to_string()),
                        },
                    )
                }

                let mut bytes: Vec<u8> = Vec::with_capacity(3 * 16);
                for ch in 0..16 {
                    bytes.push(0xB0 + ch); // Dumper off
                    bytes.push(64);
                    bytes.push(0);
                }
                if let Err(e) = midi_writer.write(&jack::RawMidi { time: 0, bytes: &bytes }) {
                    Self::resp(
                        &resp_sender,
                        Resp::Err {
                            seq: None,
                            error: CmdError::MidiWriteError(e.to_string()),
                        },
                    )
                }

                *pstate = PlayState::Init;
            },
        }
    }

    pub fn open(
        app_name: &str,
        client_factory: Option<
            Box<dyn FnOnce(&str, jack::ClientOptions) -> Result<JCProxy, jack::Error>>,
        >,
    ) -> Result<(Self, jack::ClientStatus), jack::Error> {
        let mut proxy = match client_factory {
            Some(factory) => factory(app_name, jack::ClientOptions::empty())?,
            None => JCProxy::new(app_name, jack::ClientOptions::empty())?,
        };
        let (cmd_sender, cmd_receiver) = sync_channel::<Cmd>(64);
        let (resp_sender, resp_receiver) = sync_channel::<Resp>(64);

        let buffer_size: u32 = proxy.buffer_size();
        let sampling_rate = proxy.sampling_rate();
        let mut state = PlayState::Init;
        let proxy_status = proxy.status;

        let mut midi_out_port = proxy.midi_out_port()?;
        let callback = move |_client: &jack::Client, ps: &jack::ProcessScope| -> jack::Control {
            let mut pstate = &mut state;
            let mut midi_writer = midi_out_port.writer(ps);

            if Self::perform_cmd(&cmd_receiver, &mut pstate, &resp_sender) == jack::Control::Quit {
                return jack::Control::Quit;
            }

            Self::perform_state(
                &mut pstate,
                &mut midi_writer,
                &resp_sender,
                buffer_size,
                sampling_rate,
            );

            jack::Control::Continue
        };

        let closer: Option<Box<dyn Send + 'static + FnOnce() -> Option<jack::Error>>> =
            proxy.closer(callback)?;

        Ok((
            Player {
                sampling_rate,
                cmd_channel: cmd_sender,
                resp_channel: Some(resp_receiver),
                closer,
            },
            proxy_status,
        ))
    }

    fn create_key_table(
        top_key: Key,
        bar_repo: &Store<u32, Bar, ModelChangeMetadata>,
    ) -> Store<u32, Key, ()> {
        let mut key_table: Store<u32, Key, ()> = Store::new(false);
        key_table.add(0, top_key, ());

        for (tick, bar) in bar_repo.iter() {
            if let Some(key) = bar.key {
                key_table.add(*tick, key, ());
            }
        }

        key_table
    }

    fn notes_by_base_start_tick(
        note_repo: &BagStore<u32, Rc<Note>, ModelChangeMetadata>,
    ) -> BagStore<u32, Rc<Note>, ()> {
        let mut notes = BagStore::new(false);

        for (_, note) in note_repo.iter() {
            notes.add(note.base_start_tick, note.clone(), ());
        }

        notes
    }

    fn create_midi_events(
        play_start_loc: Option<PlayStartTick>,
        top_rhythm: Rhythm,
        top_key: Key,
        note_repo: &BagStore<u32, Rc<Note>, ModelChangeMetadata>,
        bar_repo: &Store<u32, Bar, ModelChangeMetadata>,
        tempo_repo: &Store<u32, Tempo, ModelChangeMetadata>,
        dumper_repo: &Store<u32, CtrlChg, ModelChangeMetadata>,
        soft_repo: &Store<u32, CtrlChg, ModelChangeMetadata>,
    ) -> Result<(MidiEvents, Vec<RenderRegionWarning>), RenderRegionError> {
        let key_table = Self::create_key_table(top_key, bar_repo);
        let mut key_finder = key_table.finder();
        let notes_by_base_start_tick = Self::notes_by_base_start_tick(&note_repo);

        let (region, warnings) = render_region(top_rhythm, bar_repo.iter().map(|(_, bar)| bar))?;
        let imaginary_top_bar = vec![(
            0,
            Bar::new(0, Some(top_rhythm), Some(top_key), repeat_set!()),
        )];
        let imaginary_empty_bar = vec![];
        let imaginary_bottom_bar: Vec<(u32, Bar)> =
            vec![(u32::MAX, Bar::new(u32::MAX, None, None, repeat_set!()))];
        let mut offset: u32 = 0;

        let chunks = region.to_chunks();
        let mut events = MidiEvents::new(&chunks);
        for chunk in chunks.iter() {
            events.add_tempo(
                offset,
                tempo_at(chunk.start_tick(), tempo_repo),
            );

            let (_size, bars) = bar_repo.range(chunk.start_tick()..=chunk.end_tick());
            let top_bars: &Vec<(u32, Bar)> = match bars.get(0) {
                Some((_idx, top_bar)) => {
                    if top_bar.start_tick != 0 {
                        &imaginary_top_bar
                    } else {
                        &imaginary_empty_bar
                    }
                }
                None => &imaginary_top_bar,
            };

            let bottom_bars: &Vec<(u32, Bar)> = match bars.last() {
                Some((_idx, last_bar)) => {
                    if last_bar.start_tick < chunk.end_tick() {
                        &imaginary_bottom_bar
                    } else {
                        &imaginary_empty_bar
                    }
                }
                None => &imaginary_bottom_bar,
            };

            let mut bar_itr = top_bars
                .iter()
                .chain(bars.iter())
                .chain(bottom_bars.iter())
                .peekable();

            let mut notes = notes_by_base_start_tick
                .range(chunk.start_tick()..chunk.end_tick())
                .peekable();
            let (_idx, dumpers) = dumper_repo.range(chunk.start_tick()..chunk.end_tick());
            let mut dumpers = dumpers.iter().peekable();
            let (_idx, softs) = soft_repo.range(chunk.start_tick()..chunk.end_tick());
            let mut softs = softs.iter().peekable();

            for (bar_from, bar_to) in sliding(&mut bar_itr) {
                let mut key = *key_finder
                .just_before(bar_from.1.start_tick())
                .map(|(_tick, key)| key)
                .unwrap_or(&top_key);

                if let Some(new_key) = bar_from.1.key {
                    key = new_key;
                }
                let mut midi_reporter = MidiReporter::new(key);

                while let Some((_tick, note)) = notes.next_if(|(_tick, note)| {
                    bar_from.0 <= note.base_start_tick && note.base_start_tick < bar_to.0
                }) {
                    for (tick, midi_src) in midi_reporter.on_note((**note).clone()).iter() {
                        events.add_midi_event(tick + offset - chunk.start_tick(), *midi_src);
                    }
                }

                while let Some((_tick, dumper)) = dumpers.next_if(|(_tick, dumper)| {
                    bar_from.0 <= dumper.start_tick && dumper.start_tick < bar_to.0
                }) {
                    for (tick, midi_src) in midi_reporter.on_dumper(*dumper).iter() {
                        events.add_midi_event(tick + offset - chunk.start_tick(), *midi_src);
                    }
                }

                while let Some((_tick, soft)) = softs.next_if(|(_tick, dumper)| {
                    bar_from.0 <= dumper.start_tick && dumper.start_tick < bar_to.0
                }) {
                    for (tick, midi_src) in midi_reporter.on_soft(*soft).iter() {
                        events.add_midi_event(tick + offset - chunk.start_tick(), *midi_src);
                    }
                }
            }

            let (_, tempos) = tempo_repo.range(chunk.start_tick()..chunk.end_tick());
            for (tick, tempo) in tempos.iter() {
                events.add_tempo((tick - chunk.start_tick()) + offset, tempo.value);
            }

            if chunk.end_tick() != u32::MAX {
                offset += chunk.len();
            }
        }

        Ok((events, warnings))
    }

    pub fn play(
        &mut self,
        seq: usize,
        play_start_loc: Option<PlayStartTick>,
        top_rhythm: Rhythm,
        top_key: Key,
        note_repo: &BagStore<u32, Rc<Note>, ModelChangeMetadata>,
        bar_repo: &Store<u32, Bar, ModelChangeMetadata>,
        tempo_repo: &Store<u32, Tempo, ModelChangeMetadata>,
        dumper_repo: &Store<u32, CtrlChg, ModelChangeMetadata>,
        soft_repo: &Store<u32, CtrlChg, ModelChangeMetadata>,
    ) -> Result<Vec<RenderRegionWarning>, PlayError> {
        let (events, warnings) = Self::create_midi_events(
            play_start_loc,
            top_rhythm,
            top_key,
            note_repo,
            bar_repo,
            tempo_repo,
            dumper_repo,
            soft_repo,
        )
        .map_err(|e| PlayError::RenderError(e.current_context().clone()))?;

        let cycles_by_tick = events.cycles_by_accum_tick(self.sampling_rate, Duration::TICK_RESOLUTION as u32);
        let start_accum_tick = match play_start_loc {
            None => 0,
            Some(loc) => {
                match loc.to_accum_tick(&events.chunks) {
                    Ok(tick) => tick,
                    Err(err) => Err(PlayError::PlayStartTickError(err))?,
                }
            }
        };
        let start_cycle = MidiEvents::accum_tick_to_cycle(
            &mut cycles_by_tick.finder(), start_accum_tick, self.sampling_rate, Duration::TICK_RESOLUTION as u32
        );
        let play_data = events.to_play_data(cycles_by_tick, self.sampling_rate, Duration::TICK_RESOLUTION as u32);

        self.cmd_channel
            .send(Cmd::Play { seq, play_data, start_cycle })
            .map_err(|_e| PlayError::SendError { seq })?;
        Ok(warnings)
    }

    pub fn stop(&mut self, seq: usize) -> Result<(), PlayError> {
        self.cmd_channel
            .send(Cmd::Stop { seq })
            .map_err(|_e| PlayError::SendError { seq })?;
        Ok(())
    }

    /// Get response from the response receiver.
    pub fn get_resp(&self) -> Result<Resp, RecvError> {
        match &self.resp_channel {
            Some(resp) => Ok(resp.recv()?),
            None => {
                panic!("Once you call the take_resp(), you need to receive responses by yourself!")
            }
        }
    }

    /// Take the response receiver. Once you take the receiver, you cannot call get_resp() any more. You need to receive responses by yourself through the taken receiver.
    /// If you have already taken the receiver before, None will return.
    pub fn take_resp(&mut self) -> Option<Receiver<Resp>> {
        self.resp_channel.take()
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        if let Some(f) = self.closer.take() {
            let handle = thread::spawn(|| f());
            for _ in 0..50 {
                if handle.is_finished() {
                    break;
                }
                thread::sleep(time::Duration::from_millis(100));
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlayError {
    RenderError(RenderRegionError),
    SendError { seq: usize },
    PlayStartTickError(ToAccumTickError),
}

impl Display for PlayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl Context for PlayError {}

// pub struct TestPlayer {
//   pub sampling_rate: usize,
//   pub cmd_channel: SyncSender<Cmd>,
//   pub resp_channel: Option<Receiver<Resp>>,

//   pub cmd_receiver: Receiver<Cmd>,
//   pub resp_sender: SyncSender<Resp>,
//   pub name: String,
// }

// impl TestPlayer {
//   pub const SAMPLING_RATE: Cell<usize> = Cell::new(48000);
//   pub const CLIENT_STATUS: Cell<jack::ClientStatus> = Cell::new(jack::ClientStatus::empty());

//   pub fn open(
//     app_name: &str,
//     _client_factory: Option<Box<impl FnOnce(&str, jack::ClientOptions) -> Result<(Client, ClientStatus), jack::Error>>>
//   ) -> Result<(Self, jack::ClientStatus), jack::Error> {
//     let (cmd_sender, cmd_receiver) = sync_channel::<Cmd>(256);
//     let (resp_sender, resp_receiver) = sync_channel::<Resp>(256);

//     Ok((
//       Self {
//         sampling_rate: Self::SAMPLING_RATE.get(),
//         cmd_channel: cmd_sender,
//         resp_channel: Some(resp_receiver),
//         cmd_receiver,
//         resp_sender,
//         name: app_name.to_owned(),
//       },
//       Self::CLIENT_STATUS.get()
//     ))
//   }

//   pub fn play(
//     &mut self,
//     seq: usize, top_rhythm: Rhythm, top_key: Key,
//     note_repo: &BagStore<u32, Rc<Note>, ModelChangeMetadata>,
//     bar_repo: &Store<u32, Bar, ModelChangeMetadata>,
//     tempo_repo: &Store<u32, Tempo, ModelChangeMetadata>,
//     dumper_repo: &Store<u32, CtrlChg, ModelChangeMetadata>,
//     soft_repo: &Store<u32, CtrlChg, ModelChangeMetadata>,
//   ) -> Result <Vec<RenderRegionWarning>, PlayError> {
//     let (events, warnings) = Player::create_midi_events(
//       top_rhythm, top_key, note_repo, bar_repo, tempo_repo, dumper_repo, soft_repo
//     ).map_err(|e| PlayError::RenderError(e.current_context().clone()))?;
//     let play_data = events.to_play_data(self.sampling_rate, Duration::TICK_RESOLUTION as u32);
//     self.cmd_channel.send(Cmd::Play { seq, play_data }).map_err(|_e| PlayError::SendError { seq })?;
//     Ok(warnings)
//   }

//   pub fn stop(&mut self, seq: usize) -> Result <(), PlayError> {
//     self.cmd_channel.send(Cmd::Stop { seq }).map_err(|_e| PlayError::SendError { seq })?;
//     Ok(())
//   }

//   pub fn get_resp(&self) -> Result<Resp, RecvError> {
//     match &self.resp_channel {
//         Some(resp) => Ok(resp.recv()?),
//         None => panic!("Once you call the take_resp(), you need to receive responses by yourself!"),
//     }
//   }

//   pub fn take_resp(&mut self) -> Option<Receiver<Resp>> {
//     self.resp_channel.take()
//   }
// }

#[cfg(test)]
mod tests {
    use super::{Cmd, PlayState, Player, Resp};
    use crate::player::TestJackClientProxy;
    use crate::player::{CmdError, CmdInfo, MidiSrc};
    use jack::ClientStatus;
    use klavier_core::bar::RepeatSet;
    use klavier_core::duration::Duration;
    use klavier_core::{
        bar::{Bar, Repeat},
        key::Key,
        note::Note,
        octave::Octave,
        pitch::Pitch,
        project::ModelChangeMetadata,
        repeat_set,
        rhythm::Rhythm,
        sharp_flat::SharpFlat,
        solfa::Solfa,
        tempo::{Tempo, TempoValue},
        trimmer::Trimmer,
        velocity::Velocity,
    };
    use klavier_helper::{bag_store::BagStore, store::Store};
    use std::{rc::Rc, sync::mpsc::sync_channel};

    #[test]
    fn empty() {
        let (events, _warnings) = Player::create_midi_events(
            None,
            Rhythm::new(4, 4),
            Key::NONE,
            &BagStore::new(false),
            &Store::new(false),
            &Store::new(false),
            &Store::new(false),
            &Store::new(false),
        )
        .unwrap();
        assert_eq!(events.events.len(), 0);
    }

    #[test]
    fn apply_tune_key() {
        let mut note_repo = BagStore::new(false);
        let note0 = Rc::new(Note {
            base_start_tick: 100,
            pitch: Pitch::new(Solfa::A, Octave::Oct4, SharpFlat::Null),
            ..Default::default()
        });
        note_repo.add(
            note0.start_tick(),
            note0.clone(),
            ModelChangeMetadata::new(),
        );

        let note1 = Rc::new(Note {
            base_start_tick: 200,
            pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Sharp),
            ..Default::default()
        });
        note_repo.add(
            note1.start_tick(),
            note1.clone(),
            ModelChangeMetadata::new(),
        );

        let note2 = Rc::new(Note {
            base_start_tick: 300,
            pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Null),
            start_tick_trimmer: Trimmer::new(-5, -10, 0, 0),
            ..Default::default()
        });
        note_repo.add(
            note2.start_tick(),
            note2.clone(),
            ModelChangeMetadata::new(),
        );

        let (events, _warnings) = Player::create_midi_events(
            None,
            Rhythm::new(4, 4),
            Key::FLAT_1,
            &note_repo,
            &Store::new(false),
            &Store::new(false),
            &Store::new(false),
            &Store::new(false),
        )
        .unwrap();
        assert_eq!(events.events.len(), 6);
        assert_eq!(
            events.events.get(&100).unwrap(),
            &vec![MidiSrc::NoteOn {
                pitch: note0.pitch,
                velocity: note0.velocity(),
                channel: Default::default()
            }]
        );
        assert_eq!(
            events
                .events
                .get(&(100 + note0.duration.tick_length()))
                .unwrap(),
            &vec![MidiSrc::NoteOff {
                pitch: note0.pitch,
                channel: Default::default()
            }]
        );
        assert_eq!(
            events.events.get(&200).unwrap(),
            &vec![MidiSrc::NoteOn {
                pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Sharp),
                velocity: note1.velocity(),
                channel: Default::default()
            }]
        );
        assert_eq!(
            events
                .events
                .get(&(200 + note1.duration.tick_length()))
                .unwrap(),
            &vec![MidiSrc::NoteOff {
                pitch: note1.pitch,
                channel: Default::default()
            }]
        );
        assert_eq!(
            events.events.get(&(300 - 5 - 10)).unwrap(),
            &vec![MidiSrc::NoteOn {
                pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Sharp),
                velocity: note2.velocity(),
                channel: Default::default()
            }]
        );
        assert_eq!(
            events
                .events
                .get(&(300 - 5 - 10 + note2.duration.tick_length()))
                .unwrap(),
            &vec![MidiSrc::NoteOff {
                pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Sharp),
                channel: Default::default()
            }]
        );
    }

    #[test]
    fn at_same_tick() {
        let mut note_repo = BagStore::new(false);
        let note0 = Rc::new(Note {
            base_start_tick: 100,
            pitch: Pitch::new(Solfa::A, Octave::Oct4, SharpFlat::Null),
            ..Default::default()
        });
        note_repo.add(
            note0.start_tick(),
            note0.clone(),
            ModelChangeMetadata::new(),
        );

        let note1 = Rc::new(Note {
            base_start_tick: 100,
            pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Sharp),
            ..Default::default()
        });
        note_repo.add(
            note1.start_tick(),
            note1.clone(),
            ModelChangeMetadata::new(),
        );

        let (events, _warnings) = Player::create_midi_events(
            None,
            Rhythm::new(4, 4),
            Key::FLAT_1,
            &note_repo,
            &Store::new(false),
            &Store::new(false),
            &Store::new(false),
            &Store::new(false),
        )
        .unwrap();
        assert_eq!(events.events.len(), 2);
        assert_eq!(
            events.events.get(&100).unwrap(),
            &vec![
                MidiSrc::NoteOn {
                    pitch: note0.pitch,
                    velocity: note0.velocity(),
                    channel: Default::default()
                },
                MidiSrc::NoteOn {
                    pitch: note1.pitch,
                    velocity: note1.velocity(),
                    channel: Default::default()
                },
            ]
        );
        assert_eq!(
            events
                .events
                .get(&(100 + note0.duration.tick_length()))
                .unwrap(),
            &vec![
                MidiSrc::NoteOff {
                    pitch: note0.pitch,
                    channel: Default::default()
                },
                MidiSrc::NoteOff {
                    pitch: note1.pitch,
                    channel: Default::default()
                },
            ]
        );

        let cycles_by_tick = events.cycles_by_accum_tick(48000, Duration::TICK_RESOLUTION as u32);
        let play_data = events.to_play_data(cycles_by_tick, 48000, 240);
        let midi_data = &play_data.midi_data;
        assert_eq!(midi_data.len(), 2);

        let cycle = 100 * 48000 * 60 / 120 / 240;
        assert_eq!(
            midi_data[0],
            (
                cycle,
                vec![
                    vec![
                        0x90,
                        Pitch::new(Solfa::A, Octave::Oct4, SharpFlat::Null).value(),
                        Velocity::default().as_u8()
                    ],
                    vec![
                        0x90,
                        Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Sharp).value(),
                        Velocity::default().as_u8()
                    ],
                ]
            )
        );
        assert_eq!(play_data.cycle_to_tick(cycle, 48000), 100);

        let cycle = ((100 + note0.duration.tick_length()) * 48000 * 60 / 120 / 240) as u64;
        assert_eq!(
            midi_data[1],
            (
                cycle,
                vec![
                    vec![
                        0x90,
                        Pitch::new(Solfa::A, Octave::Oct4, SharpFlat::Null).value(),
                        0
                    ],
                    vec![
                        0x90,
                        Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Sharp).value(),
                        0
                    ]
                ]
            )
        );
        assert_eq!(play_data.cycle_to_tick(cycle, 48000), 340);
    }

    #[test]
    fn one_bar() {
        let mut note_repo = BagStore::new(false);
        let note0 = Rc::new(Note {
            base_start_tick: 100,
            pitch: Pitch::new(Solfa::A, Octave::Oct4, SharpFlat::Null),
            ..Default::default()
        });
        note_repo.add(
            note0.start_tick(),
            note0.clone(),
            ModelChangeMetadata::new(),
        );

        let note1 = Rc::new(Note {
            base_start_tick: 200,
            pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Sharp),
            ..Default::default()
        });
        note_repo.add(
            note1.start_tick(),
            note1.clone(),
            ModelChangeMetadata::new(),
        );

        let note2 = Rc::new(Note {
            base_start_tick: 300,
            pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Null),
            start_tick_trimmer: Trimmer::new(-5, -10, 0, 0),
            ..Default::default()
        });
        note_repo.add(
            note2.start_tick(),
            note2.clone(),
            ModelChangeMetadata::new(),
        );

        let mut bar_repo: Store<u32, Bar, ModelChangeMetadata> = Store::new(false);
        bar_repo.add(
            300,
            Bar::new(300, None, Some(Key::FLAT_1), repeat_set!()),
            ModelChangeMetadata::new(),
        );

        let (events, _warnings) = Player::create_midi_events(
            None,
            Rhythm::new(4, 4),
            Key::SHARP_1,
            &note_repo,
            &bar_repo,
            &Store::new(false),
            &Store::new(false),
            &Store::new(false),
        )
        .unwrap();
        assert_eq!(events.events.len(), 6);
        assert_eq!(
            events.events.get(&100).unwrap(),
            &vec![MidiSrc::NoteOn {
                pitch: note0.pitch,
                velocity: note0.velocity(),
                channel: Default::default()
            }]
        );
        assert_eq!(
            events
                .events
                .get(&(100 + note0.duration.tick_length()))
                .unwrap(),
            &vec![MidiSrc::NoteOff {
                pitch: note0.pitch,
                channel: Default::default()
            }]
        );
        assert_eq!(
            events.events.get(&200).unwrap(),
            &vec![MidiSrc::NoteOn {
                pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Sharp),
                velocity: note1.velocity(),
                channel: Default::default()
            }]
        );
        assert_eq!(
            events
                .events
                .get(&(200 + note1.duration.tick_length()))
                .unwrap(),
            &vec![MidiSrc::NoteOff {
                pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Sharp),
                channel: Default::default()
            }]
        );
        assert_eq!(
            events.events.get(&(300 - 5 - 10)).unwrap(),
            &vec![MidiSrc::NoteOn {
                pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Flat),
                velocity: note2.velocity(),
                channel: Default::default()
            }]
        );
        assert_eq!(
            events
                .events
                .get(&(300 - 5 - 10 + note2.duration.tick_length()))
                .unwrap(),
            &vec![MidiSrc::NoteOff {
                pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Flat),
                channel: Default::default()
            }]
        );
    }

    #[test]
    fn with_repeat() {
        let mut note_repo = BagStore::new(false);
        let note0 = Rc::new(Note {
            base_start_tick: 100,
            pitch: Pitch::new(Solfa::A, Octave::Oct4, SharpFlat::Null),
            ..Default::default()
        });
        note_repo.add(
            note0.start_tick(),
            note0.clone(),
            ModelChangeMetadata::new(),
        );

        let note1 = Rc::new(Note {
            base_start_tick: 200,
            pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Sharp),
            start_tick_trimmer: Trimmer::new(-1, 0, 0, 0),
            ..Default::default()
        });
        note_repo.add(
            note1.start_tick(),
            note1.clone(),
            ModelChangeMetadata::new(),
        );

        let note2 = Rc::new(Note {
            base_start_tick: 300,
            pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Null),
            start_tick_trimmer: Trimmer::new(-5, -10, 0, 0),
            ..Default::default()
        });
        note_repo.add(
            note2.start_tick(),
            note2.clone(),
            ModelChangeMetadata::new(),
        );

        let mut bar_repo: Store<u32, Bar, ModelChangeMetadata> = Store::new(false);
        bar_repo.add(
            200,
            Bar::new(200, None, Some(Key::FLAT_1), repeat_set!(Repeat::End)),
            ModelChangeMetadata::new(),
        );

        let (events, _warnings) = Player::create_midi_events(
            None,
            Rhythm::new(4, 4),
            Key::SHARP_1,
            &note_repo,
            &bar_repo,
            &Store::new(false),
            &Store::new(false),
            &Store::new(false),
        )
        .unwrap();
        assert_eq!(events.events.len(), 8);
        assert_eq!(
            events.events.get(&100).unwrap(),
            &vec![MidiSrc::NoteOn {
                pitch: note0.pitch,
                velocity: note0.velocity(),
                channel: Default::default()
            }]
        );
        assert_eq!(
            events
                .events
                .get(&(100 + note0.duration.tick_length()))
                .unwrap(),
            &vec![MidiSrc::NoteOff {
                pitch: note0.pitch,
                channel: Default::default()
            }]
        );
        assert_eq!(
            events.events.get(&300).unwrap(),
            &vec![MidiSrc::NoteOn {
                pitch: note0.pitch,
                velocity: note0.velocity(),
                channel: Default::default()
            }]
        );
        assert_eq!(
            events
                .events
                .get(&(300 + note0.duration.tick_length()))
                .unwrap(),
            &vec![MidiSrc::NoteOff {
                pitch: note0.pitch,
                channel: Default::default()
            }]
        );
        assert_eq!(
            events.events.get(&(400 - 1)).unwrap(),
            &vec![MidiSrc::NoteOn {
                pitch: note1.pitch,
                velocity: note1.velocity(),
                channel: Default::default()
            }]
        );
        assert_eq!(
            events
                .events
                .get(&(400 - 1 + note1.duration.tick_length()))
                .unwrap(),
            &vec![MidiSrc::NoteOff {
                pitch: note1.pitch,
                channel: Default::default()
            }]
        );
        assert_eq!(
            events.events.get(&(500 - 5 - 10)).unwrap(),
            &vec![MidiSrc::NoteOn {
                pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Sharp),
                velocity: note2.velocity(),
                channel: Default::default()
            }]
        );
        assert_eq!(
            events
                .events
                .get(&(500 - 5 - 10 + note2.duration.tick_length()))
                .unwrap(),
            &vec![MidiSrc::NoteOff {
                pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Sharp),
                channel: Default::default()
            }]
        );
    }

    //      0    100  200  300  400 480 560   720        960
    //      V    V    V    V    V  :|   V     V          |
    //      <-- note0 --><--note1--><--note2--><--note3-->
    // tempo|120                |30     120
    #[test]
    fn repeat_and_tempo() {
        let mut note_repo = BagStore::new(false);
        let mut bar_repo: Store<u32, Bar, ModelChangeMetadata> = Store::new(false);
        let mut tempo_repo = Store::new(false);

        let note0 = Rc::new(Note {
            base_start_tick: 0, pitch: Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Null), ..Default::default()
        });
        note_repo.add(
            note0.start_tick(), note0.clone(), ModelChangeMetadata::new(),
        );

        let note1 = Rc::new(Note {
            base_start_tick: 240, pitch: Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Null), ..Default::default()
        });
        note_repo.add(
            note1.start_tick(), note1.clone(), ModelChangeMetadata::new(),
        );

        let note2 = Rc::new(Note {
            base_start_tick: 480, pitch: Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Null), ..Default::default()
        });
        note_repo.add(
            note2.start_tick(), note2.clone(), ModelChangeMetadata::new(),
        );

        let note3 = Rc::new(Note {
            base_start_tick: 720, pitch: Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Null), ..Default::default()
        });
        note_repo.add(
            note3.start_tick(), note3.clone(), ModelChangeMetadata::new(),
        );

        tempo_repo.add(400, Tempo::new(400, 30), ModelChangeMetadata::new());
        tempo_repo.add(560, Tempo::new(560, 120), ModelChangeMetadata::new());

        bar_repo.add(
            480,
            Bar::new(480, None, None, repeat_set!(Repeat::End)),
            ModelChangeMetadata::new(),
        );

        bar_repo.add(
            960,
            Bar::new(960, None, None, repeat_set!()),
            ModelChangeMetadata::new(),
        );

        let (events, _warnings) = Player::create_midi_events(
            None,
            Rhythm::new(2, 4),
            Key::SHARP_1,
            &note_repo,
            &bar_repo,
            &tempo_repo,
            &Store::new(false),
            &Store::new(false),
        ).unwrap();

        let cycles_by_tick = events.cycles_by_accum_tick(48000, Duration::TICK_RESOLUTION as u32);
        let play_data = events.to_play_data(cycles_by_tick, 48000, 240);
        let midi_data = &play_data.midi_data;

        // tick: 400, cycle = 40000
        // tick: 480, cycle = 40000 + 32000 = 72000
        // tick: 720, cycle = 72000 + 240*48000*60/120/240 = 96000
        // tick: 960, cycle = 96000 + (240-80)*48000*60/120/240 + 80*48000*60/30/240 = 144000
        // tick: 1040, cycle = 144000 + 80*48000*60/30/240 = 176000
        // tick: 1200, cycle = 176000 + (240-80)*48000*60/120/240 = 192000
        // tick: 1440, cycle = 192000 + 240*48000*60/120/240 = 216000

        let cycle = 0 * 48000 * 60 / 120 / 240;
        assert_eq!(
            midi_data[0],
            (
                cycle,
                vec![vec![
                    0x90, Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Sharp).value(), Velocity::default().as_u8()
                ]]
            )
        );
        assert_eq!(play_data.cycle_to_tick(cycle, 48000), 0);

        let cycle = 240 * 48000 * 60 / 120 / 240;
        assert_eq!(
            midi_data[1],
            (
                cycle,
                vec![vec![
                    0x90, Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Sharp).value(), 0
                ], vec![
                    0x90, Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Sharp).value(), Velocity::default().as_u8()
                ]]
            )
        );
        assert_eq!(play_data.cycle_to_tick(cycle, 48000), 240);

        let cycle = 400 * 48000 * 60 / 120 / 240
             + 80 * 48000 * 60 / 30 / 240;
        assert_eq!(
            midi_data[2],
            (
                cycle,
                vec![vec![
                    0x90, Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Sharp).value(), 0
                ], vec![
                    0x90, Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Sharp).value(), Velocity::default().as_u8()
                ]]
            )
        );
        assert_eq!(play_data.cycle_to_tick(cycle, 48000), 480);

        let cycle = cycle
             + 240 * 48000 * 60 / 120 / 240;
        assert_eq!(
            midi_data[3],
            (
                cycle,
                vec![vec![
                    0x90, Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Sharp).value(), 0
                ], vec![
                    0x90, Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Sharp).value(), Velocity::default().as_u8()
                ]]
            )
        );
        assert_eq!(play_data.cycle_to_tick(cycle, 48000), 480 + 240);

        let cycle = cycle + (240-80)*48000*60/120/240 + 80*48000*60/30/240;
        assert_eq!(
            midi_data[4],
            (
                cycle,
                vec![vec![
                    0x90, Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Sharp).value(), 0
                ], vec![
                    0x90, Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Sharp).value(), Velocity::default().as_u8()
                ]]
            )
        );
        assert_eq!(play_data.cycle_to_tick(cycle, 48000), 960);

        let cycle = cycle + 80*48000*60/30/240 + (240-80)*48000*60/120/240;
        assert_eq!(
            midi_data[5],
            (
                cycle,
                vec![vec![
                    0x90, Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Sharp).value(), 0
                ], vec![
                    0x90, Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Sharp).value(), Velocity::default().as_u8()
                ]]
            )
        );
        assert_eq!(play_data.cycle_to_tick(cycle, 48000), 1200);

        let cycle = cycle + 240*48000*60/120/240;
        assert_eq!(
            midi_data[6],
            (
                cycle,
                vec![vec![
                    0x90, Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Sharp).value(), 0
                ]]
            )
        );
        assert_eq!(play_data.cycle_to_tick(cycle, 48000), 1440);
    }

    #[test]
    fn with_dc() {
        let mut note_repo = BagStore::new(false);
        let note0 = Rc::new(Note {
            base_start_tick: 100,
            pitch: Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Null),
            ..Default::default()
        });
        note_repo.add(
            note0.start_tick(),
            note0.clone(),
            ModelChangeMetadata::new(),
        );

        let note1 = Rc::new(Note {
            base_start_tick: 500,
            pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Sharp),
            ..Default::default()
        });
        note_repo.add(
            note1.start_tick(),
            note1.clone(),
            ModelChangeMetadata::new(),
        );

        let note2 = Rc::new(Note {
            base_start_tick: 600,
            pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Null),
            ..Default::default()
        });
        note_repo.add(
            note2.start_tick(),
            note2.clone(),
            ModelChangeMetadata::new(),
        );

        let mut bar_repo: Store<u32, Bar, ModelChangeMetadata> = Store::new(false);
        bar_repo.add(
            480,
            Bar::new(
                480,
                None,
                Some(Key::FLAT_1),
                repeat_set!(Repeat::End, Repeat::Fine),
            ),
            ModelChangeMetadata::new(),
        );
        bar_repo.add(
            960,
            Bar::new(960, None, None, repeat_set!(Repeat::Dc)),
            ModelChangeMetadata::new(),
        );

        let mut tempo_repo = Store::new(false);
        tempo_repo.add(200, Tempo::new(200, 300), ModelChangeMetadata::new());

        //      0    100  200  300 340 400   480 500  600      740   840 960
        //      V    V    V    V   V   V     :|  V    V        V     V   |D.C.
        //            <-- note0 -->               <-- note1 -->      V
        //                                             <-- note 2 -->
        // tempo|120      |300

        let (events, _warnings) = Player::create_midi_events(
            None,
            Rhythm::new(2, 4),
            Key::SHARP_1,
            &note_repo,
            &bar_repo,
            &tempo_repo,
            &Store::new(false),
            &Store::new(false),
        )
        .unwrap();
        assert_eq!(events.events.len(), 10);
        assert_eq!(
            events.events.get(&100).unwrap(),
            &vec![MidiSrc::NoteOn {
                pitch: Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Sharp),
                velocity: note0.velocity(),
                channel: Default::default()
            }]
        );
        assert_eq!(
            events
                .events
                .get(&(100 + note0.duration.tick_length()))
                .unwrap(),
            &vec![MidiSrc::NoteOff {
                pitch: Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Sharp),
                channel: Default::default()
            }]
        );
        assert_eq!(
            events.events.get(&580).unwrap(),
            &vec![MidiSrc::NoteOn {
                pitch: Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Sharp),
                velocity: note0.velocity(),
                channel: Default::default()
            }]
        );
        assert_eq!(
            events
                .events
                .get(&(580 + note0.duration.tick_length()))
                .unwrap(),
            &vec![MidiSrc::NoteOff {
                pitch: Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Sharp),
                channel: Default::default()
            }]
        );
        assert_eq!(
            events.events.get(&980).unwrap(),
            &vec![MidiSrc::NoteOn {
                pitch: note1.pitch,
                velocity: note1.velocity(),
                channel: Default::default()
            }]
        );
        assert_eq!(
            events
                .events
                .get(&(980 + note1.duration.tick_length()))
                .unwrap(),
            &vec![MidiSrc::NoteOff {
                pitch: note1.pitch,
                channel: Default::default()
            }]
        );
        assert_eq!(
            events.events.get(&1080).unwrap(),
            &vec![MidiSrc::NoteOn {
                pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Sharp),
                velocity: note2.velocity(),
                channel: Default::default()
            }]
        );
        assert_eq!(
            events
                .events
                .get(&(1080 + note2.duration.tick_length()))
                .unwrap(),
            &vec![MidiSrc::NoteOff {
                pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Sharp),
                channel: Default::default()
            }]
        );
        assert_eq!(
            events.events.get(&(480 * 3 + 100)).unwrap(),
            &vec![MidiSrc::NoteOn {
                pitch: Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Sharp),
                velocity: note0.velocity(),
                channel: Default::default()
            }]
        );
        assert_eq!(
            events
                .events
                .get(&(480 * 3 + 100 + note0.duration.tick_length()))
                .unwrap(),
            &vec![MidiSrc::NoteOff {
                pitch: Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Sharp),
                channel: Default::default()
            }]
        );

        assert_eq!(events.tempo_table.len(), 7);
        let mut z = events.tempo_table.iter();
        assert_eq!(z.next(), Some(&(0, TempoValue::new(120))));
        assert_eq!(z.next(), Some(&(200, TempoValue::new(300))));
        assert_eq!(z.next(), Some(&(480, TempoValue::new(120))));
        assert_eq!(z.next(), Some(&(680, TempoValue::new(300))));
        assert_eq!(z.next(), Some(&(480 * 2, TempoValue::new(300))));
        assert_eq!(z.next(), Some(&(480 * 3, TempoValue::new(120))));
        assert_eq!(z.next(), Some(&(480 * 3 + 200, TempoValue::new(300))));
        assert_eq!(z.next(), None);

        let cycles_by_tick = events.cycles_by_accum_tick(48000, Duration::TICK_RESOLUTION as u32);
        let play_data = events.to_play_data(cycles_by_tick, 48000, 240);
        let midi_data = &play_data.midi_data;
        assert_eq!(midi_data.len(), 10);

        //      0    100  200  300 340 400   480 500  600      740   840 960
        //      |    |    |    |   |   |     :|  |    |        |     |   |D.C.
        //      |     <-- note0 -->               <-- note1 -->      |
        //      |                                      <-- note 2 -->
        // tempo|120      |300

        let cycle = 100 * 48000 * 60 / 120 / 240;
        assert_eq!(
            midi_data[0],
            (
                cycle,
                vec![vec![
                    0x90,
                    Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Sharp).value(),
                    Velocity::default().as_u8()
                ]]
            )
        );
        assert_eq!(play_data.cycle_to_tick(cycle, 48000), 100);

        let cycle = (200 * 48000 * 60 / 120 + 140 * 48000 * 60 / 300) / 240;
        assert_eq!(
            midi_data[1],
            (
                cycle,
                vec![vec![
                    0x90,
                    Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Sharp).value(),
                    0
                ]]
            )
        );
        assert_eq!(play_data.cycle_to_tick(cycle, 48000), 340);

        let cycle =
            (200 * 48000 * 60 / 120 + 280 * 48000 * 60 / 300 + 100 * 48000 * 60 / 120) / 240;
        assert_eq!(
            midi_data[2],
            (
                cycle,
                vec![vec![
                    0x90,
                    Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Sharp).value(),
                    Velocity::default().as_u8()
                ]]
            )
        );
        assert_eq!(play_data.cycle_to_tick(cycle, 48000), 580);
        assert_eq!(play_data.accum_tick_to_tick(580), 100);

        let cycle = (200 * 48000 * 60 / 120
            + 280 * 48000 * 60 / 300
            + 200 * 48000 * 60 / 120
            + 140 * 48000 * 60 / 300)
            / 240;
        assert_eq!(
            midi_data[3],
            (
                cycle,
                vec![vec![
                    0x90,
                    Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Sharp).value(),
                    0
                ]]
            )
        );
        assert_eq!(play_data.cycle_to_tick(cycle, 48000), 820);
        assert_eq!(play_data.accum_tick_to_tick(820), 340);

        let cycle = (200 * 48000 * 60 / 120
            + 280 * 48000 * 60 / 300
            + 200 * 48000 * 60 / 120
            + 300 * 48000 * 60 / 300)
            / 240;
        assert_eq!(
            midi_data[4],
            (
                cycle,
                vec![vec![0x90, note1.pitch.value(), Velocity::default().as_u8()]]
            )
        );
        assert_eq!(play_data.cycle_to_tick(cycle, 48000), 980);
        assert_eq!(play_data.accum_tick_to_tick(980), 500);

        let cycle = (200 * 48000 * 60 / 120
            + 280 * 48000 * 60 / 300
            + 200 * 48000 * 60 / 120
            + 400 * 48000 * 60 / 300)
            / 240;
        assert_eq!(
            midi_data[5],
            (
                cycle,
                vec![vec![
                    0x90,
                    Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Sharp).value(),
                    Velocity::default().as_u8()
                ]]
            )
        );
        assert_eq!(play_data.cycle_to_tick(cycle, 48000), 1080);
        assert_eq!(play_data.accum_tick_to_tick(1080), 600);

        let cycle = (200 * 48000 * 60 / 120
            + 280 * 48000 * 60 / 300
            + 200 * 48000 * 60 / 120
            + 540 * 48000 * 60 / 300)
            / 240;
        assert_eq!(
            midi_data[6],
            (cycle, vec![vec![0x90, note1.pitch.value(), 0]])
        );
        assert_eq!(play_data.cycle_to_tick(cycle, 48000), 1220);
        assert_eq!(play_data.accum_tick_to_tick(1220), 740);

        let cycle = (200 * 48000 * 60 / 120
            + 280 * 48000 * 60 / 300
            + 200 * 48000 * 60 / 120
            + 640 * 48000 * 60 / 300)
            / 240;
        assert_eq!(
            midi_data[7],
            (cycle, vec![vec![0x90, note1.pitch.value(), 0]])
        );
        assert_eq!(play_data.cycle_to_tick(cycle, 48000), 1320);
        assert_eq!(play_data.accum_tick_to_tick(1320), 840);

        let cycle = (200 * 48000 * 60 / 120
            + 280 * 48000 * 60 / 300
            + 200 * 48000 * 60 / 120
            + 760 * 48000 * 60 / 300
            + 100 * 48000 * 60 / 120)
            / 240;
        assert_eq!(
            midi_data[8],
            (
                cycle,
                vec![vec![
                    0x90,
                    Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Sharp).value(),
                    Velocity::default().as_u8()
                ]]
            )
        );
        assert_eq!(play_data.cycle_to_tick(cycle, 48000), 1540);
        assert_eq!(play_data.accum_tick_to_tick(1540), 100);

        let cycle = (200 * 48000 * 60 / 120
            + 280 * 48000 * 60 / 300
            + 200 * 48000 * 60 / 120
            + 760 * 48000 * 60 / 300
            + 200 * 48000 * 60 / 120
            + 140 * 48000 * 60 / 300)
            / 240;
        assert_eq!(
            midi_data[9],
            (
                cycle,
                vec![vec![
                    0x90,
                    Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Sharp).value(),
                    0
                ]]
            )
        );
        assert_eq!(play_data.cycle_to_tick(cycle, 48000), 1780);
        assert_eq!(play_data.accum_tick_to_tick(1780), 340);
    }

    #[test]
    fn with_ds() {
        let mut note_repo = BagStore::new(false);
        let note0 = Rc::new(Note {
            base_start_tick: 100,
            pitch: Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Null),
            ..Default::default()
        });
        note_repo.add(
            note0.start_tick(),
            note0.clone(),
            ModelChangeMetadata::new(),
        );
        let note1 = Rc::new(Note {
            base_start_tick: 500,
            pitch: Pitch::new(Solfa::G, Octave::Oct4, SharpFlat::Null),
            ..Default::default()
        });
        note_repo.add(
            note1.start_tick(),
            note1.clone(),
            ModelChangeMetadata::new(),
        );

        let mut bar_repo: Store<u32, Bar, ModelChangeMetadata> = Store::new(false);
        bar_repo.add(
            480,
            Bar::new(480, None, None, repeat_set!(Repeat::Segno)),
            ModelChangeMetadata::new(),
        );
        bar_repo.add(
            960,
            Bar::new(960, None, None, repeat_set!(Repeat::Ds)),
            ModelChangeMetadata::new(),
        );

        let tempo_repo = Store::new(false);

        let (events, _warnings) = Player::create_midi_events(
            None,
            Rhythm::new(2, 4),
            Key::NONE,
            &note_repo,
            &bar_repo,
            &tempo_repo,
            &Store::new(false),
            &Store::new(false),
        )
        .unwrap();

        assert_eq!(events.events.len(), 6);
        assert_eq!(
            events.events.get(&100).unwrap(),
            &vec![MidiSrc::NoteOn {
                pitch: note0.pitch,
                velocity: note0.velocity(),
                channel: Default::default()
            }]
        );
        assert_eq!(
            events.events.get(&340).unwrap(),
            &vec![MidiSrc::NoteOff {
                pitch: note0.pitch,
                channel: Default::default()
            }]
        );

        assert_eq!(
            events.events.get(&500).unwrap(),
            &vec![MidiSrc::NoteOn {
                pitch: note1.pitch,
                velocity: note1.velocity(),
                channel: Default::default()
            }]
        );
        assert_eq!(
            events.events.get(&740).unwrap(),
            &vec![MidiSrc::NoteOff {
                pitch: note1.pitch,
                channel: Default::default()
            }]
        );

        assert_eq!(
            events.events.get(&980).unwrap(),
            &vec![MidiSrc::NoteOn {
                pitch: note1.pitch,
                velocity: note1.velocity(),
                channel: Default::default()
            }]
        );
        assert_eq!(
            events.events.get(&1220).unwrap(),
            &vec![MidiSrc::NoteOff {
                pitch: note1.pitch,
                channel: Default::default()
            }]
        );

        let cycles_by_tick = events.cycles_by_accum_tick(48000, Duration::TICK_RESOLUTION as u32);
        let play_data = events.to_play_data(cycles_by_tick, 48000, 240);
        let midi_data = &play_data.midi_data;

        //      0    100  200  300 340 400 480    500  600  700 740   900 960
        //      |    |    |    |   |   |   |Segno |    |    |   |     |   |D.S.
        //            <-- note0 -->                <-- note1 -->
        // tempo|120

        let cycle = 100 * 48000 * 60 / 120 / 240;
        assert_eq!(
            midi_data[0],
            (
                cycle,
                vec![vec![0x90, note0.pitch.value(), Velocity::default().as_u8()]]
            )
        );
        assert_eq!(play_data.cycle_to_tick(cycle, 48000), 100);
        assert_eq!(play_data.accum_tick_to_tick(100), 100);

        let cycle = 340 * 48000 * 60 / 120 / 240;
        assert_eq!(
            midi_data[1],
            (cycle, vec![vec![0x90, note0.pitch.value(), 0]])
        );
        assert_eq!(play_data.cycle_to_tick(cycle, 48000), 340);
        assert_eq!(play_data.accum_tick_to_tick(340), 340);

        let cycle = 500 * 48000 * 60 / 120 / 240;
        assert_eq!(
            midi_data[2],
            (
                cycle,
                vec![vec![0x90, note1.pitch.value(), Velocity::default().as_u8()]]
            )
        );
        assert_eq!(play_data.cycle_to_tick(cycle, 48000), 500);
        assert_eq!(play_data.accum_tick_to_tick(500), 500);

        let cycle = 740 * 48000 * 60 / 120 / 240;
        assert_eq!(
            midi_data[3],
            (cycle, vec![vec![0x90, note1.pitch.value(), 0]])
        );
        assert_eq!(play_data.cycle_to_tick(cycle, 48000), 740);
        assert_eq!(play_data.accum_tick_to_tick(740), 740);

        let cycle = 980 * 48000 * 60 / 120 / 240;
        assert_eq!(
            midi_data[4],
            (
                cycle,
                vec![vec![0x90, note1.pitch.value(), Velocity::default().as_u8()]]
            )
        );
        assert_eq!(play_data.cycle_to_tick(cycle, 48000), 980);
        assert_eq!(play_data.accum_tick_to_tick(980), 500);

        let cycle = 1220 * 48000 * 60 / 120 / 240;
        assert_eq!(
            midi_data[5],
            (cycle, vec![vec![0x90, note1.pitch.value(), 0]])
        );
        assert_eq!(play_data.cycle_to_tick(cycle, 48000), 1220);
        assert_eq!(play_data.accum_tick_to_tick(1220), 740);
    }

    #[test]
    fn play_send_cmd() {
        let mut note_repo = BagStore::new(false);
        let note0 = Rc::new(Note {
            base_start_tick: 100,
            pitch: Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Null),
            ..Default::default()
        });
        note_repo.add(
            note0.start_tick(),
            note0.clone(),
            ModelChangeMetadata::new(),
        );

        let (events, _warnings) = Player::create_midi_events(
            None,
            Rhythm::new(2, 4),
            Key::SHARP_1,
            &note_repo,
            &Store::new(false),
            &Store::new(false),
            &Store::new(false),
            &Store::new(false),
        )
        .unwrap();

        let cycles_by_tick = events.cycles_by_accum_tick(48000, Duration::TICK_RESOLUTION as u32);
        let play_data0 = events.to_play_data(cycles_by_tick, 48000, 240);

        let factory = Box::new(|app_name: &str, _options| {
            Ok(TestJackClientProxy {
                status: ClientStatus::empty(),
                app_name: app_name.to_owned(),
                buffer_size: 2048,
                sampling_rate: 48000,
            })
        });
        let (mut player, status) = Player::open("my player", Some(factory)).unwrap();
        let (cmd_sender, cmd_receiver) = sync_channel::<Cmd>(64);

        player.cmd_channel = cmd_sender;

        let _warnings = player
            .play(
                1,
                None,
                Rhythm::new(2, 4),
                Key::SHARP_1,
                &note_repo,
                &Store::new(false),
                &Store::new(false),
                &Store::new(false),
                &Store::new(false),
            )
            .unwrap();

        let cmd = cmd_receiver.recv().unwrap();

        match cmd {
            Cmd::Play { seq, play_data, start_cycle } => {
                assert_eq!(seq, 1);

                let midi_data0: Vec<&(u64, Vec<Vec<u8>>)> = play_data0.midi_data.iter().collect();
                let midi_data1: Vec<&(u64, Vec<Vec<u8>>)> = play_data.midi_data.iter().collect();
                assert_eq!(midi_data0, midi_data1);
            }
            Cmd::Stop { seq: _ } => panic!("test failed"),
        };
    }

    #[test]
    fn play_cmd_change_state() {
        let mut note_repo = BagStore::new(false);
        let note0 = Rc::new(Note {
            base_start_tick: 100,
            pitch: Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Null),
            ..Default::default()
        });
        note_repo.add(
            note0.start_tick(),
            note0.clone(),
            ModelChangeMetadata::new(),
        );

        let (events, _warnings) = Player::create_midi_events(
            None,
            Rhythm::new(2, 4),
            Key::SHARP_1,
            &note_repo,
            &Store::new(false),
            &Store::new(false),
            &Store::new(false),
            &Store::new(false),
        )
        .unwrap();

        let cycles_by_tick = events.cycles_by_accum_tick(48000, Duration::TICK_RESOLUTION as u32);
        let play_data0 = events.to_play_data(cycles_by_tick, 48000, 240);
        let factory = Box::new(|app_name: &str, _options| {
            Ok(TestJackClientProxy {
                status: ClientStatus::empty(),
                app_name: app_name.to_owned(),
                buffer_size: 2048,
                sampling_rate: 48000,
            })
        });
        let (mut player, status) = Player::open("my player", Some(factory)).unwrap();
        let (cmd_sender, cmd_receiver) = sync_channel::<Cmd>(64);
        let (resp_sender, resp_receiver) = sync_channel::<Resp>(64);

        player.cmd_channel = cmd_sender;

        let _warnings = player
            .play(
                1,
                None,
                Rhythm::new(2, 4),
                Key::SHARP_1,
                &note_repo,
                &Store::new(false),
                &Store::new(false),
                &Store::new(false),
                &Store::new(false),
            )
            .unwrap();

        let mut state = PlayState::Init;
        let ctrl = Player::perform_cmd(&cmd_receiver, &mut state, &resp_sender);
        assert_eq!(ctrl, jack::Control::Continue);

        match &state {
            PlayState::Init => panic!("Should playing."),
            PlayState::Playing {
                seq,
                current_loc,
                play_data,
            } => {
                assert_eq!(*seq, 1);
                assert_eq!(*current_loc, 0);
                let midi_data0: Vec<&(u64, Vec<Vec<u8>>)> = play_data0.midi_data.iter().collect();
                let midi_data1: Vec<&(u64, Vec<Vec<u8>>)> = play_data.midi_data.iter().collect();
                assert_eq!(midi_data0, midi_data1);
            },
            PlayState::Stopping => panic!("Should playing.")
        }
        let resp = resp_receiver.recv().unwrap();
        assert_eq!(resp, Resp::Ok { seq: 1 });

        // Play again while playing.
        let _warnings = player
            .play(
                2,
                None,
                Rhythm::new(2, 4),
                Key::SHARP_1,
                &note_repo,
                &Store::new(false),
                &Store::new(false),
                &Store::new(false),
                &Store::new(false),
            )
            .unwrap();

        let ctrl = Player::perform_cmd(&cmd_receiver, &mut state, &resp_sender);
        assert_eq!(ctrl, jack::Control::Continue);

        match &state {
            PlayState::Init => panic!("Should playing."),
            PlayState::Playing {
                seq,
                current_loc,
                play_data,
            } => {
                assert_eq!(*seq, 1);
                assert_eq!(*current_loc, 0);
                let midi_data0: Vec<&(u64, Vec<Vec<u8>>)> = play_data0.midi_data.iter().collect();
                let midi_data1: Vec<&(u64, Vec<Vec<u8>>)> = play_data.midi_data.iter().collect();
                assert_eq!(midi_data0, midi_data1);
            },
            PlayState::Stopping => panic!("Should playing.")
        }
        let resp = resp_receiver.recv().unwrap();
        assert_eq!(
            resp,
            Resp::Err {
                seq: Some(2),
                error: CmdError::AlreadyPlaying
            }
        );

        // Stop playing.
        player.stop(3).unwrap();
        let ctrl = Player::perform_cmd(&cmd_receiver, &mut state, &resp_sender);
        assert_eq!(ctrl, jack::Control::Continue);

        match &state {
            PlayState::Init => {}
            PlayState::Playing {
                seq: _,
                current_loc: _,
                play_data: _,
            } => panic!("Should init"),
            PlayState::Stopping => {}
        }
        let resp = resp_receiver.recv().unwrap();
        assert_eq!(
            resp,
            Resp::Info {
                seq: Some(3),
                info: CmdInfo::PlayingEnded
            }
        );

        // Stop again.
        player.stop(4).unwrap();
        let ctrl = Player::perform_cmd(&cmd_receiver, &mut state, &resp_sender);
        assert_eq!(ctrl, jack::Control::Continue);

        match &state {
            PlayState::Init => {}
            PlayState::Playing {
                seq: _,
                current_loc: _,
                play_data: _,
            } => panic!("Should init"),
            PlayState::Stopping => {}
        }
        let resp = resp_receiver.recv().unwrap();
        assert_eq!(resp, Resp::Ok { seq: 4 });

        drop(player);
        let ctrl = Player::perform_cmd(&cmd_receiver, &mut state, &resp_sender);
        assert_eq!(ctrl, jack::Control::Quit);
    }

    #[test]
    fn specify_start_tick() {
        let mut note_repo = BagStore::new(false);
        let note0 = Rc::new(Note {
            base_start_tick: 100,
            pitch: Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Null),
            ..Default::default()
        });
        note_repo.add(
            note0.start_tick(),
            note0.clone(),
            ModelChangeMetadata::new(),
        );
        let note1 = Rc::new(Note {
            base_start_tick: 500,
            pitch: Pitch::new(Solfa::G, Octave::Oct4, SharpFlat::Null),
            ..Default::default()
        });
        note_repo.add(
            note1.start_tick(),
            note1.clone(),
            ModelChangeMetadata::new(),
        );

        let mut bar_repo: Store<u32, Bar, ModelChangeMetadata> = Store::new(false);
        bar_repo.add(
            480,
            Bar::new(480, None, None, repeat_set!(Repeat::Segno)),
            ModelChangeMetadata::new(),
        );
        bar_repo.add(
            960,
            Bar::new(960, None, None, repeat_set!(Repeat::Ds)),
            ModelChangeMetadata::new(),
        );

        let tempo_repo = Store::new(false);

        //      0    100  200  300 340 400 480    500  600  700 740   900 960
        //      |    |    |    |   |   |   |Segno |    |    |   |     |   |D.S.
        //            <-- note0 -->                <-- note1 -->
        // tempo|120

        let (events, _warnings) = Player::create_midi_events(
            None,
            Rhythm::new(2, 4),
            Key::NONE,
            &note_repo,
            &bar_repo,
            &tempo_repo,
            &Store::new(false),
            &Store::new(false),
        )
        .unwrap();

        assert_eq!(events.events.len(), 6);
        assert_eq!(
            events.events.get(&100).unwrap(),
            &vec![MidiSrc::NoteOn {
                pitch: note0.pitch,
                velocity: note0.velocity(),
                channel: Default::default()
            }]
        );
        assert_eq!(
            events.events.get(&340).unwrap(),
            &vec![MidiSrc::NoteOff {
                pitch: note0.pitch,
                channel: Default::default()
            }]
        );

        assert_eq!(
            events.events.get(&500).unwrap(),
            &vec![MidiSrc::NoteOn {
                pitch: note1.pitch,
                velocity: note1.velocity(),
                channel: Default::default()
            }]
        );
        assert_eq!(
            events.events.get(&740).unwrap(),
            &vec![MidiSrc::NoteOff {
                pitch: note1.pitch,
                channel: Default::default()
            }]
        );

        assert_eq!(
            events.events.get(&980).unwrap(),
            &vec![MidiSrc::NoteOn {
                pitch: note1.pitch,
                velocity: note1.velocity(),
                channel: Default::default()
            }]
        );
        assert_eq!(
            events.events.get(&1220).unwrap(),
            &vec![MidiSrc::NoteOff {
                pitch: note1.pitch,
                channel: Default::default()
            }]
        );
    }

    #[test]
    fn key_changes() {
        let mut note_repo = BagStore::new(false);
        let note0 = Rc::new(Note {
            base_start_tick: 480,
            pitch: Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Null),
            ..Default::default()
        });
        note_repo.add(
            note0.start_tick(),
            note0.clone(),
            ModelChangeMetadata::new(),
        );

        let mut bar_repo: Store<u32, Bar, ModelChangeMetadata> = Store::new(false);
        bar_repo.add(
            240,
            Bar::new(240, None, Some(Key::NONE), repeat_set!()),
            ModelChangeMetadata::new(),
        );
        bar_repo.add(
            480,
            Bar::new(480, None, None, repeat_set!()),
            ModelChangeMetadata::new(),
        );

        let (events, _warnings) = Player::create_midi_events(
            None,
            Rhythm::new(4, 4),
            Key::SHARP_1,
            &note_repo,
            &bar_repo,
            &Store::new(false),
            &Store::new(false),
            &Store::new(false),
        ).unwrap();

        assert_eq!(events.events.len(), 2);
        assert_eq!(
            events.events.get(&480).unwrap(),
            &vec![MidiSrc::NoteOn {
                pitch: note0.pitch,
                velocity: note0.velocity(),
                channel: Default::default()
            }]
        );
    }
}
