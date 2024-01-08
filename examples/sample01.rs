use std::rc::Rc;
use klavier_core::{note::Note, pitch::Pitch, solfa::Solfa, octave::Octave, sharp_flat::SharpFlat, project::ModelChangeMetadata, bar::{Bar, Repeat, RepeatSet}, key::Key, repeat_set, tempo::Tempo, rhythm::Rhythm};
use klavier_helper::{bag_store::BagStore, store::Store};
use klavier_jack::player::Player;

fn main() {
  let (mut player, status) = Player::open("klavier", None).unwrap();
  println!("Status: {:?}", status);

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

  player.play(
    1, Rhythm::new(2, 4),
    Key::SHARP_1,
    &note_repo,
    &bar_repo,
    &tempo_repo,
    &Store::new(false),
    &Store::new(false),
  ).unwrap();

  loop {
    let resp = player.get_resp();
    println!("resp: {:?}", resp);
  }
}
