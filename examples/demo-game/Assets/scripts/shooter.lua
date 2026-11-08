local Shooter = {}

local BULLET = "/prefabs/bullet.rveprefab"
local TOWER = "/prefabs/tower.rveprefab"
-- При 60 Гц пуля за шаг проходит 0.5 м; окно стены 0.5 м плюс диаметр пули — 0.6 м, туннеля нет.
local BULLET_SPEED = 30.0
local MUZZLE_DISTANCE = 1.0
local TOWER_POSITION = { x = 2.5, y = 0.0, z = -4.0 }

function Shooter:on_start()
  self.tower_wanted = false
  self.tower = nil
end

function Shooter:on_update(dt)
  if rve.input.action_pressed("fire") then
    self:fire(rve.find(self.object.id))
  end
  if rve.input.action_pressed("spawn_heavy") then
    self.tower_wanted = not self.tower_wanted
    if not self.tower_wanted then
      self:remove_tower()
    end
  end
  if self.tower_wanted then
    self:sync_tower()
  end
end

function Shooter:fire(camera)
  local matrix = camera:world_matrix()
  if matrix == nil then
    return
  end
  -- world_matrix приходит по столбцам: третий столбец — локальная ось Z камеры, а камера смотрит по -Z.
  local fx, fy, fz = -matrix[9], -matrix[10], -matrix[11]
  rve.spawn(BULLET, {
    position = {
      x = matrix[13] + fx * MUZZLE_DISTANCE,
      y = matrix[14] + fy * MUZZLE_DISTANCE,
      z = matrix[15] + fz * MUZZLE_DISTANCE,
    },
    velocity = { x = fx * BULLET_SPEED, y = fy * BULLET_SPEED, z = fz * BULLET_SPEED },
  })
end

-- Башня появляется, когда префаб загружен. Загрузка идёт в фоне, поэтому спавн ждёт состояния loaded.
function Shooter:sync_tower()
  if self.tower ~= nil then
    return
  end
  local state = rve.prefabs.state(TOWER)
  if state == "unloaded" then
    rve.prefabs.load(TOWER)
  elseif state == "loaded" then
    self.tower = rve.spawn(TOWER, { position = TOWER_POSITION })
  elseif state == "failed" then
    self.tower_wanted = false
    rve.log_warn("tower prefab failed to load")
  end
end

-- Второе нажатие удаляет башню и выгружает префаб: после удаления экземпляра выгрузка освобождает память.
function Shooter:remove_tower()
  if self.tower ~= nil then
    if self.tower:is_alive() then
      rve.destroy(self.tower)
    end
    self.tower = nil
  end
  if rve.prefabs.state(TOWER) ~= "unloaded" then
    rve.prefabs.unload(TOWER)
  end
end

return Shooter
