-- VESSEL SPAWNER — when the scene loads with `shipyard.launch` set, rebuild
-- the saved blueprint as a LIVE compound assembly beside the pad: one Vessel
-- root (RigidBody assembly) + every blueprint part spawned as its child, then
-- `assembly.rebuild` gathers them into the physics compound. The blueprint is
-- self-contained (id, prefab, pose, mass, thrust…) — no builder registry here.
--
-- v1: the vessel arrives as an honest physical object (walk around it, watch
-- a bad design tip over). The blueprint-driven flight controller comes next.

defaults = {
  offset_x = 14.0,   -- pad-relative spawn offset
  drop = 0.4,        -- spawn slightly high; the compound settles onto the pad
}

local pending = 0
local vessel_node = nil

local function spawn_parts(vessel, bp)
  local vx, vy, vz = vessel.x, vessel.y, vessel.z
  local total = 0
  for _, d in pairs(bp.parts) do
    total = total + 1
    pending = pending + 1
    spawn(d.prefab, vec3(vx + d.x, vy + d.y, vz + d.z), function(part)
      part.yaw = d.yaw or 0
      pending = pending - 1
      if pending == 0 and vessel_node then
        assembly.rebuild(vessel_node)
        log("vessel assembled: " .. total .. " parts on the pad")
      end
    end, vessel)
  end
end

function start(node)
  if save.get("shipyard.launch") ~= 1 then return end
  save.set("shipyard.launch", 0)
  local bp = save.get("shipyard.blueprint")
  if not bp or not bp.parts then return end

  local pad = find("Ship")
  local px, py, pz = 0, 0, 0
  if pad then px, py, pz = pad.x, pad.y, pad.z end
  spawn("Vessel", vec3(px + params.offset_x, py + params.drop, pz), function(v)
    vessel_node = v
    spawn_parts(v, bp)
  end)
end
