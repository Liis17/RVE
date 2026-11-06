local Rotate = {}

Rotate.properties = {
  speed = { type = "float", default = 30, min = -720, max = 720, label = "Speed (deg/s)" },
}

function Rotate:on_start()
  rve.log_info("Rotate started; broadcasting demo_ready")
  rve.broadcast("demo_ready", "Lua start callback")
end

function Rotate:on_update(dt)
  local context = rve.input.context()
  if rve.input.action_pressed("Escape") then
    if context == "game" then
      rve.input.set_context("menu")
      rve.log_info("Input context: menu")
    else
      rve.input.set_context("game")
      rve.log_info("Input context: game")
    end
    return
  end
  if context == "game" then
    if rve.input.action_pressed("Jump") then
      rve.log_info("Jump action")
    end
    local move_x, move_y = rve.input.vector("Move")
    local position = self.transform.position
    self.transform.position = {
      x = position.x + move_x * dt,
      y = position.y + move_y * dt,
      z = position.z
    }
  end
  local rotation = self.transform.rotation
  self.transform.rotation = {
    x = rotation.x,
    y = rotation.y + self.params.speed * dt,
    z = rotation.z
  }
end

return Rotate
