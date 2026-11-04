# How this repository's server implements the protocol

The [protocol docs](docs/README.md) describe vanilla `acServer` v1.15 and
AssettoServer without reference to any one server. This file records what the
server in `rust/` does where the docs leave a choice, harden past vanilla, or
deviate.

The server implements every `[K]` behaviour, follow the vanilla column of
every `[K*]`, and carry AssettoServer's features (doc 9) behind
`extra_cfg.yml` switches ([README](README.md) "Configuration"). Each
deviation a client could observe is pinned by a probe against the reference
binary; [`test/COMPARE.md`](test/COMPARE.md) lists the probes and the
recorded deviations. The hard-coded abuse limits are tabled in the README
("Abuse limits").

## Connection (doc 1)

- **Duplicate guid.** A join from a guid that is already connected drops the
  old connection at step 9 and reuses its slot when the model matches.
- **Zero guid, blank name** are refused with `0x6F` (steps 2 and 6), as
  AssettoServer does.
- **CSP build.** The build number at the end of `Features` is always read. A
  build below `MinimumCSPVersion`, or none, is refused when that is set.
- **Locked entry list.** With pickup mode off or `LOCKED_ENTRY_LIST=1`, a guid
  without a free reserved slot is closed at step 4 with no reply packet. A
  booking server closes an unbooked guid the same way.
- **`IS_OPEN=0`** is enforced in pickup mode too.
- **Which slot.** A slot reserved by `GUID=` is kept for its owner, never
  given to someone else as a last resort.
- **`MAX_CLIENTS`** admission counts every occupied slot, loading or not.
- **Checksums** are never gated: the deferred kick is used. `collider.kn5` and
  a track-root `surfaces.ini` are added to the path list, as AssettoServer
  does.
- **`CLIENT_SEND_INTERVAL_HZ`** is also read as `FPS` or `REFRESH_RATE_HZ`.
- **`0x43`.** The server closes the TCP connection at once.
- **`LOOP_MODE=0`.** After the last session the server shuts down cleanly with
  exit code 0, so a service manager can restart it.

## TCP (doc 2)

- **Chat.** The sender id is replaced by the real sender. Control characters
  other than tab are stripped, and a blank line is dropped. No rate limit by
  default (`ChatRateLimitMs`).
- **Commands.** `/client_list` is implemented (one
  `<id>: <name> (<guid>) <model>` line per driver). `/kick_id` on an empty
  slot is refused. Kicks go through vanilla's `KickUser`/`BanUser` sequence.
- **`0x3F`.** A page past the list is clamped to an empty page.
- **Laps.** A lap under 1 s still counts but cannot become the best lap.
- **`RandomSeed`** is one random value per run.
- **Leaderboard** sorting is stable, so ties keep their order.
- **`0x52`** settings go out in file order.
- **Votes.** One vote start per 30 s per driver.

## UDP (doc 3)

- **`0x4E`.** A move of an existing association must stay on the old IP or
  the TCP peer's `/24` (`/48`). `ValidateUDPSource` (off by default) applies
  the `/24` match to the first association too.
- **`0x46`.** NaN and out-of-range updates are dropped
  (`ValidatePositionUpdates`, on). Each client may send
  `max(100, 2 × CLIENT_SEND_INTERVAL_HZ)` updates a second, with half a
  second of burst.
- **`0xC8`, `0xF9`** replies are capped at 20 a second per source IP.
- **Clock.** The server clock restarts at 0 at the first session change after
  7 days of uptime, moving every stored offset with it. A session running 17
  more days without a change still overflows.
- **Relay.** A relay thread runs vanilla's main-loop pass: an accepted `0x46`
  marks the car pending, and each pass sends every pending car's latest
  update in slot order, then sleeps `SLEEP_TIME` ms (50 ms while no client is
  ready). Passes run concurrently with packet handling. `SLEEP_TIME` below 1
  runs as 1.
- **Bandwidth optimizer**, always on (`[BANDWIDTH_OPTIMIZER]` is ignored):
  - Ranked once per `CLIENT_SEND_INTERVAL_HZ` tick, not every pass. Each
    relayed update advances the receiver's skip counter, as in vanilla.
  - Tiers of 7 (vanilla: 5): every update, every 2nd, every 3rd, every 4th.
    AI cars are ranked in the same list.
  - A car within 100 m, or whose distance is unknown, is never thinned.
  - **Network bubble** (`NetworkBubbleDistance`, default 500 m): a car further
    away, or any car of a spectating driver, gets a fifth tier, every 5th
    update. AssettoServer instead sends it at
    `OutsideNetworkBubbleRefreshRateHz` (default 4 Hz), which is ignored. A
    spectator's ranks are measured from the car they watch. With 0, players'
    cars have no bubble and AI cars use 500 m.
- **`0x48`.** With `BatchFarPositions` (default on), the cars outside a
  player's bubble are held and sent once per tick in `0x48`s of up to 20,
  shared with that player's AI cars; a lone car goes as a `0x46`. Batched
  cars lose `Gas` and `PerformanceDelta`. Custom-update clients get every car
  in `0xAB 0x03` instead.

## CSP (doc 4)

- `EnableCSP`, `EnableClientMessages` and `EnableUdpClientMessages` default
  to on: nothing reaches a client that announced no CSP build. Relayed
  messages go only to other ready CSP clients, are capped at 2048 bytes and
  30 a second (burst 30) per client and transport.
- **`f16`** rounds to nearest, as C#'s `(Half)` does.
- **Extra options.** `VERIFY_STEAM_API_INTEGRITY` and
  `MIN_TIME_BETWEEN_COLLISIONS` are not injected. A CSP older than build 1937
  gets no extra options when the welcome would pass 2039 characters.
- **Not implemented:** chat-encoded server messages, and Lua key hashing at
  runtime (keys are precomputed constants).

## HTTP (doc 5)

- `/INFO` and `/JSON` reply `application/json` with
  `Access-Control-Allow-Origin: *`. An unrouted path gets `404`.
- `/ENTRY` HTML-escapes the session name and every cell.
- **Booking.** The `0x41` vanilla misaddresses is not sent at all.
- **Entry-list reload.** At a loop back to the first session, drivers whose
  slot still fits (same model, still reserved for them) stay connected and the
  rest are disconnected. A file that fails to load or check keeps the
  previous list.
- **`/RECONFIG`.** The settings are discarded, as in vanilla. The error text
  is serde's, and the HTTP library answers `400` to a raw `"` in the query.
- **Content downloads** (`EnableContentDownloads`) are built from `--content`
  on first request and cached next to the content (`cars/<carId>.zip`,
  `…/skins/<skinId>.zip`, `tracks/<trackId>.zip`; delete to rebuild, `500` if
  the directory is read-only). Archives are deflate, rooted like the disk.
  Only entry-list models are offered, and only when the folder holds a model
  (a `.kn5` other than `collider.kn5`), so a content tree trimmed to checksum
  data offers nothing.
- `/api/scripts/<n>` serves the local `CSPServerScripts` and the built-in
  scripts. `/assets/*` is not served.
- **Lobby.** Joins and leaves are polled every 5 s for the subscriptions
  update, so it can lag by 5 s and miss a join and leave inside one poll. The
  keepalive's `timeleft` does not underflow. After two
  `ERROR - RESTART YOUR SERVER…` replies the server re-registers, with backoff.

## Plugin API (doc 6)

- **`0xC9`** for an unknown id answers with `IsConnected` false and empty
  strings.
- **`0xCC`** errors read `ERROR: ACSP_GET_SESSION_INFO index <n> out of bound`,
  not vanilla's `… out of bounds: <n>`. Not yet aligned.
- **`0xCD`** stores Time as whole minutes and WaitTime as whole seconds.
- **`0xD1`** runs the RCON command set below, which takes the command
  **without** its `/`; `/ballast 2 30` is an unknown command. Whether vanilla
  wants the slash is unmeasured.

## RCON (doc 7)

The command set is a superset of the vanilla chat commands: `kick` takes a
session id, `ban` and `say` are added, and `client_list` returns its lines in
one ResponseValue. With `EnableAdminCommands`, AssettoServer's commands work
too. RCON allows 8 connections and 10 s to authenticate, and refuses all
logins when there is no admin password.

## Results and config (doc 8)

- Results are written under the cfg directory, with `_<server name>` added to
  the file name.
- **INI parsing** (`rust/src/config/ini.rs`) follows vanilla's `//` comment
  rule and default name, but trims keys, values and section headers and skips
  a BOM. It keeps surrounding quotes and reads decimal numbers only.
- **A value its setting cannot hold refuses to start**, rather than falling
  back to a default or wrapping. Each setting has a type: ports `u16`, `MAX_CLIENTS` 0 to
  255, percentages and seconds non-negative, `IS_OPEN`, `BLACKLIST_MODE`,
  `START_RULE`, `ABS_ALLOWED` and `TC_ALLOWED` 0 to 2, flags a number or
  true/false, the pit window non-negative, and wind directions 0 to 360. An
  empty value still means the default. The error names the key and the value.
  `IDLE_MODE` and `AI_LEVEL` are ignored.
- `SPECTATOR_MODE=1` is accepted (vanilla refuses to start).
- There is no Steam session-ticket validation.

## AssettoServer features (doc 9)

The features follow AssettoServer 0.0.54, with these differences:

- **Built-in scripts.** `acserver_admin.lua` is the in-game configuration
  window (AssettoServer's configuration parts, enabled by
  `EnableLiveConfiguration`). `ai_debug.lua` (with `AiParams.Debug`) draws an
  overlay from the `ai_debug` event: key `0xB8DC08B3`, fields
  `int16 ClosestAiObstacles[20]`, `uint8 CurrentSpeeds[20]`,
  `uint8 MaxSpeeds[20]`, `uint8 SessionIds[20]`, `uint8 TargetSpeeds[20]`,
  sent to each CSP client every 100 ms. Each row is one AI slot's car nearest
  that player (km/h; gap in m, `-1` for none; unused rows id `255`).
- **`AS_ApiKey`** is answered only while there is at least one server script.
  The other `AS_*` Lua events are not implemented.
- **AI positions** are ranked with players' cars (above) instead of sent
  every tick inside the bubble.
- **ReportPlugin.** `<host>` is `ServerAddress`, else the public IP looked up
  from ip-api.com at start, with `HTTP_PORT`. An upload must come from the
  `X-Car-Index` client's IP (or loopback) with the right key. Replies: `200`
  saved as `reports/<id>.zip` and `<id>.json` (entry list and the last
  `ClipDurationSeconds` of joins, leaves and chat), `400` no client in that
  slot, `403` wrong key or IP, `405` not `POST`, `429` under 30 s since the
  player's last clip, `500` not written. `WebhookUrl` is ignored.
- **RaceChallengePlugin** gives the race to the follower when the leader
  teleports (over 200 m in one check); AssettoServer means to, but its swap
  assigns one car to both places.
- **Not implemented:** `/legal`, Steam family-sharing bans, the CSP admin UI
  other than the configuration window, `MandatoryClientSecurityLevel`, the
  Discord audit plugin, AC Pro guids (`EnableACProSupport`).
