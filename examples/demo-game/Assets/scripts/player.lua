local Player = {}

Player.properties = {
  speed = { type = "float", default = 4.0, min = 0.0, max = 20.0, label = "Walk speed (m/s)" },
}

function Player:on_start()
  local player = rve.find(self.object.id)
  local controller = player:get_controller()
  controller.use_gravity = true
  controller.gravity_scale = 1.0
  controller.jump_height = 1.0
  player:set_controller(controller)
  self.grounded = false
  self.coins = 0
  rve.audio.set_group_volume("ambient", 0.8)
end

function Player:on_update()
  -- WASD задаёт движение в осях мира: камера стоит за игроком и смотрит вдоль -Z.
  local dx, dz = 0.0, 0.0
  if rve.input.key_down("W") then dz = dz - 1.0 end
  if rve.input.key_down("S") then dz = dz + 1.0 end
  if rve.input.key_down("A") then dx = dx - 1.0 end
  if rve.input.key_down("D") then dx = dx + 1.0 end
  local length = math.sqrt(dx * dx + dz * dz)
  if length > 0.0 then
    dx = dx / length * self.params.speed
    dz = dz / length * self.params.speed
  end

  if self.grounded and rve.input.key_pressed("Space") then
    local player = rve.find(self.object.id)
    if player:jump() then
      rve.audio.play("/audio/jump.wav", { group = "effects" })
    end
  end

  local body = rve.find(self.object.id)
  local result = body:simple_move({ x = dx, y = 0.0, z = dz })
  self.grounded = result.grounded
end

function Player:on_message(sender, name, value)
  if name == "coin_collected" then
    self.coins = self.coins + value
    rve.log_info("Coins collected: " .. tostring(self.coins))
  end
end

return Player
