-- acserver-rs in-game configuration window (EnableLiveConfiguration).
-- The configuration parts are AssettoServer's assettoserver.lua:
-- Copyright (C) 2023 Niewiarowski, compujuckel. AGPL-3.0-or-later.

local baseUrl = "http://" .. ac.getServerIP() .. ":" .. ac.getServerPortHTTP()
local configUrl = baseUrl .. "/api/configuration"
local configurationLoading = false
local configuration
local authHeaders = {}

local function getConfiguration()
    web.get(configUrl, authHeaders, function (err, response)
        if response.status == 200 then
            ac.log("config loaded")
            configuration = stringify.parse(response.body)
            configurationLoading = false
        end
    end)
end

local function setValue(key, value)
    web.post(configUrl .. "?key=" .. key .."&value=" .. tostring(value), authHeaders, function (err, response)
        ac.debug("err", err)
        ac.debug("response", stringify(response))

        if response.status ~= 200 then
            ui.toast(ui.Icons.Ban, "Error updating " .. key .. " (" .. response.status .. ")")
        else
            local parsed = stringify.parse(response.body)
            if parsed.Status ~= "OK" then
                ui.toast(ui.Icons.Ban, "Error updating " .. key .. " (" .. parsed.ErrorMessage .. ")")
            else
                ui.toast(ui.Icons.Confirm, key .. " set to " .. tostring(value))
            end
        end
    end)
end

local apiKeyEvent = ac.OnlineEvent({
    ac.StructItem.key("AS_ApiKey"),
    key = ac.StructItem.string(32)
}, function (sender, message)
    if sender ~= nil then return end
    ac.debug("key", message.key)
    authHeaders["X-Car-Id"] = car.sessionID
    authHeaders["X-Api-Key"] = message.key
end)

apiKeyEvent({ key = "" })

local function ui_configObject(name, obj)
    if obj == nil then return end

    for i, value in ipairs(obj.Properties) do
        if value.Type == "object" then
            ui.treeNode(value.Name, nil, function () ui_configObject(name .. "." .. value.Name, value.Value) end)
        elseif value.Type == "list" then
            ui.treeNode(value.Name, nil, function ()
                for j, listItem in ipairs(value.Value) do
                    if value.EntryType == "object" then
                        ui.treeNode(j - 1, nil, function () ui_configObject(name .. "." .. value.Name .. "." .. j - 1, listItem) end)
                    else
                        ui.text(listItem)
                    end
                end
            end)
        elseif value.Type == "dict" then
            ui.treeNode(value.Name, nil, function ()
                for key, listItem in pairs(value.Value) do
                    if value.EntryType == "object" then
                        ui.treeNode(key, nil, function () ui_configObject(name .. "." .. value.Name .. "." .. key, listItem) end)
                    else
                        ui.treeNode(key, nil, function () ui.text(listItem) end)
                    end
                end
            end)
        else
            ui.beginGroup()
            if value.ReadOnly then ui.pushStyleColor(ui.StyleColor.Text, rgbm.new("#999")) end
            ui.textAligned(value.Name, nil, vec2(150, 0))
            if value.ReadOnly then ui.popStyleColor() end
            ui.endGroup()
            if value.Description ~= nil and ui.itemHovered() then
                ui.setTooltip(value.Description)
            end
            ui.sameLine()

            local id = name .. "." .. value.Name

            if value.Type == "System.Boolean" then
                if ui.checkbox("###" .. id, value.Value) and not value.ReadOnly then
                    value.Value = not value.Value
                end
            elseif value.Type == "enum" then
                ui.setNextItemWidth(ui.availableSpaceX() - 42)
                if ui.beginCombo("###" .. id, value.Value) then
                    for j, enumValue in ipairs(value.ValidValues) do
                        if ui.selectable(enumValue, enumValue == value.Value) then
                            value.Value = enumValue
                        end
                    end
                    ui.endCombo()
                end
            else
                local flags = ui.InputTextFlags.None
                if value.ReadOnly then
                    flags = ui.InputTextFlags.ReadOnly
                end
                ui.setNextItemWidth(ui.availableSpaceX() - 42)
                value.Value = ui.inputText("###" .. id, value.Value, flags)
            end
            ui.sameLine()

            if ui.button("###btn.".. id, vec2(30, 0), value.ReadOnly and ui.ButtonFlags.Disabled or 0) then
                ac.debug("lastId", id)
                ac.debug("lastValue", value.Value)
                setValue(id, value.Value)
            end
            ui.addIcon(ui.Icons.Save, vec2(16,16))
        end
    end
end

local function tab_Configuration()
    ui.textWrapped("This feature is experimental! Changed values will not persist after a server restart.")
    ui.childWindow("configuration", ui.availableSpace(), function ()
        ui_configObject("Root", configuration)
    end)
end

local function tab_About()
    ui.textWrapped("This server runs acserver-rs, a free Assetto Corsa server released under the GNU Affero General Public License, version 3 or later. Parts of it, including this window, come from AssettoServer by Niewiarowski and compujuckel, also AGPL.")
    ui.text("")
    ui.textWrapped("Admins can change the server's live settings in the Configuration tab. Changes last until the server restarts.")
end

local function window_Server()
    ui.tabBar("main_tabBar", function ()
        ui.tabItem("About", tab_About)
        if sim.isAdmin then
            if configuration == nil and not configurationLoading then
                configurationLoading = true
                getConfiguration()
            end
            ui.tabItem("Configuration", tab_Configuration)
        end
    end)
end

ui.registerOnlineExtra(ui.Icons.Settings, "Server", function () return true end, window_Server, nil, ui.OnlineExtraFlags.Tool)
