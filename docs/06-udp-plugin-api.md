# 6. UDP Plugin API (ACSP)

An external process (the "plugin") exchanges UDP datagrams with the server,
using **protocol version 4**. Every packet id and layout is `[K]`. The places
where AssettoServer behaves differently are tagged `[K*]`, and a faithful
replacement should do what vanilla does.

## 6.1 Setup

In `server_cfg.ini`:

- `UDP_PLUGIN_ADDRESS = <ip>:<port>` — where the server **sends** events (the plugin's
  listening port).
- `UDP_PLUGIN_LOCAL_PORT = <port>` — where the server **listens** for requests.
  It must differ from the address port. Datagrams are accepted only from the
  configured plugin address.

Datagrams use the game UDP format (`u8 packet_id` + payload, no length prefix,
at most 1500 bytes) and the types from doc 1 §1.2. At startup the server sends
`Version`, then `NewSession` for the current session.

## 6.2 Packet ID registry (`UdpPluginProtocol`)

Server → plugin (events / responses):

| ID | Name |
|---|---|
| `0x32` | NewSession |
| `0x33` | NewConnection (car connected) |
| `0x34` | ClosedConnection (car disconnected) |
| `0x35` | CarUpdate (realtime position) |
| `0x36` | CarInfo (response to GetCarInfo) |
| `0x37` | EndSession |
| `0x38` | Version |
| `0x39` | Chat (player chat relayed) |
| `0x3A` | ClientFirstUpdate (client loaded) |
| `0x3B` | SessionInfo (response to GetSessionInfo / SetSessionInfo) |
| `0x3C` | Error |
| `0x49` | LapCompleted `[K*]` — AssettoServer relays the game `0x49` leaderboard verbatim (doc 2 §2.2). Published ACSP references show a different per-entry layout; check it against the plugins you need |
| `0x82` | ClientEvent (collision) |

Plugin → server (requests):

| ID | Name |
|---|---|
| `0x38` | Version (query — server re-sends its Version) |
| `0xC8` | SetRealtimePosInterval |
| `0xC9` | GetCarInfo |
| `0xCA` | SendChat (to one car) |
| `0xCB` | BroadcastChat |
| `0xCC` | GetSessionInfo |
| `0xCD` | SetSessionInfo |
| `0xCE` | KickUser |
| `0xCF` | NextSession |
| `0xD0` | RestartSession |
| `0xD1` | AdminCommand `[K*]` — vanilla runs it as an admin chat command; AssettoServer ignores it |

## 6.3 Server → plugin packets

### 0x38 — Version

| Field | Type | Notes |
|---|---|---|
| ProtocolVersion | `u8` | 4 |

### 0x32 — NewSession / 0x3B — SessionInfo

Same layout; `0x32` is pushed on session change (and at startup), `0x3B` answers
`GetSessionInfo`/`SetSessionInfo`.

| Field | Type | Notes |
|---|---|---|
| ProtocolVersion | `u8` | 4 |
| SessionIndex | `u8` | index of the described session |
| CurrentSessionIndex | `u8` | |
| SessionCount | `u8` | |
| ServerName | `wstring8` | |
| Track | `string8` | |
| TrackConfig | `string8` | |
| Name | `string8` | session name |
| SessionType | `u8` | 0–3 |
| SessionTime | `u16` | minutes |
| SessionLaps | `u16` | |
| SessionWaitTime | `u16` | |
| AmbientTemperature | `u8` | °C |
| RoadTemperature | `u8` | °C |
| WeatherGraphics | `string8` | not a `wstring8` (measured) |
| ElapsedMs | `i32` | ms since session start (negative during wait time) |

### 0x37 — EndSession

| Field | Type | Notes |
|---|---|---|
| ReportJsonFilename | `wstring8` | path of the results file, relative to the server's working directory, e.g. `results/2026_9_23_14_22_PRACTICE.json` |

Vanilla sends `0x37` only when it wrote a results file, which it does only for
a session in which a lap was completed. It goes out at the session change,
just before the next `0x32`, whether the session ended on its clock or by
`NextSession`. `[K*]` AssettoServer sends an empty
filename.

### 0x33 — NewConnection / 0x34 — ClosedConnection

| Field | Type |
|---|---|
| DriverName | `wstring8` |
| DriverGuid | `wstring8` |
| SessionId | `u8` |
| CarModel | `string8` |
| CarSkin | `string8` |

`NewConnection` fires when a player connects; `ClosedConnection` when they leave.

### 0x3A — ClientFirstUpdate

| Field | Type | Notes |
|---|---|---|
| SessionId | `u8` | the player finished loading (first position update) |

Vanilla sends it partway through the first-update snapshot (doc 1 §1.5): after
the `0x70` BoP and before the welcome message, setup and DRS zones.

### 0x35 — CarUpdate

Sent for each loaded car once per realtime interval. Vanilla sends nothing until
the plugin sets an interval with `0xC8`, and stops again if it is set to 0
(measured). `[K*]` AssettoServer starts at a 1000 ms default.

| Field | Type |
|---|---|
| SessionId | `u8` |
| Position | `vec3` |
| Velocity | `vec3` |
| Gear | `u8` |
| EngineRpm | `u16` |
| NormalizedSplinePosition | `f32` |

### 0x36 — CarInfo

Response to `GetCarInfo`.

| Field | Type |
|---|---|
| CarId | `u8` |
| IsConnected | `bool` |
| Model | `wstring8` |
| Skin | `wstring8` |
| DriverName | `wstring8` |
| DriverTeam | `wstring8` |
| DriverGuid | `wstring8` |

### 0x39 — Chat

Every chat message, `/` commands included (sent before the command acts, `/admin`'s password and all; measured).

| Field | Type |
|---|---|
| SessionId | `u8` |
| Message | `wstring8` |

### 0x82 — ClientEvent

Collisions:

| Field | Type | Present when |
|---|---|---|
| EventType | `u8` | always — `0x0A` collision with car, `0x0B` with environment |
| SessionId | `u8` | always |
| TargetSessionId | `u8` | EventType == `0x0A` |
| Speed | `f32` | always (relative speed, km/h) |
| WorldPosition | `vec3` | always |
| RelPosition | `vec3` | always |

### 0x3C — Error

| Field | Type |
|---|---|
| Message | `wstring8` |

## 6.4 Plugin → server packets

### 0xC8 — SetRealtimePosInterval

| Field | Type | Notes |
|---|---|---|
| Interval | `u16` | ms between `CarUpdate` pushes |

### 0xC9 — GetCarInfo

| Field | Type |
|---|---|
| SessionId | `u8` |

### 0xCA — SendChat

A server chat message to one car:

| Field | Type | Notes |
|---|---|---|
| SessionId | `u8` | destination car |
| Message | `wstring8` | delivered as game chat from session ID 255 |

### 0xCB — BroadcastChat

| Field | Type |
|---|---|
| Message | `wstring8` |

### 0xCC — GetSessionInfo

| Field | Type | Notes |
|---|---|---|
| SessionIndex | `i16` | `-1` = current session |

Response: `0x3B SessionInfo`, or `0x3C Error`
`ERROR: ACSP_GET_SESSION_INFO out of bounds: <index>` (text read from the
binary, not measured).

### 0xCD — SetSessionInfo

`[K*]` Overwrites a configured session **without** switching to it. If the
index is the current session, its new time and laps apply from then on.

| Field | Type |
|---|---|
| SessionIndex | `u8` |
| SessionName | `wstring8` |
| SessionType | `u8` |
| Laps | `i32` |
| Time | `i32` (ms) |
| WaitTime | `u32` (ms) |

Vanilla accepts `SessionIndex < sessionCount`, index 0 included; AssettoServer
accepts only `0 < SessionIndex < sessionCount` and switches to the session.
Response: `0x3B SessionInfo`, or `0x3C Error`
`ERROR: ACSP_SET_SESSION_INFO out of bounds: <index>`.

### 0xCE — KickUser

| Field | Type |
|---|---|
| SessionId | `u8` |

Kicks exactly as `/kick` does (doc 2): the `<name> kicked from the server` chat,
then `0x68` with `BLACKLIST_MODE` as the reason, with the mode's blacklisting.
Out-of-range → `0x3C Error` with message `ERROR: ACSP_KICK_USER index <id> out of bound`.

### 0xCF — NextSession / 0xD0 — RestartSession

No payload.

### 0xD1 — AdminCommand

| Field | Type | Notes |
|---|---|---|
| Command | `wstring8` | an admin chat command, e.g. `/ballast 2 30` |

Vanilla runs it with admin rights (whether it needs the leading `/` has not
been measured). `[K*]` AssettoServer ignores it.
