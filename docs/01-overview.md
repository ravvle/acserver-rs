# 1. Overview, Wire Types & Connection Lifecycle

Everything in this document is `[K]` unless tagged. Tags are explained in the
[README](README.md#origin-tags).

## 1.1 Architecture

A game client holds two channels to the server at once:

- **TCP** (`TCP_PORT`): anything that must arrive reliably and in order:
  handshake, chat, session and lap events, checksums.
- **UDP** (`UDP_PORT`, usually the same number): high-rate traffic that can
  tolerate loss: positions, ping, lobby probes.

The server also runs an HTTP server (`HTTP_PORT`, doc 5) and an optional UDP
plugin interface (doc 6). AssettoServer adds an optional RCON service (doc 7).

A car is identified by its **session id** (`u8`), its 0-based index in the
entry list. Session id `0xFF` means "the server", for example as the sender of
a server chat message.

## 1.2 Primitive types

All values are **little-endian** and structures are **packed** (no padding).

| Name | Size | Description |
|---|---|---|
| `u8` / `i8` | 1 | unsigned / signed byte |
| `u16` / `i16` | 2 | 16-bit integer |
| `u32` / `i32` | 4 | 32-bit integer |
| `u64` / `i64` | 8 | 64-bit integer |
| `f16` | 2 | IEEE 754 half-precision float |
| `f32` | 4 | IEEE 754 single-precision float |
| `bool` | 1 | `0` false, anything else true (written as `1`) |
| `vec3` | 12 | 3 × `f32`: x, y, z |
| `damage` | 20 | 5 × `f32`: damage level per body zone |

### Strings

| Name | Length prefix | Body |
|---|---|---|
| `string8` | `u8`, in **bytes** | UTF-8 |
| `string16` | `u16`, in **bytes** | UTF-8 |
| `wstring8` | `u8`, in **characters** | UTF-32LE, 4 bytes per character |
| `wstring16` | `u16`, in **characters** | UTF-32LE, 4 bytes per character |
| `cstr<N>` | none | fixed `N`-byte UTF-8 buffer, zero-padded; trim trailing `0x00` on read |

For `string*` the length counts bytes; for `wstring*` it counts characters,
each 4 bytes on the wire.

## 1.3 Framing

**TCP.** Every packet in both directions is:

```
u16 length      // payload length, excluding these 2 bytes
u8  packet_id   // first payload byte
... fields
```

AssettoServer's receive buffer is 2046 bytes, and a larger `length` closes the
connection. Sends may be up to 65535 bytes. A zero-length read or EOF ends the
connection. Several packets may share one TCP write, each with its own length
prefix.

**UDP.** One packet per datagram, no length prefix, at most 1500 bytes:

```
u8 packet_id
... fields
```

## 1.4 Packet-id registry

All ids are `[K]` except `0xAB`.

| ID | Name | Direction | Transport |
|---|---|---|---|
| `0x0D` | P2PUpdate (push-to-pass) | both | TCP |
| `0x0E` | MandatoryPitUpdate | both | TCP |
| `0x3B` | Blacklisted | S→C | TCP |
| `0x3C` | WrongPassword | S→C | TCP |
| `0x3D` | HandshakeRequest | C→S | TCP |
| `0x3E` | HandshakeResponse | S→C | TCP |
| `0x3F` | CarListRequest | C→S | TCP |
| `0x40` | CarList | S→C | TCP |
| `0x41` | ServerRunning: `u32` ms until the next session; sent by vanilla to a joiner while no session runs, never by AssettoServer | S→C | TCP |
| `0x42` | UnsupportedProtocol | S→C | TCP |
| `0x43` | CleanExitDrive | C→S | TCP |
| `0x44` | Checksum | C→S | TCP |
| `0x45` | NoSlotsAvailable | S→C | TCP |
| `0x46` | PositionUpdate | both | UDP |
| `0x47` | Chat | both | TCP |
| `0x48` | MegaPacket (batched positions) | S→C | UDP |
| `0x49` | LapCompleted | both | TCP |
| `0x4A` | CurrentSessionUpdate | S→C | TCP |
| `0x4B` | RaceOver | S→C | TCP |
| `0x4C` | Pulse: marks the connection ready without a first update `[K]`; ignored by AssettoServer | C→S | UDP |
| `0x4D` | CarDisconnected | S→C | TCP |
| `0x4E` | CarConnect (UDP association) | both | UDP |
| `0x4F` | SessionRequest | C→S | UDP |
| `0x50` | TyreCompoundChange | both | TCP |
| `0x51` | WelcomeMessage | S→C | TCP |
| `0x52` | CarSetup | S→C | TCP |
| `0x53` | DrsZonesUpdate | S→C | TCP |
| `0x54` | SunAngleUpdate | S→C | TCP |
| `0x56` | DamageUpdate | both | TCP |
| `0x57` | RaceStart | S→C | UDP |
| `0x58` | SectorSplit | both | TCP |
| `0x5A` | CarConnected | S→C | TCP |
| `0x5B` | DriverInfoUpdate | S→C | TCP |
| `0x64` | VoteNextSession | both | TCP |
| `0x65` | VoteRestartSession | both | TCP |
| `0x66` | VoteKickUser | both | TCP |
| `0x67` | VoteQuorumNotReached | S→C | TCP |
| `0x68` | KickCar | S→C | TCP |
| `0x6E` | SessionClosed | S→C | TCP |
| `0x6F` | AuthFailed | S→C | TCP |
| `0x70` | BoPUpdate (ballast & restrictor) | S→C | TCP |
| `0x78` | WeatherUpdate | S→C | TCP |
| `0x82` | ClientEvent (collisions, penalties) | both | TCP |
| `0xAB` | `[CSP]` Extended (doc 4) | both | TCP + UDP |
| `0xC8` | LobbyCheck | both | UDP |
| `0xF8` | PingPong (client's reply) | C→S | UDP |
| `0xF9` | PingUpdate (server's request) | S→C | UDP |

## 1.5 Connection lifecycle

```
Client                                    Server
  |--- TCP connect ------------------------>|
  |--- 0x3D HandshakeRequest -------------->|  validate
  |<-- 0x3E HandshakeResponse --------------|  (or an error packet, then close)
  |                                         |  0x5A CarConnected → everyone else
  |--- UDP 0x4E CarConnect [sessionId] ---->|  associate UDP endpoint
  |<-- UDP 0x4E ----------------------------|
  |--- 0x44 Checksum ---------------------->|  verify files
  |--- UDP 0x46 PositionUpdate (first) ---->|  makes the client ready; triggers the snapshot
  |<-- 0x5B, 0x78, 0x49, 0x0D/0x0E, 0x70, … |
  |                                         |
  |=== steady state ========================|
  |--- UDP 0x46 at CLIENT_SEND_INTERVAL_HZ >|
  |<-- UDP 0x46 each remote car's update ---|  relayed each loop pass
  |<-- UDP 0xF9 every 1 s ------------------|
  |--- UDP 0xF8 --------------------------->|
  |    chat, laps, votes … over TCP         |
  |                                         |
  |--- 0x43 CleanExitDrive ---------------->|
  |                                         |  0x4D CarDisconnected → everyone
```

### Handshake validation

On `0x3D` the server runs these checks in order and answers the first failure:

| # | Check | Reply | Origin |
|---|---|---|---|
| 1 | Packet is `0x3D` with `ClientVersion == 202` | `0x42 UnsupportedProtocol` | `[K]` |
| 2 | Steam guid is non-zero | `0x6F AuthFailed` | `[AS]` |
| 3 | Guid not in `blacklist.txt` | `0x3B Blacklisted` | `[K]` |
| 4 | Password matches the server or admin password (if one is set) | `0x3C WrongPassword` | `[K]` |
| 5 | Session is open to joins | `0x6E SessionClosed` | `[K]` |
| 6 | Driver name is not empty | `0x6F AuthFailed` | `[AS]` |
| 7 | Required CSP features are present | `0x6F AuthFailed` | `[CSP]` |
| 8 | Slot filters (Steam auth `[K]`, whitelist `[AS]`, …) | `0x6F` or `0x45` | mixed |
| 9 | A free slot exists for the requested car | `0x45 NoSlotsAvailable` | `[K]` |

A pure Kunos server needs steps 1, 3, 4, 5 and 9, plus Steam ticket validation
when `AUTH` is configured.

- **Step 2.** Vanilla admits a zero guid. With `EnableACProSupport`,
  AssettoServer derives one from the name (`xxHash64(name) | 1<<63`).
- **Step 3.** Vanilla reads `blacklist.txt` from its **working directory**, one
  decimal Steam guid per line. A copy in `cfg/` is ignored.
- **Step 4.** With pickup mode off or `LOCKED_ENTRY_LIST=1`, only a guid with
  a free `GUID=`-reserved slot for the requested model may join, and it skips
  the password (official guide). What vanilla sends anyone else has not been
  measured. A booking server admits only booked guids (doc 5 §5.2a).
- **Step 5.** With `PICKUP_MODE_ENABLED=1`, vanilla v1.15 admits joins even when
  every session has `IS_OPEN=0`. `IS_OPEN` only gates joins with pickup off.
- **Step 6.** Vanilla admits a blank name.
- **Step 7.** Vanilla ignores the `Features` field.
- **Step 9.** A join that names **no** car model gets `0x45`, not an arbitrary
  free slot.
- `[K*]` **A guid that is already connected.** Vanilla seats it a second time,
  in another slot, leaving the old connection as a ghost car. Dropping the old
  connection and reusing its slot (when the model matches) lets a player whose
  link died rejoin at once with the slot's results.

**Which slot `[K]`.** Every slot remembers its last driver (name, team,
nation, guid), kept after they leave and overwritten by each join. Among the
free slots with the requested model, vanilla (`onNewConnectionPickup`) takes:

1. the first reserved for the guid by `GUID=`, or whose last driver had this
   guid. The slot keeps its laps, so a driver who drops and rejoins finds them;
2. otherwise the first slot nobody has used, else the first free one. That
   slot's laps are cleared (`ResetCarResults`) and the leaderboard re-sorted.

Either way the slot's tyre compound is cleared. Vanilla's last resort in
step 2 may also be a slot reserved by `GUID=` (read from the binary, not
measured).

On success the slot is reserved, an admin-password match grants admin rights,
and `0x3E` is sent. AssettoServer truncates names to 25 characters; vanilla
keeps them as sent (a 36-character name round-trips intact). Any packet other
than `0x3D` before a successful handshake drops the connection.

### UDP association

The client learns the UDP port from `0x3E`, then sends `0x4E` plus its `u8`
session id from its UDP socket. The server binds that source address to the
car and replies with a single `0x4E` byte. From then on, UDP from that address
counts as that car. Every `0x4E` is answered, and one from a new address moves
the car there, so a client survives a NAT rebinding (doc 3 `0x4E`).

### First update

The server does not treat a client as "in the world" until its first UDP
position update, which triggers this snapshot (measured on v1.15):

| # | Packet | Contents |
|---|---|---|
| 1 | `0x5B DriverInfoUpdate` | **every** entry-list slot, with its last driver's name (doc 2 `0x5B`) |
| 2 | `0x78 WeatherUpdate` | current conditions |
| 3 | `0x49 LapCompleted` | leaderboard sync, session id `0xFF` |
| 4 | `0x50` + `0x0D` + `0x0E` | **per slot**, in slot order: the slot's last tyre compound if it reported one and is not the joiner's, then a P2P / mandatory-pit pair |
| 5 | `0x70 BoPUpdate` | **one** packet covering every slot |
| 6 | `0x51 WelcomeMessage` | only if `WELCOME_MESSAGE` is set |
| 7 | `0x52 CarSetup` | only if the slot has a `FIXED_SETUP` |
| 8 | `0x53 DrsZonesUpdate` | **last**, only if the track has `drs_zones.ini` |

Rows 6–8 are in vanilla's source order (`onConnectionFirstUpdate`). The
plugin's `0x3A` fires between rows 5 and 6.

No `0x54 SunAngleUpdate` is sent here: the angle was already in `0x3E`. The
other clients get no `0x5B` refresh when someone loads, since their driver
list already names every slot; they only hear the `0x5A` sent at handshake.
`[CSP]` AssettoServer adds car-visibility and CSP-handshake packets to the
snapshot.

**Readiness `[K]`.** Vanilla keeps one flag per connection (`IsHandshakeDone`),
set by the client's first UDP `0x46`, or by a UDP `0x4C` Pulse, which sets it
without sending the snapshot. Until it is set, the client:

- receives no TCP broadcast (chat, `0x5A`, `0x4D`, BoP, damage, tyres, votes,
  kicks, sun, race start, race over, leaderboard), except `0x4A` session
  changes, which go to every connection;
- receives no relayed `0x46` and no `0xF9` ping;
- does not count toward the vote quorum, the `/INFO` and lobby `clients`, or
  the end-of-session checks (doc 2).

A loading client catches up from the snapshot and its own `0x3F` requests.

**Checksum gating.**

- **Vanilla `[K]`.** The first update always fires the snapshot. A client whose
  `0x44` was wrong or never arrived is kicked (`0x68`, reason `ChecksumFailed`)
  right after its snapshot: the "deferred kick".
- **AssettoServer `[AS]`.** Position updates are ignored until the `0x44` has
  been processed, so plugins can use the result in the welcome message. A
  single test update sent before the checksum is lost, and only the next one
  triggers the snapshot. Real clients stream every tick and never notice.

A vanilla-faithful server should not gate; it should rely on the deferred kick.

### Timeouts and rate limits

Vanilla has **no** timeouts or rate limits. It sets no socket deadlines (only
`TCP_NODELAY` and a 60 s TCP keepalive), never drops a quiet client, and does
not rate-limit chat. Its only flood handling is a log line, `Warning, packet
burst detected`, when one loop drains more than 100 UDP datagrams.

| What | Value | Origin |
|---|---|---|
| Ping request (`0xF9`) interval | 1000 ms | `[K]` |
| Position broadcast rate | `CLIENT_SEND_INTERVAL_HZ` | `[K]` |
| TCP keepalive | 60 s | `[K]` |
| TCP receive timeout before first update | 5 min | `[AS]` |
| TCP send timeout | 30 s | `[AS]` |
| Handshake → first update | `PlayerLoadingTimeoutMinutes` | `[AS]` |
| Checksum response | `PlayerChecksumTimeoutSeconds` | `[AS]` |
| Ping timeout (no `0xF8`) | 15 s | `[AS]` |
| Chat rate limit | 1 message/s, excess dropped | `[AS]` |

A driver parked in the pits sends nothing over TCP for minutes, so a TCP idle
timeout would drop them. A chat rate limit silently swallows commands typed in
quick succession.

### Checksums

`0x3E` carries a list of file paths. The client answers with `0x44`: one
16-byte **MD5** per path, in order, then one more for its car's `data.acd`, so
`16 × (N + 1)` bytes in total.

The path list is:

- `system/data/surfaces.ini`
- `content/tracks/<track>/data/surfaces.ini`, or with a track layout
  `content/tracks/<track>/<layout>/data/surfaces.ini`
- every `*.kn5` and every `models*.ini` directly in `content/tracks/<track>/`.
  The glob takes every layout's models file, not only the configured one's.
- `[AS]` a `surfaces.ini` directly in the track folder, and
  `content/cars/<model>/collider.kn5` for each model. Vanilla checks neither.

The final digest is compared with the MD5 of `content/cars/<model>/data.acd`. A
car without one is compared with vanilla's folder checksum of
`content/cars/<model>/data/`: one MD5 over the contents of every file under it,
in order of lower-cased path. A car with neither passes. A mismatch or a wrong
blob length earns the deferred `ChecksumFailed` kick.

- `[AS]` `EnableAlternativeCarChecksums` accepts any `data*.acd`.
- `[CSP]` With a minimum CSP version configured, `surfaces.ini` is hashed with
  its first `SURFACE_0` replaced by `CSP`, matching what CSP does client-side.

### Disconnect

A client leaves cleanly with `0x43 CleanExitDrive`, or abruptly through a
socket error or timeout. Either way the server broadcasts `0x4D
CarDisconnected` and frees the slot. A kick sends `0x68 KickCar` instead, which
tells everyone (the target included) why (doc 2 §2.1).

After `0x43`, vanilla leaves its side of the TCP connection open and the client
closes it (seen on a real client). Closing it at once is equivalent.

## 1.6 Sessions

Sessions form an ordered list of types: Booking=0, Practice=1, Qualifying=2,
Race=3. `[K*]` AssettoServer supports only pickup mode. Vanilla's booking mode
(a `[BOOK]` session with pickup mode off) is in doc 5 §5.2a.

- A transition broadcasts `0x4A CurrentSessionUpdate` and then `0x78` over TCP.
  Loaded clients then get a UDP `0x57 RaceStart` at once and each second after
  until the session is 5 s old (doc 3 `0x57`).
- **Start time.** Only a race waits: it starts `max(WAIT_TIME, 20)` seconds after
  the switch. Practice and qualifying ignore `WAIT_TIME` and start at once
  (measured with `WAIT_TIME` 0, 5, 45 for a race, 30 for the others).
- Results are tracked per slot, published in `0x49` leaderboards and in
  `0x4B RaceOver` when the session ends.
- A client whose UDP `0x4F SessionRequest` names the wrong session type is sent
  a fresh `0x4A`. This is how late joiners resync.
- **After the last session**, `LOOP_MODE=1` (the default when the key is absent)
  starts the list again from the first. With `LOOP_MODE=0` vanilla exits
  instead, once any reversed-grid repeat of the last race has run (doc 2 `0x4A`).

Session ends, grids and restarts are in doc 2 §2.2 (`0x4A`).
