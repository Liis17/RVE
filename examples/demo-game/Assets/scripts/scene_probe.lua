-- F6 switches between the physics and prefab scenes. This persistent root also shows Lua ScriptRef.
local Probe = {}

Probe.properties = {
  spin_speed = { type = "float", default = 45, min = 0, max = 720, label = "Spin speed (deg/s)" },
}

function Probe:on_start()
  rve.log_info("scene probe: current scene " .. rve.scene.current())
end

function Probe:on_update()
  if not rve.input.action_pressed("SceneCycle") then
    return
  end
  local current = rve.scene.current()
  local target = current == "scenes/physics.rvescene"
    and "scenes/second.rvescene"
    or "scenes/physics.rvescene"
  rve.scene.load(target)
  rve.log_info("scene probe: loading " .. target .. " (F6 returns to physics)")
end

function Probe:on_message(sender, name, value)
  if name == "rust_script_ref" then
    rve.log_info("scene probe received Rust ScriptRef message from " .. sender .. ": " .. value)
  end
end

return Probe
