use std::{sync::mpsc::{sync_channel, SyncSender, SendError, Receiver, RecvError}, rc::Rc, collections::{HashMap, BTreeMap}, fmt::Display};

use jack::{AsyncClient, ClosureProcessHandler};
use klavier_core::{note::Note, project::{ModelChangeMetadata, tempo_at}, bar::Bar, tempo::{Tempo, TempoValue}, ctrl_chg::CtrlChg, key::Key, rhythm::Rhythm, repeat::{render_region, RenderRegionError, Chunk}, sharp_flat::SharpFlat, solfa::Solfa, octave::Octave, pitch::Pitch, repeat_set, velocity::Velocity, duration::Duration, global_repeat::RenderRegionWarning};
use klavier_core::bar::RepeatSet;
use klavier_helper::{bag_store::BagStore, store::Store, sliding};
use error_stack::{Result, Report, Context};
use klavier_core::channel::Channel;

// Accumulated tick after rendering repeats.
type AccumTick = u32;

pub struct Player {
  pub sampling_rate: usize,
  cmd_channel: SyncSender<Cmd>,
  resp_channel: Receiver<Resp>,
  closer: Box<dyn FnOnce() -> Option<jack::Error>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum MidiSrc {
  NoteOn {
    channel: Channel,
    pitch: Pitch,
    velocity: Velocity
  },
  NoteOff {
    channel: Channel,
    pitch: Pitch
  },
  CtrlChg {
    channel: Channel,
    number: u8,
    velocity: Velocity
  },
}

impl MidiSrc {
  fn render_to(self, buf: &mut Vec<u8>) {
    match self {
        MidiSrc::NoteOn { channel, pitch, velocity } => {
          buf.push(0b10010000 | channel.as_u8());
          buf.push(pitch.value() as u8);
          buf.push(velocity.as_u8());
        }
        MidiSrc::NoteOff { channel, pitch } => {
          buf.push(0b10010000 | channel.as_u8());
          buf.push(pitch.value() as u8);
          buf.push(0);
        }
        MidiSrc::CtrlChg { channel, number, velocity } => {
          buf.push(0b10110000 | channel.as_u8());
          buf.push(number);
          buf.push(velocity.as_u8());
        }
    }
  }
}

pub struct PlayData {
  midi_data: Store<u64, Vec<u8>, ()>,
  // Key: cycle, Value: tick
  table_for_tracking: Store<u64, (AccumTick, TempoValue), ()>,
  chunks: Store<AccumTick, Chunk, ()>,
}

impl PlayData {
  pub fn cycle_to_tick(&self, cycle: u64, sampling_rate: u32) -> AccumTick {
    let mut finder = self.table_for_tracking.finder();
    match finder.just_before(cycle) {
      Some((c, (tick, tempo))) => {
        tick + 
        ((cycle - c) * tempo.as_u16() as u64 * Duration::TICK_RESOLUTION as u64 / sampling_rate as u64 / 60) as u32
      }
      None => {
        (cycle * TempoValue::default().as_u16() as u64 * Duration::TICK_RESOLUTION as u64 / sampling_rate as u64 / 60) as u32
      }
    }
  }

  pub fn accum_tick_to_tick(&self, tick: AccumTick) -> u32 {
    match self.chunks.finder().just_before(tick) {
      Some((at_tick, chunk)) => {
        chunk.start_tick() + (tick - at_tick)
      }
      None => {
        tick
      }
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
      key, accidental: HashMap::new()
    }
  }

  fn on_note(&mut self, note: Note) -> Vec<(u32, MidiSrc)> {
    let base_pitch = note.pitch;
    let sharp_flat = base_pitch.sharp_flat();

    fn create_event(note: &Note, pitch: Pitch) -> Vec<(u32, MidiSrc)> {
      let mut ret = Vec::with_capacity(2);

      if ! note.tied {
        ret.push((
          note.start_tick(),      
          MidiSrc::NoteOn {
            pitch, velocity: note.velocity(), channel: note.channel
          }
        ));
      }

      if ! note.tie {
        ret.push((
          note.start_tick() + note.duration.tick_length(),
          MidiSrc::NoteOff {
            pitch, channel: note.channel
          }
        ));
      }

      ret
    }

    if sharp_flat == SharpFlat::Null {
      let pitch = match self.accidental.get(&(base_pitch.solfa(), base_pitch.octave())) {
        None => base_pitch.apply_key(self.key).unwrap_or(base_pitch),
        Some(sharp_flat) => Pitch::value_of(base_pitch.solfa(), base_pitch.octave(), *sharp_flat).unwrap_or(base_pitch),
      };
      create_event(&note, pitch)
    } else {
      self.accidental.insert((base_pitch.solfa(), base_pitch.octave()), base_pitch.sharp_flat());
      create_event(&note, base_pitch)
    }
  }

  fn on_dumper(&mut self, dumper: CtrlChg) -> Vec<(u32, MidiSrc)> {
    vec![(
      dumper.start_tick,
      MidiSrc::CtrlChg { channel: dumper.channel, number: 64, velocity: dumper.velocity }
    )]
  }

  fn on_soft(&mut self, dumper: CtrlChg) -> Vec<(u32, MidiSrc)> {
    vec![(
      dumper.start_tick,
      MidiSrc::CtrlChg { channel: dumper.channel, number: 67, velocity: dumper.velocity }
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
    let mut offset: u32 = 0;
    let mut buf :Store<AccumTick, Chunk, ()> = Store::new(false);

    for c in chunks {
      buf.add(offset, c.clone(), ());
      if c.end_tick() != u32::MAX {
        offset += c.len();
      }
    }    

    Self {
      events: BTreeMap::new(),
      tempo_table: Store::new(false),
      chunks: buf,
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
    }
  }

  fn add_tempo(&mut self, tick: AccumTick, tempo: TempoValue) {
    self.tempo_table.add(tick, tempo, ());
  }

  fn cycles_by_tick(&self, sampling_rate: usize, ticks_per_quarter: u32) -> Store<AccumTick, (TempoValue, u64), ()> {
    let mut cycles: u64 = 0;
    let mut buf = Store::with_capacity(self.tempo_table.len(), false);
    let mut prev_tick: AccumTick = 0;
    let mut prev_tempo: u16 = TempoValue::default().as_u16();

    for (tick, tempo) in self.tempo_table.iter() {
      cycles += (tick - prev_tick) as u64 * sampling_rate as u64 * 60 / prev_tempo as u64 / ticks_per_quarter as u64;
      buf.add(*tick, (*tempo, cycles), ());
      prev_tick = *tick;
      prev_tempo = tempo.as_u16();
    }

    buf
  }

  fn to_play_data(self, sampling_rate: usize, ticks_per_quarter: u32) -> PlayData {
    let cycles_by_tick = self.cycles_by_tick(sampling_rate, ticks_per_quarter);
    let mut cycles_by_tick = cycles_by_tick.finder();
    let mut midi_data = Store::new(false);
    let mut table_for_tracking = Store::new(false);

    for (tick, events) in self.events.iter() {
      let c = match cycles_by_tick.just_before(*tick) {
        Some((t, (tempo, cycles))) =>
          *cycles + (*tick - *t) as u64 * sampling_rate as u64 * 60 / tempo.as_u16() as u64 / ticks_per_quarter as u64,
        None =>
          *tick as u64 * sampling_rate as u64 * 60 / TempoValue::default().as_u16() as u64 / ticks_per_quarter as u64,
      };
      let mut midi = vec![];
      for e in events.iter() {
        e.render_to(&mut midi);
      }
      midi_data.add(c, midi, ());
    }    

    for (tick, tempo) in self.tempo_table.iter() {
      let c = match cycles_by_tick.just_before(*tick) {
        Some((t, (tempo, cycles))) =>
          *cycles + (*tick - *t) as u64 * sampling_rate as u64 * 60 / tempo.as_u16() as u64 / ticks_per_quarter as u64,
        None =>
          *tick as u64 * sampling_rate as u64 * 60 / TempoValue::default().as_u16() as u64 / ticks_per_quarter as u64,
      };
      table_for_tracking.add(c, (*tick, tempo.clone()), ());
    }

    PlayData { midi_data, table_for_tracking, chunks: self.chunks }
  }
}

pub enum Cmd {
  Play {
    seq: usize,
    play_data: PlayData,
  },
  Stop {
    seq: usize,
  }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum CmdError {
  AlreadyPlaying,
  MidiWriteError(String),
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum CmdInfo {
  PlayingEnded,
  CurrentLoc(u32, AccumTick),
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Resp {
  Err {
    seq: Option<usize>,
    error: CmdError,
  },
  Info {
    seq: Option<usize>,
    info: CmdInfo,
  },
  Ok {
    seq: usize,
  }
}

pub enum PlayState {
  Init,
  Playing {
    seq: usize,
    current_loc: u64,
    play_data: PlayData,
  }
}

impl Player {
  pub fn open(app_name: &str) -> Result<(Self, jack::ClientStatus), jack::Error> {
    let (client, status) = jack::Client::new(app_name, jack::ClientOptions::empty())?;
    let mut out_port = client.register_port("MIDI OUT", jack::MidiOut::default())?;
    let (cmd_sender, cmd_receiver) = sync_channel::<Cmd>(64);
    let (resp_sender, resp_receiver) = sync_channel::<Resp>(64);

    let buffer_size: u32 = client.buffer_size();
    let sampling_rate = client.sample_rate();
    let mut state = PlayState::Init;

    let callback = move |_client: &jack::Client, ps: &jack::ProcessScope| -> jack::Control {
      let mut pstate = &mut state;

      fn resp(resp_sender: &SyncSender<Resp>, resp: Resp) {
        if let Err(e) = resp_sender.send(resp) {
          println!("Cannot send response: {:?}.", e);
        }
      }

      let mut midi_writer = out_port.writer(ps);

      match cmd_receiver.try_recv() {
        Ok(cmd) => {
          match cmd {
            Cmd::Play { seq, play_data } => {
              match pstate {
                PlayState::Init => {
                  *pstate = PlayState::Playing { seq, current_loc: 0, play_data };
                  resp(&resp_sender, Resp::Ok { seq });
                }
                PlayState::Playing { seq: _seq, current_loc: _, play_data: _ } => {
                  resp(&resp_sender, Resp::Err { seq: Some(seq), error: CmdError::AlreadyPlaying });
                }
              }
            }
            Cmd::Stop { seq } => {
              match pstate {
                PlayState::Init => {
                  resp(&resp_sender, Resp::Ok { seq });
                },
                PlayState::Playing { seq: _seq, current_loc: _, play_data: _ } => {
                  *pstate = PlayState::Init;
                  resp(&resp_sender, Resp::Ok { seq });
                }
              }
            }
          }
        }
        Err(err) =>
          if err == std::sync::mpsc::TryRecvError::Disconnected {
            return jack::Control::Quit;
          }
      };

      match &mut pstate {
        PlayState::Init => {},
        PlayState::Playing { seq: playing_seq, current_loc, play_data } => {
          let (_idx, data) = play_data.midi_data.range(*current_loc..(*current_loc + buffer_size as u64));
          for (cycle, midi) in data.iter() {
            let offset = (cycle - *current_loc) as u32;

            if let Err(e) = midi_writer.write(&jack::RawMidi { time: offset, bytes: midi }) {
              resp(&resp_sender, Resp::Err { seq: None, error: CmdError::MidiWriteError(e.to_string()) })
            }
          }

          let new_loc = *current_loc + buffer_size as u64;
          let ended = play_data.midi_data.peek_last().map(|(cycle, _midi) | *cycle < new_loc).unwrap_or(false);
          if ended {
            resp(&resp_sender, Resp::Info { seq: Some(*playing_seq), info: CmdInfo::PlayingEnded });
            *pstate = PlayState::Init;
          } else {
            let accum_tick = play_data.cycle_to_tick(*current_loc, sampling_rate as u32);
            let tick = play_data.accum_tick_to_tick(accum_tick);
            resp(&resp_sender, Resp::Info { seq: Some(*playing_seq), info: CmdInfo::CurrentLoc(tick, accum_tick) });
            *current_loc = new_loc;
          }
        }
      };

      jack::Control::Continue
    };

    let active_client: AsyncClient<(), ClosureProcessHandler<_>> = client.activate_async((), jack::ClosureProcessHandler::new(callback))?;
    let closer = move || active_client.deactivate().err();

    Ok((Player { sampling_rate, cmd_channel: cmd_sender, resp_channel: resp_receiver, closer: Box::new(closer) }, status))    
  }

  fn create_key_table(top_key: Key, bar_repo: &Store<u32, Bar, ModelChangeMetadata>) -> Store<u32, Key, ()> {
    let mut key_table: Store<u32, Key, ()> = Store::new(false);
    key_table.add(0, top_key, ());

    for (tick, bar) in bar_repo.iter() {
      if let Some(key) = bar.key {
        key_table.add(*tick, key, ());
      }
    }

    key_table
  }

  fn notes_by_base_start_tick(note_repo: &BagStore<u32, Rc<Note>, ModelChangeMetadata>) -> BagStore<u32, Rc<Note>, ()> {
    let mut notes = BagStore::new(false);

    for (_, note) in note_repo.iter() {
      notes.add(note.base_start_tick, note.clone(), ());
    }

    notes
  }

  fn create_midi_events(
    top_rhythm: Rhythm, top_key: Key,
    note_repo: &BagStore<u32, Rc<Note>, ModelChangeMetadata>,
    bar_repo: &Store<u32, Bar, ModelChangeMetadata>,
    tempo_repo: &Store<u32, Tempo, ModelChangeMetadata>,
    dumper_repo: &Store<u32, CtrlChg, ModelChangeMetadata>,
    soft_repo: &Store<u32, CtrlChg, ModelChangeMetadata>
  ) -> Result<(MidiEvents, Vec<RenderRegionWarning>), RenderRegionError> {
    let key_table = Self::create_key_table(top_key, bar_repo);
    let mut key_finder = key_table.finder();
    let notes_by_base_start_tick = Self::notes_by_base_start_tick(&note_repo);

    let (region, warnings)
      = render_region(top_rhythm, bar_repo.iter().map(|(_, bar)| bar))?;
    let imaginary_top_bar = vec![(0, Bar::new(0, Some(top_rhythm), Some(top_key), repeat_set!()))];
    let imaginary_empty_bar = vec![];
    let imaginary_bottom_bar: Vec<(u32, Bar)> = vec![(u32::MAX, Bar::new(u32::MAX, None, None, repeat_set!()))];
    let mut offset: u32 = 0;

    let chunks = region.to_chunks();
    let mut events = MidiEvents::new(&chunks);
    for chunk in chunks.iter() {
      events.add_tempo(chunk.start_tick() + offset, tempo_at(chunk.start_tick(), tempo_repo));

      let (_size, bars) = bar_repo.range(chunk.start_tick()..=chunk.end_tick());
      let top_bars = match bars.get(0) {
          Some((_idx, top_bar)) =>
            if top_bar.start_tick != 0 {
              &imaginary_top_bar
            } else {
              &imaginary_empty_bar
            },
          None => &imaginary_top_bar
      };

      let bottom_bars = match bars.last() {
          Some((_idx, last_bar)) =>
            if last_bar.start_tick < chunk.end_tick() {
              &imaginary_bottom_bar
            } else {
              &imaginary_empty_bar
            }
          None => &imaginary_bottom_bar
      };

      let mut bar_itr =
        top_bars.iter().chain(bars.iter()).chain(bottom_bars.iter()).peekable();

      let key = key_finder.just_before(chunk.start_tick()).map(|(_tick, key)| key).unwrap_or(&top_key);
      let mut notes = notes_by_base_start_tick.range(chunk.start_tick()..chunk.end_tick()).peekable();
      let (_idx, dumpers) = dumper_repo.range(chunk.start_tick()..chunk.end_tick());
      let mut dumpers = dumpers.iter().peekable();
      let (_idx, softs) = soft_repo.range(chunk.start_tick()..chunk.end_tick());
      let mut softs = softs.iter().peekable();

      for (bar_from, bar_to) in sliding(&mut bar_itr) {
        let key = if let Some(key) = bar_from.1.key { key } else { *key };
        let mut midi_reporter = MidiReporter::new(key);
        
        while let Some((_tick, note)) = notes.next_if(|(_tick, note)| bar_from.0 <= note.base_start_tick && note.base_start_tick < bar_to.0) {
          for (tick, midi_src) in midi_reporter.on_note((**note).clone()).iter() {
            events.add_midi_event(tick + offset - chunk.start_tick(), *midi_src);
          }
        }

        while let Some((_tick, dumper)) = dumpers.next_if(|(_tick, dumper)| bar_from.0 <= dumper.start_tick && dumper.start_tick < bar_to.0) {
          for (tick, midi_src) in midi_reporter.on_dumper(*dumper).iter() {
            events.add_midi_event(tick + offset + chunk.start_tick(), *midi_src);
          }
        }

        while let Some((_tick, soft)) = softs.next_if(|(_tick, dumper)| bar_from.0 <= dumper.start_tick && dumper.start_tick < bar_to.0) {
          for (tick, midi_src) in midi_reporter.on_soft(*soft).iter() {
            events.add_midi_event(tick + offset + chunk.start_tick(), *midi_src);
          }
        }
      }

      let (_, tempos) = tempo_repo.range(chunk.start_tick()..chunk.end_tick());
      for (tick, tempo) in tempos.iter() {
        events.add_tempo(tick + offset + chunk.start_tick(), tempo.value);
      }

      if chunk.end_tick() != u32::MAX {
        offset += chunk.len();
      }
    }

    Ok((events, warnings))
  }

  pub fn play(
    &mut self,
    top_rhythm: Rhythm, top_key: Key,
    note_repo: &BagStore<u32, Rc<Note>, ModelChangeMetadata>,
    bar_repo: &Store<u32, Bar, ModelChangeMetadata>,
    tempo_repo: &Store<u32, Tempo, ModelChangeMetadata>,
    dumper_repo: &Store<u32, CtrlChg, ModelChangeMetadata>,
    soft_repo: &Store<u32, CtrlChg, ModelChangeMetadata>,
  ) -> Result <Vec<RenderRegionWarning>, PlayError> {
    let (events, warnings) = Self::create_midi_events(
      top_rhythm, top_key, note_repo, bar_repo, tempo_repo, dumper_repo, soft_repo
    ).map_err(|e| PlayError::RenderError(e))?;
    let play_data = events.to_play_data(self.sampling_rate, Duration::TICK_RESOLUTION as u32);
    let seq: usize = 1;
    self.cmd_channel.send(Cmd::Play { seq, play_data }).map_err(|e| PlayError::SendError(e))?;
    Ok(warnings)
  }

  pub fn get_resp(&self) -> Result<Resp, RecvError> {
    Ok(self.resp_channel.recv()?)
  }
}

#[derive(Debug)]
pub enum PlayError {
  RenderError(Report<RenderRegionError>),
  SendError(SendError<Cmd>),
}

impl Display for PlayError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "{:?}", self)
  }
}

impl Context for PlayError {}

#[cfg(test)]
mod tests {
  use std::rc::Rc;
  use klavier_core::{rhythm::Rhythm, key::Key, note::Note, pitch::Pitch, solfa::Solfa, octave::Octave, sharp_flat::SharpFlat, trimmer::Trimmer, project::ModelChangeMetadata, bar::{Bar, Repeat}, repeat_set, tempo::{Tempo, TempoValue}, velocity::Velocity};
  use klavier_helper::{bag_store::BagStore, store::Store};
  use crate::player::MidiSrc;
  use super::Player;
  use klavier_core::bar::RepeatSet;

  #[test]
  fn empty() {
    let (events, _warnings) = Player::create_midi_events(
      Rhythm::new(4, 4),
      Key::NONE,
      &BagStore::new(false),
      &Store::new(false),
      &Store::new(false),
      &Store::new(false),
      &Store::new(false)
    ).unwrap();
    assert_eq!(events.events.len(), 0);
  }

  #[test]
  fn apply_tune_key() {
    let mut note_repo = BagStore::new(false);
    let note0 = Rc::new(
      Note {
        base_start_tick: 100,
        pitch: Pitch::new(Solfa::A, Octave::Oct4, SharpFlat::Null),
        ..Default::default()
      }
    );
    note_repo.add(note0.start_tick(), note0.clone(), ModelChangeMetadata::new());

    let note1 = Rc::new(
      Note {
        base_start_tick: 200,
        pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Sharp),
        ..Default::default()
      }
    );
    note_repo.add(note1.start_tick(), note1.clone(), ModelChangeMetadata::new());

    let note2 = Rc::new(
      Note {
        base_start_tick: 300,
        pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Null),
        start_tick_trimmer: Trimmer::new(-5, -10, 0, 0),
        ..Default::default()
      }
    );
    note_repo.add(note2.start_tick(), note2.clone(), ModelChangeMetadata::new());

    let (events, _warnings) = Player::create_midi_events(
      Rhythm::new(4, 4),
      Key::FLAT_1,
      &note_repo,
      &Store::new(false),
      &Store::new(false),
      &Store::new(false),
      &Store::new(false)
    ).unwrap();
    assert_eq!(events.events.len(), 6);
    assert_eq!(events.events.get(&100).unwrap(), &vec![MidiSrc::NoteOn { pitch: note0.pitch, velocity: note0.velocity(), channel: Default::default() }]);
    assert_eq!(events.events.get(&(100 + note0.duration.tick_length())).unwrap(), &vec![MidiSrc::NoteOff { pitch: note0.pitch, channel: Default::default() }]);
    assert_eq!(
      events.events.get(&200).unwrap(),
      &vec![
        MidiSrc::NoteOn {
          pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Sharp),
          velocity: note1.velocity(),
          channel: Default::default()
        }
      ]
    );
    assert_eq!(events.events.get(&(200 + note1.duration.tick_length())).unwrap(), &vec![MidiSrc::NoteOff { pitch: note1.pitch, channel: Default::default() }]);
    assert_eq!(
      events.events.get(&(300 - 5 - 10)).unwrap(),
      &vec![
        MidiSrc::NoteOn {
          pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Sharp),
          velocity: note2.velocity(),
          channel: Default::default()
        }
      ]
    );
    assert_eq!(
      events.events.get(&(300 - 5 - 10 + note2.duration.tick_length())).unwrap(),
      &vec![MidiSrc::NoteOff { pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Sharp), channel: Default::default() }]
    );
  }

  #[test]
  fn one_bar() {
    let mut note_repo = BagStore::new(false);
    let note0 = Rc::new(
      Note {
        base_start_tick: 100,
        pitch: Pitch::new(Solfa::A, Octave::Oct4, SharpFlat::Null),
        ..Default::default()
      }
    );
    note_repo.add(note0.start_tick(), note0.clone(), ModelChangeMetadata::new());

    let note1 = Rc::new(
      Note {
        base_start_tick: 200,
        pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Sharp),
        ..Default::default()
      }
    );
    note_repo.add(note1.start_tick(), note1.clone(), ModelChangeMetadata::new());

    let note2 = Rc::new(
      Note {
        base_start_tick: 300,
        pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Null),
        start_tick_trimmer: Trimmer::new(-5, -10, 0, 0),
        ..Default::default()
      }
    );
    note_repo.add(note2.start_tick(), note2.clone(), ModelChangeMetadata::new());

    let mut bar_repo: Store<u32, Bar, ModelChangeMetadata> = Store::new(false);
    bar_repo.add(300, Bar::new(300, None, Some(Key::FLAT_1), repeat_set!()), ModelChangeMetadata::new());

    let (events, _warnings) = Player::create_midi_events(
      Rhythm::new(4, 4),
      Key::SHARP_1,
      &note_repo,
      &bar_repo,
      &Store::new(false),
      &Store::new(false),
      &Store::new(false)
    ).unwrap();
    assert_eq!(events.events.len(), 6);
    assert_eq!(events.events.get(&100).unwrap(), &vec![MidiSrc::NoteOn { pitch: note0.pitch, velocity: note0.velocity(), channel: Default::default() }]);
    assert_eq!(events.events.get(&(100 + note0.duration.tick_length())).unwrap(), &vec![MidiSrc::NoteOff { pitch: note0.pitch, channel: Default::default() }]);
    assert_eq!(
      events.events.get(&200).unwrap(),
      &vec![
        MidiSrc::NoteOn {
          pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Sharp),
          velocity: note1.velocity(),
          channel: Default::default()
        }
      ]
    );
    assert_eq!(
      events.events.get(&(200 + note1.duration.tick_length())).unwrap(),
      &vec![MidiSrc::NoteOff { pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Sharp), channel: Default::default() }]
    );
    assert_eq!(
      events.events.get(&(300 - 5 - 10)).unwrap(),
      &vec![
        MidiSrc::NoteOn {
          pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Flat),
          velocity: note2.velocity(),
          channel: Default::default()
        }
      ]
    );
    assert_eq!(
      events.events.get(&(300 - 5- 10 + note2.duration.tick_length())).unwrap(),
      &vec![MidiSrc::NoteOff { pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Flat), channel: Default::default() }]
    );
  }  

  #[test]
  fn with_repeat() {
    let mut note_repo = BagStore::new(false);
    let note0 = Rc::new(
      Note {
        base_start_tick: 100,
        pitch: Pitch::new(Solfa::A, Octave::Oct4, SharpFlat::Null),
        ..Default::default()
      }
    );
    note_repo.add(note0.start_tick(), note0.clone(), ModelChangeMetadata::new());

    let note1 = Rc::new(
      Note {
        base_start_tick: 200,
        pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Sharp),
        start_tick_trimmer: Trimmer::new(-1, 0, 0, 0),
        ..Default::default()
      }
    );
    note_repo.add(note1.start_tick(), note1.clone(), ModelChangeMetadata::new());

    let note2 = Rc::new(
      Note {
        base_start_tick: 300,
        pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Null),
        start_tick_trimmer: Trimmer::new(-5, -10, 0, 0),
        ..Default::default()
      }
    );
    note_repo.add(note2.start_tick(), note2.clone(), ModelChangeMetadata::new());

    let mut bar_repo: Store<u32, Bar, ModelChangeMetadata> = Store::new(false);
    bar_repo.add(200, Bar::new(200, None, Some(Key::FLAT_1), repeat_set!(Repeat::End)), ModelChangeMetadata::new());

    let (events, _warnings) = Player::create_midi_events(
      Rhythm::new(4, 4),
      Key::SHARP_1,
      &note_repo,
      &bar_repo,
      &Store::new(false),
      &Store::new(false),
      &Store::new(false)
    ).unwrap();
    assert_eq!(events.events.len(), 8);
    assert_eq!(events.events.get(&100).unwrap(), &vec![MidiSrc::NoteOn { pitch: note0.pitch, velocity: note0.velocity(), channel: Default::default() }]);
    assert_eq!(
      events.events.get(&(100 + note0.duration.tick_length())).unwrap(),
      &vec![MidiSrc::NoteOff { pitch: note0.pitch, channel: Default::default() }]
    );
    assert_eq!(events.events.get(&300).unwrap(), &vec![MidiSrc::NoteOn { pitch: note0.pitch, velocity: note0.velocity(), channel: Default::default() }]);
    assert_eq!(
      events.events.get(&(300 + note0.duration.tick_length())).unwrap(),
      &vec![MidiSrc::NoteOff { pitch: note0.pitch, channel: Default::default() }]
    );
    assert_eq!(events.events.get(&(400 - 1)).unwrap(), &vec![MidiSrc::NoteOn { pitch: note1.pitch, velocity: note1.velocity(), channel: Default::default() }]);
    assert_eq!(
      events.events.get(&(400 - 1 + note1.duration.tick_length())).unwrap(),
      &vec![MidiSrc::NoteOff { pitch: note1.pitch, channel: Default::default() }]
    );
    assert_eq!(
      events.events.get(&(500 - 5 - 10)).unwrap(),
      &vec![
        MidiSrc::NoteOn {
          pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Sharp),
          velocity: note2.velocity(),
          channel: Default::default()
        }
      ]
    );
    assert_eq!(
      events.events.get(&(500 - 5 - 10 + note2.duration.tick_length())).unwrap(),
      &vec![
        MidiSrc::NoteOff {
          pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Sharp), channel: Default::default()
        }
      ]
    );
  }  

  #[test]
  fn with_dc() {
    let mut note_repo = BagStore::new(false);
    let note0 = Rc::new(
      Note {
        base_start_tick: 100,
        pitch: Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Null),
        ..Default::default()
      }
    );
    note_repo.add(note0.start_tick(), note0.clone(), ModelChangeMetadata::new());

    let note1 = Rc::new(
      Note {
        base_start_tick: 500,
        pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Sharp),
        ..Default::default()
      }
    );
    note_repo.add(note1.start_tick(), note1.clone(), ModelChangeMetadata::new());

    let note2 = Rc::new(
      Note {
        base_start_tick: 600,
        pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Null),
        ..Default::default()
      }
    );
    note_repo.add(note2.start_tick(), note2.clone(), ModelChangeMetadata::new());

    let mut bar_repo: Store<u32, Bar, ModelChangeMetadata> = Store::new(false);
    bar_repo.add(480, Bar::new(480, None, Some(Key::FLAT_1), repeat_set!(Repeat::End, Repeat::Fine)), ModelChangeMetadata::new());
    bar_repo.add(960, Bar::new(960, None, None, repeat_set!(Repeat::Dc)), ModelChangeMetadata::new());

    let mut tempo_repo = Store::new(false);
    tempo_repo.add(200, Tempo::new(200, 300), ModelChangeMetadata::new());

    //      0    100  200  300 340 400   480 500  600      740   840 960
    //      |    |    |    |   |   |     :|  |    |        |     |   |D.C.
    //            <-- note0 -->               <-- note1 -->      |
    //                                             <-- note 2 -->
    // tempo|120      |300

    let (events, _warnings) = Player::create_midi_events(
      Rhythm::new(2, 4),
      Key::SHARP_1,
      &note_repo,
      &bar_repo,
      &tempo_repo,
      &Store::new(false),
      &Store::new(false)
    ).unwrap();
    assert_eq!(events.events.len(), 10);
    assert_eq!(
      events.events.get(&100).unwrap(),
      &vec![
        MidiSrc::NoteOn {
          pitch: Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Sharp),
          velocity: note0.velocity(),
          channel: Default::default()
        }
      ]
    );
    assert_eq!(
      events.events.get(&(100 + note0.duration.tick_length())).unwrap(),
      &vec![MidiSrc::NoteOff { pitch: Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Sharp), channel: Default::default() }]
    );
    assert_eq!(
      events.events.get(&580).unwrap(),
      &vec![
        MidiSrc::NoteOn {
          pitch: Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Sharp),
          velocity: note0.velocity(),
          channel: Default::default()
        }
      ]
    );
    assert_eq!(
      events.events.get(&(580 + note0.duration.tick_length())).unwrap(),
      &vec![MidiSrc::NoteOff { pitch: Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Sharp), channel: Default::default()}]
    );
    assert_eq!(events.events.get(&980).unwrap(), &vec![MidiSrc::NoteOn { pitch: note1.pitch, velocity: note1.velocity(), channel: Default::default() }]);
    assert_eq!(events.events.get(&(980 + note1.duration.tick_length())).unwrap(), &vec![MidiSrc::NoteOff { pitch: note1.pitch, channel: Default::default() }]);
    assert_eq!(
      events.events.get(&1080).unwrap(),
      &vec![
        MidiSrc::NoteOn {
          pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Sharp),
          velocity: note2.velocity(),
          channel: Default::default()
        }
      ]
    );
    assert_eq!(
      events.events.get(&(1080 + note2.duration.tick_length())).unwrap(),
      &vec![
        MidiSrc::NoteOff {
          pitch: Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Sharp), channel: Default::default()
        }
      ]
    );
    assert_eq!(
      events.events.get(&(480 * 3 + 100)).unwrap(),
      &vec![
        MidiSrc::NoteOn {
          pitch: Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Sharp),
          velocity: note0.velocity(),
          channel: Default::default()
        }
      ]
    );
    assert_eq!(
      events.events.get(&(480 * 3 + 100 + note0.duration.tick_length())).unwrap(),
      &vec![
        MidiSrc::NoteOff {
          pitch: Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Sharp), channel: Default::default()
        }
      ]
    );
 
    assert_eq!(events.tempo_table.len(), 6);
    let mut z = events.tempo_table.iter();
    assert_eq!(z.next(), Some(&(0, TempoValue::new(120))));
    assert_eq!(z.next(), Some(&(200, TempoValue::new(300))));
    assert_eq!(z.next(), Some(&(480, TempoValue::new(120))));
    assert_eq!(z.next(), Some(&(680, TempoValue::new(300))));
    assert_eq!(z.next(), Some(&(480 * 3, TempoValue::new(120))));
    assert_eq!(z.next(), Some(&(480 * 3 + 200, TempoValue::new(300))));
    assert_eq!(z.next(), None);

    let play_data = events.to_play_data(48000, 240);
    let midi_data = &play_data.midi_data;
    assert_eq!(midi_data.len(), 10);
 
    //      0    100  200  300 340 400   480 500  600      740   840 960
    //      |    |    |    |   |   |     :|  |    |        |     |   |D.C.
    //      |     <-- note0 -->               <-- note1 -->      |
    //      |                                      <-- note 2 -->
    // tempo|120      |300

    let cycle = 100 * 48000 * 60 / 120 / 240;
    assert_eq!(midi_data[0], (
      cycle, vec![0x90, Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Sharp).value(), Velocity::default().as_u8()])
    );
    assert_eq!(play_data.cycle_to_tick(cycle, 48000), 100);

    let cycle = (200 * 48000 * 60 / 120 + 140 * 48000 * 60 / 300) / 240;
    assert_eq!(midi_data[1], (
      cycle, vec![0x90, Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Sharp).value(), 0])
    );
    assert_eq!(play_data.cycle_to_tick(cycle, 48000), 340);

    let cycle = (200 * 48000 * 60 / 120 + 280 * 48000 * 60 / 300 + 100 * 48000 * 60 / 120) / 240;
    assert_eq!(midi_data[2], (
      cycle, vec![0x90, Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Sharp).value(), Velocity::default().as_u8()])
    );
    assert_eq!(play_data.cycle_to_tick(cycle, 48000), 580);
    assert_eq!(play_data.accum_tick_to_tick(580), 100);

    let cycle = (200 * 48000 * 60 / 120 + 280 * 48000 * 60 / 300 + 200 * 48000 * 60 / 120 + 140 * 48000 * 60 / 300) / 240;
    assert_eq!(midi_data[3], (
      cycle, vec![0x90, Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Sharp).value(), 0])
    );
    assert_eq!(play_data.cycle_to_tick(cycle, 48000), 820);
    assert_eq!(play_data.accum_tick_to_tick(820), 340);

    let cycle = (200 * 48000 * 60 / 120 + 280 * 48000 * 60 / 300 + 200 * 48000 * 60 / 120 + 300 * 48000 * 60 / 300) / 240;
    assert_eq!(midi_data[4], (
      cycle, vec![0x90, note1.pitch.value(), Velocity::default().as_u8()])
    );
    assert_eq!(play_data.cycle_to_tick(cycle, 48000), 980);
    assert_eq!(play_data.accum_tick_to_tick(980), 500);

    let cycle = (200 * 48000 * 60 / 120 + 280 * 48000 * 60 / 300 + 200 * 48000 * 60 / 120 + 400 * 48000 * 60 / 300) / 240;
    assert_eq!(midi_data[5], (
      cycle, vec![0x90, Pitch::new(Solfa::B, Octave::Oct4, SharpFlat::Sharp).value(), Velocity::default().as_u8()])
    );
    assert_eq!(play_data.cycle_to_tick(cycle, 48000), 1080);
    assert_eq!(play_data.accum_tick_to_tick(1080), 600);

    let cycle = (200 * 48000 * 60 / 120 + 280 * 48000 * 60 / 300 + 200 * 48000 * 60 / 120 + 540 * 48000 * 60 / 300) / 240;
    assert_eq!(midi_data[6], (
      cycle, vec![0x90, note1.pitch.value(), 0])
    );
    assert_eq!(play_data.cycle_to_tick(cycle, 48000), 1220);
    assert_eq!(play_data.accum_tick_to_tick(1220), 740);

    let cycle = (200 * 48000 * 60 / 120 + 280 * 48000 * 60 / 300 + 200 * 48000 * 60 / 120 + 640 * 48000 * 60 / 300) / 240;
    assert_eq!(midi_data[7], (
      cycle, vec![0x90, note1.pitch.value(), 0])
    );
    assert_eq!(play_data.cycle_to_tick(cycle, 48000), 1320);
    assert_eq!(play_data.accum_tick_to_tick(1320), 840);

    let cycle = (200 * 48000 * 60 / 120 + 280 * 48000 * 60 / 300 + 200 * 48000 * 60 / 120 + 760 * 48000 * 60 / 300 + 100 * 48000 * 60 / 120) / 240;
    assert_eq!(midi_data[8], (
      cycle, vec![0x90, Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Sharp).value(), Velocity::default().as_u8()])
    );
    assert_eq!(play_data.cycle_to_tick(cycle, 48000), 1540);
    assert_eq!(play_data.accum_tick_to_tick(1540), 100);
 
    let cycle = (200 * 48000 * 60 / 120 + 280 * 48000 * 60 / 300 + 200 * 48000 * 60 / 120 + 760 * 48000 * 60 / 300 + 200 * 48000 * 60 / 120 + 140 * 48000 * 60 / 300) / 240;
    assert_eq!(midi_data[9], (
      cycle, vec![0x90, Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Sharp).value(), 0])
    );
    assert_eq!(play_data.cycle_to_tick(cycle, 48000), 1780);
    assert_eq!(play_data.accum_tick_to_tick(1780), 340);
  }  

  #[test]
  fn with_ds() {
    let mut note_repo = BagStore::new(false);
    let note0 = Rc::new(
      Note {
        base_start_tick: 100,
        pitch: Pitch::new(Solfa::F, Octave::Oct4, SharpFlat::Null),
        ..Default::default()
      }
    );
    note_repo.add(note0.start_tick(), note0.clone(), ModelChangeMetadata::new());
    let note1 = Rc::new(
      Note {
        base_start_tick: 500,
        pitch: Pitch::new(Solfa::G, Octave::Oct4, SharpFlat::Null),
        ..Default::default()
      }
    );
    note_repo.add(note1.start_tick(), note1.clone(), ModelChangeMetadata::new());

    let mut bar_repo: Store<u32, Bar, ModelChangeMetadata> = Store::new(false);
    bar_repo.add(480, Bar::new(480, None, None, repeat_set!(Repeat::Segno)), ModelChangeMetadata::new());
    bar_repo.add(960, Bar::new(960, None, None, repeat_set!(Repeat::Ds)), ModelChangeMetadata::new());

    let tempo_repo = Store::new(false);

    let (events, _warnings) = Player::create_midi_events(
      Rhythm::new(2, 4),
      Key::NONE,
      &note_repo,
      &bar_repo,
      &tempo_repo,
      &Store::new(false),
      &Store::new(false)
    ).unwrap();

    assert_eq!(events.events.len(), 6);
    assert_eq!(
      events.events.get(&100).unwrap(),
      &vec![MidiSrc::NoteOn { pitch: note0.pitch, velocity: note0.velocity(), channel: Default::default() }]
    );
    assert_eq!(
      events.events.get(&340).unwrap(),
      &vec![MidiSrc::NoteOff { pitch: note0.pitch, channel: Default::default() }]
    );

    assert_eq!(
      events.events.get(&500).unwrap(),
      &vec![MidiSrc::NoteOn { pitch: note1.pitch, velocity: note1.velocity(), channel: Default::default() }]
    );
    assert_eq!(
      events.events.get(&740).unwrap(),
      &vec![MidiSrc::NoteOff { pitch: note1.pitch, channel: Default::default() }]
    );

    assert_eq!(
      events.events.get(&980).unwrap(),
      &vec![MidiSrc::NoteOn { pitch: note1.pitch, velocity: note1.velocity(), channel: Default::default() }]
    );
    assert_eq!(
      events.events.get(&1220).unwrap(),
      &vec![MidiSrc::NoteOff { pitch: note1.pitch, channel: Default::default() }]
    );

    let play_data = events.to_play_data(48000, 240);
    let midi_data = &play_data.midi_data;

    //      0    100  200  300 340 400 480    500  600  700 740   900 960
    //      |    |    |    |   |   |   |Segno |    |    |   |     |   |D.S.
    //            <-- note0 -->                <-- note1 -->
    // tempo|120

    let cycle = 100 * 48000 * 60 / 120 / 240;
    assert_eq!(midi_data[0], (cycle, vec![0x90, note0.pitch.value(), Velocity::default().as_u8()]));
    assert_eq!(play_data.cycle_to_tick(cycle, 48000), 100);
    assert_eq!(play_data.accum_tick_to_tick(100), 100);

    let cycle = 340 * 48000 * 60 / 120 / 240;
    assert_eq!(midi_data[1], (cycle, vec![0x90, note0.pitch.value(), 0]));
    assert_eq!(play_data.cycle_to_tick(cycle, 48000), 340);
    assert_eq!(play_data.accum_tick_to_tick(340), 340);

    let cycle = 500 * 48000 * 60 / 120 / 240;
    assert_eq!(midi_data[2], (cycle, vec![0x90, note1.pitch.value(), Velocity::default().as_u8()]));
    assert_eq!(play_data.cycle_to_tick(cycle, 48000), 500);
    assert_eq!(play_data.accum_tick_to_tick(500), 500);

    let cycle = 740 * 48000 * 60 / 120 / 240;
    assert_eq!(midi_data[3], (cycle, vec![0x90, note1.pitch.value(), 0]));
    assert_eq!(play_data.cycle_to_tick(cycle, 48000), 740);
    assert_eq!(play_data.accum_tick_to_tick(740), 740);

    let cycle = 980 * 48000 * 60 / 120 / 240;
    assert_eq!(midi_data[4], (cycle, vec![0x90, note1.pitch.value(), Velocity::default().as_u8()]));
    assert_eq!(play_data.cycle_to_tick(cycle, 48000), 980);
    assert_eq!(play_data.accum_tick_to_tick(980), 500);

    let cycle = 1220 * 48000 * 60 / 120 / 240;
    assert_eq!(midi_data[5], (cycle, vec![0x90, note1.pitch.value(), 0]));
    assert_eq!(play_data.cycle_to_tick(cycle, 48000), 1220);
    assert_eq!(play_data.accum_tick_to_tick(1220), 740);
  } 
}