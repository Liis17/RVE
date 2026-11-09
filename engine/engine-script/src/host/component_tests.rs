use super::ScriptHost;
use crate::{ScriptEventLevel, ScriptTime};
use engine_scene::world::ParticleCommand;
use engine_scene::{AudioRequest, AudioRolloff, SceneDocument, World};
use std::collections::BTreeMap;

const SCENE: &str = r#"
[[assets]]
path = "/driver.lua"
[[assets]]
path = "/audio/hum.wav"
[[assets]]
path = "/models/crate.fbx"

[[objects]]
id = "camera"
[objects.camera]

[[objects]]
id = "driver"
[[objects.lite_scripts]]
path = "/driver.lua"

[[objects]]
id = "lamp"
[objects.point_light]
intensity = 1.0

[[objects]]
id = "crate"
[objects.mesh_renderer]
model = "/models/crate.fbx"

[[objects]]
id = "fire"
[objects.particle_emitter]

[[objects]]
id = "sky"
[objects.environment.procedural]
zenith_color = [0.1, 0.2, 0.8]
horizon_color = [0.6, 0.6, 0.6]
ground_color = [0.1, 0.1, 0.1]
sun_size_deg = 1.0
sun_intensity = 3.0

[[objects]]
id = "tower"
[objects.mesh_renderer]
model = "/models/crate.fbx"
[objects.lod_group]
cull_distance = 50.0
[[objects.lod_group.levels]]
model = "/models/crate.fbx"
distance = 10.0

[[objects]]
id = "speaker"
[objects.audio_source]
clip = "/audio/hum.wav"
"#;

fn load(source: &str) -> (World, ScriptHost) {
    let world =
        World::from_document(&SceneDocument::parse("test.rvescene", SCENE).unwrap()).unwrap();
    let sources = BTreeMap::from([("driver.lua".to_owned(), source.to_owned())]);
    let host = ScriptHost::load(&world, "scene", &sources, None).unwrap();
    (world, host)
}

fn run_once(world: &mut World, host: &mut ScriptHost) {
    host.update(world, 1.0 / 60.0, ScriptTime::default());
}

fn errors(host: &mut ScriptHost) -> Vec<String> {
    host.drain_events()
        .into_iter()
        .filter(|event| event.level == ScriptEventLevel::Error)
        .map(|event| event.message)
        .collect()
}

#[test]
fn component_writes_reach_the_world_after_a_successful_call() {
    let (mut world, mut host) = load(
        r#"
local Script = {}
function Script:on_update()
  rve.find("lamp"):set_light({ intensity = 3.0, color = {x = 0.0, y = 1.0, z = 0.0}, enabled = false })
  rve.find("crate"):set_visible(false)
  rve.find("crate"):set_material_params({ metallic = 0.5, base_color = {x = 0.2, y = 0.4, z = 0.6} })
  rve.find("fire"):particles_stop()
  rve.find("fire"):particles_emit(12)
  rve.find("sky"):set_environment({ intensity = 2.0, sky = { sun_size_deg = 2.0 } })
  rve.find("tower"):set_lod_distances({ 20.0 }, 80.0)
  rve.find("speaker"):set_audio_source({ volume = 0.5, spatial_blend = 0.25, rolloff = "linear", pitch = 2.0 })
  rve.audio.play("/audio/hum.wav", { follow = rve.find("speaker"), spatial_blend = 1.0, force_mono = true })
end
return Script
"#,
    );
    run_once(&mut world, &mut host);
    let errs = errors(&mut host);
    assert!(errs.is_empty(), "{errs:?}");

    let lamp = world.handle("lamp").unwrap();
    let light = world.object(lamp).unwrap().point_light.unwrap();
    assert_eq!((light.intensity, light.color), (3.0, [0.0, 1.0, 0.0]));
    assert!(!world.object(lamp).unwrap().light_enabled);

    let crate_handle = world.handle("crate").unwrap();
    assert!(!world.object(crate_handle).unwrap().mesh_visible);
    let material = world
        .object(crate_handle)
        .unwrap()
        .mesh_renderer
        .as_ref()
        .unwrap()
        .material
        .clone()
        .unwrap();
    assert_eq!(
        (material.metallic, material.base_color),
        (Some(0.5), Some([0.2, 0.4, 0.6]))
    );

    let fire = world.handle("fire").unwrap();
    assert!(!world.object(fire).unwrap().particles_playing);
    assert_eq!(
        host.drain_particle_commands(),
        vec![ParticleCommand::Burst {
            object: fire,
            count: 12
        }]
    );

    let sky = world.handle("sky").unwrap();
    let environment = world.object(sky).unwrap().environment.clone().unwrap();
    assert_eq!(environment.intensity, 2.0);
    assert_eq!(environment.procedural.unwrap().sun_size_deg, 2.0);
    assert_eq!(
        environment.procedural.unwrap().sun_intensity,
        3.0,
        "неуказанные поля сохраняются"
    );

    let tower = world.object(world.handle("tower").unwrap()).unwrap();
    let group = tower.lod_group.clone().unwrap();
    assert_eq!(
        (group.levels[0].distance, group.cull_distance),
        (20.0, Some(80.0))
    );

    let speaker = world.object(world.handle("speaker").unwrap()).unwrap();
    let audio = speaker.audio_source.clone().unwrap();
    assert_eq!(audio.volume, 0.5);
    assert_eq!(audio.spatial_blend, 0.25);
    assert_eq!(audio.rolloff, AudioRolloff::Linear);
    assert_eq!(audio.pitch, 2.0);
    assert_eq!(
        audio.clip.as_str(),
        "audio/hum.wav",
        "клип сцены не меняется"
    );

    let play = host
        .drain_audio_requests()
        .into_iter()
        .find_map(|request| match request {
            AudioRequest::PlayClip { params, .. } => Some(params),
            _ => None,
        })
        .expect("play request");
    assert_eq!(play.follow, Some(world.handle("speaker").unwrap()));
    assert_eq!((play.spatial_blend, play.force_mono), (1.0, true));
}

#[test]
fn an_invalid_value_refuses_the_whole_call_and_keeps_earlier_writes_out() {
    let (mut world, mut host) = load(
        r#"
local Script = {}
function Script:on_update()
  rve.find("crate"):set_visible(false)
  rve.find("lamp"):set_light({ intensity = -1.0 })
end
return Script
"#,
    );
    run_once(&mut world, &mut host);
    assert!(
        !errors(&mut host).is_empty(),
        "неверная интенсивность отклоняется ошибкой вызова"
    );
    let crate_handle = world.handle("crate").unwrap();
    assert!(
        world.object(crate_handle).unwrap().mesh_visible,
        "запись, сделанная до ошибки, не применяется"
    );
    let lamp = world.handle("lamp").unwrap();
    assert_eq!(
        world.object(lamp).unwrap().point_light.unwrap().intensity,
        1.0
    );
}

#[test]
fn missing_components_and_bad_ranges_are_refused_at_the_call() {
    for body in [
        r#"rve.find("crate"):set_light({ intensity = 1.0 })"#,
        r#"rve.find("lamp"):particles_stop()"#,
        r#"rve.find("crate"):set_material_params({ metallic = 1.5 })"#,
        r#"rve.find("fire"):particles_emit(0)"#,
        r#"rve.find("tower"):set_lod_distances({ 30.0, 20.0 }, nil)"#,
        r#"rve.find("sky"):set_environment({ intensity = -1.0 })"#,
        r#"rve.find("speaker"):set_audio_source({ pitch = 0.0 })"#,
        r#"rve.audio.play("/audio/hum.wav", { spatial_blend = 2.0, position = {x=0, y=0, z=0} })"#,
    ] {
        let (mut world, mut host) = load(&format!(
            "local Script = {{}}\nfunction Script:on_update()\n  {body}\nend\nreturn Script\n"
        ));
        run_once(&mut world, &mut host);
        assert!(
            !errors(&mut host).is_empty(),
            "ожидалась ошибка вызова: {body}"
        );
        assert!(host.drain_particle_commands().is_empty(), "{body}");
        assert!(host.drain_audio_requests().is_empty(), "{body}");
    }
}

#[test]
fn a_clip_cannot_follow_an_object_that_is_inactive() {
    let (mut world, mut host) = load(
        r#"
local Script = {}
function Script:on_update()
  rve.find("tower"):set_active(false)
  rve.audio.play("/audio/hum.wav", { follow = rve.find("tower") })
end
return Script
"#,
    );
    run_once(&mut world, &mut host);
    assert!(
        !errors(&mut host).is_empty(),
        "follow на неактивный объект отклоняется"
    );
    assert!(host.drain_audio_requests().is_empty());
}

#[test]
fn a_caught_environment_error_leaves_the_intensity_unchanged() {
    let (mut world, mut host) = load(
        r#"
local Script = {}
function Script:on_update()
  local ok = pcall(function()
    rve.find("sky"):set_environment({ intensity = 2.0, sky = { sun_size_deg = 0.0 } })
  end)
  if not ok then
    rve.find("crate"):set_visible(false)
  end
end
return Script
"#,
    );
    let sky = world.handle("sky").unwrap();
    let before = world.object(sky).unwrap().environment.clone();
    run_once(&mut world, &mut host);
    assert!(
        !world.object(world.handle("crate").unwrap()).unwrap().mesh_visible,
        "вызов должен отказать, а скрипт продолжить"
    );
    let after = world.object(sky).unwrap().environment.clone();
    assert_eq!(format!("{after:?}"), format!("{before:?}"));
}

#[test]
fn a_caught_light_error_leaves_the_light_unchanged() {
    let (mut world, mut host) = load(
        r#"
local Script = {}
function Script:on_update()
  local ok = pcall(function()
    rve.find("lamp"):set_light({ intensity = 3.0, enabled = "no" })
  end)
  if not ok then
    rve.find("crate"):set_visible(false)
  end
end
return Script
"#,
    );
    run_once(&mut world, &mut host);
    assert!(
        !world.object(world.handle("crate").unwrap()).unwrap().mesh_visible,
        "вызов должен отказать, а скрипт продолжить"
    );
    let lamp = world.object(world.handle("lamp").unwrap()).unwrap();
    assert_eq!(lamp.point_light.unwrap().intensity, 1.0);
    assert!(lamp.light_enabled);
}
