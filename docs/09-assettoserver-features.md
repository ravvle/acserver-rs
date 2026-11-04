# 9. AssettoServer Features

What AssettoServer's own features send and accept: packets, CSP messages, Lua
events, extra options and HTTP endpoints. All of it is `[AS]` (riding on
`[CSP]` where noted), so a Kunos drop-in can skip it. Each feature is off
unless switched on in AssettoServer's `extra_cfg.yml`; the key names are given
so a replacement can read the same file. Behaviour follows AssettoServer
0.0.54.

## 9.1 Overview

| Feature | `extra_cfg.yml` | Wire surfaces | Section |
|---|---|---|---|
| Custom updates | `EnableCustomUpdate` | `0xAB 0x03` UDP, `CUSTOM_UPDATE_FORMAT` extra option | doc 4 §4.2 |
| Network bubble | `NetworkBubbleDistance`, `OutsideNetworkBubbleRefreshRateHz` | `0x46` rate | doc 3 `0x46` |
| WeatherFX | `EnableWeatherFx` | `0xAB 0x01` replaces `0x78` and `0x54`; `WEATHERFX_V1` required | §9.4 |
| Server scripts | `CSPServerScripts` | `[SCRIPT_*]` extra options, `GET /api/scripts/<n>` | §9.2 |
| `AS_ApiKey` | any server script | Lua event | §9.3 |
| AI traffic | `EnableAi`, `AiParams` | `0x5A`/`0x4D` per AI slot, `0x48` or `0xAB 0x03`, `0xAB 0x02`, CarColorChange | §9.5 |
| Automatic kicks | `EnableAntiAfk`, `MaxPing`, `PlayerChecksumTimeoutSeconds` | chat, `0xAB 0x05`, `0x68` | §9.6 |
| User groups, admin commands | `UserGroups`, `AdminUserGroup`, … | `0x6F`, `0x3B`, chat replies | §9.7 |
| Live configuration | — | `/api/configuration`, in-game window | §9.8, doc 5 §5.7 |
| Plugins | `EnablePlugins` | per plugin | §9.9 |
| Server details, content downloads | `EnableServerDetails` | `/api/details`, `/content/*`, `ℹ<port>` | doc 5 §5.3–5.4 |
| RCON | `EnableRcon`, `RconPort` | TCP console | doc 7 |

## 9.2 Extra options and server scripts

A CSP client's `0x51` welcome message carries one INI document (doc 4 §4.5),
assembled from the configured extra options and what the features need:

- `[EXTRA_DATA] CUSTOM_UPDATE_FORMAT = '…'` with custom updates (doc 4 §4.2).
- One section per server script, numbered from 10:
  `[SCRIPT_<10+n>-<name>] SCRIPT = '<url>'`. A local script is served at
  `/api/scripts/<n>` and referenced as
  `http://{ServerIP}:{ServerHTTPPort}/api/scripts/<n>`, which CSP fills in. A
  configured `http(s)://` URL is passed on as written.
- Plugin sections, such as ReportPlugin's `[REPLAY_CLIPS]` (§9.9).
- AssettoServer also injects `[EXTRA_TWEAKS] VERIFY_STEAM_API_INTEGRITY = 1`
  (Steam auth off) and `MIN_TIME_BETWEEN_COLLISIONS = 2`.

A CSP build older than 1937 crashes on a welcome longer than 2039 characters,
so such a client must get the plain welcome.

Plugins add their own scripts to the list: AutoModerationPlugin's
`automoderation.lua`, and the in-game configuration window (§9.8).

## 9.3 Lua events

Lua online events (doc 4 §4.4) that the server sends or answers. Server-sent
ones go over TCP as `0xAB 0x03`, sender `0xFF`, type `60000` (LuaMessage),
then the `u32` key and the fields. Keys are computed with xxHash3 0.7.0
(doc 4 §4.4).

| Event | Key | Fields, in wire order | Direction |
|---|---|---|---|
| `AS_ApiKey` | `0x27612FAB` | `char key[32]` | C→S request, answered to the sender only |
| `autoModerationFlag` | `0x79096D99` | `uint8 flags`: `1` no lights, `2` blocking the road, `4` wrong way | S→C |

- **`AS_ApiKey`.** A client message of type 60000 or 60001 with this key is
  answered, not relayed. The reply carries the connection's API key, 32 hex
  characters, random per connection, which authenticates
  `/api/configuration` (doc 5 §5.7).
- **`autoModerationFlag`.** Sent to a player whenever their flags change, with
  the whole current set; `0` clears the warning.

The other built-in events (`AS_CollisionUpdate`, `AS_TeleportCar`,
`AS_LuaReady`, `AS_RequestResetCar`) are in doc 4 §4.4.

## 9.4 WeatherFX

The server owns the weather. Every second it sends `0xAB 0x01` (doc 4 §4.2)
to every ready client, and never sends `0x78` or `0x54`; a joining client gets
one in place of the snapshot's `0x78`. `WEATHERFX_V1` is a mandatory CSP
feature: it is listed in `/api/details` `features`, a join without it gets
`0x6F` (doc 4 §4.6), and the CSP handshake (doc 4 §4.3) carries
`RequiresWeatherFx = 1`.

- **Weather type.** `[WEATHER_n] GRAPHICS` written as `<graphics>_type=<id>`;
  without a type, Clear. `/api/details` `currentWeatherId` is the type's name.
- **Transition.** `UpcomingWeatherType` and `TransitionValue` (0–65535) blend
  the old type into the new over the transition time.
- **Clock.** `UnixTimestamp` starts at `SUN_ANGLE`'s time of day and runs at
  `TIME_OF_DAY_MULT`; with `EnableRealTime` it is the wall clock in the
  track's time zone. `LockServerDate` keeps today's date.
- **Rain and grip.** Rain intensity, wetness and water come from the type.
  `TrackGrip` is the dynamic track grip less
  `RainTrackGripReductionPercent` × (0.3 × wetness + 0.7 × water).

Type ids (`DefaultWeatherTypeProvider`):

| Id | Type | Id | Type | Id | Type |
|---|---|---|---|---|---|
| 0 | LightThunderstorm | 11 | HeavySnow | 22 | Smoke |
| 1 | Thunderstorm | 12 | LightSleet | 23 | Haze |
| 2 | HeavyThunderstorm | 13 | Sleet | 24 | Sand |
| 3 | LightDrizzle | 14 | HeavySleet | 25 | Dust |
| 4 | Drizzle | 15 | Clear | 26 | Squalls |
| 5 | HeavyDrizzle | 16 | FewClouds | 27 | Tornado |
| 6 | LightRain | 17 | ScatteredClouds | 28 | Hurricane |
| 7 | Rain | 18 | BrokenClouds | 29 | Cold |
| 8 | HeavyRain | 19 | OvercastClouds | 30 | Hot |
| 9 | LightSnow | 20 | Fog | 31 | Windy |
| 10 | Snow | 21 | Mist | 32 | Hail |

## 9.5 AI traffic

An entry-list slot with `AI=fixed` is always traffic and never open to
players. One with `AI=auto` is traffic while no player holds it; a join takes
it from the traffic at once. Vanilla keeps both kinds closed.

- **Announce.** A slot's first spawned AI car is announced with `0x5A`
  (doc 2): its session id, the name `<NamePrefix> <id>` and an empty nation.
  When the slot's last car despawns, or a player takes the slot, it gets a
  `0x4D`, so a player taking an `auto` slot produces `0x4D` then the player's
  `0x5A`.
- **Positions.** AI cars go to a custom-update client as `0xAB 0x03`, and to
  any other as `0x48` (doc 3) batches of up to 20, with ping 0 and a
  receiver-clock timestamp. With overbooking one slot drives several cars far
  apart, and each player gets the one nearest them under the slot's id.
- **Visibility.** With `AiParams.HideAiCars`, CSP clients get `0xAB 0x02`
  (doc 4 §4.2) marking each AI slot invisible, and visible once a player takes
  it.
- **Paint.** An AI slot whose skin allows colour changes gets a random paint
  per car: a CarColorChange client message (TCP `0xAB 0x03`, type `15000`,
  doc 4 §4.3) from the slot's id with a `u8` RGBA payload, sent whenever the
  car a client sees for that slot changes.
- **Collisions.** A player's `0x82` car collision (doc 2) with an AI slot
  stops that AI car briefly, except inside `IgnorePlayerObstacleSpheres`.
- **Road.** The splines are the track's `ai/fast_lane.aip`, or `fast_lane*.ai`
  files with an optional `config.yml` of junctions.

## 9.6 Automatic kicks

These follow AssettoServer's `KickAsync`, not vanilla's kick sequence
(doc 2 §2.1): a chat line to everyone, the CSP kick message (`0xAB 0x05`,
doc 4 §4.2) to a CSP target, then `0x68`. There is no `0x4D`.

| Trigger | Chat to everyone | Kick message | `0x68` reason |
|---|---|---|---|
| AFK | `<name> has been kicked from the server for being AFK.` | `You have been kicked for being AFK` | 4 |
| High ping | `… for high ping (<ms>ms).` | `You have been kicked for high ping (<ms>ms)` | 4 |
| AutoModeration (§9.9) | `… for driving without lights.`, `… the wrong way.`, `… blocking the road.` | `You have been kicked for <reason>` | 4 |
| Checksum timeout | `<name> did not send the requested checksums.` | none | 3 |
| `/ban <player> [reason]` | `<name> has been banned from the server[ for <reason>].` | `You have been banned[ for <reason>]` | 2 |

A minute before an AFK kick the player gets a private
`You will be kicked in 1 minute for being AFK.` Admins are exempt from the AFK
and AutoModeration kicks.

## 9.7 User groups and admin commands

User groups are lists of Steam ids, one per line, re-read when the file
changes:

- `AdminUserGroup` members join as admin.
- Once `WhitelistUserGroup` lists anyone, everyone else's join gets `0x6F`
  `You are not whitelisted on this server` (doc 1 §1.5 step 8).
- `BlacklistUserGroup` members get `0x3B`, as for `blacklist.txt`; a member
  still connected when the file changes is kicked.

Without Steam ticket validation a joining Steam id is unverified, so anyone
who knows an admin's id joins as admin. AssettoServer refuses to start with an
admin group unless `IgnoreConfigurationErrors.UnsafeAdminWhitelist` is set.

AssettoServer's chat commands (doc 8 §8.3) reply as server chat (`0x47` from
`0xFF`) to the caller, and as text over RCON (doc 7).

## 9.8 Live configuration

The configuration tree is readable and partly writable at runtime through
`/api/configuration` (doc 5 §5.7). CSP's in-game "Server" window is a server
script that uses this endpoint with the `AS_ApiKey` key. A change applies at
once and is not saved.

## 9.9 Plugins

AssettoServer's bundled plugins, each enabled by name in `EnablePlugins` and
configured by a tagged YAML document after the main one
(`--- !WordFilterConfiguration`):

| Plugin | On the wire |
|---|---|
| `AutoModerationPlugin` | `autoModerationFlag` (§9.3) and `automoderation.lua`, which loads `GET /automoderation/{no_lights,no_parking,wrong_way}.png`; chat warnings at half each rule's duration, then kicks (§9.6) |
| `GeoIPPlugin` | nothing; it only logs |
| `LiveWeatherPlugin` | WeatherFX transitions (§9.4), from `GET https://api.openweathermap.org/data/2.5/weather` |
| `RaceChallengePlugin` | reads `0x46` `StatusFlags`: three light flashes challenge the car ahead, hazards accept; results in chat |
| `RandomWeatherPlugin`, `VotingWeatherPlugin` | WeatherFX transitions; the vote is in chat (`/w <n>`) |
| `ReportPlugin` | `[REPLAY_CLIPS]` extra option and `POST /report` (below) |
| `TimeDilationPlugin` | WeatherFX clock rate by sun altitude |
| `WordFilterPlugin` | a matching name gets `0x6F` `Prohibited username. Change your Online Name in Settings > Content Manager > Drive > Online Name.`; a matching chat line is dropped, or bans (§9.6, reason `prohibited language`) |

### ReportPlugin upload

The extra options carry:

```
[REPLAY_CLIPS]
UPLOAD_URL = 'http://<host>/report?key=<key>'
DURATION = <ClipDurationSeconds>
```

`<key>` is random per run. `<host>` is the server's public address and HTTP
port; CSP does not fill `{ServerIP}` into this URL. On Ctrl+Shift+S, CSP
posts the clip (a zip) to that URL with header `X-Car-Index: <session id>`.
`/report <reason>` then files the player's last clip with a reason.
