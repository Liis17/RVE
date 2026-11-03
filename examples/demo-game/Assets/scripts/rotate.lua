local Rotate = {}

Rotate.properties = {
  speed = { type = "float", default = 30, min = -720, max = 720, label = "Speed (deg/s)" },
}

function Rotate:on_start()
  rve.log_info("Rotate started; broadcasting demo_ready")
  rve.broadcast("demo_ready", "Lua start callback")
end

function Rotate:on_update(dt)
  if rve.input.key_pressed("Space") then
    rve.log_info("Space pressed")
  end
  local rotation = self.transform.rotation
  self.transform.rotation = {
    x = rotation.x,
    y = rotation.y + self.params.speed * dt,
    z = rotation.z
  }
end

return Rotate
