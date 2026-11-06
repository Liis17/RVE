local Player = {}

Player.properties = {
  speed = { type = "float", default = 4.0, min = 0.0, max = 20.0, label = "Walk speed (m/s)" },
  jump_speed = { type = "float", default = 5.0, min = 0.0, max = 30.0, label = "Jump speed (m/s)" },
  gravity = { type = "float", default = 9.81, min = 0.0, max = 60.0, label = "Gravity (m/s^2)" },
}

function Player:on_start()
  self.vertical_speed = 0.0
  self.grounded = false
  self.coins = 0
end

function Player:on_update(dt)
  -- WASD задаёт движение в осях мира: камера стоит за игроком и смотрит вдоль -Z.
  local dx, dz = 0.0, 0.0
  if rve.input.key_down("W") then dz = dz - 1.0 end
  if rve.input.key_down("S") then dz = dz + 1.0 end
  if rve.input.key_down("A") then dx = dx - 1.0 end
  if rve.input.key_down("D") then dx = dx + 1.0 end
  local length = math.sqrt(dx * dx + dz * dz)
  if length > 0.0 then
    dx = dx / length * self.params.speed * dt
    dz = dz / length * self.params.speed * dt
  end

  if self.grounded and rve.input.key_pressed("Space") then
    self.vertical_speed = self.params.jump_speed
  end
  self.vertical_speed = self.vertical_speed - self.params.gravity * dt

  local body = rve.find(self.object.id)
  local result = body:move({ x = dx, y = self.vertical_speed * dt, z = dz })
  self.grounded = result.grounded
  if self.grounded and self.vertical_speed < 0.0 then
    self.vertical_speed = 0.0
  end
end

function Player:on_message(sender, name, value)
  if name == "coin_collected" then
    self.coins = self.coins + value
    rve.log_info("Coins collected: " .. tostring(self.coins))
  end
end

return Player
