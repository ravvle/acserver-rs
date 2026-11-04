-- AI traffic debug overlay for acserver-rs (EnableAi with AiParams.Debug).
-- The server sends, for every AI car near this player, its current, target
-- and maximum speed (km/h) and its gap to the AI car ahead (m, -1 for none);
-- this draws them above the car.

local ROWS = 20
local stats = {}

ac.OnlineEvent({
    ac.StructItem.key("ai_debug"),
    SessionIds = ac.StructItem.array(ac.StructItem.byte(), ROWS),
    CurrentSpeeds = ac.StructItem.array(ac.StructItem.byte(), ROWS),
    TargetSpeeds = ac.StructItem.array(ac.StructItem.byte(), ROWS),
    MaxSpeeds = ac.StructItem.array(ac.StructItem.byte(), ROWS),
    ClosestAiObstacles = ac.StructItem.array(ac.StructItem.int16(), ROWS),
}, function (sender, message)
    if sender ~= nil then return end -- only the server sends these
    for i = 0, ROWS - 1 do
        local id = message.SessionIds[i]
        if id ~= 255 then
            stats[id] = string.format("%d / %d / %d km/h, gap %d m",
                message.CurrentSpeeds[i], message.TargetSpeeds[i],
                message.MaxSpeeds[i], message.ClosestAiObstacles[i])
        end
    end
end)

function script.draw3D()
    local camera = ac.getCameraPosition()
    for i = 0, sim.carsCount - 1 do
        local car = ac.getCar(i)
        local text = car and stats[car.sessionID]
        if text and car.position:closerToThan(camera, 200) then
            render.debugText(car.position + vec3(0, 2, 0), text)
        end
    end
end
