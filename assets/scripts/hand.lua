
local cam
function update(node, dt)
  if not cam then cam = findScript("third_person_camera") end
  local layer = node:getcomponent("UiLayer")
  if layer and cam then layer.enabled = cam.firstPerson and true or false end
end
