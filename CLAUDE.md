# CLAUDE.md

Guidance for Claude Code in this repository.

## What this repo is

1. **`docs/`** — a language-agnostic reference for the Assetto Corsa multiplayer
   protocol (protocol **202**, ACSP **v4**), documented from AssettoServer and
   checked against vanilla acServer v1.15. Everything is tagged by origin: `[K]`
   Kunos, `[K*]` Kunos where AssettoServer deviates, `[AS]` AssettoServer,
   `[CSP]` Custom Shaders Patch, `[CM]` Content Manager. Start at
   `docs/README.md`.
2. **`rust/`** — acserver-rs (binary `acserver-rs`), a replacement for Kunos `acServer` built from those
   docs. It reads vanilla `server_cfg.ini` / `entry_list.ini`, keeps state in
   memory, and writes vanilla `results/*.json`. `README.md` is the user-facing
   overview.
3. **`test/`** is a Go module of test tooling only: the reference
   comparison, extension and performance tests, the load generator
   `test/cmd/loadtest`, and the test client's wire code in
   `test/internal/wire` and `test/internal/proto`.

The docs are the source of truth. The server implements `[K]`, follows the
vanilla side of every `[K*]`, and adds hardening that no honest client can
observe. The docs are server-agnostic: they never mention this repository's
server. What it chooses, hardens or deviates on goes in `IMPLEMENTATION.md`,
organised by doc.

## Build / test / run

The toolchain is a pinned nightly (`rust/rust-toolchain.toml`) because
`rust/.cargo/config.toml` enables the next-generation trait solver
(`-Znext-solver=globally`). Polonius is off: it crashes that nightly's
compiler on a dependency. Moving the pin means re-running clippy, the tests
and the probes.

```sh
cd rust
cargo fmt --check                            # must be clean
cargo clippy --all-targets -- -D warnings    # must be clean
cargo test                                   # unit and in-process integration tests, ~3 min
cargo build --release                        # rust/target/release/acserver-rs

cd ../test
go vet ./... && gofmt -l .                   # test tooling; must be clean / print nothing
go test -race -timeout 30m ./...             # builds the release binary, runs it out of process;
                                             # ~8 min with a reference install, so the
                                             # default 10m timeout fails

rust/target/release/acserver-rs --cfg cfg [--content /path/to/assettocorsa] [--debug]

# Real ceiling with clients in their own processes:
cd test && LOADTEST_CLIENTS=255 LOADTEST_PROCS=5 go test -run TestMultiProcessGridRace . -v
```

The Go tests use `ACSERVER_BIN` when set, else build `rust/` once per run. With
a Kunos install (`AC_REFERENCE_SERVER`, `AC_REFERENCE_DIR`, `AC_CONTENT`, or
`kunosserverfiles/`), they diff the server against it packet by packet
(`test/COMPARE.md`). Deployable binaries are static musl builds
(`deploy/README.md`).

Follow `RUST-GUIDELINES.md`: simple, readable code unless the simple form has a
measured, significant cost. Use a standard crate when it reduces complexity:
axum and tower-http for HTTP, serde_json for JSON, serde_norway for YAML,
`LengthDelimitedCodec` for TCP and RCON framing, parking_lot locks, glam
vectors, governor rate limits, walkdir, rstar for the AI spline index, half for
`f16`, and an INI parser with vanilla's ks/ini rules (`src/config/ini.rs`).
There are no existing users, so keep no backwards-compatibility code.

## Architecture

```
rust/src/main.rs     process wiring: config, server, HTTP/plugin/RCON/lobby
rust/src/wire        LE primitives, string8/16, wstring8/16 (UTF-32LE), cstr, TCP framing, f16
rust/src/proto.rs    packet ids and enums (single source for 0x.. values)
rust/src/config      server_cfg.ini / entry_list.ini / extra_cfg.yml → typed structs
rust/src/server      the core (below)
rust/src/httpapi     /INFO, /JSON, /api/details, content downloads, lobby register/ping,
                     acServerManager endpoints (/SUB, /UNSUB, /CANJOIN, /ENTRY, …)
rust/src/plugin.rs   ACSP v4; implements server::PluginBridge
rust/src/ai          AI traffic: road network from ai/ splines, one AI car's driving
rust/src/rcon.rs     optional Source-RCON
rust/src/results.rs  results/*.json writer
rust/tests           in-process integration tests (common/ is the harness)
test                 Go module: reference comparison, extension, plugin API and performance tests
test/cmd/loadtest    Go: protocol load generator (test/multiprocess_test.go drives it)
test/internal/wire, proto  Go: the test client's wire code
```

`rust/src/server`:

| File | Contents |
|---|---|
| `mod.rs` | `Server`, listeners, lifecycle, blacklist |
| `client.rs`, `connection.rs` | per-connection state, accept loop, TCP read and write tasks, dispatch, disconnect |
| `entrycar.rs` | one entry-list slot: occupant, BoP, P2P, last position, result |
| `handshake.rs` | `0x3D` validation order, `0x3E`, `AUTH_PLUGIN_ADDRESS` |
| `checksum.rs` | checksum paths from `--content`, `0x44`, deferred kick |
| `firstupdate.rs` | first-update snapshot, leaderboard ranking |
| `tcp_handlers.rs` | chat, car list, P2P, pit, tyres, damage, sectors, laps, client events |
| `commands.rs` | `/` commands and `execute_admin_command` (shared with RCON and ACSP) |
| `vote.rs` | session and kick votes |
| `csp.rs` | CSP `0xAB`: client messages, handshake, kick message, extra options |
| `udp.rs` | UDP receive, `0x4E`, inbound `0x46`, `0xF8`, `0x4F` |
| `loops.rs` | relay pass every `SLEEP_TIME` ms: relay, pings, sun, `0x57`; 1 s ping-timeout check |
| `broadcast.rs` | every S→C packet builder |
| `session.rs` | session list, clock, transitions, grids |
| `weather.rs` | ambient conditions, dynamic track |
| `setup.rs`, `drs.rs` | `FIXED_SETUP` and DRS zones |
| `results.rs` | results recorder |
| `booking.rs` | booking mode: `/SUB`, `/UNSUB`, `/CANJOIN`, listed slots |
| `usergroups.rs` | `UserGroups` files and both `blacklist.txt`s, re-read every 5 s |
| `admin_commands.rs`, `moderation.rs` | `EnableAdminCommands`; AFK, ping and checksum kicks |
| `scripts.rs`, `weatherfx.rs` | `CSPServerScripts` and `AS_ApiKey`; WeatherFX `0xAB 0x01` |
| `traffic.rs` | AI traffic: AI slots, spawner, obstacle loop, sending in `0x48`/`0xAB 0x03` |
| `live_config.rs` | `Live` settings (read through `Server::live()`), `/set`, `/api/configuration` |
| `plugins/` | AssettoServer's bundled plugins (`EnablePlugins`) |

## Conventions

- **Wire code goes through `src/wire`.** Never hand-roll little-endian math or
  string encodings. `wstring*` is UTF-32LE with a **character** count;
  `string*` is UTF-8 with a **byte** count. A new packet gets its id in
  `src/proto.rs`, plus a builder in `broadcast.rs` (S→C) or a handler in
  `tcp_handlers.rs` / `udp.rs` (C→S).
- **Packet builders stay flat:** one `w.x()` per field, in wire order, so they
  read against the doc tables.
- **Typed config.** Each setting has the type of the values it may take; a
  value that does not fit refuses to start. Prefer lossless conversions
  (`From`) to `as` at the wire.
- **Concurrency.** Each TCP connection has a read task and a write task on the
  Tokio runtime (`connection.rs`). `send_tcp` enqueues, so a broadcast never
  blocks on a slow socket. Two plain threads run the hot loops: `udp-receive`,
  which stores each accepted `0x46` and marks the car pending, and `relay`,
  which runs vanilla's main-loop pass every `SLEEP_TIME` ms (pending
  positions, then the per-car ping, sun and `0x57` timers) and, once per
  `CLIENT_SEND_INTERVAL_HZ` tick, re-ranks the optimizer and sends the
  far-car `0x48`s and the AI (`maybe_tick`). The per-receiver priorities and
  held far positions belong to the relay thread alone. `EntryCar` and
  `WeatherState` guard their state with locks, `Client` uses atomics. Put any
  new shared state behind a lock or atomic.
- **Vanilla faithfulness.** For a `[K*]`, implement the vanilla column and cite
  the doc section in a comment. CSP (`0xAB`) is handled in `csp.rs`, opt-in
  through `extra_cfg.yml`.
- **Extensions** are opt-in in `extra_cfg.yml`. `ExtraConfig::default()`
  enables vanilla behaviour plus position validation, the CSP basics and
  AssettoServer's position handling (network bubble, `BatchFarPositions`,
  `EnableCustomUpdate`; README "Positions"). Settings an admin may change at
  runtime are read through `Server::live()`, not `cfg`.
- **Every inbound handler:** ask what an unauthenticated peer could make it do an
  unbounded number of times.
- Match the surrounding comment density and naming.

## Verifying against the docs

The wire layouts in `broadcast.rs`, `handshake.rs`, `udp.rs` and `plugin.rs`
must match the tables in `docs/02`–`06` field for field, in order. When code
and doc disagree, one of them is a bug; work out which. For example, the
inbound `0x46` payload is 61 bytes (62 with its id), not 57. When behaviour
changes, update the doc (vanilla or AssettoServer behaviour) or
`IMPLEMENTATION.md` (ours), and `README.md` if an operator would notice.

**Binary research.** `kunosserverfiles/re/` (gitignored, local only) holds a
reverse-engineering pass over the vanilla Linux `acServer`: the function map
with original source lines, struct layouts, call graph, per-function strings,
goroutine and channel map, disassembly and Ghidra pseudocode, plus the scripts
that rebuild them. Start at its `README.md`. It is derived from Kunos's binary,
so never commit or quote it at length. Use it to find what to probe, then pin
the behaviour with a reference probe and describe it in the docs in our own
words.

## Behaviour notes

These are implemented and pinned by probes. Detail is in the docs.

- **Grid and restart** (docs/02 `0x4A`). The grid is the finishing order of the
  session that just ended (`SessionManager::set_grid`, called from `advance_from`
  before results are cleared). A restart clears results first, so the grid falls
  back to entry-list order, and drops the session's rubber
  (`WeatherState::reset_lap_grip`). With `REVERSED_GRID_RACE_POSITIONS`, a last
  race runs again on a reversed grid (`SessionManager::next_index`,
  `reverse_race_grid`); without `LOOP_MODE` (default on) the server shuts down
  after the last session.
- **Race end** (docs/02 "End of a race", `SessionManager::on_race_lap`). Vanilla's
  leader model: a lap race finishes a car at `LAPS`, a timed race the next
  leader crossing after `TIME` (one lap later with `RACE_EXTRA_LAP`); everyone
  else on their next lap after that. `RACE_OVER_TIME` counts from the first
  finish. A race with too few loaded cars is skipped (`race_abandoned`).
- **Leaderboard order** (docs/02 `0x49`) is stored (`Server::rank`, vanilla's
  `bestLapRank`) and re-sorted only on a counted lap, a new driver taking a slot
  and the session's end. The sort is stable; vanilla's unstable one can swap ties.
- **Slots** (docs/01 §1.5 "Which slot"). Each slot remembers its last driver
  (`EntryCar` `driver`): `identity()` returns it for an empty slot, so `0x5B`,
  `0x40`, `/JSON` and results keep naming it, and its guid reclaims the slot with
  its laps. A newcomer prefers an unused slot and clears the laps of the one it
  takes. The slot's last `0x50` goes into later joiners' snapshots. Every join
  restores the entry-list BoP (`EntryCar::reset_bop`), broadcasting `0x70` if it
  changed.
- **Readiness** (docs/01 §1.5). `Client::ready` is vanilla's `IsHandshakeDone`,
  set by the first UDP `0x46` or a `0x4C`. It gates every TCP broadcast except
  `0x4A`, the position relay, pings, the vote quorum and the `/INFO` and lobby
  client counts (`Server::ready_count`). `handshaked` gates only `0x4A`.
- **Session end** (docs/02 "End of a session"). Practice and qualifying wait up
  to `QUALIFY_MAX_WAIT_PERC` of the fastest lap once time is up, ending early
  when no ready car is both on its last lap and above 5 m/s. The next lap
  after time-up is a car's last; later laps are refused in every session type.
  Session index 0 resets grip; any other index blends `SESSION_TRANSFER`. The sun
  is computed from the session's start each pass (`sun_at`), so it sits below
  `SUN_ANGLE` in a race countdown, and clamps at ±80°. Every switch, restart
  included, zeroes the 60 s `0x54` timer (`Server::step_sun`), so a server up
  over a minute sends the new angle at once.
- **Kicks** all go through `admin_kick` (vanilla `KickUser` / `BanUser`): a kick
  sends `BLACKLIST_MODE` as its reason and applies the mode. That covers
  `/kick`, a passing vote, ACSP `0xCE` and a client-sent `0x3B`.
- **Votes** (docs/02 `0x64`–`0x66`). One vote at a time, matched by kind alone. The
  quorum is recomputed live from ready clients. A vote is decided only at `VOTE_DURATION` expiry, on
  yes > quorum. `LastVoter` moves only on a yes. A failed vote's `0x67` carries
  the wrapped countdown.
- **Ping** is the mean of the samples so far, the last 50 once full, with no
  ramp from zero (`Client::record_pong`, reference `UpdatePingAvg`). It is used
  for both the reported ping and the clock offset.
- **Entry list.** `MODEL=*N` wildcards resolve against `CARS=`
  (`config::solve_car_wildcard`, docs/08 §8.1). `FIXED_SETUP=name` loads
  `setups/name` verbatim (no `.ini` added); an empty value means
  `setups/<MODEL>.ini`. Every section is sent, `VALUE` or 0. DRS zones come
  from the track's `drs_zones.ini` (needs `--content`). After `0x70` the first
  update sends welcome, setup, then DRS, each only when present.
- **Official-guide options** all take effect: reserved/shared-GUID slots,
  `LOCKED_ENTRY_LIST`, `PICKUP_MODE_ENABLED=0`, `IS_OPEN` 0/1/2 (following the
  guide, not the binary's pickup quirk), `BLACKLIST_MODE`, `/ban_id` appending to
  `<cfg dir>/blacklist.txt`, `QUALIFY_MAX_WAIT_PERC`,
  `REVERSED_GRID_RACE_POSITIONS`, `TIME_OF_DAY_MULT` (the sun moves at vanilla's
  rate, 0 as 1, with a `0x54` every 60 s). Weather changes only at session
  boundaries. `NUM_THREADS` and `UDP_LOGGER` are accepted no-ops; `SLEEP_TIME`
  sets the relay pass;
  `SEND_BUFFER_SIZE` / `RECV_BUFFER_SIZE` size the game UDP socket.
- **`AUTH_PLUGIN_ADDRESS`**: `GET http://<addr>&GUID=<guid>&PSW=<pw>`. `OK` admits,
  `DENY[|reason]` rejects with `0x6F`, and a transport error fails **open**, as
  the reference does.
- **Comparison harness** pins temperature variation, wind and dynamic-track
  `RANDOMNESS`, because the two servers roll them independently.

## Known limitations

Honest gaps, not doc errors. Keep this list current.

- **Deliberate deviations from vanilla**, each pinned by a probe (full table in
  `test/COMPARE.md`): chat sender substitution, NaN `0x46` dropped, joins with no
  guid or a blank name rejected, a duplicate guid replacing the old connection,
  a repeated `0x3D` ignored, the `0x3F` page clamped, empty-slot `/kick_id` and
  kick votes refused, chat control characters (not tabs) stripped, a 1 s best-lap floor,
  `IS_OPEN=0` enforced in pickup mode, `/client_list` implemented, `404` for
  unrouted HTTP, a JSON content type plus CORS headers on HTTP replies,
  `/ENTRY` HTML-escaping driver names, and `/RECONFIG` answering with serde's
  error text. A config value that does not fit its setting refuses to start.
  Unprobed supersets: `collider.kn5` and a track-root `surfaces.ini` in the
  checksum list, and a per-IP cap on `0xF9` replies.
- **Rate and resource limits** (tabled in `README.md`) are hard-coded and unprobed,
  because observing them would take a flood.
- `SPECTATOR_MODE=1` is accepted, but vanilla refuses to start with it, so there
  is no reference behaviour. The game counts only the cars that are not
  spectators in `0x4A`, `0x49` and `0x4B`, so a spectator slot makes it misread
  them (`TestSpectatorSlot`).
- RCON and ACSP `AdminCommand` accept a superset of the chat commands
  (`kick` by id, `ban`, `say`).
- No Steam session-ticket validation.
- Positions go out as `0x46` datagrams, one per pending car per relay pass
  (`relay_loop`), as vanilla's main loop sends them, except to CSP clients with
  custom updates (`0xAB 0x03` batches) and for cars outside the network bubble
  (held for the tick's `0x48`). `SLEEP_TIME` below 1 runs as 1 ms, where vanilla
  spins. The optimizer is always on (`[BANDWIDTH_OPTIMIZER]` is ignored): tiers
  of 7 ranked once per tick, AI included, nothing thinned within 100 m.
- CSP is partial: no chat-encoded server messages (docs/04).
- With pickup mode off (booking or not) the slot array runs to `MAX_CLIENTS`
  with a `listed` flag and lock-protected entry-list fields per slot, so a
  session id stays its index; every car listing goes through `listed_cars()`.
  A loop back re-reads `entry_list.ini` (`Server::reload_entry_list`). Vanilla
  orphans connected clients there; ours keeps drivers whose slot still fits.

## AssettoServer features and licensing

The server carries the AssettoServer features (README "Configuration"):
custom updates, WeatherFX, CSP server scripts, AI traffic (`src/ai/`,
`src/server/traffic.rs`), user groups and the extra admin commands, the
automatic kicks, AssettoServer's bundled plugins (enabled by `EnablePlugins`),
and live configuration. Each is off by default in `extra_cfg.yml`, except
position handling, which no client notices: `NetworkBubbleDistance` defaults
to 500 m, as AssettoServer's does, so players further apart get each other a
tier below the optimizer's lowest, every fifth update, and `BatchFarPositions`
(default on) holds those cars to the next tick and sends them batched in
`0x48`s, in the AI's when there are any (the one visible change: a `0x48`
drops their `Gas` and `PerformanceDelta`); the bandwidth optimizer is always
on, ranks AI and players' cars as one list in tiers of 7 once per
`CLIENT_SEND_INTERVAL_HZ` tick (vanilla: every relay pass), and never thins a
car within 100 m; and `EnableCustomUpdate` defaults to on. These are design
decisions, tested end to end by `TestExt*` in `test/extensions_test.go`.

The repository is AGPL-3.0-or-later, the same licence as AssettoServer, so
AssettoServer code (plugins included) may be copied or translated directly.
Keep its copyright line ("Copyright (C) Niewiarowski, compujuckel") in a
comment at the top of any file that takes code from it. This does not cover
Kunos's binary: the `kunosserverfiles/re/` rule above still applies.
`THIRD_PARTY_LICENSES` ships in the release bundles and lists every crate
linked into `acserver-rs` on any release target. It is generated by cargo-about
(`rust/about.toml`, `rust/about.hbs`); after changing dependencies run
`cd rust && cargo about generate about.hbs -o ../THIRD_PARTY_LICENSES`. A pull
request fails while it is stale.

## Git

Commit directly to `main`. `cargo fmt --check`, `cargo clippy`, `cargo test`,
and, in `test/`, `go vet`, `gofmt` and `go test -race -timeout 30m ./...`
must all pass first.
