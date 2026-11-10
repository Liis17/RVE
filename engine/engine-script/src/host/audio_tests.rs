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

#[test]
fn audio_requests_per_frame_are_capped_and_the_overflow_is_a_script_error() {
    let (mut world, mut host) = load(
        r#"
local Script = {}
function Script:on_update()
  local accepted = 0
  for index = 1, 300 do
    if pcall(rve.audio.play, "/audio/hum.wav") then
      accepted = accepted + 1
    end
  end
  if accepted ~= 256 then
    error("accepted " .. accepted)
  end
end
return Script
"#,
    );
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());
    assert_eq!(
        host.drain_audio_requests().len(),
        crate::transaction::MAX_AUDIO_REQUESTS_PER_FRAME
    );
    assert!(
        host.drain_events()
            .iter()
            .all(|event| event.level != ScriptEventLevel::Error),
        "лимит отклоняет запрос, а не ломает скрипт"
    );
}

#[test]
fn volume_above_the_cap_is_rejected_in_lua_and_the_cap_itself_is_accepted() {
    let (mut world, mut host) = load(
        r#"
local Script = {}
function Script:on_update()
  assert(pcall(rve.audio.set_group_volume, "music", 4))
  assert(not pcall(rve.audio.set_group_volume, "music", 4.5))
  assert(not pcall(rve.audio.play, "/audio/hum.wav", {volume = 5}))
  assert(pcall(rve.audio.play, "/audio/hum.wav", {volume = 4}))
end
return Script
"#,
    );
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());
    assert_eq!(host.drain_audio_requests().len(), 2);
    assert!(
        host.drain_events()
            .iter()
            .all(|event| event.level != ScriptEventLevel::Error)
    );
}

#[test]
fn sdk_and_scene_agree_on_the_volume_cap() {
    assert_eq!(rve_sdk::MAX_AUDIO_VOLUME, engine_scene::MAX_AUDIO_VOLUME);
}

#[test]
fn set_voice_position_is_rejected_for_a_two_d_voice_and_accepted_for_a_spatial_one() {
    let (mut world, mut host) = load(
        r#"
local Script = {}
function Script:on_update()
  if self.flat == nil then
    self.flat = rve.audio.play("/audio/hum.wav")
    self.spatial = rve.audio.play("/audio/hum.wav", {position = {x = 0, y = 0, z = 0}})
  elseif self.phase == nil then
    self.phase = "spatial"
    rve.audio.set_voice_position(self.spatial, {x = 1, y = 2, z = 3})
  else
    rve.audio.set_voice_position(self.flat, {x = 1, y = 2, z = 3})
  end
end
return Script
"#,
    );
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());
    assert_eq!(host.drain_audio_requests().len(), 2);

    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());
    let requests = host.drain_audio_requests();
    assert!(
        matches!(requests.as_slice(), [AudioRequest::SetVoicePosition { .. }]),
        "{requests:?}"
    );
    assert!(host.drain_events().is_empty());

    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());
    assert!(host.drain_audio_requests().is_empty());
    assert!(
        host.drain_events()
            .iter()
            .any(|event| event.level == ScriptEventLevel::Error
                && event.message.contains("spatial voice")),
        "2D-голос должен отвергаться с ошибкой"
    );
}

#[test]
fn set_voice_position_on_a_voice_played_in_the_same_call_is_rejected_without_queueing() {
    let (mut world, mut host) = load(
        r#"
local Script = {}
function Script:on_update()
  local voice = rve.audio.play("/audio/hum.wav")
  rve.audio.set_voice_position(voice, {x = 1, y = 2, z = 3})
end
return Script
"#,
    );
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());
    assert!(host.drain_audio_requests().is_empty());
    assert!(
        host.drain_events()
            .iter()
            .any(|event| event.level == ScriptEventLevel::Error
                && event.message.contains("spatial voice"))
    );
}

#[test]
fn play_with_both_position_and_follow_is_a_script_error_and_queues_nothing() {
    let (mut world, mut host) = load(
        r#"
local Script = {}
function Script:on_update()
  rve.audio.play("/audio/hum.wav", {position = {x = 1, y = 0, z = 0}, follow = rve.find("player")})
end
return Script
"#,
    );
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());
    assert!(host.drain_audio_requests().is_empty());
    assert!(
        host.drain_events()
            .iter()
            .any(|event| event.level == ScriptEventLevel::Error
                && event.message.contains("нельзя задать одновременно")),
    );
}
