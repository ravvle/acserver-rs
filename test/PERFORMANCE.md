# Performance vs the original binary

The performance tests run the same workload against the original Kunos
`acServer`, our server (the release build of `rust/`, or `ACSERVER_BIN`)
and, with `ASSETTOSERVER_DIR`, AssettoServer, each as a separate process,
one after another, and measure only the server. They are opt-in because each
takes many minutes.

`ASSETTOSERVER_DIR=<install>` is the folder holding the `AssettoServer`
binary. It runs with its own defaults, plus the two
`IgnoreConfigurationErrors` flags (`MissingCarChecksums`,
`MissingTrackParams`) and the `ADMIN_PASSWORD` it needs to start on the
generated config. At startup it looks up the host's public IP and downloads
`data_track_params.ini`.

| Variable | Effect |
|---|---|
| `PERF_TARGETS` | Comma-separated labels to run: `vanilla`, `ours`, `assettoserver` |
| `PERF_LOG_DIR` | Keeps each server's log there |

Every run reports the packets received by id (`tcp_by_id`, `udp_by_id`),
and the server's bandwidth (`peak_out_mbps`, `avg_out_mbps`, `peak_in_mbps`,
`avg_in_mbps`): the bytes its clients receive and send, sampled every second,
with 28 bytes of IPv4 and UDP header added per datagram and TCP counted as
payload plus its 2-byte length prefix. The peak is the busiest second.
**CPU** is user + system time from `/proc/<pid>/stat`; 100% means one full
core. **Peak RSS** is sampled from `/proc/<pid>/status` every 100 ms. Every
client drains its UDP socket and answers each `0xF9` with a `0xF8` as a real
client does (AssettoServer drops a client after 15 s without one).

## Freeroam traffic (`TestPerformanceSRP`)

A server in the style of the Shutoko Revival Project. It runs our server,
the original acServer (players only, since it has no AI traffic) when a
reference install is found, and ASSETTOSERVER_DIR; `PERF_TARGETS` (`vanilla`,
`ours`, `assettoserver`) picks among them:

```sh
cd test && RUN_PERF_SRP=1 ASSETTOSERVER_DIR=/path/to/assettoserver go test . \
  -run '^TestPerformanceSRP$' -v -count=1 -timeout=30m
```

- A 16 km highway loop with an AI spline (one version -1 `fast_lane.ai`, read
  by both servers).
- 120 players (`SRP_PLAYERS`) joining over 30 s, driving the loop at 80–120
  km/h in convoys of 1 to 6, the convoys spread evenly round it, so each
  player has a few cars close by and most far away. They are vanilla clients
  (no CSP).
- 60 `AI=fixed` traffic slots (`SRP_AI_SLOTS`), up to 300 AI cars
  (`SRP_MAX_AI`), 10 per player, under the same `extra_cfg.yml` for both.
- 4 minutes (`SRP_DURATION`); the steady CPU figure starts 45 s in, once the
  AI has spawned. Every player must join and stay connected.

## Race simulation (`TestPerformanceRaceSim`)

A realistic club race at three field sizes, 16, 32 and 64 players, with the
same targets as `TestPerformanceSRP`. Each run takes about 5 minutes, so all
three fields on all three servers take about 50:

```sh
cd test && RUN_PERF_RACESIM=1 ASSETTOSERVER_DIR=/path/to/assettoserver go test . \
  -run '^TestPerformanceRaceSim$' -v -count=1 -timeout=90m
```

`-run '^TestPerformanceRaceSim$/^64$'` runs one field; `RACESIM_PLAYERS=<n>`
runs a field of any other size in place of the three.

- A 2.8 km circuit (start straight, hairpin, sweepers, tight infield), with a
  speed profile from cornering, braking and acceleration limits: a clean lap
  is 74 s.
- The players join in the first 20 s and sit on a
  two-wide grid until the green light (`WAIT_TIME=40`). They race 3 laps
  (`RACESIM_LAPS`), each up to 3.5% off the fastest pace, sending sector
  splits and laps with their real times. A few cars spin and stop for 8–20 s,
  so they drop to the back or get lapped. Finishers do a slow cool-down lap.
- The drive is precomputed and seeded, so every server gets the same race.
  The field runs nose to tail (under 100 m), strings out (100–500 m) and
  spreads across the circuit (over 500 m). The harness tracks how long each
  pair of cars spends in each band, and what rate of the other's positions
  each car received there (`bands`).
- Measures the same as `TestPerformanceSRP`; the race CPU figure runs from
  the green light to the end of the cool-down.

## Results

Full test and performance run on 2026-11-04 at commit `e35bb01` (clean
tree). Every performance test ran **vanilla Kunos acServer v1.15, the Rust
server and AssettoServer 0.0.54**, one target after another, never in
parallel.

**Host:** a 4-core / 8-thread laptop CPU on Linux. Servers and the simulated clients share this one machine,
so the clients' own CPU competes with the server's (see Caveats). Rust nightly
from `rust/rust-toolchain.toml` (release build); the test harness on Go
1.27.3.

**How to read the columns**

- *CPU %*: server process user+system time (`/proc/<pid>/stat`) divided by the
  run's wall time; 100% is one full core.
- *Peak RSS*: highest resident memory sampled every 100 ms.
- *UDP / TCP packets*: datagrams / frames the simulated clients received. *Total*
  is their sum. *UDP MiB* is the UDP payload received, without headers.
- *Upload* is server to clients, *download* is clients to server, in Mbit/s
  with 28 B of IPv4+UDP header per datagram. It is sampled each second; peak
  is the busiest second.
- *µs CPU / datagram* is server CPU time divided by UDP datagrams delivered.

### Functional tests

| Suite | Command | Result | Wall time |
|---|---|---|---:|
| Rust fmt, clippy | `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` | clean | <1 s (cached build) |
| Rust tests | `cargo test` | pass: 127 tests (93 unit, 34 integration), 0 failed | 145 s |
| Go vet, gofmt | `go vet ./...`, `gofmt -l .` | clean | 1 s |
| Go suite, reference comparison included | `go test -race -timeout 30m ./...` with `AC_REFERENCE_*` set | pass | 648 s |
| Multi-process grid race | `LOADTEST_CLIENTS=255 LOADTEST_PROCS=5 go test -run TestMultiProcessGridRace .` | pass: 255 clients joined, 255 saw `RaceOver` (join 34–40 ms for three processes, ~1.1 s for two; `RaceOver` 31–37 ms) | 20 s |
| AI on a real spline | `AI_SPLINE_AIP=<SRP fast_lane.aip> AI_SPLINE_TRACK=shutoko_revival_project_094_ptb1 go test -run TestExtAiOnRealSpline .` | pass: 7 AI cars drove the spline | 11 s |

There are no Rust benches in the repo.

### Freeroam traffic, 120 players + 60 AI slots (`TestPerformanceSRP`)

`RUN_PERF_SRP=1`, all three targets, 240 s each. A 16 km highway loop with an AI spline;
120 players join over 30 s and drive at 80–120 km/h in convoys; Rust and
AssettoServer run 60 `AI=fixed` slots (up to 300 AI cars). Vanilla has no AI
traffic, so it serves the 120 players alone. Steady CPU starts 45 s in.
All 120 players joined and none dropped, on every server.

| Server | Steady CPU | Overall CPU % | CPU time | Peak RSS | UDP pkts | TCP pkts | **Total pkts** | UDP MiB | of which `0x48` | Positions delivered (players / AI) | Peak up | Avg up | Peak down | Avg down | µs CPU / datagram |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| Vanilla (no AI) | 110.1% | 103.2% | 247.8 s | 9.6 MiB | 28,875,894 | 51,145 | 28,927,039 | 1,679 | 0 | 28,850,631 / 0 | 116.8 | 85.7 | 1.59 | 1.48 | 8.58 |
| Rust | 17.9% | 16.7% | 40.1 s | 19.1 MiB | 4,104,565 | 52,653 | 4,157,218 | 1,182 | 1,148,912 | 13,289,856 / 8,081,401 | 50.9 | 45.2 | 1.60 | 1.48 | 9.76 |
| AssettoServer | 120.0% | 117.0% | 280.7 s | 181.0 MiB | 15,492,615 | 63,408 | 15,556,023 | 1,526 | 812,353 | 14,654,437 / 12,276,213 | 80.1 | 67.9 | 1.60 | 1.48 | 18.12 |

Steady-state window: Vanilla 24,873,147 datagrams / 24,851,204 car entries, Rust 3,702,175 datagrams / 18,928,886 car entries, AssettoServer 13,868,098 datagrams / 23,948,501 car entries.

UDP packets by id: **Vanilla** `46`:28,850,631, `57`:36, `f9`:25,227; **Rust** `46`:2,928,717, `48`:1,148,912, `57`:36, `f9`:26,900; **AssettoServer** `46`:14,654,437, `48`:812,353, `f9`:25,825.

Upload/download in Mbit/s. µs CPU / datagram is not comparable across
servers here: Rust batches many car positions (and the AI's) into each
`0x48`, so a datagram carries more work.

- **CPU:** Rust 18% of a core, against vanilla 110% and AssettoServer 120%.
  Vanilla and AssettoServer each need more than one core for this load.
- **Bandwidth:** peak upload vanilla 117 Mbit/s with no AI, AssettoServer 80,
  Rust 51. Batching far cars (and the AI) into `0x48` saves per-datagram
  headers.
- **Memory:** AssettoServer 181 MiB, Rust 19, vanilla 10.

### Club race, 16, 32 and 64 players (`TestPerformanceRaceSim`)

`RUN_PERF_RACESIM=1`, all three targets. A 2,835 m circuit (clean lap 74.1
s), a two-wide grid, 3 laps, seeded so every server gets the same race. Each
run is about 305 s (313 s at 64 players); race CPU runs from the green light
to the end of the cool-down (265–273 s). Every player joined and none
dropped, on every server. Upload/download in Mbit/s.

#### 16 players

| Server | Race CPU | Overall CPU % | CPU time | Peak RSS | UDP pkts | TCP pkts | **Total pkts** | UDP MiB | of which `0x48` | Positions delivered | Peak up | Avg up | Peak down | Avg down | µs CPU / datagram | Lap broadcasts | `RaceOver` seen |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| Vanilla | 9.7% | 9.5% | 29.2 s | 8.9 MiB | 1,260,038 | 2,999 | 1,263,037 | 73 | 0 | 1,254,828 | 3.2 | 2.9 | 0.21 | 0.20 | 23.17 | 784 | 16 |
| Rust | 2.0% | 2.0% | 6.0 s | 10.9 MiB | 1,126,730 | 2,999 | 1,129,729 | 65 | 969 | 1,123,599 | 3.1 | 2.6 | 0.21 | 0.20 | 5.36 | 784 | 16 |
| AssettoServer | 3.7% | 4.2% | 12.9 s | 140.5 MiB | 1,229,274 | 5,927 | 1,235,201 | 71 | 0 | 1,224,765 | 3.2 | 2.9 | 0.21 | 0.20 | 10.49 | 784 | 0 |

UDP packets by id: **Vanilla** `46`:1,254,828, `57`:519, `f9`:4,691; **Rust** `46`:1,120,534, `48`:969, `57`:520, `f9`:4,707; **AssettoServer** `46`:1,224,765, `f9`:4,509.

Positions each car received of each other car, by distance (share of
pair-time in brackets):

| Distance | Vanilla | Rust | AssettoServer |
|---|---:|---:|---:|
| Under 100 m (78.0%) | 18.1 Hz | 18.1 Hz | 18.1 Hz |
| 100–500 m (18.9%) | 18.1 Hz | 9.6 Hz | 18.1 Hz |
| Over 500 m (3.2%) | 18.1 Hz | 3.6 Hz | 3.7 Hz |

#### 32 players

| Server | Race CPU | Overall CPU % | CPU time | Peak RSS | UDP pkts | TCP pkts | **Total pkts** | UDP MiB | of which `0x48` | Positions delivered | Peak up | Avg up | Peak down | Avg down | µs CPU / datagram | Lap broadcasts | `RaceOver` seen |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| Vanilla | 21.9% | 21.1% | 64.4 s | 8.9 MiB | 5,181,268 | 11,885 | 5,193,153 | 301 | 0 | 5,170,867 | 13.5 | 12.1 | 0.42 | 0.41 | 12.43 | 3,104 | 32 |
| Rust | 4.0% | 3.9% | 11.9 s | 11.3 MiB | 4,379,932 | 11,885 | 4,391,817 | 255 | 2,325 | 4,375,903 | 12.8 | 10.3 | 0.43 | 0.41 | 2.72 | 3,104 | 32 |
| AssettoServer | 10.3% | 10.5% | 32.1 s | 142.9 MiB | 5,087,912 | 14,137 | 5,102,049 | 296 | 0 | 5,078,919 | 13.9 | 11.9 | 0.43 | 0.41 | 6.32 | 3,104 | 0 |

UDP packets by id: **Vanilla** `46`:5,170,867, `57`:1,049, `f9`:9,352; **Rust** `46`:4,367,170, `48`:2,325, `57`:1,050, `f9`:9,387; **AssettoServer** `46`:5,078,919, `f9`:8,993.

Positions each car received of each other car, by distance (share of
pair-time in brackets):

| Distance | Vanilla | Rust | AssettoServer |
|---|---:|---:|---:|
| Under 100 m (75.8%) | 18.1 Hz | 18.1 Hz | 18.0 Hz |
| 100–500 m (22.5%) | 18.1 Hz | 5.9 Hz | 18.0 Hz |
| Over 500 m (1.7%) | 18.1 Hz | 3.6 Hz | 3.7 Hz |

#### 64 players

| Server | Race CPU | Overall CPU % | CPU time | Peak RSS | UDP pkts | TCP pkts | **Total pkts** | UDP MiB | of which `0x48` | Positions delivered | Peak up | Avg up | Peak down | Avg down | µs CPU / datagram | Lap broadcasts | `RaceOver` seen |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| Vanilla | 65.2% | 62.4% | 195.1 s | 9.3 MiB | 21,613,867 | 47,317 | 21,661,184 | 1,256 | 0 | 21,592,608 | 54.9 | 49.4 | 0.85 | 0.82 | 9.02 | 12,352 | 64 |
| Rust | 21.0% | 20.1% | 63.0 s | 12.0 MiB | 14,941,808 | 47,319 | 14,989,127 | 871 | 14,948 | 14,976,400 | 50.6 | 34.3 | 0.85 | 0.82 | 4.21 | 12,352 | 64 |
| AssettoServer | 25.0% | 24.7% | 77.3 s | 149.7 MiB | 21,006,890 | 37,982 | 21,044,872 | 1,221 | 0 | 20,988,437 | 55.4 | 48.0 | 0.85 | 0.82 | 3.68 | 12,352 | 0 |

UDP packets by id: **Vanilla** `46`:21,592,608, `57`:2,075, `f9`:19,184; **Rust** `46`:14,905,527, `48`:14,948, `57`:2,072, `f9`:19,261; **AssettoServer** `46`:20,988,437, `f9`:18,453.

Positions each car received of each other car, by distance (share of
pair-time in brackets):

| Distance | Vanilla | Rust | AssettoServer |
|---|---:|---:|---:|
| Under 100 m (58.1%) | 18.0 Hz | 18.0 Hz | 17.9 Hz |
| 100–500 m (39.2%) | 18.0 Hz | 4.9 Hz | 17.9 Hz |
| Over 500 m (2.7%) | 18.1 Hz | 3.6 Hz | 3.7 Hz |

At 64 players Rust's clients also got two `0x4D` (CarDisconnected), from
clients that left first at the end of the run; the TCP traffic is otherwise
the same packets as vanilla's.

- **CPU** (race): Rust 2.0% / 4.0% / 21.0% at 16 / 32 / 64 players, vanilla
  9.7% / 21.9% / 65.2%, AssettoServer 3.7% / 10.3% / 25.0%. Rust is lowest
  at every size. At 64 the simulated clients compete for the same 8 threads
  (see Caveats), so its jump from 32 is partly the host's.
- **Rust sends 11% / 15% / 31% fewer datagrams than vanilla** at 16 / 32 /
  64 players, and as much less data, by thinning cars 100 m and further away.
  The bigger the field, the more cars fall into its lower tiers: the 100–500 m
  rate drops from 9.6 Hz to 5.9 and 4.9.
- **Vanilla never thins** in any field: its optimizer needs
  `[BANDWIDTH_OPTIMIZER]` `ENABLED=1`, and this config does not set it.
  AssettoServer thins only beyond its 500 m bubble (3.7 Hz).
- **Memory:** vanilla 9 MiB, Rust 11–12, AssettoServer 141–150.
- **`RaceOver`:** AssettoServer never sends it, so the count is 0:
  AssettoServer 0.0.54 never ends a race. `ACServer.IsSessionOver` returns
  false for every race session (only practice and qualifying end, on time),
  and the finish it records per lap (`HasCompletedLastLap`, `EndTime`) is
  never read. AssettoServer also sends `0x54` (sun angle) every second.

### Caveats

- One run per scenario: no repeats, so no error bars. Compare servers within one
  table, not across runs; this host's load varies between sessions.
- The simulated clients run on the same 8 threads as the server, so their CPU
  competes with the server's. The harness's own load grows with the square of
  the field.
- CPU and RSS come from the server process only; the client harness and
  AssettoServer's startup network lookups are not counted.
- The host suspended itself partway through the first 64-player Rust run, so
  that run was discarded; Rust and AssettoServer's 64-player runs were done
  again under `systemd-inhibit --what=sleep:idle`. On a laptop, wrap any long
  run in it.
- Raw logs for the run (per-server logs, JSON result lines) were kept outside the
  repo; the JSON lines are what the tables above were generated from.
