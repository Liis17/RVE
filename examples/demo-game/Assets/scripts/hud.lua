local Hud = {}

function Hud:on_start()
  self.canvas = rve.ui.canvas(self.object)
  self.status = self.canvas and self.canvas:find("status") or nil
  self.progress = self.canvas and self.canvas:find("progress") or nil
  self.last_value = 0.35
end

function Hud:on_ui_event(event)
  if event.kind == "click" and event.id == "fire" then
    rve.log_info("HUD: fire button clicked")
  elseif event.kind == "value_changed" and event.id == "charge" then
    self.last_value = event.value
    if self.progress then
      self.progress:set_value(event.value)
    end
    if self.status then
      self.status:set_text(string.format("Charge: %d%%", math.floor(event.value * 100 + 0.5)))
    end
  elseif event.kind == "value_changed" and event.id == "name" and self.status then
    self.status:set_text("Player: " .. tostring(event.value))
  elseif event.kind == "submit" and event.id == "name" then
    rve.log_info("HUD name submitted: " .. tostring(event.value))
  end
end

return Hud
