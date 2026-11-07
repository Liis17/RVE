local Bullet = {}

-- Пуля исчезает через 3 секунды, даже если никуда не попала.
local LIFETIME = 3.0
-- След лежит на поверхности: его полутолщина равна масштабу по Z (0.004 м).
local HOLE_OFFSET = 0.004
local HOLE = "/prefabs/bullet_hole.rveprefab"

function Bullet:on_start()
  self.hit = false
  rve.destroy(self.object.id, LIFETIME)
end

function Bullet:on_collision_enter(other, contact)
  -- Первое столкновение уже запланировало удаление; повторные события этой пули пропускаем.
  if self.hit then
    return
  end
  self.hit = true

  -- contact.normal направлен от пули к цели, поэтому внешняя нормаль поверхности имеет обратный знак.
  local n = contact.normal
  local length = math.sqrt(n.x * n.x + n.y * n.y + n.z * n.z)
  if length > 1e-6 then
    local nx, ny, nz = -n.x / length, -n.y / length, -n.z / length
    local p = contact.point
    -- Углы в градусах с порядком YXZ: локальная ось +Z совпадает с нормалью, если
    -- X = -asin(ny) и Y = atan(nx, nz). Ось Z у следа — тонкая, поэтому след ложится плашмя.
    rve.spawn(HOLE, {
      position = {
        x = p.x + nx * HOLE_OFFSET,
        y = p.y + ny * HOLE_OFFSET,
        z = p.z + nz * HOLE_OFFSET,
      },
      rotation = {
        x = math.deg(math.asin(math.max(-1.0, math.min(1.0, -ny)))),
        y = math.deg(math.atan(nx, nz)),
        z = 0.0,
      },
    })
  end
  rve.destroy(self.object.id)
end

return Bullet
