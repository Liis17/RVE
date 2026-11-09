use super::*;
use crate::ScriptFrameTime;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

fn scripted(source: &str) -> (World, ScriptHost) {
    let world = world(
        r#"
[[assets]]
path = "/stage5.lua"
[[objects]]
id = "runner"
[[objects.lite_scripts]]
path = "/stage5.lua"
"#,
    );
    let host =
        ScriptHost::load(&world, "scene", &sources(&[("stage5.lua", source)]), None).unwrap();
    (world, host)
}

fn frame(dt: f32, scale: f32, time: ScriptFrameTime, frame: u64) -> ScriptFrameTime {
    ScriptFrameTime::advance(
        dt,
        scale,
        time.elapsed,
        time.unscaled_elapsed,
        time.fixed_dt,
        frame,
    )
}

fn messages(host: &mut ScriptHost) -> Vec<String> {
    host.drain_events()
        .into_iter()
        .map(|event| event.message)
        .collect()
}

#[test]
fn fixed_updates_precede_update_snapshot_messages_and_late_update() {
    let source = r#"
local Script = {}
function Script:on_start()
  rve.send("runner", "snapshot", true)
end
function Script:on_fixed_update(dt)
  assert(math.abs(dt - rve.time.fixed_dt) < 0.00001)
  rve.log_info("fixed")
end
function Script:on_update()
  rve.log_info("update")
end
function Script:on_message(_, name)
  if name == "snapshot" then rve.log_info("message") end
end
function Script:on_late_update()
  rve.log_info("late")
end
return Script
"#;
    let (mut world, mut host) = scripted(source);
    host.set_fixed_step_config(10, 2).unwrap();
    host.update_frame(
        &mut world,
        ScriptFrameTime::advance(0.25, 1.0, 0.0, 0.0, 0.1, 1),
    );

    let emitted = messages(&mut host);
    let ordered: Vec<_> = emitted
        .iter()
        .filter(|message| ["fixed", "update", "message", "late"].contains(&message.as_str()))
        .cloned()
        .collect();
    assert_eq!(
        ordered,
        ["fixed", "fixed", "update", "message", "late"],
        "{emitted:?}"
    );
}

#[test]
fn fixed_update_counts_match_physics_substeps_across_variable_frame_deltas() {
    let source = r#"
local Script = {}
function Script:on_fixed_update() rve.log_info("fixed") end
return Script
"#;
    let (mut world, mut host) = scripted(source);
    const FIXED_DT: f32 = 0.1;
    const MAX_SUBSTEPS: usize = 2;
    host.set_fixed_step_config(10, MAX_SUBSTEPS).unwrap();
    let mut physics_clock = engine_physics::FixedStepClock::new(10, MAX_SUBSTEPS).unwrap();
    let mut scaled_elapsed = 0.0;
    let mut unscaled_elapsed = 0.0;
    for (frame_number, raw_dt, expected_substeps) in
        [(1, 0.05, 0), (2, 0.07, 1), (3, 0.5, 2), (4, 0.05, 1)]
    {
        let time = ScriptFrameTime::advance(
            raw_dt,
            1.0,
            scaled_elapsed,
            unscaled_elapsed,
            FIXED_DT,
            frame_number,
        );
        scaled_elapsed = time.elapsed;
        unscaled_elapsed = time.unscaled_elapsed;
        assert_eq!(physics_clock.advance(time.dt).unwrap(), expected_substeps);
        host.update_frame(&mut world, time);
        let fixed_count = event_messages(&mut host)
            .iter()
            .filter(|message| *message == "fixed")
            .count();
        assert_eq!(fixed_count, expected_substeps, "frame {frame_number}");
    }
}

#[test]
fn shared_clock_clamps_accumulates_scaled_and_unscaled_time() {
    let first = ScriptFrameTime::advance(3.0, 2.0, 0.0, 0.0, 1.0 / 60.0, 1);
    assert_eq!(first.unscaled_dt, 0.25);
    assert_eq!(first.dt, 0.5);
    assert_eq!(first.elapsed, 0.5);
    assert_eq!(first.unscaled_elapsed, 0.25);

    let paused = ScriptFrameTime::advance(
        0.1,
        0.0,
        first.elapsed,
        first.unscaled_elapsed,
        first.fixed_dt,
        2,
    );
    assert_eq!(paused.dt, 0.0);
    assert_eq!(paused.elapsed, first.elapsed);
    assert!(paused.unscaled_elapsed > first.unscaled_elapsed);

    let invalid_raw_delta = ScriptFrameTime::advance(
        f32::NAN,
        1.0,
        paused.elapsed,
        paused.unscaled_elapsed,
        paused.fixed_dt,
        3,
    );
    assert_eq!(invalid_raw_delta.dt, 0.0);
    assert_eq!(invalid_raw_delta.unscaled_dt, 0.0);
}

#[test]
fn zero_scale_pauses_scaled_timers_but_frame_coroutines_keep_advancing() {
    let source = r#"
local Script = {}
function Script:on_start()
  rve.timer.after(0.1, function() rve.log_info("scaled timer fired") end)
  rve.start_coroutine(function()
    rve.wait_frames(1)
    rve.log_info("frame wait resumed")
  end)
end
function Script:on_fixed_update() rve.log_info("fixed") end
function Script:on_update(dt)
  assert(math.abs(dt - rve.time.dt) < 0.00001)
  if rve.time.frame == 1 then
    assert(rve.time.unscaled_dt == 0.25)
    assert(rve.time.scale == 1)
    rve.time.scale = 0
    assert(rve.time.scale == 0)
  elseif rve.time.frame == 2 then
    assert(dt == 0 and rve.time.elapsed == 0.25)
    assert(rve.time.unscaled_dt > 0 and rve.time.unscaled_elapsed > 0.25)
  end
end
return Script
"#;
    let (mut world, mut host) = scripted(source);
    host.set_fixed_step_config(10, 1).unwrap();
    let mut time = ScriptFrameTime::advance(2.0, 1.0, 0.0, 0.0, 0.1, 1);
    host.update_frame(&mut world, time);
    assert_eq!(host.time_scale(), 0.0, "{:?}", messages(&mut host));

    for frame_number in 2..=3 {
        time = frame(0.1, host.time_scale(), time, frame_number);
        host.update_frame(&mut world, time);
    }
    let emitted = messages(&mut host);
    assert_eq!(
        emitted.iter().filter(|message| *message == "fixed").count(),
        1
    );
    assert!(
        emitted
            .iter()
            .any(|message| message == "frame wait resumed")
    );
    assert!(
        !emitted
            .iter()
            .any(|message| message == "scaled timer fired")
    );
    assert!(!emitted.iter().any(|message| message.contains("Error")));
}

#[test]
fn timers_repeat_once_per_update_and_can_cancel_their_owner_timer() {
    let source = r#"
local Script = {}
function Script:on_start()
  local repeat_count = 0
  rve.timer.every(0.1, function(id)
    repeat_count = repeat_count + 1
    rve.log_info("repeat:" .. repeat_count)
    if repeat_count == 2 then assert(rve.timer.cancel(id)) end
  end)
  rve.timer.after(0.15, function() rve.log_info("once") end)
end
return Script
"#;
    let (mut world, mut host) = scripted(source);
    let mut time = ScriptFrameTime::default();
    for frame_number in 1..=4 {
        time = frame(0.1, 1.0, time, frame_number);
        host.update_frame(&mut world, time);
    }
    let emitted = messages(&mut host);
    assert_eq!(
        emitted
            .iter()
            .filter(|message| *message == "repeat:1")
            .count(),
        1
    );
    assert_eq!(
        emitted
            .iter()
            .filter(|message| *message == "repeat:2")
            .count(),
        1
    );
    assert_eq!(
        emitted.iter().filter(|message| *message == "once").count(),
        1
    );
}

#[test]
fn owner_disable_cancels_timers_before_their_due_phase() {
    let source = r#"
local Script = {}
function Script:on_start()
  rve.timer.after(0.1, function() rve.log_info("disabled timer fired") end)
  self.script:set_enabled(false)
end
return Script
"#;
    let (mut world, mut host) = scripted(source);
    host.update_frame(
        &mut world,
        ScriptFrameTime::advance(0.1, 1.0, 0.0, 0.0, 1.0 / 60.0, 1),
    );
    host.update_frame(
        &mut world,
        ScriptFrameTime::advance(0.1, 1.0, 0.1, 0.1, 1.0 / 60.0, 2),
    );
    assert!(
        !messages(&mut host)
            .iter()
            .any(|message| message == "disabled timer fired")
    );
}

#[test]
fn coroutine_frame_seconds_and_predicate_waits_resume_at_their_due_frames() {
    let source = r#"
local Script = {}
function Script:on_start()
  rve.start_coroutine(function()
    rve.wait_frames(2)
    rve.log_info("frames:" .. rve.time.frame)
  end)
  rve.start_coroutine(function()
    rve.wait(0.2)
    rve.log_info("seconds:" .. rve.time.frame)
  end)
  rve.start_coroutine(function()
    rve.wait_until(function() return rve.time.frame >= 3 end)
    rve.log_info("predicate:" .. rve.time.frame)
  end)
end
return Script
"#;
    let (mut world, mut host) = scripted(source);
    let mut time = ScriptFrameTime::default();
    for frame_number in 1..=4 {
        time = frame(0.1, 1.0, time, frame_number);
        host.update_frame(&mut world, time);
    }
    let emitted = messages(&mut host);
    assert!(emitted.iter().any(|message| message == "predicate:3"));
    assert!(emitted.iter().any(|message| message == "frames:4"));
    assert!(emitted.iter().any(|message| message == "seconds:4"));
}

#[test]
fn coroutine_thread_timeout_is_not_swallowed_by_pcall() {
    let source = r#"
local Script = {}
function Script:on_start()
  rve.start_coroutine(function()
    pcall(function() while true do end end)
    rve.log_info("coroutine pcall escaped")
  end)
end
return Script
"#;
    let (mut world, mut host) = scripted_with_limit(source, Duration::from_millis(20));
    host.update_frame(
        &mut world,
        ScriptFrameTime::advance(0.1, 1.0, 0.1, 0.1, 1.0 / 60.0, 1),
    );
    host.update_frame(
        &mut world,
        ScriptFrameTime::advance(0.1, 1.0, 0.2, 0.2, 1.0 / 60.0, 2),
    );
    let emitted = messages(&mut host);
    assert!(
        emitted
            .iter()
            .any(|message| message.contains("Lua call exceeded time limit"))
    );
    assert!(
        !emitted
            .iter()
            .any(|message| message == "coroutine pcall escaped")
    );
}

#[test]
fn wait_until_predicate_uses_the_coroutine_deadline() {
    let source = r#"
local Script = {}
function Script:on_start()
  rve.start_coroutine(function()
    rve.wait_until(function() while true do end end)
    rve.log_info("wait_until escaped")
  end)
end
return Script
"#;
    let (mut world, mut host) = scripted_with_limit(source, Duration::from_millis(20));
    host.update_frame(
        &mut world,
        ScriptFrameTime::advance(0.1, 1.0, 0.1, 0.1, 1.0 / 60.0, 1),
    );
    host.update_frame(
        &mut world,
        ScriptFrameTime::advance(0.1, 1.0, 0.2, 0.2, 1.0 / 60.0, 2),
    );
    let emitted = messages(&mut host);
    assert!(
        emitted
            .iter()
            .any(|message| message.contains("Lua call exceeded time limit")),
        "{emitted:?}"
    );
    assert!(
        !emitted
            .iter()
            .any(|message| message == "wait_until escaped")
    );
}

#[test]
fn interrupt_stops_a_coroutine_loop_even_when_it_uses_pcall() {
    let world = world(
        r#"
[[assets]]
path = "/stage5.lua"
[[objects]]
id = "runner"
[[objects.lite_scripts]]
path = "/stage5.lua"
"#,
    );
    let interrupt = ScriptInterruptHandle::new();
    let mut host = ScriptHost::load_with_interrupt(
        &world,
        "scene",
        &sources(&[(
            "stage5.lua",
            r#"
local Script = {}
function Script:on_start()
  rve.start_coroutine(function()
    signal()
    pcall(function() while true do end end)
    rve.log_info("coroutine interrupt escaped")
  end)
end
return Script
"#,
        )]),
        None,
        interrupt.clone(),
    )
    .unwrap();
    let (signal_tx, signal_rx) = mpsc::channel();
    host.lua
        .globals()
        .set(
            "signal",
            host.lua
                .create_function(move |_, ()| {
                    signal_tx.send(()).unwrap();
                    Ok(())
                })
                .unwrap(),
        )
        .unwrap();
    let cancel = interrupt.clone();
    let worker = thread::spawn(move || {
        signal_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("coroutine entered the loop");
        cancel.interrupt();
    });

    let mut world = world;
    host.update_frame(
        &mut world,
        ScriptFrameTime::advance(0.1, 1.0, 0.1, 0.1, 1.0 / 60.0, 1),
    );
    host.update_frame(
        &mut world,
        ScriptFrameTime::advance(0.1, 1.0, 0.2, 0.2, 1.0 / 60.0, 2),
    );
    worker.join().unwrap();
    let emitted = messages(&mut host);
    assert!(
        emitted
            .iter()
            .any(|message| message == "scripts interrupted")
    );
    assert!(
        !emitted
            .iter()
            .any(|message| message == "coroutine interrupt escaped")
    );
}

#[test]
fn destroying_an_owner_cancels_its_timers_and_coroutines() {
    let source = r#"
local Script = {}
function Script:on_update()
  if rve.time.frame == 1 then
    rve.timer.after(0.1, function() rve.log_info("destroyed timer fired") end)
    rve.start_coroutine(function()
      rve.wait_frames(1)
      rve.log_info("destroyed coroutine resumed")
    end)
    rve.destroy(self.object)
  end
end
function Script:on_destroy() rve.log_info("owner destroyed") end
return Script
"#;
    let (mut world, mut host) = scripted(source);
    for frame_number in 1..=3 {
        host.update_frame(
            &mut world,
            ScriptFrameTime::advance(
                0.1,
                1.0,
                (frame_number - 1) as f64 * 0.1,
                (frame_number - 1) as f64 * 0.1,
                1.0 / 60.0,
                frame_number,
            ),
        );
    }
    let emitted = messages(&mut host);
    assert!(emitted.iter().any(|message| message == "owner destroyed"));
    assert!(
        !emitted
            .iter()
            .any(|message| message == "destroyed timer fired")
    );
    assert!(
        !emitted
            .iter()
            .any(|message| message == "destroyed coroutine resumed")
    );
}

#[test]
fn invalid_time_scale_does_not_commit() {
    let source = r#"
local Script = {}
function Script:on_update() rve.time.scale = -1 end
return Script
"#;
    let (mut world, mut host) = scripted(source);
    host.update_frame(
        &mut world,
        ScriptFrameTime::advance(0.1, 1.0, 0.1, 0.1, 1.0 / 60.0, 1),
    );
    assert_eq!(host.time_scale(), 1.0);
    let emitted = messages(&mut host);
    assert!(
        emitted
            .iter()
            .any(|message| message.contains("time scale must be finite and nonnegative")),
        "{emitted:?}"
    );
}

fn scripted_with_limit(source: &str, call_time_limit: Duration) -> (World, ScriptHost) {
    let world = world(
        r#"
[[assets]]
path = "/stage5.lua"
[[objects]]
id = "runner"
[[objects.lite_scripts]]
path = "/stage5.lua"
"#,
    );
    let host = ScriptHost::load_with_options(
        &world,
        "scene",
        &sources(&[("stage5.lua", source)]),
        None,
        ScriptLoadOptions {
            call_time_limit: Some(call_time_limit),
            ..ScriptLoadOptions::default()
        },
    )
    .unwrap();
    (world, host)
}
