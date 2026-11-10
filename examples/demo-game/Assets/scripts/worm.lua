local Worm = {}

-- Демо скелетной анимации: один разовый изгиб, затем переход в цикл Wiggle.
-- Окончание разового клипа приходит скрипту сообщением animation_finished.
function Worm:on_start()
  rve.find(self.object.id):play_animation("worm_curl", { fade = 0.2, loop = false, ["then"] = "Wiggle" })
  rve.log_info("Worm: curl, then Wiggle")
end

function Worm:on_message(sender, name, value)
  if name == "animation_finished" then
    rve.log_info("Worm: " .. value .. " finished")
  end
end

return Worm
