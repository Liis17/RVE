local Rotate = {}

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
    y = rotation.y + (self.params.speed or 30.0) * dt,
    z = rotation.z
  }
end

return Rotate
