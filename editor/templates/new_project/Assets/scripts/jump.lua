local Jump = {}

Jump.properties = {
  height = { type = "float", default = 2.0, min = 0.0, max = 10.0, label = "Jump Height" },
  duration = { type = "float", default = 0.8, min = 0.1, max = 5.0, label = "Jump Duration" },
}

function Jump:on_start()
  self.base_y = self.transform.position.y
  self.elapsed = nil
end

function Jump:on_update(dt)
  if self.elapsed == nil and rve.input.key_pressed("Space") then
    self.base_y = self.transform.position.y
    self.elapsed = 0.0
  end

  if self.elapsed ~= nil then
    self.elapsed = self.elapsed + dt
    local progress = math.min(self.elapsed / self.params.duration, 1.0)
    local eased = progress * progress * (3.0 - 2.0 * progress)
    self.transform.position.y = self.base_y + self.params.height * 4.0 * eased * (1.0 - eased)
    if progress >= 1.0 then
      self.transform.position.y = self.base_y
      self.elapsed = nil
    end
  end
end

return Jump
