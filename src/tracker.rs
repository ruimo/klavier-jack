use std::{fmt::Display, sync::{Arc, Mutex}, thread, rc::Rc};

use klavier_core::{bar::Bar, rhythm::Rhythm, key::Key, note::Note, project::ModelChangeMetadata, tempo::Tempo, ctrl_chg::CtrlChg, global_repeat::RenderRegionWarning};
use klavier_helper::{bag_store::BagStore, store::Store};
use crate::player::{PlayError, AccumTick};
use error_stack::{Result, Context};
use tracing::error;

#[cfg(not(test))]
use crate::player::JackClientProxy;

#[cfg(test)]
use crate::player::TestJackClientProxy as JackClientProxy;

//#[cfg(not(test))]
use crate::player::Player;

//#[cfg(test)]
//use crate::player::TestPlayer as Player;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Status {
  Disconnected,
  Connected,
  StartingPlay { seq: usize },
  StoppingPlay { seq: usize },
  Playing { seq: usize, tick: u32, accum_tick: AccumTick },
}

pub struct Tracker {
  seq: usize,
  player: Option<Player>,
  status: Arc<Mutex<Status>>,
  client_factory: Option<Box<dyn FnOnce(&str, jack::ClientOptions) -> Result<JackClientProxy, jack::Error>>>,
}

#[derive(Debug, PartialEq)]
pub enum TrackerError {
  PlayerNotOpened,
  PlayerStopping { seq: usize },
  PlayerErr(PlayError),
}

impl Display for TrackerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
      write!(f, "{:?}", self)
    }
}

impl Context for TrackerError {}

impl Tracker {
  pub fn new(client_factory: Option<Box<dyn FnOnce(&str, jack::ClientOptions) -> Result<JackClientProxy, jack::Error>>>) -> Self {
    Self {
      seq: 0,
      player: None,
      status: Arc::new(Mutex::new(Status::Disconnected)),
      client_factory,
    }
  }

  pub fn run(&mut self, name: &str) -> Result<jack::ClientStatus, jack::Error> {
    if *self.status.lock().unwrap() == Status::Disconnected {
      let (mut player, status) = Player::open(name, self.client_factory.take())?;
      *self.status.lock().unwrap() = Status::Connected;
      let receiver = player.take_resp().unwrap();
      self.player = Some(player);

      let cur_status = self.status.clone();
      thread::spawn(move || {
        loop {
          match receiver.recv() {
            Ok(resp) => match resp {
              crate::player::Resp::Err { seq, error } => {
                tracing::error!("Player command error seq: {:?}, error: {:?}", seq, error);
              }
              crate::player::Resp::Info { seq, info } => match info {
                crate::player::CmdInfo::PlayingEnded => {
                  let mut p = cur_status.lock().unwrap();
                  if let Status::StoppingPlay { seq } = *p {
                    tracing::info!("Player stopped seq: {}", seq);
                    *p = Status::Connected;
                  }
                }
                crate::player::CmdInfo::CurrentLoc{ seq, tick, accum_tick } => {
                  *cur_status.lock().unwrap() = Status::Playing { seq, tick, accum_tick }
                }
              }
              crate::player::Resp::Ok { seq } => {
                tracing::info!("Player command ok seq: {}", seq);
              }
            }
            Err(err) => {
              println!("recv error: {:?}", err);
              error!("Player receiver disconnected {:?}", err);
              *cur_status.lock().unwrap() = Status::Disconnected;
              break;
            }
          }
        }
      });      

      Ok(status)
    } else {
      // Already running. Do noting.
      Ok(jack::ClientStatus::empty())
    }
  }

  pub fn play(
    &mut self,
    seq: usize, top_rhythm: Rhythm, top_key: Key,
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
          self.seq,
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
        jack.stop(self.seq).map_err(|e| {
          let top = TrackerError::PlayerErr(e.current_context().clone());
          e.change_context(top)
        })?;
        *self.status.lock().unwrap() = Status::StoppingPlay { seq };

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

    let tracker = Tracker::new(Some(factory));
    assert_eq!(tracker.seq, 0);
    assert_eq!(tracker.player.is_none(), true);
    assert_eq!(tracker.status(), Status::Disconnected);
  }

  #[test]
  fn disconnected_play() {
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
    let mut tracker = Tracker::new(Some(factory));
    let result = tracker.play(
      1, Rhythm::new(2, 4),
      Key::SHARP_1,
      &BagStore::new(false),
      &Store::new(false),
      &Store::new(false),
      &Store::new(false),
      &Store::new(false),
    ).err().unwrap();
    assert_eq!(*result.current_context(), TrackerError::PlayerNotOpened);
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
    let mut tracker = Tracker::new(Some(factory));
    let result = tracker.stop(1).err().unwrap();
    assert_eq!(*result.current_context(), TrackerError::PlayerNotOpened);
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
    let mut tracker = Tracker::new(Some(factory));
    let resp = tracker.run("my name").unwrap();
    assert_eq!(resp, jack::ClientStatus::INIT_FAILURE);

    let result = tracker.play(
      1, Rhythm::new(2, 4),
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