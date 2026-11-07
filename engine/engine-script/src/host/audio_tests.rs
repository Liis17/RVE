use super::ScriptHost;
use crate::{ScriptEventLevel, ScriptTime};
use engine_scene::{AudioGroup, AudioRequest, SceneDocument, World};
use std::collections::{BTreeMap, HashSet};

const PLAYER: &str = r#"
[[assets]]
path = "/audio/hum.wav"
[[assets]]
path = "/player.lua"
[[objects]]
id = "camera"
[objects.camera]

[[objects]]
id = "player"
[objects.audio_source]
clip = "/audio/hum.wav"

[[objects]]
id = "silent"
[[objects.lite_scripts]]
path = "/player.lua"
"#;

fn world() -> World {
    World::from_document(&SceneDocument::parse("test.rvescene", PLAYER).unwrap()).unwrap()
}

fn load(source: &str) -> (World, ScriptHost) {
    let world = world();
    let sources = BTreeMap::from([("player.lua".to_owned(), source.to_owned())]);
    let host = ScriptHost::load(&world, "scene", &sources, None).unwrap();
    (world, host)
}

#[test]
fn audio_requests_appear_only_after_a_successful_commit() {
    let (mut world, mut host) = load(
        r#"
local Script = {}
function Script:on_update()
  rve.find("player"):play_sound()
  rve.audio.set_group_volume("music", 0.5)
  rve.audio.play("/audio/hum.wav", {group = "ambient", volume = 0.25, looping = true, position = {x = 1, y = 2, z = 3}})
end
return Script
"#,
    );
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());
    let requests = host.drain_audio_requests();
    assert_eq!(requests.len(), 3, "{requests:?}");
    assert!(matches!(requests[0], AudioRequest::PlaySource { .. }));
    assert!(matches!(
        requests[1],
        AudioRequest::SetGroupVolume {
            group: AudioGroup::Music,
            volume
        } if volume == 0.5
    ));
    match &requests[2] {
        AudioRequest::PlayClip { path, params, .. } => {
            assert_eq!(path.as_str(), "audio/hum.wav");
            assert_eq!(params.group, AudioGroup::Ambient);
            assert_eq!(params.volume, 0.25);
            assert!(params.looping);
            assert_eq!(params.position, Some([1.0, 2.0, 3.0]));
        }
        other => panic!("ожидался PlayClip: {other:?}"),
    }
    assert!(
        host.drain_audio_requests().is_empty(),
        "очередь забирается один раз"
    );
}

#[test]
fn a_failing_script_discards_its_audio_requests() {
    let (mut world, mut host) = load(
        r#"
local Script = {}
function Script:on_update()
  rve.find("player"):play_sound()
  rve.audio.play("/audio/hum.wav")
  error("boom")
end
return Script
"#,
    );
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());
    assert!(host.drain_audio_requests().is_empty());
    assert!(
        host.drain_events()
            .iter()
            .any(|event| event.level == ScriptEventLevel::Error && event.message.contains("boom"))
    );
}

#[test]
fn voice_ids_are_nonzero_and_unique_within_a_session() {
    let (mut world, mut host) = load(
        r#"
local Script = {}
function Script:on_update()
  local first = rve.audio.play("/audio/hum.wav")
  local second = rve.audio.play("/audio/hum.wav")
  rve.find("player"):play_sound()
  assert(first ~= second and first > 0 and second > 0)
  rve.audio.stop(first)
end
return Script
"#,
    );
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());
    let ids: HashSet<_> = host
        .drain_audio_requests()
        .into_iter()
        .filter_map(|request| match request {
            AudioRequest::PlayClip { voice, .. } | AudioRequest::PlaySource { voice, .. } => {
                Some(voice)
            }
            _ => None,
        })
        .collect();
    assert_eq!(ids.len(), 3, "каждый запуск получает свой id");
}

#[test]
fn play_sound_without_an_audio_source_is_a_lua_error() {
    let (mut world, mut host) = load(
        r#"
local Script = {}
function Script:on_update()
  local ok = pcall(function() rve.find("silent"):play_sound() end)
  assert(not ok)
  local stopped = pcall(function() rve.find("silent"):stop_sound() end)
  assert(not stopped)
end
return Script
"#,
    );
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());
    assert!(host.drain_audio_requests().is_empty());
    assert!(host.drain_events().is_empty());
}

#[test]
fn invalid_group_volume_or_path_is_rejected_without_queueing() {
    let (mut world, mut host) = load(
        r#"
local Script = {}
function Script:on_update()
  assert(not pcall(rve.audio.set_group_volume, "sfx", 1))
  assert(not pcall(rve.audio.set_group_volume, "music", -1))
  assert(not pcall(rve.audio.set_group_volume, "music", 0/0))
  assert(not pcall(rve.audio.play, "/audio/hum.wav", {group = "master"}))
  assert(not pcall(rve.audio.play, "/audio/hum.wav", {volume = -0.5}))
  assert(not pcall(rve.audio.play, "../escape.wav"))
  assert(not pcall(rve.audio.stop, 0))
  assert(rve.find("player") ~= nil)
end
return Script
"#,
    );
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());
    assert!(host.drain_audio_requests().is_empty());
}
