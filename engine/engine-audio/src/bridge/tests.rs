use super::*;
use crate::{AudioHandle, Command};
use engine_scene::{AudioGroup, ClipParams, SceneDocument, VoiceId, World};
use flume::{Receiver, Sender};
use std::cell::RefCell;

const SCENE: &str = r#"
[[assets]]
path = "/audio/hum.wav"
[[assets]]
path = "/audio/shot.wav"

[[objects]]
id = "camera"
[objects.camera]

[[objects]]
id = "speaker"
[objects.transform]
position = [3.0, 0.0, 0.0]
[objects.audio_source]
clip = "/audio/hum.wav"
group = "ambient"
looping = true
play_on_start = true
spatial = true

[[objects]]
id = "shot"
[objects.audio_source]
clip = "/audio/shot.wav"
"#;

fn world(text: &str) -> World {
    let document = SceneDocument::parse("scenes/test.rvescene", text).unwrap();
    World::from_document(&document).unwrap()
}

fn voice(id: u64) -> VoiceId {
    VoiceId::from_raw(id).expect("тестовый id ненулевой")
}

struct Rig {
    bridge: AudioBridge,
    handle: AudioHandle,
    commands: Receiver<Command>,
    events: Sender<AudioEvent>,
    sent: RefCell<Vec<Command>>,
}

impl Rig {
    fn new() -> Self {
        let (handle, commands, events) = AudioHandle::detached();
        Self {
            bridge: AudioBridge::new(handle.clone()),
            handle,
            commands,
            events,
            sent: RefCell::new(Vec::new()),
        }
    }

    /// Забирает команды, которые бридж отправил в хендл.
    fn take(&self) -> Vec<Command> {
        let mut sent = self.sent.borrow_mut();
        sent.extend(self.commands.try_iter());
        std::mem::take(&mut *sent)
    }
}

fn play_source(object: ObjectHandle, voice: VoiceId) -> AudioRequest {
    AudioRequest::PlaySource { object, voice }
}

#[test]
fn start_preloads_clips_and_plays_on_start_sources() {
    let mut rig = Rig::new();
    let world = world(SCENE);
    rig.bridge.start(&world);
    let sent = rig.take();
    assert!(
        matches!(&sent[0], Command::Preload(paths)
            if paths == &["audio/hum.wav".to_owned(), "audio/shot.wav".to_owned()]),
        "{sent:?}"
    );
    match &sent[1] {
        Command::Play { path, request, .. } => {
            assert_eq!(path, "audio/hum.wav");
            assert_eq!(request.group, AudioGroup::Ambient);
            assert!(request.looping);
            assert_eq!(
                request.spatial,
                Some(Distances {
                    min: 1.0,
                    max: 30.0
                })
            );
        }
        other => panic!("ожидался Play: {other:?}"),
    }
    assert_eq!(
        sent.len(),
        2,
        "one-shot без play_on_start не запускается: {sent:?}"
    );
    assert!(
        rig.bridge
            .sources
            .contains_key(&world.handle("speaker").unwrap())
    );
}

#[test]
fn looping_source_is_not_started_twice() {
    let mut rig = Rig::new();
    let world = world(SCENE);
    rig.bridge.start(&world);
    rig.take();
    let speaker = world.handle("speaker").unwrap();
    rig.bridge.apply([play_source(speaker, voice(50))], &world);
    assert!(rig.take().is_empty());
}

#[test]
fn one_shot_source_replays_after_its_voice_ends_and_restarts_while_playing() {
    let mut rig = Rig::new();
    let world = world(SCENE);
    let shot = world.handle("shot").unwrap();

    rig.bridge.apply([play_source(shot, voice(1))], &world);
    assert!(matches!(rig.take().as_slice(), [Command::Play { voice: v, .. }] if *v == voice(1)));

    // Повторный запуск во время игры перезапускает голос.
    rig.bridge.apply([play_source(shot, voice(2))], &world);
    let restart = rig.take();
    assert!(
        matches!(restart.as_slice(), [Command::Stop(old), Command::Play { voice: new, .. }]
            if *old == voice(1) && *new == voice(2)),
        "{restart:?}"
    );

    rig.events.send(AudioEvent::VoiceEnded(voice(2))).unwrap();
    rig.bridge.apply([play_source(shot, voice(3))], &world);
    assert!(matches!(rig.take().as_slice(), [Command::Play { voice: v, .. }] if *v == voice(3)));
}

#[test]
fn emitter_follows_the_world_matrix_of_the_source() {
    let mut rig = Rig::new();
    let first = world(SCENE);
    rig.bridge.start(&first);
    let speaker = first.handle("speaker").unwrap();
    let speaker_voice = rig.bridge.sources[&speaker];
    rig.bridge.sync_positions(&first);
    let lane = rig.handle.lane_of(speaker_voice).unwrap();
    assert_eq!(rig.handle.spatial().emitter(lane), [3.0, 0.0, 0.0]);

    let moved = world(&SCENE.replace("position = [3.0, 0.0, 0.0]", "position = [5.0, 0.0, 0.0]"));
    rig.bridge.sync_positions(&moved);
    assert_eq!(rig.handle.spatial().emitter(lane), [5.0, 0.0, 0.0]);
}

#[test]
fn removed_source_stops_its_voice() {
    let mut rig = Rig::new();
    let first = world(SCENE);
    rig.bridge.start(&first);
    rig.take();
    let speaker = first.handle("speaker").unwrap();
    let speaker_voice = rig.bridge.sources[&speaker];

    let speaker_source = "[objects.audio_source]\nclip = \"/audio/hum.wav\"\ngroup = \"ambient\"\nlooping = true\nplay_on_start = true\nspatial = true\n";
    let without_audio = world(&SCENE.replace(speaker_source, ""));
    rig.bridge.sync_positions(&without_audio);
    let sent = rig.take();
    assert!(
        matches!(sent.as_slice(), [Command::Stop(stopped)] if *stopped == speaker_voice),
        "{sent:?}"
    );
    assert!(rig.bridge.sources.is_empty());
}

#[test]
fn clip_outside_assets_still_plays_at_its_position() {
    let mut rig = Rig::new();
    let world = world(SCENE);
    let clip = VoiceId::allocate();
    rig.bridge.apply(
        [AudioRequest::PlayClip {
            voice: clip,
            path: engine_files::VirtualPath::parse("audio/undeclared.wav").unwrap(),
            params: ClipParams {
                position: Some([1.0, 2.0, 3.0]),
                ..ClipParams::default()
            },
        }],
        &world,
    );
    let sent = rig.take();
    let Some(Command::Play { lane, request, .. }) = sent.first() else {
        panic!("ожидался Play: {sent:?}");
    };
    assert_eq!(request.spatial, Some(SCRIPT_DISTANCES));
    assert_eq!(rig.handle.spatial().emitter(*lane), [1.0, 2.0, 3.0]);
}

#[test]
fn volume_override_applies_to_the_current_voice_and_the_next_start() {
    let mut rig = Rig::new();
    let world = world(SCENE);
    let shot = world.handle("shot").unwrap();
    rig.bridge.apply([play_source(shot, voice(1))], &world);
    rig.take();

    rig.bridge.apply(
        [AudioRequest::SetSourceVolume {
            object: shot,
            volume: 0.25,
        }],
        &world,
    );
    assert!(matches!(
        rig.take().as_slice(),
        [Command::SetVoiceVolume { voice: v, volume }] if *v == voice(1) && *volume == 0.25
    ));

    rig.events.send(AudioEvent::VoiceEnded(voice(1))).unwrap();
    rig.bridge.apply([play_source(shot, voice(2))], &world);
    match rig.take().as_slice() {
        [Command::Play { request, .. }] => assert_eq!(request.volume, 0.25),
        other => panic!("ожидался Play: {other:?}"),
    }
}

#[test]
fn stop_source_and_group_volume_reach_the_handle() {
    let mut rig = Rig::new();
    let world = world(SCENE);
    let shot = world.handle("shot").unwrap();
    rig.bridge.apply([play_source(shot, voice(1))], &world);
    rig.take();
    rig.bridge.apply(
        [
            AudioRequest::StopSource { object: shot },
            AudioRequest::SetGroupVolume {
                group: AudioGroup::Music,
                volume: 0.5,
            },
        ],
        &world,
    );
    let sent = rig.take();
    assert!(
        matches!(sent.as_slice(), [Command::Stop(stopped), Command::SetGroupVolume { group: AudioGroup::Music, volume }]
            if *stopped == voice(1) && *volume == 0.5),
        "{sent:?}"
    );
    assert!(rig.bridge.sources.is_empty());
}
