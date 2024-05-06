use std::{fmt::Display, sync::{Arc, Mutex}, thread, rc::Rc};

use klavier_core::{bar::Bar, rhythm::Rhythm, key::Key, note::Note, project::ModelChangeMetadata, tempo::Tempo, ctrl_chg::CtrlChg, global_repeat::RenderRegionWarning};
use klavier_helper::{bag_store::BagStore, store::Store};
use crate::player::PlayError;
use error_stack::{Result, Context};
use tracing::error;
use crate::player;
use klavier_core::play_start_tick::PlayStartTick;
use klavier_core::repeat::AccumTick;

#[cfg(not(test))]
use player::JackClientProxy;

#[cfg(test)]
use player::TestJackClientProxy as JackClientProxy;

//#[cfg(not(test))]
use player::Player;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Status {
  Stopped,
  StartingPlay { seq: usize },
  StoppingPlay { seq: usize },
  Playing { seq: usize, tick: u32, accum_tick: AccumTick },
  Disconnected,
}

pub struct Tracker {
  seq: usize,
  player: Option<Player>,
  status: Arc<Mutex<Status>>,
}

#[derive(Debug, PartialEq)]
pub enum TrackerError {
  PlayerNotOpened,
  PlayerStopping { seq: usize },
  PlayerErr(PlayError),
  NotPlaying(Status),
}

impl Display for TrackerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
      write!(f, "{:?}", self)
    }
}

impl Context for TrackerError {}

impl Tracker {
  pub fn run(
    name: &str, mut client_factory: Option<Box<dyn FnOnce(&str, jack::ClientOptions) -> Result<JackClientProxy, jack::Error>>>
  ) -> Result<(Self, jack::ClientStatus), jack::Error> {
      let (mut player, client_status) = Player::open(name, client_factory.take())?;
      let receiver = player.take_resp().unwrap();
      let status = Arc::new(Mutex::new(Status::Stopped));
      let moved_status = status.clone();
      thread::spawn(move || {
        loop {
          match receiver.recv() {
            Ok(resp) => match resp {
              player::Resp::Err { seq, error } => {
                tracing::error!("Player command error seq: {:?}, error: {:?}", seq, error);
              }
              player::Resp::Info { seq, info } => match info {
                player::CmdInfo::PlayingEnded => {
                  *moved_status.lock().unwrap() = Status::Stopped;
                }
                player::CmdInfo::CurrentLoc{ seq, tick, accum_tick } => {
                  *moved_status.lock().unwrap() = Status::Playing { seq, tick, accum_tick }
                }
              }
              player::Resp::Ok { seq } => {
                tracing::info!("Player command ok seq: {}", seq);
              }
            }
            Err(err) => {
              println!("recv error: {:?}", err);
              error!("Player receiver disconnected {:?}", err);
              *moved_status.lock().unwrap() = Status::Disconnected;
              break;
            }
          }
        }
      });      

      Ok((
        Self {
          seq: 0,
          player: Some(player),
          status,
        },
        client_status
      ))
  }

  pub fn play(
    &mut self,
    seq: usize, play_start_tick: Option<PlayStartTick>, top_rhythm: Rhythm, top_key: Key,
    note_repo: &BagStore<u32, Rc<Note>, ModelChangeMetadata>,
    bar_repo: &Store<u32, Bar, ModelChangeMetadata>,
    tempo_repo: &Store<u32, Tempo, ModelChangeMetadata>,
    dumper_repo: &Store<u32, CtrlChg, ModelChangeMetadata>,
    soft_repo: &Store<u32, CtrlChg, ModelChangeMetadata>,
  ) -> Result <Vec<RenderRegionWarning>, TrackerError> {
    match &mut self.player {
      Some(jack) => {
        if let Status::StoppingPlay { seq } = *self.status.lock().unwrap() {
          Err(TrackerError::PlayerStopping { seq })?;
        }

        let p = jack.play(
          self.seq, play_start_tick,
          top_rhythm, top_key, note_repo, bar_repo, tempo_repo, dumper_repo, soft_repo
        ).map_err(|e| {
          let top = TrackerError::PlayerErr(e.current_context().clone());
          e.change_context(top)
        })?;
        *self.status.lock().unwrap() = Status::StartingPlay { seq };
        self.seq += 1;

        Ok(p)
      }
      None => Err(TrackerError::PlayerNotOpened)?,
    }
  }

  pub fn stop(&mut self, seq: usize) -> Result <(), TrackerError> {
    match &mut self.player {
      Some(jack)=> {
        let mut pstatus = self.status.lock().unwrap();
        if let Status::Playing { seq: _, tick: _, accum_tick: _ } = *pstatus {
          jack.stop(self.seq).map_err(|e| {
            let top = TrackerError::PlayerErr(e.current_context().clone());
            e.change_context(top)
          })?;
          *pstatus = Status::StoppingPlay { seq };
        } else {
          Err(TrackerError::NotPlaying(*pstatus))?
        }

        Ok(())
      }
      None => Err(TrackerError::PlayerNotOpened)?,
    }
  }

  pub fn status(&self) -> Status {
    *self.status.lock().unwrap()
  }
}

#[cfg(test)]
mod tests {
  use jack::ClientStatus;
  use klavier_core::{rhythm::Rhythm, key::Key};
  use klavier_helper::{bag_store::BagStore, store::Store};
  use crate::tracker::{Status, TrackerError};
  use super::Tracker;
  use crate::player::TestJackClientProxy;
  use klavier_core::play_start_tick::PlayStartTick;

  #[test]
  fn new() {
    let factory = Box::new(|app_name: &str, _options|
      Ok(
        TestJackClientProxy {
          status: ClientStatus::empty(),
          app_name: app_name.to_owned(),
          buffer_size: 2048,
          sampling_rate: 48000,
        }
      )
    );

    let (tracker, _status) = Tracker::run("app name", Some(factory)).unwrap();
    assert_eq!(tracker.seq, 0);
    assert_eq!(tracker.player.is_none(), false);
    assert_eq!(tracker.status(), Status::Stopped);
  }

  #[test]
  fn disconnected_stop() {
    let factory = Box::new(|app_name: &str, _options|
      Ok(
        TestJackClientProxy {
          status: ClientStatus::empty(),
          app_name: app_name.to_owned(),
          buffer_size: 2048,
          sampling_rate: 48000,
        }
      )
    );
    let (mut tracker, _status) = Tracker::run("app name", Some(factory)).unwrap();
    assert_eq!(tracker.status(), Status::Stopped);
    let result = tracker.stop(1).err().unwrap();
    assert_eq!(*result.current_context(), TrackerError::NotPlaying(Status::Stopped));
  }

  #[test]
  fn run() {
    let factory = Box::new(|app_name: &str, _options|
      Ok(
        TestJackClientProxy {
          status: ClientStatus::INIT_FAILURE,
          app_name: app_name.to_owned(),
          buffer_size: 2048,
          sampling_rate: 48000,
        }
      )
    );
    let (mut tracker, status) = Tracker::run("my name", Some(factory)).unwrap();
    assert_eq!(status, jack::ClientStatus::INIT_FAILURE);
    let play_start_loc = None;

    let result = tracker.play(
      1, play_start_loc, Rhythm::new(2, 4),
      Key::SHARP_1,
      &BagStore::new(false),
      &Store::new(false),
      &Store::new(false),
      &Store::new(false),
      &Store::new(false),
    );
    assert_eq!(result.is_ok(), true);
    assert_eq!(tracker.status(), Status::StartingPlay { seq: 1 });
  }
}