-- Демонстрация API смены сцен и ссылок на экземпляры: свойство читается и пишется через
-- obj:script, а корень с keep_on_load сохраняет состояние экземпляра при загрузке другой сцены.
local Probe = {}

Probe.properties = {
  spin_speed = { type = "float", default = 45, min = 0, max = 720, label = "Spin speed (deg/s)" },
}

function Probe:on_start()
  self.loads_seen = 0
  rve.log_info("scene probe: current scene " .. rve.scene.current())
end

function Probe:on_update(dt)
  self.loads_seen = self.loads_seen + 1
  if self.loads_seen % 600 ~= 0 then
    return
  end
  local self_ref = rve.find(self.object.id):script("/scripts/scene_probe.lua")
  local speed = self_ref:get("spin_speed")
  local next_speed = speed < 360 and speed * 1.5 or 45
  self_ref:set("spin_speed", next_speed)
  rve.log_info(string.format("scene probe: spin_speed %.1f -> %.1f", speed, next_speed))
end

return Probe
