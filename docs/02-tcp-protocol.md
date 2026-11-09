# 2. TCP Game Protocol

Every packet travels in the TCP frame from doc 1 §1.3. Field tables list the
payload **after** the packet-id byte, in wire order. Types are defined in doc 1
§1.2. Everything is `[K]` unless tagged. "Measured" means captured from vanilla
acServer v1.15.

## 2.1 Client → Server

### 0x3D — HandshakeRequest

The first packet on every connection. Validation is in doc 1 §1.5.

| Field | Type | Notes |
|---|---|---|
| ClientVersion | `u16` | must be `202` |
| Guid | `string8` | SteamID64 as a decimal string |
| Name | `wstring8` | driver name |
| Team | `string8` | |
| Nation | `string8` | 3-letter code |
| RequestedCar | `string8` | car model folder name |
| Password | `string8` | server or admin password; may be empty |
| Features | `string16` | `[CSP]` optional, present only if ≥ 2 bytes remain. Comma-separated feature flags ending in the CSP build number, e.g. `SPECTATING_AWARE,CUSTOM_UPDATE,2144`. Vanilla ignores it |
| SessionTicketLength | `i16` | optional, read only if ≥ 2 more bytes remain |
| SessionTicket | bytes | Steam session ticket; read only if the length exactly matches the remaining bytes |

### 0x43 — CleanExitDrive

No payload.

### 0x44 — Checksum

| Field | Type | Notes |
|---|---|---|
| Checksum | bytes | rest of packet: `16 × (N + 1)` bytes, one MD5 per path from `0x3E`, then the car's `data.acd` (doc 1 §1.5) |

### 0x47 — Chat

| Field | Type | Notes |
|---|---|---|
| SessionId | `u8` | `[K*]` vanilla relays whatever id the client wrote here, so anyone can speak as anyone. AssettoServer substitutes the real sender |
| Message | `wstring8` | |

A message starting with `/` is a chat command (below) and is not relayed.
Anything else goes to everyone as `0x47` (§2.2). Vanilla relays every message,
including whitespace-only lines and bursts. `[AS]` AssettoServer drops blank
messages and rate-limits each client to one message per second.

#### Chat commands

Commands, argument shapes and reply strings as measured. Replies are `0x47` from
session id `255`, sent only to the caller unless noted.

| Command | Who | Reply |
|---|---|---|
| `/help` | anyone | `/admin /next_session /restart_session /kick /client_list /kick_id /ban_id /ballast /restrictor` |
| `/admin <pw>` | anyone | `You are now Admin for this server` or `Command refused` |
| `/next_session` | admin | `OK. Moving to next session` |
| `/restart_session` | admin | `OK. Restarting session` |
| `/kick <driverName>` | admin | kick sequence (below); unknown → `ERROR: Cannot find driver named <name>` |
| `/kick_id <sessionId>` | admin | kick sequence; unknown → `ERROR: Cannot find driver id <id>` |
| `/ban_id <sessionId>` | admin | kick sequence with `banned` / `BANNED` wording |
| `/ballast <sessionId> <kg>` | admin | `OK, Ballast set to 50.0 Kg for <name> [<team>]` |
| `/restrictor <sessionId> <pct>` | admin | `OK, Restrictor set to 20.0 for <name> [<team>]` |
| `/client_list` | admin | `Command:/client_list not recognized` — advertised but never implemented |
| anything else | anyone | `Command:<whole message> not recognized` |
| admin command, not admin | — | `You are not admin on this server` |

While any client is still loading, `/next_session` and `/restart_session` refuse
with `Can't skip session. New player connection is in process` or `Can't restart
session. New player connection is in process`.

Things that are easy to get wrong:

- **`/kick` takes a driver *name*.** `/kick 5` kicks the driver named "5". Use
  `/kick_id` for a session id.
- **Matching is exact.** `/HELP` is not `/help`.
- **The kick sequence is three packets:** a chat broadcast to everyone
  (`<name> kicked from the server` or `<name> banned from the server`), the
  `0x68 KickCar`, and then a private `OK. Kicking <name>` or `OK. <name> BANNED`
  to the admin. There is **no `0x4D`**.
- **Kick reasons.** `/kick` and `/kick_id` send `BLACKLIST_MODE` itself as the
  reason (`0`, `1` or `2`), and the chat still says `kicked`. Mode 1 also
  blacklists the guid until restart, and mode 2 appends it to `blacklist.txt` as
  well. `/ban_id` always sends `2`, says `banned` and appends to
  `blacklist.txt`. The same kick (vanilla `KickUser`) serves a passing kick vote,
  ACSP `0xCE` and a client-sent `0x3B`.
- **`/kick_id` does not check the slot is occupied.** An empty slot is "kicked"
  with a blank name and a `0x68` for a car nobody drives.
- **`/ballast` and `/restrictor` errors:**
  - wrong shape (missing, extra or non-numeric argument):
    `SYNTAX ERROR: Use 'ballast [driver numeric id] [kg]'`, or
    `… 'restrictor [driver numeric id] [0-100]'`
  - out of range: the value is **refused, not clamped**, and this is checked
    before the car lookup:
    - `ERROR: Ballasts cannot be negative'`
    - `ERROR: Ballasts cannot be higher than %fKg'` (the cap is `MAX_BALLAST_KG`)
    - `ERROR: Restrictor cannot be negative'`
    - `ERROR: Restrictors cannot be higher than 100'`

    Copy these exactly, stray trailing quote included.
  - unknown car: `ERROR: Cannot find car to add ballast` (`restrictor` uses its
    own noun).
- **A successful `/ballast` or `/restrictor`** broadcasts one `0x70` covering
  **every** slot.

AssettoServer's command set differs (doc 8 §8.3).

### 0x3F — CarListRequest

| Field | Type | Notes |
|---|---|---|
| PageIndex | `u8` | first entry-list index; the reply holds up to 10 cars |

`[K*]` Vanilla does not bounds-check this. Page 200 on a 3-car list reads past
its array and returns `count=59` with junk rows. Clamp it to an empty page.

### 0x0D — P2PUpdate (push-to-pass)

| Field | Type | Notes |
|---|---|---|
| P2PCount | `i16` | `-1` asks for the current count, which is answered to the sender only. Any other value is an activation |
| Active | `bool` | |

An activation is broadcast as `0x0D` (§2.2) carrying the count the client
reported. `[AS]` AssettoServer keeps its own tally and decrements it instead.

### 0x0E — MandatoryPitRequest

| Field | Type | Notes |
|---|---|---|
| MandatoryPit | `bool` | pit stop done; broadcast to all |

### 0x50 — TyreCompoundChangeRequest

| Field | Type |
|---|---|
| CompoundName | `string8` |

### 0x56 — DamageUpdate

| Field | Type |
|---|---|
| DamageZoneLevel | `damage` |

Relayed to the others with the sender's id prepended, and stored. It shows up in
`0x40`.

### 0x58 — SectorSplit

| Field | Type | Notes |
|---|---|---|
| SplitIndex | `u8` | |
| SplitTime | `u32` | ms |
| Cuts | `u8` | |

Forwarded unprocessed to every other car, with the sender's id prepended.
Vanilla does not echo it to the sender.

### 0x49 — LapCompleted

| Field | Type | Notes |
|---|---|---|
| Timestamp | `u32` | |
| LapTime | `u32` | ms |
| SplitCount | `u8` | |
| Splits | `SplitCount × u32` | sector times, ms |
| Cuts | `u8` | |
| NumLap | `u8` | |

The server records the lap and broadcasts the leaderboard (§2.2 `0x49`).

- **The server counts laps itself.** `NumLap` is ignored: a client that says
  "lap 1" every time still gets 1, 2, 3 (measured).
- A lap with `Cuts > 0` does **not** count towards `NumLaps` and cannot be the
  best lap, except in a race, where every lap counts. The leaderboard is still
  broadcast.
- Every lap, including cut ones, advances the dynamic track by
  `1% / LAP_GAIN`. With `SESSION_START=96` and `LAP_GAIN=30`, grip goes from
  `0.96` to `0.960333`.
- A lap whose `Timestamp` does not advance past the car's previous lap is dropped.
- In a race, a lap completed before the start is dropped without a `0x49`. A
  car that joins after the start races normally.

### 0x64 — VoteNextSession / 0x65 — VoteRestartSession

| Field | Type |
|---|---|
| Vote | `bool` |

### 0x66 — VoteKickUser

| Field | Type |
|---|---|
| TargetSessionId | `u8` |
| Vote | `bool` |

Voting rules are under §2.2 `0x64`–`0x66`. `[AS]` AssettoServer can disable either
kind of vote in its config; vanilla always processes them.

### 0x82 — ClientEvent

| Field | Type |
|---|---|
| Count | `i16` |

Then `Count` events:

| Field | Type | Present when |
|---|---|---|
| Type | `u8` | always: `0x0A` car collision, `0x0B` environment collision, `0x0C` jump start |
| TargetSessionId | `u8` | `0x0A` |
| Speed | `f32` | `0x0A`, `0x0B` (relative, km/h) |
| Position | `vec3` | `0x0A`, `0x0B` (world) |
| RelPosition | `vec3` | `0x0A`, `0x0B` (relative to car) |

Collisions go only to the plugin (doc 6), never to other clients, and may arrive
up to ~5 s late. A jump start is re-broadcast as `0x82` (§2.2).

The count is required for a jump start too. A bare `0x82 0x0C` is ignored:
nothing is relayed and the connection stays up (measured on v1.15). The game
itself sends its jump start bare, without the count (game client code as
ported by rustyAC), so with vanilla a real client's jump start is never
re-broadcast and never reaches the plugin. Only a client that writes the
count gets the §2.2 `0x82`.

### 0xAB — Extended `[CSP]`

Only CSP clients send this, and a Kunos server can ignore it. Subtypes: `0x00`
SpectateCar (`u8 SessionId`, `u8 CameraMode`; own id or out-of-range clears it),
`0x03` ClientMessage. See doc 4.

## 2.2 Server → Client

### Handshake rejections

Sent instead of `0x3E`, then the connection is closed.

| ID | Payload |
|---|---|
| `0x3B` Blacklisted | none. A client that *sends* `0x3B` is kicked (the kick sequence above) `[K]` |
| `0x3C` WrongPassword | none |
| `0x42` UnsupportedProtocol | `u16` required version (202) |
| `0x45` NoSlotsAvailable | none |
| `0x6E` SessionClosed | none |
| `0x6F` AuthFailed | `wstring8` reason |

### 0x3E — HandshakeResponse

| Field | Type | Notes |
|---|---|---|
| ServerName | `wstring8` | |
| UdpPort | `u16` | |
| RefreshRateHz | `u8` | tick rate |
| TrackName | `string8` | |
| TrackConfig | `string8` | |
| CarModel | `string8` | assigned car |
| CarSkin | `string8` | |
| SunAngle | `f32` | degrees, −80..80 over the day |
| AllowedTyresOutCount | `i16` | |
| AllowTyreBlankets | `bool` | |
| TractionControlAllowed | `u8` | 0/1/2 (2 = factory) |
| ABSAllowed | `u8` | 0/1/2 |
| StabilityAllowed | `bool` | |
| AutoClutchAllowed | `bool` | |
| JumpStartPenaltyMode | `u8` | |
| MechanicalDamageRate | `f32` | `DAMAGE_MULTIPLIER / 100` (`MECHANICAL_DAMAGE_RATE` is AssettoServer's name) |
| FuelConsumptionRate | `f32` | |
| TyreConsumptionRate | `f32` | |
| IsVirtualMirrorForced | `bool` | |
| MaxContactsPerKm | `u8` | missing or `0` → `5`; negative → `0` (no limit); `> 255` fails at startup |
| RaceOverTime | `i32` | ms |
| ResultScreenTime | `i32` | ms |
| HasExtraLap | `bool` | |
| IsGasPenaltyDisabled | `bool` | |
| PitWindowStart | `i16` | see below |
| PitWindowEnd | `i16` | |
| InvertedGridPositions | `i16` | |
| SessionId | `u8` | this client's slot |
| SessionCount | `u8` | |
| Sessions | repeat | per session: `u8` type, `u16` laps, `u16` minutes |
| CurrentSession.Name | `string8` | |
| CurrentSession.Id | `u8` | index into the list |
| CurrentSession.Type | `u8` | 0 Booking, 1 Practice, 2 Qualifying, 3 Race |
| CurrentSession.Time | `u16` | minutes |
| CurrentSession.Laps | `u16` | |
| TrackGrip | `f32` | 0..1 |
| SpawnPosition | `u8` | pit box; the session id |
| SessionTime | `i64` | elapsed ms in the current session |
| ChecksumCount | `u8` | |
| ChecksumPaths | `ChecksumCount × string8` | doc 1 §1.5 |
| LegalTyres | `string8` | e.g. `"SV;SM"` |
| RandomSeed | `i32` | the same for every client; see below |
| CurrentTime | `i32` | server clock, ms (`0xF9` `Time`) |

**RandomSeed and CurrentTime** (measured on v1.15). Every client of a server
gets the same `RandomSeed`; it was identical across restarts minutes apart and
different later, so vanilla derives it from the clock at a coarse granularity.
`CurrentTime` is the server clock at the handshake, not 0 as AssettoServer
writes it.

**Pit window.** Vanilla zeroes both ends at startup, logging `WARNING: pitstop
window needs a valid range`, unless the window opens before it closes and closes
within the race (by lap for a lap race, by minute for a timed race). A race with
neither laps nor a time cannot have a window.

### 0x40 — CarList

The reply to `0x3F`.

| Field | Type | Notes |
|---|---|---|
| PageIndex | `u8` | echoed |
| Count | `u8` | ≤ 10 |

Per car:

| Field | Type | Notes |
|---|---|---|
| SessionId | `u8` | |
| Model | `string8` | |
| Skin | `string8` | |
| DriverName | `string8` | empty if none |
| DriverTeam | `string8` | |
| NationCode | `string8` | |
| IsSpectator | `bool` | |
| DamageZoneLevel | `damage` | the car's last reported `0x56`, not zeroes |

### 0x47 — Chat

| Field | Type | Notes |
|---|---|---|
| SessionId | `u8` | sender; `0xFF` = server |
| Message | `wstring8` | |

### 0x51 — WelcomeMessage

| Field | Type | Notes |
|---|---|---|
| (unknown) | `u8` | always 0 |
| Message | `wstring16` | plain text. `[CSP]` may carry encoded extra options (doc 4 §4.5) |

### 0x5A — CarConnected

Sent to the rest of the grid **at handshake**, while the new driver is still
loading. A client that only handshakes still produces one.

| Field | Type |
|---|---|
| SessionId | `u8` |
| Name | `string8` |
| Nation | `string8` |

### 0x4D — CarDisconnected

| Field | Type |
|---|---|
| SessionId | `u8` |

### 0x5B — DriverInfoUpdate

| Field | Type |
|---|---|
| Count | `u8` |
| per car: SessionId | `u8` |
| per car: Name | `wstring8` |

`[K*]` Vanilla lists **every slot**. A slot's name is its last driver's and
outlives the connection: a driver who has left is still named until someone
else takes the slot. A slot nobody has used has an empty name, so a fresh
3-slot list with one driver gives `Count=3` and names `"Dumper", "", ""`.
Measured on v1.15, including a real client 23 s after it left. The same last
driver (name, team, nation, guid) is what `0x40`, `/JSON`, plugin car info and
the results file show for an empty slot, so a driver who left mid-session keeps
their name on their laps. AssettoServer lists only occupied slots.

### 0x4A — CurrentSessionUpdate

| Field | Type | Notes |
|---|---|---|
| Name | `string8` | |
| Id | `u8` | session index |
| Type | `u8` | 0–3 |
| Time | `u16` | minutes |
| Laps | `u16` | |
| TrackGrip | `f32` | |
| Grid | `N × u8` | session ids in grid order; N = entry-list size |
| StartTime | `i64` | session start in the **receiver's** clock: server clock − its offset (doc 3 §0xF8), ms |

**StartTime** is written per client. Vanilla subtracts each receiver's clock
offset from the server-clock start, so the value is comparable to that client's
own clock. A client whose clock is far from the server's (any real one, after
hours of server uptime) reads a raw server-clock start as a session that has
not begun, and keeps its car in neutral in the pits. A client with no pong yet
has offset 0 and gets server clock.

**Grid.** On a session change the grid is the finishing order of the session
**that just ended**: the leaderboard order (`0x49` Ranking), sorted once more
at the switch. That is how qualifying sets the race grid.

**Reversed-grid race.** With `REVERSED_GRID_RACE_POSITIONS` set, a race that is
the last session is **run twice**: when it ends, the same race starts again on
a reversed grid, and only after the repeat does the server move on. A race that
is the only session is reversed every time. The reversed grid lists the cars
that finished, in finishing order, then the rest, and reverses the first
`REVERSED_GRID_RACE_POSITIONS` positions (`-1`, or more than the finishers:
all finishers). Vanilla's arithmetic has a quirk: one more than the number of
finishers also swaps in the first car that did not finish (measured on
v1.15). Cars without a time
tie, and occupied slots rank above empty ones. (Vanilla builds the grid in
`broadcastSession` before `NewSessionResult` clears the results.)

**Restart.** A restart clears the results **first**, so the grid falls back to
entry-list order. It also removes the rubber laid down during the session, but
does not re-run the `SESSION_TRANSFER` blend. The restarted session starts on the
same surface as the original.

**Grip on a new session.** Advancing to any session but the first blends
`SESSION_START` with `SESSION_TRANSFER` % of the current grip, ± `RANDOMNESS`.
Advancing to session index 0 (a `LOOP_MODE` wrap) resets grip to
`SESSION_START` ± `RANDOMNESS`, with nothing carried over (vanilla
`DynamicTrackDef.OnNewSession`).

**Per-car reset.** Every session change resets each car's mandatory-pit flag to
false and its P2P count to `-1`. Ballast and restrictor are **not** reset; a
new connection to the slot resets them (`0x70`).

**Order on the wire.** A session change sends `0x4A`, then `0x78 WeatherUpdate`,
then (after `/next_session`) the admin's chat reply. The first UDP `0x57`
follows at once (doc 3 `0x57`).

#### End of a session

Practice and qualifying end the same way (vanilla `Step`):

1. When the clock runs out, the session enters a final-lap window of
   `QUALIFY_MAX_WAIT_PERC` % of the fastest lap so far. With no lap set, the
   window is empty.
2. During the window, a car's next completed lap is its last: it is recorded
   and marks the car `HasCompletedLastLap`. Any later lap from that car is
   dropped without a `0x49`, in every session type.
3. `0x4B RaceOver` is broadcast when the window closes, or **at once** when no
   ready car (doc 1 §1.5) is both still on its last lap and faster than 5 m/s
   (`|Velocity|` of its last `0x46`). A session with nobody on track therefore
   ends as soon as its time is up.
4. `max(RESULT_SCREEN_TIME, 10)` seconds later, the next session's `0x4A`,
   `0x78` and `0x54` (the reset sun) follow. Measured with 3, 5, 7 and 20. The
   `0x54` follows any switch once the server is a minute old (`0x54`).

#### End of a race

Measured on a lap race:

- A race car's `LapTime` in `0x49` is its **time since the start** at its latest
  lap, not a sum of lap times.
- The leader finishes on reaching `LAPS`. From then on, **every other car
  finishes on its next lap**, however many it has. A finished car's
  `HasCompletedLastLap` is 1, and its later laps are dropped without a `0x49`.
- `0x4B` goes out as soon as every loaded car has finished, right after the
  last `0x49`. Otherwise it goes out `max(RACE_OVER_TIME, 30)` seconds after the
  first car finished (measured with 3, 10 and 40).
- No chat message announces the finish.

A **timed race** (`LAPS=0`, `TIME`) runs until `TIME` has passed since the
start. After that (vanilla `SessionResult.OnLapCompleted`):

- The next car to cross **as leader** (more laps than anyone so far) finishes.
  With `RACE_EXTRA_LAP`, that first crossing instead sets a target one lap on,
  and the leader finishes on reaching it.
- Every other car finishes on its first lap after the leader has finished. A
  car crossing before the leader does not finish, whatever the clock says.
- `RACE_OVER_TIME` counts from the first finish, as in a lap race; a race whose
  leader never crosses again never ends on its own.

Measured on v1.15 with a one-minute race.

**A race with too few cars is skipped** (vanilla `Session.IsOver`, checked
every pass): it moves straight to the next session, with no `0x4B` and no
result screen, when `IS_OPEN=0` and fewer than two cars are loaded (countdown
included), when `IS_OPEN=2` and fewer than two are loaded after the start, or
when no car is loaded after the start. Practice and qualifying never end this
way (measured on v1.15).

### 0x49 — LapCompleted (leaderboard)

| Field | Type | Notes |
|---|---|---|
| SessionId | `u8` | who completed the lap; `0xFF` for a plain sync |
| LapTime | `u32` | ms |
| Cuts | `u8` | |
| LapCount | `u8` | number of entries: **one per slot**, empty and spectator slots included. The game skips this byte (below) |

Entries, ranked:

| Field | Type | Notes |
|---|---|---|
| SessionId | `u8` | |
| LapTime | `u32` | race: total time, `0` if none. Otherwise: best lap, `999999999` if none |
| NumLaps | `u16` | |
| HasCompletedLastLap | `u8` | 1 once the car has finished the race; always 0 outside one |

Then:

| Field | Type |
|---|---|
| TrackGrip | `f32` |

**How the game reads the rows.** The game skips `LapCount` and reads one entry
per car of the car list it loaded at its join (`0x40`), less the spectators'
(game client code as ported by rustyAC). It reads `0x4A`'s grid and `0x4B`'s
entries the same way. It asks for the car list only once, so these three
packets must keep the row count it loaded for as long as it stays connected.
A count that differs shifts every later field: in `0x4A`, the StartTime.

**Ranking.** In a race: most laps first, then least total time. Otherwise: best
lap, fastest first, and on a tie a connected slot before an empty one.

The order is **stored**, not recomputed per packet (vanilla `bestLapRank`). It
starts in slot order with each session and is re-sorted only when:

- a lap counts: any race lap, or an uncut lap in practice or qualifying;
- a new driver takes a slot (doc 1 §1.5 "Which slot"), sorted while that slot
  is still empty;
- the session ends, just before the results file and the grid.

Every `0x49`, `0x4B` and grid in between reads the stored order. So a driver
who joins or leaves moves nobody, and a cut lap in practice changes nothing.
Measured on v1.15: after the first car left, a second car's cut lap left the
board in slot order, and its clean lap then put it first. Vanilla sorts with
Go's unstable `sort.Sort`, so tied cars can swap places (a real grid came out
`22, 0, 2…21, 1, 23`). A stable sort matches every ordered position and keeps
ties in their previous order.

Results belong to the **slot**, not the driver. A departed driver's lap stays on
the board until the session changes, or until a new driver takes that slot,
which clears it. A driver who rejoins gets their old slot back with their laps.

### 0x4B — RaceOver

| Field | Type | Notes |
|---|---|---|
| per slot: SessionId | `u8` | every slot, ranked as in `0x49` |
| per slot: Time | `u32` | as in `0x49` |
| per slot: NumLaps | `u16` | |
| PickupMode | `bool` | after the results. Each entry is 7 bytes. The game reads one per car of its list (see `0x49`); a reader without the car list can read entries while ≥ 8 bytes remain |

### 0x0D — P2PUpdate

| Field | Type |
|---|---|
| SessionId | `u8` |
| P2PCount | `i16` |
| Active | `bool` |

Broadcast to everyone, the sender included. The count is **client-authoritative**:
vanilla relays the value the car reported. A slot that has never used P2P reports
`-1`. Only the `-1` query is answered from server state.

### 0x0E — MandatoryPitUpdate

| Field | Type |
|---|---|
| SessionId | `u8` |
| MandatoryPit | `bool` |

Broadcast to everyone, the sender included.

### 0x50 — TyreCompoundUpdate

| Field | Type |
|---|---|
| SessionId | `u8` |
| CompoundName | `string8` |

Relayed to the **other** cars only, not echoed back.

### 0x56 — DamageUpdate

| Field | Type |
|---|---|
| SessionId | `u8` |
| DamageZoneLevel | `damage` |

### 0x58 — SectorSplit

| Field | Type |
|---|---|
| SessionId | `u8` |
| SplitIndex | `u8` |
| SplitTime | `u32` |
| Cuts | `u8` |

### 0x70 — BoPUpdate

| Field | Type |
|---|---|
| Count | `u8` |
| per car: SessionId | `u8` |
| per car: BallastKg | `f32` |
| per car: Restrictor | `f32` |

`[K*]` `Count` is a real list length. Vanilla sends a single packet covering
every slot, both in the snapshot and after `/ballast`. AssettoServer sends one
packet per car with `Count=1`. Clients accept either.

**A `/ballast` or `/restrictor` lasts only for that connection.** Every join,
including the same driver rejoining, restores the slot's entry-list `BALLAST`
and `RESTRICTOR` (vanilla `createNewCarConnection`). If that changed them, the
loaded clients get a `0x70` right after the join's `0x5A` (measured on v1.15).

### 0x52 — CarSetup

| Field | Type | Notes |
|---|---|---|
| Fixed | `bool` | always true |
| Count | `u8` | |
| per setting: Name | `string8` | e.g. `"PRESSURE_LF"` |
| per setting: Value | `f32` | |

The settings are **every section** of the setup file, each worth its `VALUE`
or 0 without one (vanilla `LoadSetup`), so a saved setup's `[CAR]` section goes
out as `CAR` = 0. Vanilla keeps them in a map, so their order is arbitrary.
`FIXED_SETUP=name` opens `setups/name` **verbatim**: `fast` and `fast.ini` are
different files, and a missing one stops startup. An empty `FIXED_SETUP=` opens
`setups/<MODEL>.ini`.

### 0x53 — DrsZonesUpdate

| Field | Type | Notes |
|---|---|---|
| Count | `u8` | |
| per zone: Start | `f32` | normalized track position |
| per zone: End | `f32` | |

### 0x78 — WeatherUpdate

| Field | Type | Notes |
|---|---|---|
| Ambient | `u8` | °C |
| Road | `u8` | °C, absolute |
| Graphics | `wstring8` | e.g. `"3_clear"` |
| WindSpeed | `i16` | |
| WindDirection | `i16` | degrees |

The config keys are `BASE_TEMPERATURE_AMBIENT` and `BASE_TEMPERATURE_ROAD` (not
`BASE_TEMP_*`). **`BASE_TEMPERATURE_ROAD` is a delta on ambient**: ambient 18,
road 6 gives `18, 24` on the wire. `VARIATION_*` randomises both by ± that
amount at session start.

### 0x54 — SunAngleUpdate

| Field | Type |
|---|---|
| SunAngle | `f32` |

The angle is `SUN_ANGLE + 0.0044° × TIME_OF_DAY_MULT` per second since the
current session's start, recomputed every main-loop pass and clamped to
±80°. A race's start lies ahead during its countdown, so the sun sits below
`SUN_ANGLE` until the green light: with `WAIT_TIME=60` a joiner's `0x3E`
carries about `SUN_ANGLE − 0.26°`.

Vanilla sends it to everyone once more than 60 s of uptime have passed since
the last one (`lastSunAngleUpdate`, checked every main-loop pass). Every session
switch, whether automatic, admin, vote or restart, resets the sun to
`SUN_ANGLE` and zeroes that timer. On a server more than a minute old the reset
angle therefore follows the switch's `0x4A` and `0x78` at once, and the next
`0x54` comes 60 s later. A younger server sends nothing until it is a minute
old (measured on v1.15). `TIME_OF_DAY_MULT=0` moves the sun as `1`
(measured). At ±80° it stops rather than wrapping round to sunrise.

### 0x82 — ClientEvent (jump start)

| Field | Type | Notes |
|---|---|---|
| Type | `u8` | `0x0C` |
| SessionId | `u8` | penalized car |

### 0x64 / 0x65 / 0x66 — Vote state

Broadcast on every vote change. The id matches the vote kind.

| Field | Type | Notes |
|---|---|---|
| Target | `u8` | kick target, else 0 |
| Quorum | `u8` | yes votes must **exceed** this |
| VoteCount | `u8` | yes votes so far |
| Time | `u32` | ms remaining; exactly `VOTE_DURATION × 1000` when a vote opens |
| LastVoter | `u8` | last driver to vote **yes** |
| LastVote | `u8` | that driver's *current* vote, 0/1 |

How votes work (reference `GetQuorumForCurrentVote`, `StepVote`):

- **One vote at a time, matched by kind.** While a vote runs, a packet of
  another kind is dropped. A packet of the same kind counts in the running poll,
  whatever target it names, so a second `0x66` neither retargets nor restarts
  the vote. Each session id holds one vote, and voting again replaces it.
- **Quorum is recomputed live** from the count of ready clients (doc 1 §1.5;
  a driver still loading does not count) on every broadcast and at expiry. For a session vote it is `trunc(connected × VOTING_QUORUM / 100)`.
  For a kick it is `trunc((connected − 1) × KICK_QUORUM / 100)`. A lone driver
  therefore has a quorum of 0.
- **Decided only when `VOTE_DURATION` expires**, never early. A vote passes if
  the yes votes are strictly greater than the quorum.
- **LastVoter / LastVote** follow one driver: LastVoter moves only on a yes vote,
  and LastVote is that driver's current vote. A no from someone else changes
  nothing. A fresh poll reports `0`/`0`.
- **A passing session vote** acts without a chat message; the `0x4A` is all
  that appears. **A passing kick** follows the admin-kick sequence:
  `<name> kicked from the server`, then the target's `0x68`, with
  `BLACKLIST_MODE` applied as for `/kick`.
- **A failed vote** ends with `0x67`.
- A vote is dropped when its session changes.

### 0x67 — VoteQuorumNotReached

| Field | Type | Notes |
|---|---|---|
| Time | `u32` | the remaining time, which has just wrapped below zero (`0xFFFF…`), not 0 |

### 0x68 — KickCar

| Field | Type | Notes |
|---|---|---|
| SessionId | `u8` | |
| Reason | `u8` | 0 VoteKicked, 1 VoteBanned, 2 VoteBlacklisted, 3 ChecksumFailed, 4 Kicked |

### 0xAB — Extended `[CSP]`

Subtypes `0x02` CarVisibilityUpdate, `0x03` ClientMessage, `0x05` kick-message
override. See doc 4.
