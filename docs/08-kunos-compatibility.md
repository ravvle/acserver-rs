# 8. Kunos Drop-in Compatibility

A checklist for building a direct replacement of the original Kunos `acServer`
(protocol 202): what to implement (§8.1), what to skip (§8.2), and every known
place where AssettoServer differs from vanilla (§8.3).

## 8.1 Must implement

**TCP (doc 2).** Everything except `0xAB`:

- Handshake `0x3D` → `0x3E`, and the rejections `0x3B` `0x3C` `0x42` `0x45` `0x6E` `0x6F`
- Checksums: the path list in `0x3E`, `0x44` verification, the deferred `0x68` kick (reason 3)
- Chat `0x47` and chat commands, car list `0x3F`/`0x40`, clean exit `0x43`
- State: `0x4A` `0x4B` `0x4D` `0x5A` `0x5B` `0x51` `0x52` `0x53` `0x54` `0x78`
- Gameplay: `0x0D` `0x0E` `0x49` `0x50` `0x56` `0x58` `0x82`
- Votes `0x64`–`0x67`, kick `0x68`, BoP `0x70`

**UDP (doc 3).** Association `0x4E`, lobby probe `0xC8`, session request `0x4F`,
position `0x46` in both directions, ping `0xF9`/`0xF8`, race start `0x57`.
Vanilla never sends `0x48`; AssettoServer sends it only for AI traffic
(doc 3 `0x48`).

**HTTP (doc 5).** `GET /INFO`, `GET /JSON|<guid>`, and the outbound lobby
registration and pings. The last is needed for public listing.

**Plugin API (doc 6).** All of ACSP v4, if plugins matter.

**Steam auth.** Validate the handshake's session ticket when `AUTH` is configured.

**Entry-list `MODEL` wildcards (`SolveCarWildcard`).** `[SERVER] CARS=` is the
ordered list of legal models. For each `CAR_n`:

1. A `MODEL` that is in `CARS` is kept.
2. `*N` resolves to the `N`-th `CARS` entry (1-based). An out-of-range `N` (`*0`,
   `*99`) picks a **random** entry. A non-numeric suffix, including a bare `*`,
   logs `Error converting` and resolves to `""`.
3. Anything else is kept as written.
4. If the result is not in `CARS`, vanilla prints
   `Error, entry list CAR_%d car %s is illegal` and exits. An empty `CARS=` is
   therefore fatal.

## 8.2 Safe to omit

| Item | Origin | Doc |
|---|---|---|
| Packet `0xAB`, all subtypes | CSP | 2–4 |
| CSP handshake, client messages, Lua events, key hashing | CSP | 4 |
| `$CSP0:` chat encoding and welcome-message extra options | CSP | 4 §4.5 |
| Custom position format (`CUSTOM_UPDATE`) | CSP | 4 §4.2 |
| CSP feature checks in the handshake | CSP | 1 §1.5 |
| `SURFACE_0` → `CSP` checksum rewrite | CSP | 1 §1.5 |
| `AS_*` Lua events | AS | 4 §4.4 |
| `/api/details` and the `ℹ<port>` name suffix | CM | 5 §5.3 |
| `/content/*` downloads | CM | 5 §5.4 |
| `/api/scripts`, `/assets`, `/api/configuration` | AS | 5 §5.5–5.7 |
| `poweredBy` in `/INFO` | AS | 5 §5.1 |
| RCON | AS | 7 |
| AC Pro name-derived guids | AS | 1 §1.5 |
| Alternative `data*.acd` checksums | AS | 1 §1.5 |
| AI cars and car-visibility hiding | AS | 9 §9.5, 4 §4.2 |
| WeatherFX, automatic kicks, user groups, live configuration, plugins | AS | 9 |
| Config switches that disable voting | AS | 2 §2.1 |

## 8.3 AssettoServer vs vanilla (`[K*]`)

For each row, a faithful replacement implements the **vanilla** column. Unless
stated otherwise, vanilla behaviour was measured against acServer v1.15.

### Joining

| Area | AssettoServer | Vanilla |
|---|---|---|
| Booking mode (type 0) | not supported | supported |
| `0x41` ServerRunning | never sent | addressed to a booked driver who joins during the booking session, but over UDP to a TCP packet's address, so it never arrives (docs/05 §5.2a) |
| Zero Steam guid | `0x6F` | admitted |
| Blank driver name | `0x6F` | admitted |
| Join naming no car | any free slot | `0x45` |
| Driver name length | truncated to 25 | kept as sent |
| `IS_OPEN=0`, pickup on | `0x6E` | admitted; `IS_OPEN` only applies with pickup off |
| `blacklist.txt` | not read | read from the working directory; `cfg/` copy ignored |
| `SPECTATOR_MODE=1` | accepted | `panic: Spectator mode not supported` |
| `MAX_CONTACTS_PER_KM` | passed through | missing/`0` → 5, negative → 0, > 255 fatal |

### First update

| Area | AssettoServer | Vanilla |
|---|---|---|
| Trigger | held until the checksum arrives | always fires; a bad checksum means a deferred kick |
| Snapshot | `0x51` `0x78` `0x54` `0x5B`, per-car `0x0E`/`0x0D`/`0x70`, `0x52` `0x53` `0x49` | `0x5B` `0x78` `0x49`, per-**slot** `0x0D`/`0x0E`, one `0x70`, [`0x53`] [`0x52`] [`0x51`] (doc 1 §1.5) |
| `0x5B` | occupied slots only | every slot, blank when free |
| `0x70` | `Count=1`, one packet per car | one packet, every slot |

### Relays and gameplay

| Area | AssettoServer | Vanilla |
|---|---|---|
| `0x46` validation | drops NaN / out-of-range | relays anything |
| `0x47` sender id | real sender | as written by the client |
| Chat rate limit | 1/s | none |
| `0x0D` P2P count | server tally, decremented | client's value relayed; fresh slot `-1` |
| `0x50` tyres | echoed to sender | others only |
| Lap with cuts | counts toward `NumLaps` | does not count outside a race; still broadcast, still adds grip |
| `NumLaps` | client's `NumLap` | the server's own count |
| Dynamic track | static at `SESSION_START` | `+1% / LAP_GAIN` per lap, cuts included |
| `0x49`/`0x4B` entries | live clients only, `0` for no lap | every slot, `999999999` for no lap (race total still `0`) |
| `[WEATHER_n]` temps | — | `BASE_TEMPERATURE_ROAD` is a delta on ambient; `VARIATION_*` randomises both |
| `0x3F` bad page | clamped | reads past the array |
| Timeouts, rate limits | doc 1 §1.5 values | none at all |

### Sessions and votes

| Area | AssettoServer | Vanilla |
|---|---|---|
| Session change | `0x4A` only | `0x4A` then `0x78` |
| End of practice/qualify | advances at expiry | `0x4B`, then `0x4A` after `max(RESULT_SCREEN_TIME, 10)` s |
| Voting | can be disabled | always on |

### Admin commands

| Area | AssettoServer | Vanilla |
|---|---|---|
| Command set | `/help` `/list` `/ban` `/say`; `/kick` by id | `/kick` by **name**, `/kick_id` `/ban_id` by id; different replies (doc 2 §2.1) |
| `/client_list` | — | advertised, but answers `not recognized` |
| Kick sequence | `0x68` + `0x4D` | chat announcement, `0x68`, private confirmation; no `0x4D` |
| Kick reasons | 4 kick, 1 ban | 0 kick, 2 ban |
| `/ballast`, `/restrictor` | `0x70` for that car | `0x70` for every slot |
| Non-admin runs admin command | "Unknown command" | `You are not admin on this server` |

### HTTP and lobby

| Area | AssettoServer | Vanilla |
|---|---|---|
| Unrouted path | `404` | `200`, empty; `/INFO` matched by prefix |
| `/JSON` | adds `Features`; `null` strings; no `DriverNation`; `IsRequestedGUID` = "is you" | `Cars` only; `""` strings; `DriverNation`; `IsRequestedGUID` = "reserved for you" |
| `/INFO` `durations` | seconds | minutes (the lobby registration uses seconds) |
| `/INFO` `session` | current type | current index |
| `/INFO` `country` without GeoIP | `["", "na"]` | `["na", "na"]` |
| Lobby `max_clients` | `MAX_CLIENTS` | `min(MAX_CLIENTS, entry-list size)` |
| Grid | whole entry list | the first `MAX_CLIENTS` entries; the rest appear nowhere (measured) |
| Lobby ping | one shape, long interval | 60 s keepalive + session update after register and at each session change |

### Plugin API and results

| Area | AssettoServer | Vanilla |
|---|---|---|
| `0x37` EndSession | empty filename | only when a results file was written, its path as a `wstring8` |
| `0x35` CarUpdate | 1000 ms default | only after `0xC8`; interval 0 stops it |
| `0x49` LapCompleted | game leaderboard relayed | published layout differs; verify against real plugins |
| `0xD1` AdminCommand | ignored | executed |
| `results/*.json` | not written | written after each session with a lap, as `results/<Y>_<M>_<D>_<h>_<m>_<TYPE>.json`. Every grid slot in `Cars` and `Result` (empty ones too, `BestLap` `999999999`), `GuidsList` `[""]`, `TotalTime` the session time at the last counted lap, lap `Timestamp` on the server clock, top-level `Events` `null` |

## 8.4 Practical notes

- **Vanilla clients** never send `0xAB` or `Features`, so a `[K]`-only server
  covers them fully.
- **CSP clients** also work against a `[K]`-only server. CSP features switch on
  only when the server answers them, so the player loses WeatherFX control,
  client messages and teleports, but nothing needed to race.
- **Content Manager** joins and browses via `/INFO` and `/JSON`. Only its
  server-details view and content downloads need the `[CM]` endpoints.
- **INI parsing.** Vanilla reads `server_cfg.ini` and `entry_list.ini` with its
  own parser, `ks/ini`. Measured against v1.15:
  - `//` starts a comment anywhere on a line, including inside a URL:
    `NAME=Join http://x` reads as `Join http:`. Inline `;` and `#` are **not**
    comments. A line starting with `;`, `#` or `//` is ignored.
  - Keys are case-sensitive and must be written exactly (`NAME = x` or an
    indented key is not found). A duplicate key takes the last value.
  - Text values are kept verbatim, surrounding spaces and quotes included.
    Numbers are trimmed; a missing or invalid number reads as 0.
  - A missing or empty `NAME` is `Assetto Corsa Server`.
  - A UTF-8 BOM or `[ SERVER ]` stops vanilla from starting.
