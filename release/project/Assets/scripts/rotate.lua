local Rotate = {}

function Rotate:on_start()
  rve.log("Rotate started")
end

function Rotate:on_update(dt)
  local rotation = self.transform.rotation
  self.transform.rotation = {
    x = rotation.x,
    y = rotation.y + (self.params.speed or 30.0) * dt,
    z = rotation.z
  }
end

return Rotate
