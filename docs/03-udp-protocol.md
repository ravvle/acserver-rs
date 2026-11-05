# 3. UDP Game Protocol

Datagrams have no length prefix: `u8 packet_id`, then the payload (doc 1 §1.3).
The server accepts game packets only from an associated address. The
exceptions are `0x4E` itself, `0xC8` and a client-sent `0xF9`. Everything is `[K]` unless tagged.

## 3.1 Client → Server

### 0x4E — CarConnect (association)

| Field | Type | Notes |
|---|---|---|
| SessionId | `u8` | must belong to a handshaked client |

The reply is a single-byte datagram, `0x4E`. Every `0x4E` is answered, a repeat
from the same address included. One from a **new** address moves the car there:
from then on the server sends to the new address and drops `0x46` from the old
one. A client whose NAT rebinds its UDP port can recover this way. Until it
re-associates, datagrams from the new port are ignored. Measured on v1.15.

**Hardening (not vanilla).** The packet proves nothing but a session id, so
any host that can reach the port can claim a slot that has not associated yet,
or move a live player's UDP elsewhere. Matching the source against the TCP
peer closes this, but an exact-IP match also rejects genuine players behind
carrier-grade NAT or dual-WAN routers, whose two flows can leave from
different public addresses; they then load until they time out. A `/24` (IPv6
`/48`) match is the practical compromise, at least for moving an existing
association.

### 0xC8 — LobbyCheck

Any datagram starting with `0xC8`. It needs no association. Reply:

| Field | Type |
|---|---|
| packet_id | `u8` = `0xC8` |
| HttpPort | `u16` |

The lobby and Content Manager use it to confirm the server is reachable over UDP.

It is answered without association (as is a client-sent `0xF9`), so it can
reflect spoofed traffic. The reply is no larger than the request and amplifies
nothing, but replies should still be rate-limited per source IP.

### 0x4F — SessionRequest

| Field | Type | Notes |
|---|---|---|
| SessionType | `u8` | the type the client thinks is running |

If it is wrong, the server sends that client a fresh `0x4A` over TCP. If it is
right, nothing is sent.

### 0x46 — PositionUpdate

A fixed 61-byte payload (62 including the id):

| Field | Type | Notes |
|---|---|---|
| PakSequenceId | `u8` | wrapping counter |
| LastRemoteTimestamp | `u32` | client clock, ms |
| Position | `vec3` | world |
| Rotation | `vec3` | Euler, radians |
| Velocity | `vec3` | m/s |
| TyreAngularSpeed FL/FR/RL/RR | `4 × u8` | value − 100 |
| SteerAngle | `u8` | value − 127 |
| WheelAngle | `u8` | (value − 127) / 2 |
| EngineRpm | `u16` | |
| Gear | `u8` | 0 reverse, 1 neutral, 2 first, … |
| StatusFlags | `u32` | §3.3 |
| PerformanceDelta | `i16` | |
| Gas | `u8` | 0–255 |
| NormalizedPosition | `f32` | spline position, 0..1 |

Handling:

- The first accepted update triggers the first-update snapshot (doc 1 §1.5).
- **Stale updates are dropped.** An update is accepted only if its
  `LastRemoteTimestamp` is **strictly greater** than the last accepted one for
  that car. This stops a reordered datagram from dragging a car backwards.
  There is no tolerance window: a delta of `+1` passes, while `0` and every
  negative delta fail. The comparison resets when the slot changes occupant or
  the session changes.
- **Relay budget (not vanilla).** Vanilla relays any rate, so one client can
  multiply a flood across every player. A per-client limit far above
  `CLIENT_SEND_INTERVAL_HZ` stops this without affecting real clients.
- `[AS]` Ignored while the client's checksum is pending, and dropped if the
  checksum failed or the CSP security level is too low.
- `[AS]` **Validation.** Rejected if position, rotation or velocity contains NaN,
  if any |position| component is > 100 000, or if any |velocity| component is
  > 500. Vanilla relays anything, NaN included, to every other car, where
  malformed values can crash physics. Recommended even for a Kunos
  replacement.

### 0xF8 — PingPong

| Field | Type | Notes |
|---|---|---|
| Time | `i32` | echoed from `0xF9` |
| ClientTime | `i32` | client clock, ms |

The server does **not** use the raw round trip directly. Vanilla (`UpdatePingAvg`)
writes each raw sample into a 50-slot ring and reports the mean of the samples
recorded so far (the last 50 once the ring is full), skipping negative ones:

```
raw          = now − Time            // stored even if negative
avg          = mean(samples so far, at most 50, skipping negatives)
CurrentPing  = avg
targetOffset = now − (avg/2 + ClientTime)
```

The pong sets only a **target**. The live `timeOffset`, which every clock
conversion uses, moves toward it on each inbound `0x46`, before the stale check
(vanilla `Car.OnMessage`):

```
diff = targetOffset − timeOffset
|diff| > 1000 or |diff| < 3  →  timeOffset = targetOffset
otherwise                    →  timeOffset ± 3 toward targetOffset
```

So the first update after the first pong snaps, and later jitter reaches relayed
timestamps 3 ms per update instead of as a jump. A client that never sends a
`0x46` keeps offset 0. Measured on v1.15: after a pong 100 ms off, relayed
timestamps shifted 3, 6, 9 and 12 ms over the next four updates.

There is no ramp from zero: measured on v1.15 with pongs 100 ms late, the
reported ping is 100 from the second `0xF9` on. After a change to 20 ms it
falls as a running mean and settles 50 samples later. Vanilla never times a
client out for missing pongs.

### 0x4C — Pulse

No payload. Vanilla marks the connection ready (doc 1 §1.5) without sending the
first-update snapshot. There is no reply.

### 0xF9 — time request

A client may send `0xF9` itself, with a `u64` token. Vanilla's socket layer
answers at once, from any source address and before any other handling, with
`0xF8`:

| Field | Type | Notes |
|---|---|---|
| Token | `u64` | echoed |
| Time | `u32` | server clock, ms |

A request over 2048 bytes is ignored. Like `0xC8`, the reply needs no
association, so it should be rate-limited per source IP.

### 0xAB — Extended `[CSP]`

Subtype `0x05` ClientMessage, only with `EnableUdpClientMessages`. See doc 4.

## 3.2 Server → Client

### 0xF9 — PingUpdate

Sent to each ready client (doc 1 §1.5: its first `0x46` or a `0x4C` has
arrived) once more than 1000 ms have passed since its last ping. Vanilla keeps
that timer per connection (`LastPingTime`, starting at 0) and checks it every
main-loop pass, so a car is pinged within milliseconds of becoming ready, then
about every second on its own phase. Measured on v1.15: 2 ms after a real
client's first `0x46`. A client still loading is not pinged, and nobody is
pinged in the server's first second.

| Field | Type | Notes |
|---|---|---|
| Time | `u32` | server clock, ms |
| CurrentPing | `u16` | averaged RTT (§3.1) |

**The server clock** is milliseconds since the server started. It reaches
`0x57` and `0x3E` as an `i32`, which overflows after 24.8 days, and `0xF9` as a
`u32` (49.7 days). A long-running server can restart the clock at 0 at a
session change and move every stored clock offset with it: `0x4A` and `0x57`
are in each client's clock, so the rebase does not show in them.

### 0x46 — PositionUpdate

**Vanilla relays once per main-loop pass.** Each pass drains every waiting
packet (an inbound `0x46` overwrites the car's stored status and flags it),
steps the session, sleeps `SLEEP_TIME` ms, then relays each flagged car's
**latest** status once, in slot order, to every ready client. So:

- A car that stops sending produces nothing (no repeat of its last position).
- At the usual `SLEEP_TIME=1` a pass takes 1–2 ms, so relays look immediate.
  Measured on v1.15: a 60 Hz sender had all 100 of 100 updates relayed with no
  measurable delay, a 500 Hz sender all 1000 of 1000.
- Two updates from one car that land in the same pass are **merged**: only the
  newest goes out. That happens in network bursts and when the server is
  overloaded (longer passes), which caps the relay rate at one update per car
  per pass whatever the clients send.
- A larger `SLEEP_TIME` turns this into a coarse relay tick (50 → about 20 Hz).
  `SLEEP_TIME=0` spins without sleeping.

`CLIENT_SEND_INTERVAL_HZ` only tells clients how often to send (`0x3E`).

`[AS]` AssettoServer relays on a fixed tick of `CLIENT_SEND_INTERVAL_HZ`
instead, sending each car's latest status once per tick. A position waits
for the next tick: measured on 0.0.54 at 18 Hz, relay delays spread evenly
from 0 to 55 ms (mean 27 ms), against under 2 ms for vanilla. A tick with no
newer update from a car can repeat its last one.

| Field | Type |
|---|---|
| SessionId | `u8` |
| PakSequenceId | `u8` |
| Timestamp | `u32` |
| Ping | `u16` |
| Position | `vec3` |
| Rotation | `vec3` |
| Velocity | `vec3` |
| TyreAngularSpeed FL/FR/RL/RR | `4 × u8` |
| SteerAngle | `u8` |
| WheelAngle | `u8` |
| EngineRpm | `u16` |
| Gear | `u8` |
| StatusFlags | `u32` |
| PerformanceDelta | `i16` |
| Gas | `u8` |

`Timestamp` is the sender's timestamp moved into the receiver's clock:

```
Timestamp = remoteTimestamp + senderOffset − receiverOffset
```

Each car's offset is its §0xF8 `timeOffset` (slewed by that same update),
**sampled when that car's last `0x46` was accepted**. A receiver that has sent nothing since its pongs began
still carries the old offset (0 before any pong), so it is sent server-clock
timestamps until its next update. Measured on v1.15 with two clients whose
clocks differ by 9.8 million ms: once both are streaming, the relayed value
tracks the receiver's clock to within ~100 ms, and any skew in the sender's
timestamp passes through unchanged.

**`[BANDWIDTH_OPTIMIZER]` `ENABLED=1`** makes distant cars update less often.
Vanilla reads it from that section of its own, not from a `[SERVER]` key. The binary
(`Car.updatePriorities`) ranks every other car by 3-D distance, counting
a car with no connection as 100 000 m away, and gives each a priority from its
rank:

| Rank (nearest first) | Priority | Sent every |
|---|---|---|
| 0–4 | 0 | update |
| 5–9 | 1 | 2nd update |
| 10–14 | 2 | 3rd update |
| ≥ 15 | 3 | 4th update |

Each receiver keeps a `skipCount` per sender. If `skipCount < priority`, the
counter is incremented and the packet is skipped. Otherwise the counter resets to
0 and the packet is sent. Counters reset with the connection.

Vanilla ranks and relays only among ready clients. It re-ranks every ready
car's view of the others each pass (`Car.Step`), and advances the skip
counters once per relay.

`[AS]` AssettoServer has no rank tiers. With a **network bubble**
(`NetworkBubbleDistance`), a car further away than that from the receiver is
sent at `OutsideNetworkBubbleRefreshRateHz` instead of every update.

### 0x48 — MegaPacket

Clients parse this, but **vanilla never sends it**. Its only sender
(`SendMegapacket`) is gated on a `ServerOptions` flag that nothing in the binary
ever sets. Measurements agree: grids of 4 to 48 cars at 20 Hz produced zero
`0x48`s. Sending it is therefore a deviation, not parity. `[AS]` AssettoServer sends
AI cars (doc 9 §9.5) to a client without CSP custom updates (doc 4 §4.2) as
`0x48`s of up to 20 cars, and player cars as single `0x46`s. A car sent in a
`0x48` loses its `Gas` and `PerformanceDelta`.

| Field | Type | Notes |
|---|---|---|
| Timestamp | `u32` | receiver-clock server time |
| Ping | `u16` | receiver's ping |
| Count | `u8` | ≤ 20 |
| Updates | repeat | the `0x46` layout above, ending at `StatusFlags` (no `PerformanceDelta`, no `Gas`) |

### 0x57 — RaceStart

Sent to every loaded client while the current session is less than 5 s old:
through a race's whole countdown, and for the first seconds of a practice or
qualifying. Vanilla keeps one timer for the server (`lastRaceStartUpdateTime`)
and sends a round whenever more than 1000 ms have passed since the last, so the
first round follows a session switch at once and the rest come about a second
apart, independent of the pings. Measured on v1.15: a real client got `0x57` 1,
1002, 2003, 3004 and 4005 ms after a switch's `0x4A`.

| Field | Type | Notes |
|---|---|---|
| StartTime | `i32` | session start in the receiver's clock, ms (doc 1 §1.6) |
| ServerTime | `u32` | now in the receiver's clock, ms |
| Ping | `u16` | receiver's ping |

Both times are the server clock minus the receiver's offset (§0xF8), as in
`0x4A`. During a countdown `ServerTime` advances by 1000 per packet while
`StartTime` stays fixed. A client that never answered a ping has offset 0 and
so sees server clock.

### 0xAB — Extended `[CSP]`

`0x01` WeatherUpdate, `0x03` CustomUpdate. See doc 4.

## 3.3 CarStatusFlags

| Mask | Meaning |
|---|---|
| `0x0000_0010` | brake lights |
| `0x0000_0020` | lights |
| `0x0000_0040` | horn |
| `0x0000_0800` | indicating left |
| `0x0000_1000` | indicating right |
| `0x0000_2000` | hazards |
| `0x0000_4000` | high beams **off** |
| `0x0020_0000` | wiper level 1 |
| `0x0040_0000` | wiper level 2 |
| `0x0060_0000` | wiper level 3 |

Other bits are passed through untouched.
