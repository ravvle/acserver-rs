# acserver-rs

**A dedicated multiplayer server for Assetto Corsa, written in Rust.**

Assetto Corsa ships with its own dedicated server, Kunos's `acServer`. This
project is a replacement for it: you run `acserver-rs` instead, with the same
config files, and players join from the game or Content Manager as they always
have. Nothing changes on the player's side.

What you get over the original:

- **The same behaviour.** Sessions, races, votes, admin commands, the lobby
  listing and the plugin API all work as they do in `acServer` v1.15. That has
  been checked packet by packet against the original binary.
- **Holes closed.** The original lets one client spoof chat, crash others' cars
  with corrupt positions, take every slot, or flood the grid. This server stops
  all of that, in ways an honest player can't notice.
- **Lower CPU use.** About half the original's on a 31- or 64-car grid.
- **AssettoServer's features**, all off unless you switch them on: AI traffic,
  CSP WeatherFX and server scripts, user groups and extra admin commands,
  automatic kicks, live configuration and AssettoServer's bundled plugins, plus
  Content Manager content downloads and an RCON console. Content Manager's
  server details are on, as in AssettoServer.
  Custom Shaders Patch (CSP) messaging and AssettoServer's position handling
  are on by default.
- **Runs on almost anything.** The original is a 32-bit Intel x86 program.
  This server is one small, self-contained binary, with no runtime or
  libraries to install, for 64-bit Intel/AMD and ARM machines alike: a
  Windows or Linux PC, an Apple silicon or Intel Mac, an ARM cloud VPS, or a
  Raspberry Pi in a cupboard (64-bit ARM, or 32-bit ARMv7 for older boards).

It speaks game protocol **202** and plugin protocol **ACSP v4**, the versions
current Assetto Corsa clients and plugins use.

## Contents

- [Getting started](#getting-started)
- [Configuration](#configuration)
- [How it differs from the original](#how-it-differs-from-the-original)
- [Documentation](#documentation)
- [Building and testing](#building-and-testing)
- [License](#license)

## Getting started

### 1. Download

Pick the line that matches the computer the server will run on, and click to
download the latest version:

| Running the server on… | Download |
|---|---|
| **Windows PC** (almost every Windows computer) | [**⬇ Windows**](../../releases/latest/download/acserver-rs-windows-amd64-ready-to-run.zip) |
| Windows on ARM (Snapdragon laptops, Surface Pro X) | [⬇ Windows ARM](../../releases/latest/download/acserver-rs-windows-arm64-ready-to-run.zip) |
| **Linux PC or server** (Intel or AMD) | [**⬇ Linux**](../../releases/latest/download/acserver-rs-linux-amd64-ready-to-run.tar.gz) |
| **Raspberry Pi 3, 4 or 5** with 64-bit OS, or an ARM cloud server | [**⬇ Linux ARM 64-bit**](../../releases/latest/download/acserver-rs-linux-arm64-ready-to-run.tar.gz) |
| Older Raspberry Pi, or 32-bit Raspberry Pi OS | [⬇ Linux ARM 32-bit](../../releases/latest/download/acserver-rs-linux-armv7-ready-to-run.tar.gz) |
| **Mac with Apple silicon** (M1 or newer) | [**⬇ macOS Apple silicon**](../../releases/latest/download/acserver-rs-darwin-arm64-ready-to-run.tar.gz) |
| Mac with an Intel processor | [⬇ macOS Intel](../../releases/latest/download/acserver-rs-darwin-amd64-ready-to-run.tar.gz) |

Not sure? On a Mac, Apple menu → **About This Mac** shows "Chip: Apple M…" or
"Processor: Intel". On a Pi or Linux, `uname -m` prints `x86_64` (Linux),
`aarch64` (ARM 64-bit) or `armv7l` (ARM 32-bit).

Unpack it anywhere: double-click the `.zip` on Windows, or run
`tar xzf acserver-rs-*.tar.gz` on Linux and macOS. You get a folder holding the
server, a working sample config in `cfg/`, the `content/` and `system/` folders
used for checksums, and a start script.

Bare binaries and `SHA256SUMS` are on the
[releases page](../../releases/latest).

### 2. Configure

Edit `cfg/server_cfg.ini` and `cfg/entry_list.ini`. They are the original
server's files, in the same format, so an existing config works unchanged. The
sample runs a 12-car MX-5 Cup at Brands Hatch: practice, qualifying and race.
At a minimum, set `NAME`, `TRACK`, `CARS` and the entry list, plus
`ADMIN_PASSWORD` if you want to run admin commands in game. The
[official Kunos server guide](docs/assettoserverguideofficial.md) explains
every option.

### 3. Start

Run `start.bat` (Windows) or `./start.sh` (Linux, macOS). The server logs to
the console, and results go to `cfg/results/`.

### 4. Open the ports

Players need these reachable through your firewall or router:

| Port (sample config) | Protocol | Key |
|---|---|---|
| 9600 | TCP and UDP | `TCP_PORT`, `UDP_PORT` |
| 8081 | TCP | `HTTP_PORT` |

With `REGISTER_TO_LOBBY=1` (as in the sample) the server registers with the
in-game server list at startup and logs an error if the lobby refuses it. Check
the server from its own machine with `curl localhost:8081/INFO`.

### Checksums

Checksums stop players joining with modified car or track files. The server
computes and enforces the same ones the original does, from the `content/` and
`system/` folders. The bundle's `content/` already holds the checksum data for
the stock Kunos cars and tracks (not the models, so it is small). For mod cars
or tracks, copy their folders from your Assetto Corsa installation into
`content/cars/` and `content/tracks/`, as you would for the original server.
The server warns at startup if `system/data/surfaces.ini` is missing. With
`--content ""`, it admits everyone unverified; the game still plays normally.

### Other ways to run it

- **From source:** `cd rust && cargo build --release` (the pinned toolchain in
  `rust/rust-toolchain.toml`), then
  `rust/target/release/acserver-rs --cfg cfg --content /path/to/assettocorsa`. For
  a binary that runs on any Linux, build the static musl target
  ([`deploy/README.md`](deploy/README.md)).
- **With the Kunos acServerManager.** Replace `server\acServer.exe` with the
  Windows build renamed to `acServer.exe` (keep the original), or point the
  manager's external-server path at it. The defaults then find the `cfg/` and
  `content/` the manager prepared; copy the game's `system/` folder there too.
  The manager only writes config and launches the server. It sends no booking
  requests and is not a plugin, so booking (`/SUB`) and plugin features need a
  separate tool.
- **A Shutoko Revival Project freeroam server** with AI traffic:
  [`cfg/srp/README.md`](cfg/srp/README.md) and the sample configs next to it.
- **As a Linux service:** [`deploy/README.md`](deploy/README.md) has a hardened
  systemd unit, a service account and firewall setup, and a template for
  running several servers.
- **Several servers from one folder:** give each its own config files and
  ports.

  ```sh
  ./acserver-rs --cfg cfg/server_cfg_prac.ini --entry cfg/entry_list_prac.ini &
  ./acserver-rs --cfg cfg/server_cfg_dtm.ini  --entry cfg/entry_list_dtm.ini  &
  ```

## Configuration

### Command line

| Flag | Default | Meaning |
|---|---|---|
| `--cfg <path>` | `cfg` | `server_cfg.ini`, or a directory containing it |
| `--entry <path>` | next to `--cfg` | `entry_list.ini` |
| `--extra <path>` | next to `--cfg` | optional `extra_cfg.yml` |
| `--content <dir>` | `content` | Assetto Corsa install root (or its `content/` folder) for checksums; `""` disables them |
| `--debug` | off | verbose logging |

The original server's flags also work: `-c` for `--cfg`, `-e` for `--entry`, and
`-platform` (ignored).

### `server_cfg.ini` and `entry_list.ini`

Every option in the [official guide](docs/assettoserverguideofficial.md) takes
effect, including:

- pickup and **booking mode** (`PICKUP_MODE_ENABLED=0` with a `[BOOK]`
  session, plus the `/SUB`, `/UNSUB`, `/CANJOIN` and `/ENTRY` endpoints);
- reserved and shared-GUID slots, `LOCKED_ENTRY_LIST`, `IS_OPEN` 0/1/2,
  `MODEL=*N` wildcards and `FIXED_SETUP`;
- lap and timed races, `RACE_EXTRA_LAP`, `REVERSED_GRID_RACE_POSITIONS`,
  qualifying-decided grids, dynamic track and weather;
- `BLACKLIST_MODE`, `/ban_id` (saved to `<cfg dir>/blacklist.txt`) and
  `AUTH_PLUGIN_ADDRESS`;
- the UDP plugin interface (`UDP_PLUGIN_ADDRESS`, `UDP_PLUGIN_LOCAL_PORT`).

The rules the original applies, which can surprise you:

- `//` starts a comment anywhere on a line, so a URL in `NAME` is cut at `//`. A
  missing `NAME` is `Assetto Corsa Server`.
- The server refuses to start on an entry-list `BALLAST` above
  `MAX_BALLAST_KG`, a `RESTRICTOR` above 100, or a `FIXED_SETUP` naming a file
  that does not exist. The name is used as written: `FIXED_SETUP=fast.ini` for
  `setups/fast.ini`.
- A `MAX_CLIENTS` above the number of `CAR_n` slots is lowered to it, with a
  warning.
- The server refuses to start on a value that does not fit its setting,
  such as `UDP_PORT=70000`, `FUEL_RATE=abc` or `ABS_ALLOWED=5`, and names the
  key. An empty value means the default.
- `WAIT_TIME` applies only to races, with a 20 s minimum; practice and
  qualifying start at once. The result screen lasts at least 10 s, and a race
  ends at least 30 s after the leader finishes.
- Sessions loop by default. `LOOP_MODE=0` stops the server (exit code 0) after
  the last session.
- With pickup mode off, `entry_list.ini` is read again each time the sessions
  loop, so edits take effect between rounds without a restart.
- `SLEEP_TIME` sets how often positions are relayed (every 1 ms in the sample
  config). `SEND_BUFFER_SIZE` and `RECV_BUFFER_SIZE` size the game UDP socket
  when above 0. `NUM_THREADS` and `UDP_LOGGER` are accepted and ignored.
- `[BANDWIDTH_OPTIMIZER]` is ignored: distant cars' positions are always sent
  less often (see [Positions](#positions)).

A server can run for months without a restart: the protocol clock resets at the
first session change after 7 days of uptime.

### `extra_cfg.yml`

Optional settings this server adds. Without the file, you get the original
behaviour plus the protections no player can notice, and AssettoServer's way
of sending positions (see [Positions](#positions)). Every other feature that a
player would notice, or that exposes something, is off. The AssettoServer
features take AssettoServer's key names and defaults, so its `extra_cfg.yml`
carries over.

| Key | Default | Effect |
|---|---|---|
| `ValidatePositionUpdates` | `true` | Drop position updates with NaN or out-of-range values |
| `ValidateUDPSource` | `false` | Require a player's UDP traffic to come from their TCP address's `/24` (IPv6 `/48`). Off because carrier-grade NAT can split a real player's connections |
| `EnableSessionVoting` | `true` | Session votes |
| `EnableKickVoting` | `true` | Kick votes |
| `WriteResultJson` | `true` | Write `results/*.json` |
| `PlayerLoadingTimeoutMinutes` | `5` | How long a player may take to load before being dropped |
| `ChatRateLimitMs` | `0` | Drop chat sent this soon after the sender's last message (AssettoServer uses 1000). CSP's chat-encoded data is exempt |
| `EnableServerDetails` | `true` | The `ℹ<port>` name suffix that points Content Manager at its server-details page. The page (`/api/details`) is always served, because CSP reads the server's features from it |
| `ServerDescription` | `""` | Text shown on that page |
| `EnableContentDownloads` | `false` | Let Content Manager download the cars, skins and track from `--content`, built as zips on first request; password-protected when the server has a password. Content without a model `.kn5` is not offered |
| `EnableCSP` | `true` | CSP kick message, car visibility, spectating and `CSPExtraOptions` ([docs/04](docs/04-csp-extensions.md)) |
| `EnableClientMessages` | `true` | Relay CSP client messages (Lua scripts, car colours, …) between CSP players over TCP, and send the CSP handshake |
| `EnableUdpClientMessages` | `true` | The same over UDP |
| `MinimumCSPVersion` | `0` | Refuse players whose CSP build is older, or who have none; 0 admits everyone |
| `CSPExtraOptions` | `""` | INI text sent to CSP players with the welcome message (docs/04 §4.5). Needs `EnableCSP` |
| `EnableRcon` / `RconPort` | `false` / `0` | Source-style RCON console ([docs/07](docs/07-rcon.md)) |
| `EnableUnlimitedP2P` | `false` | Push-to-pass never runs out |
| `Country` | `[]` | `[name, ISO code]` shown in `/INFO`; empty reports `["na", "na"]` |
| `EnableCustomUpdate` | `true` | Send positions to CSP clients that support it as `0xAB 0x03` batches of up to 20 cars, instead of one `0x46` per car ([docs/04 §4.2](docs/04-csp-extensions.md)). Needs `EnableCSP` |
| `CSPServerScripts` | `[]` | Lua scripts every CSP client loads. A path (relative to the cfg folder) is served at `/api/scripts/<n>`; an `http(s)://` URL is passed to CSP as it is. Needs `EnableCSP` |
| `EnableWeatherFx` | `false` | Run CSP WeatherFX (see below). Players without WeatherFX are refused, which includes a player who has CSP but starts the game from the vanilla server browser: they must join through Content Manager, or the server must run with this off. Needs `EnableCSP` |
| `EnableRealTime` | `false` | WeatherFX follows the real clock in the track's time zone |
| `LockServerDate` | `true` | WeatherFX keeps today's date while `TIME_OF_DAY_MULT` runs the clock |
| `RainTrackGripReductionPercent` | `0` | Grip a fully wet track loses, as a fraction (0.1 = 10%) |
| `TrackTimezone` | `""` | The track's IANA time zone, such as `Asia/Tokyo`. Empty: the `TIMEZONE` of the track's section in `cfg/data_track_params.ini`, else UTC |
| `EnableAi` | `false` | AI traffic (see below). Needs `--content` and `PICKUP_MODE_ENABLED=1` |
| `AiParams` | | Traffic tuning, as AssettoServer's `AiParams` block |
| `UserGroups` | `{}` | Named Steam ID lists: `name: file`, one id per line, relative to the working directory. A missing file is created; a changed one is re-read within 5 s |
| `AdminUserGroup` | `""` | Members join as admin, without `/admin` |
| `WhitelistUserGroup` | `""` | Once this group lists anyone, only its members may join |
| `BlacklistUserGroup` | `""` | Banned on top of `blacklist.txt`; bans are added to it, and members still connected when the file changes are kicked |
| `UserGroupCommandPermissions` | `[]` | `- UserGroup: <group>` / `Commands: [whois, pit]`: admin commands the group may run without being admin |
| `EnableAdminCommands` | `false` | The extra chat commands below. Off: only the vanilla ones |
| `ForceLights` | `false` | Every car's lights are forced on for everyone else |
| `IgnoreConfigurationErrors.UnsafeAdminWhitelist` | `false` | Start even though the admin group lists ids (see below) |
| `EnableAntiAfk` | `false` | Kick players whose inputs have not changed for `MaxAfkTimeMinutes` (default 10), with a chat warning a minute before. Admins are exempt |
| `AfkKickBehavior` | `PlayerInput` | `PlayerInput`: any change of lights, gas or steering, or a chat line, counts as activity. `MinimumSpeed`: only input changes above 20 km/h count |
| `MaxPing` / `MaxPingSeconds` | `0` / `10` | Kick a player whose ping stays above `MaxPing` ms for more than `MaxPingSeconds`. 0 disables it |
| `PlayerChecksumTimeoutSeconds` | `0` | Kick a player who has not answered the checksum request this many seconds after loading. 0 disables it |
| `NetworkBubbleDistance` | `500` | A car further than this many metres from a player, or any car of a spectating driver, reaches them every fifth update, a tier below the bandwidth optimizer's lowest (3.6 Hz at 18 Hz). Cars inside keep their optimizer tier. 0 disables it for players' cars; AI cars then use 500 m. The default is AssettoServer's, which sends 4 Hz outside it; its `OutsideNetworkBubbleRefreshRateHz` is ignored |
| `BatchFarPositions` | `true` | Cars outside a player's network bubble reach a client without CSP custom updates once per `CLIENT_SEND_INTERVAL_HZ` tick, up to 20 to a `0x48` MegaPacket (shared with its AI cars), instead of as single `0x46`s. A lone car still goes as a `0x46`. A `0x48` has no `Gas` or `PerformanceDelta`, so a batched car's arrive as 0. Not an AssettoServer key |

The CSP features are on by default because they only reach players who run CSP;
everyone else sees exactly what the original server sends.

### Admins, user groups and commands

There is no Steam ticket check, so a joining player's Steam ID is not
verified: anyone who knows an admin's id can join as admin. As in
AssettoServer, the server refuses to start with a non-empty
`AdminUserGroup` unless `IgnoreConfigurationErrors.UnsafeAdminWhitelist` is
set. The whitelist and blacklist have the same weakness, but they only
decide who may join, not who gets admin rights.

With `EnableAdminCommands` these chat commands are added. They also work
over RCON and ACSP, where they reply as text, except `/ping`, which needs a
player. A `<player>` is a session id
or a name: exact, then ignoring case, then part of a name.

| Command | Who | Effect |
|---|---|---|
| `/ping` | everyone | Your ping |
| `/time` | everyone | The in-game time |
| `/ban <player> [reason]` | admin | Ban, telling everyone why; the player sees the reason on CSP's kick screen |
| `/unban <steam id>` | admin | Lift a ban, from `blacklist.txt` and the blacklist group |
| `/whitelist <steam id>` | admin | Add to the whitelist group |
| `/whois <player>` | admin | IP, Steam profile, ping, position and speed |
| `/pit <player>` | admin | Send to the pits |
| `/distance <player>` | admin | Distance from you to them, metres |
| `/forcelights <on\|off> <player>` | admin | Force their lights on for everyone else |
| `/restrict <player> <restrictor> <ballast>` | admin | Set both |
| `/say <text>` | admin | Broadcast `CONSOLE: <text>` |
| `/setweather <id>` | admin | Apply `[WEATHER_<id>]` now |
| `/setgrip <0-1>` | admin | Set the track grip |

The vanilla commands (`/kick`, `/kick_id`, `/ban_id`, `/ballast`, …) keep
their vanilla behaviour. These need their feature and work without
`EnableAdminCommands`: `/setcspweather`, `/cspweather`, `/setrain` and
`/settime` (WeatherFX), and `/setaioverbooking <n>` (AI traffic).

Not implemented from AssettoServer: `/legal`, Steam family-sharing bans, and
the CSP admin UI.
`MandatoryClientSecurityLevel` is not implemented either: in AssettoServer
0.0.54 nothing sets a client's security level (only a separate, closed-source
plugin would), so any level above 0 would just drop every player's
positions.

### WeatherFX

The server owns the weather. Every second it sends the weather type and any
transition in progress, the temperatures, the wind, the rain, the track's
wetness and puddles, and the date and time, all as `0xAB 0x01`. `0x78` and
`0x54` are no longer sent.

- **Weather type:** taken from `[WEATHER_n] GRAPHICS` in AssettoServer's form,
  `3_clear_type=<id>`. Without a type it is Clear.
- **Time of day:** starts at `SUN_ANGLE` and runs at `TIME_OF_DAY_MULT`.
- **Rain:** wets the track and costs grip (`RainTrackGripReductionPercent`).
- **Admin commands** (chat, RCON or ACSP):
  - `/setcspweather <type> [seconds]` changes the weather over a transition,
    30 s by default. Give the type by name (`Rain`) or by id; `/cspweather`
    lists them.
  - `/setrain <intensity> <wetness> <water>` sets the rain directly.
  - `/settime <hh:mm>` sets the time of day.

### AI traffic

Entry-list slots with `AI=fixed` are always traffic. `AI=auto` slots are
traffic until a player takes one, and become traffic again when that player
leaves. Without `EnableAi`, both kinds stay closed to players, as in vanilla.

The road comes from the track's AI splines:
`<content>/content/tracks/<track>/<config>/ai/` or `<track>/ai/`.

- **Files:** either a `fast_lane.aip` package, or the `fast_lane*.ai` files
  plus an optional `config.yml` that defines junctions.
- **Lanes:** neighbouring lanes are found automatically, `LaneWidthMeters`
  apart.

The cars are simulated on the server:

- **Driving:** each AI car follows its lane at its own top speed, slows for
  corners, and keeps a gap to the car or player ahead.
- **Junctions:** it takes junctions at random, indicating as `config.yml` says.
- **Collisions:** it stops for a moment after a player hits it, and it honks
  when stuck.
- **Spawning:** cars spawn ahead of active players, preferring the players with
  the least traffic near them, and despawn once no player is near.
- **Overbooking:** one slot can drive several cars far apart. Each player sees
  the one nearest them.
- **Sending:** each player gets its AI cars batched, up to 20 to a datagram,
  as AssettoServer does: `0xAB 0x03` with `EnableCustomUpdate` and a client
  that supports it, else `0x48` MegaPacket
  ([docs/03](docs/03-udp-protocol.md)). Player cars go out as single
  `0x46`s, except those outside the player's network bubble, which share the
  AI cars' `0x48`s (`BatchFarPositions`).
- **Rate:** AI cars are ranked with the players' cars as one list, and
  thinned by the same tiers (see "Positions" below). AI outside the network bubble (500 m without
  `NetworkBubbleDistance`) goes every fifth tick. AssettoServer sends every
  AI car inside the bubble every tick.

The number of cars follows `AiPerPlayerTargetCount`, `MaxAiTargetCount`,
`TrafficDensity` and `HourlyTrafficDensity`. Further tuning:

- **Per model:** `CarSpecificOverrides`, including `AllowedLanes`
  (`Left`, `Middle`, `Right`) and `MinLaneCount` / `MaxLaneCount`, which limit
  where a model spawns.
- **Per lane count:** `LaneCountSpecificOverrides` (`<lanes>:` with
  `MinAiSafetyDistanceMeters` and `MaxAiSafetyDistanceMeters`) sets the gaps
  for cars spawned on roads with that many lanes.
- **Ignore zones:** inside `IgnorePlayerObstacleSpheres`
  (`- Center: {X: 0, Y: 0, Z: 0}` with `RadiusMeters: 50`), AI cars ignore
  players and do not stop when hit.
- **Paint:** an AI slot whose `SKIN` carries CSP driver options allowing colour
  changes (`<skin>/<options>`, as Content Manager writes them) gets a random
  everyday paint per car. This needs `EnableClientMessages`.
- **Debug overlay:** `Debug: true` loads a built-in CSP script that shows each
  AI car's current, target and top speed and its gap to the car ahead. It
  needs `EnableCSP` and `EnableClientMessages`.

### Live configuration

With `EnableLiveConfiguration: true`, admins can change settings while the
server runs. Changes last until restart and are not written back to the
files. Three ways in, all using AssettoServer's key paths:

- **Chat or RCON:** `/set <key> <value>`, e.g. `/set Extra.MaxPing 300` or
  `/set Extra.AiParams.MaxSpeedKph 90`.
- **In game:** a "Server" window in CSP's online extras (needs `EnableCSP`),
  whose Configuration tab lists every setting and saves one at a time.
- **HTTP:** `GET /api/configuration` returns the tree as a Lua table;
  `POST /api/configuration?key=Root.<path>&value=<v>` sets one. The caller
  must be a connected admin, sending `X-Car-Id` (its session id) and
  `X-Api-Key` (the key a CSP script receives with the `AS_ApiKey` event).

The settings that can change:

- **`Server`:** `TimeOfDayMultiplier`.
- **`Extra`:** `ForceLights`, `EnableAntiAfk`, `MaxAfkTimeMinutes`,
  `AfkKickBehavior`, `MaxPing`, `MaxPingSeconds`,
  `PlayerChecksumTimeoutSeconds`, `NetworkBubbleDistance`, `BatchFarPositions`, `EnableRealTime`,
  `LockServerDate`, `RainTrackGripReductionPercent`, `ChatRateLimitMs`,
  `EnableSessionVoting` and `EnableKickVoting`.
- **`Extra.AiParams`:** every number and switch, except `LaneWidthMeters`,
  `SmoothCamber`, `NamePrefix` and `AiBehaviorUpdateIntervalHz`, which only
  take effect at start. Lists and maps are shown but cannot be changed.

Everything else is read at start and shown read-only, or not shown at all.

### Plugins

AssettoServer's bundled plugins are built in, except the Discord audit
plugin. As in AssettoServer, none runs until it is named in `EnablePlugins`.
Each takes its settings from a tagged document after the main one in
`extra_cfg.yml`, with AssettoServer's keys:

```yaml
EnablePlugins:
  - WordFilterPlugin
  - VotingWeatherPlugin
---
!WordFilterConfiguration
ProhibitedChatPatterns: ['badword']
---
!VotingWeatherConfiguration
VotingIntervalMinutes: 10
```

| Plugin | Settings document | What it does | Needs |
|---|---|---|---|
| `AutoModerationPlugin` | `!AutoModerationConfiguration` (optional) | `WrongWayKick`, `NoLightsKick` and `BlockingRoadKick`, each `Enabled`, `DurationSeconds` and a speed limit: a warning at half the duration, a kick after it. CSP players see a flag on screen | `EnableAi` for wrong way and blocking (the road comes from the AI spline). Night for no lights is the sun 12° below the horizon: with WeatherFX from the track's position, otherwise the vanilla sun angle |
| `GeoIPPlugin` | `!GeoIPConfiguration` (`DatabasePath`) | Logs each player's country and location | A MaxMind GeoLite2 or GeoIP2 City `.mmdb` |
| `LiveWeatherPlugin` | `!LiveWeatherConfiguration` (`OpenWeatherMapApiKey`, `UpdateIntervalMinutes`) | Follows the real weather at the track | `EnableWeatherFx`, the track's position |
| `RaceChallengePlugin` | none | Flash your lights three times at the car ahead, or `/race <player>`; `/accept` (or hazards) starts the race | |
| `RandomWeatherPlugin` | `!RandomWeatherConfiguration` (optional) | Random weather changes, weighted by `WeatherWeights` | `EnableWeatherFx` |
| `ReportPlugin` | `!ReportConfiguration` (optional) | CSP players upload a replay clip (Ctrl+Shift+S), saved with a chat and join log in `reports/`; `/report <reason>` adds the reason. `WebhookUrl` is ignored (no Discord); Clips upload to the server's public IP, looked up from ip-api.com at start as AssettoServer does; `ServerAddress` ("host" or "host:port") overrides it, which a LAN server needs. CSP does not fill `{ServerIP}` into this URL, so a failed lookup leaves uploads broken until `ServerAddress` is set | `EnableCSP` |
| `TimeDilationPlugin` | `!TimeDilationConfiguration` (`LookupTable` of `SunAngle` → `TimeMult`) | Time speed by the sun's altitude, so nights can pass faster | `EnableWeatherFx`, the track's position |
| `VotingWeatherPlugin` | `!VotingWeatherConfiguration` (optional) | A weather vote every `VotingIntervalMinutes`, with `/w <n>` | `EnableWeatherFx` |
| `WordFilterPlugin` | `!WordFilterConfiguration` (optional) | Refuses names, drops chat lines, and bans for chat lines matching case-insensitive regular expressions | |

The track's position is `TrackLatitude` and `TrackLongitude` in the main
document, or `LATITUDE` and `LONGITUDE` of the track's section in
`cfg/data_track_params.ini`.

One deliberate difference: when a race leader jumps more than 200 m in one
check (a teleport), RaceChallenge gives the race to the follower. AssettoServer
0.0.54 means to do the same, but its swap assigns the same car to both
places.

### Positions

These change how often or how positions are packed, not what they say. Only
`BatchFarPositions` is visible to a client, and only for cars over 500 m away.

- The network bubble is on by default (`NetworkBubbleDistance: 500`):
  players more than 500 m apart get each other's positions every fifth
  update, where the original sends every one. Set it to 0 to send them all.
- The bandwidth optimizer is always on, and `[BANDWIDTH_OPTIMIZER]` is
  ignored. Vanilla turns it on only with that key. Each
  player's view of every other car, AI included, is ranked by distance: the
  nearest 7 get every update, the next 7 every second, the next 7 every
  third and the rest every fourth (vanilla: tiers of 5, players only). A car
  within 100 m, or whose position is not known yet, is never thinned out,
  whatever its rank. A spectating
  player's ranks are measured from the car they watch.
- The ranking is redone once per `CLIENT_SEND_INTERVAL_HZ` tick (18 Hz by
  default), not on every relay pass as vanilla does. Positions are still
  relayed every pass, at the tiers of the last tick, and each relayed update
  advances its receiver's skip counter as in vanilla.
- `EnableCustomUpdate` defaults to on, so CSP clients that support it get
  player cars as `0xAB 0x03` batches. Set it to `false` for single `0x46`s.
- `BatchFarPositions` defaults to on: other clients get the cars outside
  their network bubble once per tick (up to 55 ms later), batched in `0x48`s,
  which vanilla never sends and which carry no `Gas` or `PerformanceDelta`. Set it to `false` (or `NetworkBubbleDistance: 0`) to send them as `0x46`s.

## How it differs from the original

Every difference below is checked by a test against the original binary
([`test/COMPARE.md`](test/COMPARE.md#recorded-deviations)). A well-behaved
client cannot tell any of them from the original, except how often positions
arrive (see [Positions](#positions)).

**Holes closed.** The original lets a client:

- chat under another driver's name (here the real sender is used);
- send NaN positions that are relayed to every other car (here they are dropped);
- take several grid slots, through repeated handshakes on one connection or one
  connection per slot with the same Steam ID;
- join with no Steam ID or a blank name;
- read past the entry list with an out-of-range car-list page (here it is clamped);
- start votes against itself or an empty slot, or `/kick_id` an empty slot.

**Other choices.** A player who reconnects with the same Steam ID replaces their
old connection at once and keeps their slot and laps. Unknown HTTP paths return
`404` rather than an empty `200`, and HTTP replies carry a JSON content type and
CORS headers. The `/ENTRY` status page HTML-escapes driver names and teams. `/client_list`, which the original lists in `/help` but never
implemented, works. The checksum list also covers each car's `collider.kn5` and
a `surfaces.ini` in the track folder. `IS_OPEN=0` is enforced in pickup mode, as
the official guide describes. `SPECTATOR_MODE=1` is accepted (the original
refuses to start). Chat control characters other than tab are stripped. Laps
under 1 s are recorded but cannot become the best lap.

### Abuse limits

The original has none of these. Each threshold is far above what real players
use, so they are fixed rather than configurable.

| Limit | Value | Without it |
|---|---|---|
| Connections before handshake | `max(32, 2 × slots)` | Any host can exhaust file descriptors |
| Handshake deadline | 10 s | An idle socket holds a slot for minutes |
| Loading deadline | `PlayerLoadingTimeoutMinutes` (5) | A client stuck loading holds its slot forever |
| Ping timeout | 15 s without a reply | A vanished client stays on the grid |
| Connections per Steam ID | 1; a rejoin replaces the old one | One client takes the whole grid |
| Occupied slots | `MAX_CLIENTS` | More drivers than `/INFO` advertises |
| CSP client messages | 30 burst, 30/s per client per transport; 2048 bytes | One client's flood is relayed to every CSP player |
| `/admin` attempts | 3 per connection | Unlimited password guessing |
| Vote starts | 1 per 30 s per driver | Vote spam reaches the whole grid |
| Lobby-probe (`0xC8`) replies | 20/s per source IP | Spoofed floods reflected at a third party |
| Time-request (`0xF9`) replies | 20/s per source IP | Spoofed floods reflected at a third party |
| Position updates | `max(100, 2 × CLIENT_SEND_INTERVAL_HZ)`/s per client | One client's flood is relayed to every player |
| UDP re-association | same IP, or the TCP address's `/24` (`/48`) | Anyone can redirect a live player's UDP with one packet |
| RCON | 8 connections, 10 s to authenticate | Idle sockets pile up |
| HTTP read/write | 5–15 s | A slow request ties up the server |

### Not supported

- Steam session-ticket validation (`AUTH`).
- From AssettoServer: `/legal`, Steam family-sharing bans, the CSP admin UI,
  `MandatoryClientSecurityLevel` and the Discord audit plugin (see
  [Admins, user groups and commands](#admins-user-groups-and-commands)).

## Documentation

| Document | What's in it |
|---|---|
| [Official Kunos server guide](docs/assettoserverguideofficial.md) | Every `server_cfg.ini` and `entry_list.ini` option |
| [`deploy/README.md`](deploy/README.md) | Running under systemd, one server or several |
| [`docs/`](docs/README.md) | A complete, language-agnostic reference for the Assetto Corsa multiplayer protocol, for anyone building a server, client or admin tool |
| [`IMPLEMENTATION.md`](IMPLEMENTATION.md) | Where this server's behaviour goes beyond the docs: choices, hardening, deviations |
| [`docs/05-http-api.md`](docs/05-http-api.md) | `/INFO`, `/JSON`, booking endpoints, Content Manager endpoints, lobby registration |
| [`docs/06-udp-plugin-api.md`](docs/06-udp-plugin-api.md) | The plugin API (ACSP v4), for writing timing, admin or league plugins |
| [`docs/07-rcon.md`](docs/07-rcon.md) | The RCON console |
| [`docs/08-kunos-compatibility.md`](docs/08-kunos-compatibility.md) | What a drop-in replacement must implement, and how AssettoServer and the original differ |
| [`docs/09-assettoserver-features.md`](docs/09-assettoserver-features.md) | AssettoServer's features on the wire: extra options, Lua events, WeatherFX, AI traffic, kicks, plugins |
| [`test/COMPARE.md`](test/COMPARE.md) | How this server is tested against the original binary |
| [`test/PERFORMANCE.md`](test/PERFORMANCE.md) | CPU, memory and traffic compared with the original |
| [`CHANGELOG.md`](CHANGELOG.md) | Release notes |

## Building and testing

```sh
cd rust
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
cargo build --release              # rust/target/release/acserver-rs
cd ../test && go test -race -timeout 30m ./...   # the comparison suite; ~8 min with a reference install
```

The server is in `rust/` (see `CLAUDE.md` for the toolchain), with its
in-process integration tests in `rust/tests/`. `test/` is a Go module of test
tooling only: its suite runs the release binary in its own
process and, with an original Kunos server install at `kunosserverfiles/`
(ignored by git), compares it against that packet by packet; see
[`test/COMPARE.md`](test/COMPARE.md).

```
rust/src           the server (config, server core, HTTP, ACSP plugin API, RCON, results, AI)
rust/tests         in-process integration tests
test               Go: the reference comparison, extension and performance tests
test/cmd/loadtest  out-of-process load generator
test/internal/wire the test client's little-endian primitives, strings, TCP framing
test/internal/proto the test client's packet ids and enums
```

## License

Copyright (C) 2026 The acserver-rs Authors.

acserver-rs is free software under the
[GNU Affero General Public License, version 3 or later](LICENSE)
(AGPL-3.0-or-later). If you run a modified version for players, the AGPL
requires you to offer those players its source code (section 13).

The binaries include open-source libraries under their own licences (MIT,
Apache-2.0 and others, all compatible with the AGPL), reproduced in
[THIRD_PARTY_LICENSES](THIRD_PARTY_LICENSES). Both files ship in every
release bundle.

Code taken from [AssettoServer](https://github.com/compujuckel/AssettoServer),
which is also AGPL-3.0-or-later, keeps its copyright notice
(Niewiarowski, compujuckel) in the files it is copied into.

This project is not affiliated with or endorsed by Kunos Simulazioni or 505
Games. Assetto Corsa is their trademark.
