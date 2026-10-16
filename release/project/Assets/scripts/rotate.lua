local Rotate = {}

function Rotate:on_update(dt)
  local rotation = self.transform.rotation
  self.transform.rotation = {
    x = rotation.x,
    y = (rotation.y + self.params.speed * dt) % 360.0,
    z = rotation.z
  }
end

return Rotate
