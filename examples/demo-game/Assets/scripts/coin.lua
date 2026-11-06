local Coin = {}

function Coin:on_trigger_enter(other)
  if other.id ~= "player" then
    return
  end

  -- Подобранная монета перестаёт быть триггером и скрывается масштабом 0.
  rve.find(self.object.id):set_collider_enabled(false)
  self.transform.scale = { x = 0.0, y = 0.0, z = 0.0 }
  rve.log_info(self.object.name .. " collected")
  rve.broadcast("coin_collected", 1)
end

return Coin
