use std::{
    fmt::Display,
    marker::PhantomData,
    rc::Rc,
    sync::mpsc::{sync_channel, Receiver, RecvError, SyncSender},
    thread::{self},
    time,
};

use error_stack::Report;
use jack::{Client, ClientStatus, Control, MidiOut, ProcessScope, RawMidi};
use klavier_core::{midi_events::{MidiEvents, PlayData, create_midi_events}, play_start_tick::ToAccumTickError, tempo::TempoValue};
use klavier_core::play_start_tick::PlayStartTick;
use klavier_core::{
    bar::Bar,
    ctrl_chg::CtrlChg,
    duration::Duration,
    global_repeat::RenderRegionWarning,
    key::Key,
    note::Note,
    project::ModelChangeMetadata,
    repeat::{RenderRegionError, AccumTick},
    rhythm::Rhythm,
    tempo::Tempo,
};
use klavier_helper::{bag_store::BagStore, store::{Store}};

#[cfg(not(test))]
use jack::MidiWriter;

pub struct Player {
    // Frequency (ex. 48kHz => 48000)
    pub sampling_rate: usize,
    cmd_channel: SyncSender<Cmd>,
    resp_channel: Option<Receiver<Resp>>,
    closer: Option<Box<dyn Send + 'static + FnOnce() -> Option<jack::Error>>>,
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
    pub fn write(&mut self, _message: &RawMidi) -> core::result::Result<(), jack::Error> {
        Ok(())
    }
}

#[cfg(not(test))]
impl JackClientProxy {
    pub fn new(app_name: &str, options: jack::ClientOptions) -> core::result::Result<Self, jack::Error> {
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

    pub fn midi_out_port(&self) -> core::result::Result<Port<MidiOut>, jack::Error> {
        Ok(self
            .client
            .as_ref()
            .map(|c| c.register_port("MIDI OUT", jack::MidiOut::default()))
            .unwrap()?)
    }

    pub fn closer<F>(
        &mut self,
        callback: F,
    ) -> core::result::Result<Option<Box<dyn Send + 'static + FnOnce() -> Option<jack::Error>>>, jack::Error>
    where
        F: 'static + Send + FnMut(&Client, &ProcessScope) -> Control,
    {
        let active_client = {
            let client = self.client.take().unwrap();
            client.activate_async((), jack::contrib::ClosureProcessHandler::new(callback))?
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
    pub fn new(_app_name: &str, _options: jack::ClientOptions) -> core::result::Result<Self, jack::Error> {
        panic!("You need to pass client_factory to Player::open() for testing.");
    }

    pub fn new_test(
        app_name: &str,
        _options: jack::ClientOptions,
        status: ClientStatus,
        buffer_size: u32,
        sampling_rate: usize,
    ) -> core::result::Result<Self, jack::Error> {
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

    pub fn midi_out_port(&self) -> core::result::Result<TestPort<MidiOut>, jack::Error> {
        Ok(TestPort {
            _phantom: PhantomData,
        })
    }

    pub fn closer<F>(
        &mut self,
        _callback: F,
    ) -> core::result::Result<Option<Box<dyn Send + 'static + FnOnce() -> Option<jack::Error>>>, jack::Error>
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
            Box<dyn FnOnce(&str, jack::ClientOptions) -> core::result::Result<JCProxy, jack::Error>>,
        >,
    ) -> core::result::Result<(Self, jack::ClientStatus), Report<jack::Error>> {
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
    ) -> core::result::Result<Vec<RenderRegionWarning>, Report<PlayError>> {
        let (events, warnings): (MidiEvents, Vec<RenderRegionWarning>) = create_midi_events(
            top_rhythm,
            top_key,
            note_repo,
            bar_repo,
            tempo_repo,
            dumper_repo,
            soft_repo,
        )
        .map_err(|e| PlayError::RenderError(e.current_context().clone()))?;

        let cycles_by_tick: Store<u32, (TempoValue, u64), ()>
          = events.cycles_by_accum_tick(self.sampling_rate, Duration::TICK_RESOLUTION as u32);
        let start_accum_tick: u32 = match play_start_loc {
            None => 0,
            Some(loc) => {
                match loc.to_accum_tick(&events.chunks) {
                    Ok(tick) => tick,
                    Err(err) => Err(PlayError::PlayStartTickError(err))?,
                }
            }
        };
        let start_cycle: u64 = MidiEvents::accum_tick_to_cycle(
            &cycles_by_tick, start_accum_tick, self.sampling_rate, Duration::TICK_RESOLUTION as u32
        );
        let play_data: PlayData = events.to_play_data(cycles_by_tick, self.sampling_rate, Duration::TICK_RESOLUTION as u32);

        self.cmd_channel
            .send(Cmd::Play { seq, play_data, start_cycle })
            .map_err(|_e| PlayError::SendError { seq })?;
        Ok(warnings)
    }

    pub fn stop(&mut self, seq: usize) -> core::result::Result<(), Report<PlayError>> {
        self.cmd_channel
            .send(Cmd::Stop { seq })
            .map_err(|_e| PlayError::SendError { seq })?;
        Ok(())
    }

    /// Get response from the response receiver.
    pub fn get_resp(&self) -> core::result::Result<Resp, RecvError> {
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

impl std::error::Error for PlayError {}

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
    use crate::player::{CmdError, CmdInfo};
    use jack::ClientStatus;
    use klavier_core::duration::Duration;
    use klavier_core::midi_events::create_midi_events;
    use klavier_core::{
        key::Key,
        note::Note,
        octave::Octave,
        pitch::Pitch,
        project::ModelChangeMetadata,
        rhythm::Rhythm,
        sharp_flat::SharpFlat,
        solfa::Solfa,
    };
    use klavier_helper::{bag_store::BagStore, store::Store};
    use std::{rc::Rc, sync::mpsc::sync_channel};

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
            ModelChangeMetadata::default()
        );

        let (events, _warnings) = create_midi_events(
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
        let (mut player, _status) = Player::open("my player", Some(factory)).unwrap();
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
            Cmd::Play { seq, play_data, start_cycle: _ } => {
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
            ModelChangeMetadata::default(),
        );

        let (events, _warnings) = create_midi_events(
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
        let (mut player, _status) = Player::open("my player", Some(factory)).unwrap();
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
}
