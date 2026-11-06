# 4. CSP Extension Protocol

**Everything here is `[CSP]`** — defined by the Custom Shaders Patch client mod
and implemented server-side by AssettoServer. None of it exists in vanilla
acServer, so a Kunos replacement can skip this document: vanilla clients never
send `0xAB`, and CSP clients degrade gracefully when nothing answers. The
`[AS]`-tagged parts are AssettoServer's own Lua events (§4.4) and extra-options
content (§4.5).

CSP extends the protocol with packet id `0xAB` ("Extended") on TCP and UDP, plus
a way to smuggle binary messages through ordinary chat text.

Relay CSP messages only to clients that announced a CSP build (doc 1 §1.5
`Features`), and bound their size and rate: every relayed message is
multiplied across the grid. What AssettoServer builds on these packets
(WeatherFX, server scripts, AI traffic, plugins) is in doc 9.

## 4.1 Subtype registries

A **subtype byte** follows `0xAB`. Its meaning depends on the transport.

TCP (`CSPMessageTypeTcp`):

| Subtype | Name | Direction |
|---|---|---|
| `0x00` | SpectateCar | C→S (doc 2 §2.1) |
| `0x02` | CarVisibilityUpdate | S→C |
| `0x03` | ClientMessage | C→S, S→C |
| `0x04` | SystemMessage | S→C (not used by AssettoServer) |
| `0x05` | KickBanMessage override | S→C |

UDP (`CSPMessageTypeUdp`):

| Subtype | Name | Direction |
|---|---|---|
| `0x01` | WeatherUpdate | S→C |
| `0x03` | CustomUpdate (position batch) | S→C |
| `0x05` | ClientMessage | C→S, S→C |

## 4.2 Simple extended packets (server → client)

### 0xAB 0x02 — CarVisibilityUpdate (TCP)

| Field | Type | Notes |
|---|---|---|
| SessionId | `u8` | |
| Visible | `u8` | 0 visible, 1 invisible (used e.g. to hide AI traffic cars) |

### 0xAB 0x05 — KickBanMessage override (TCP)

Sent immediately before a kick to replace the client's generic kick screen text.

| Field | Type | Notes |
|---|---|---|
| Message | `u32` length + UTF-8 bytes | note the unusual **4-byte** length prefix |

### 0xAB 0x01 — CSP WeatherUpdate (UDP)

WeatherFX state. All temperatures/values are `f16`. `[AS]` converts with C#'s
`(Half)` cast, which rounds to nearest:

| Field | Type |
|---|---|
| UnixTimestamp | `u64` |
| WeatherType | `u8` (WeatherFX type id) |
| UpcomingWeatherType | `u8` |
| TransitionValue | `u16` (0–65535 progress between the two) |
| TemperatureAmbient | `f16` |
| TemperatureRoad | `f16` |
| TrackGrip | `f16` |
| WindDirectionDeg | `f16` |
| WindSpeed | `f16` |
| Humidity | `f16` |
| Pressure | `f16` |
| RainIntensity | `f16` |
| RainWetness | `f16` |
| RainWater | `f16` |

### 0xAB 0x03 — CustomUpdate (UDP, position batch)

Sent instead of `0x46`/`0x48` to clients that advertised the `CUSTOM_UPDATE` feature in
their handshake (and when `EnableCustomUpdate` is on). Layout:

| Field | Type |
|---|---|
| Count | `u8` (≤ 20) |

Per update (note **Gas before PerformanceDelta**, unlike `0x46`):

```
u8  SessionId        u8  PakSequenceId    u32 Timestamp     u16 Ping
vec3 Position        vec3 Rotation        vec3 Velocity
u8 TyreAngularSpeed FL/FR/RL/RR           u8 SteerAngle     u8 WheelAngle
u16 EngineRpm        u8 Gear              u32 StatusFlags
u8 Gas               i16 PerformanceDelta
```

The exact field set is negotiated by sending this schema string to the client via extra
options (`[EXTRA_DATA] CUSTOM_UPDATE_FORMAT = '...'`):

```
packet:
  group:
    sessionID
    pakSequenceID
    timestamp
    ping: ushort
    pos: float3
    rotation: float3
    velocity: float3
    tyreAngularSpeedFL: byte, -100, tyreAngularSpeed
    tyreAngularSpeedFR: byte, -100, tyreAngularSpeed
    tyreAngularSpeedRL: byte, -100, tyreAngularSpeed
    tyreAngularSpeedRR: byte, -100, tyreAngularSpeed
    steerAngle: byte, -127
    wheelAngle: byte, -127, /2
    engineRPM: ushort
    gear: byte
    statusBytes
    gas: byte, /255
    performanceDelta: short
```

The `-100`, `-127`, `/2` and `/255` annotations are the byte encodings those
fields use throughout the protocol.

## 4.3 Client messages (0xAB subtype 0x03 TCP / 0x05 UDP)

A generic message bus. After the subtype byte:

**Client → server:**

| Field | Type | Notes |
|---|---|---|
| Type | `u16` | `CSPClientMessageType`, see below |
| ... | | type-specific payload |

**Server → client (broadcast/forward):**

| Field | Type | Notes |
|---|---|---|
| SessionId | `u8` | originating car; `0xFF` = server |
| Type | `u16` | |
| ... | | payload (forwarded verbatim) |

`CSPClientMessageType` values handled or known:

| Value | Name |
|---|---|
| 0 | HandshakeIn (S→C) |
| 1 | HandshakeOut (C→S) |
| 2–5 | Signature(Verification)In/Out |
| 1000 / 1001 | ConditionsV1 / V2 |
| 2000 | ChatSharedSetup |
| 10000 / 10001 | TrackSharedTrigger(Allow) |
| 14000 | DriverNameChange |
| 15000 | CarColorChange |
| 15001 / 15002 | CarTyreState / CarPartsState |
| 30000 / 30001 | NewModeChaseAnnouncement / Capture |
| 50000 | AdminPenalty |
| 60000 | LuaMessage |
| 60001 | LuaMessageTargeted |
| 60002 | LuaMessageRanged (UDP only) |
| 60003 | LuaMessageRangedTargeted (UDP only) |

Server dispatch rules:

- **Lua messages (60000–60003)**: parsed further, see §4.4.
- **HandshakeOut (1, TCP)**: see below.
- **AdminPenalty (50000, TCP)**: only honored from admins; forwarded to the target car
  with sender ID rewritten to 255. Payload: `u16 mode` (1 MandatoryPits, 2 TeleportToPits,
  3 SlowDown, 4 BlackFlag, 5 BlackFlagRelease — only 1 and 2 known to work), `u16
  carIndex`, `i32 penaltyArgument`, `cstr<64> message`, `u64 signature`.
- **Any other type**: broadcast verbatim to all clients (same transport it arrived on),
  with the sender's session ID.

### CSP handshake

After connecting, the server sends (TCP, as part of the first-update batch, only when
`EnableClientMessages` is on):

`0xAB 0x03` + `u8 SessionId=255` + `u16 Type=0 (HandshakeIn)` + `u32 MinVersion` +
`bool RequiresWeatherFx`

The client answers:

`0xAB 0x03` + `u16 Type=1 (HandshakeOut)` + `u32 Version` + `bool IsWeatherFxActive` +
`u8 InputMethod (0 Unknown, 1 Keyboard, 2 Gamepad, 3 Wheel)` + `bool IsRainFxActive` +
`u8 padding` + `u64 UniqueKey` (hardware ID).

## 4.4 Lua online events (types 60000–60003)

Payload after the `u16` type:

| Field | Type | Present when |
|---|---|---|
| Range | `f16` | Ranged types (60002/60003) — max distance in meters |
| TargetSessionId | `u8` | Targeted types (60001/60003) |
| LuaPacketType | `u32` | always — key hash identifying the message struct |
| Data | bytes | remainder; the packed struct fields |

Routing: targeted messages go only to the target client; ranged messages only to cars
within `Range` meters of the sender; otherwise broadcast. When forwarding, the server
rewrites the header to `SessionId (sender)` + `Type (60000 or 60001)` + `[TargetSessionId]`
+ `u32 LuaPacketType` + data. **Trailing zero bytes may be stripped** from Lua message
payloads by CSP; readers must treat missing trailing bytes as zeros.

### Key hash (`LuaPacketType`) computation

The 32-bit key identifies a Lua struct layout. It is derived from a canonical structure
definition string:

1. Order the fields: sort by size **descending**, ties broken by field name (ordinal
   ascending); then apply CSP's 8-byte packing reorder (a smaller field may be pulled
   forward to fill the remainder of an 8-byte slot — see
   `acc-lua-sdk common/ac_struct_item.lua`).
2. Build the definition string: for each field `"<ctype> <name>;"` with C type names
   (`bool, uint8_t, char, uint16_t, int16_t, uint32_t, int, uint64_t, int64_t, float,
   double, vec2, vec3, vec4, rgbm`), arrays as `<ctype> <name>[N];`; append `//<Key>`
   where `<Key>` is the event's key string.
3. Remove all whitespace from the definition, prefix with `server_script`, and hash with
   **xxHash3-64 version 0.7.0** (the old algorithm revision — modern xxh3 gives different
   results; AssettoServer vendors the 0.7.0 C++ code for compatibility).
4. Fold to 32 bits: `key = (u32)hash ^ (u32)(hash >> 32)`.

Wire encoding of struct fields: fixed-size scalars as their C type (LE); strings as
fixed-size zero-padded UTF-8 buffers (`char name[N]`); `rgbm` as 4 × `f32` (r, g, b, mult
each 0..1, mapped from RGBA bytes /255). The **last** field's fixed buffer may be written
unpadded (trailing zeros stripped).

### Built-in server events `[AS]`

The mechanism is CSP's; these events (the `AS_` prefix) are AssettoServer's.
Clients use matching Lua structs:

| Key | Fields | Purpose |
|---|---|---|
| `AS_ApiKey` | `key: char[32]` | sends the per-client HTTP API key after Lua ready |
| `AS_CollisionUpdate` | `enabled: bool` | toggles collisions for a car |
| `AS_TeleportCar` | `position: vec3`, `direction: vec3`, `velocity: vec3` | teleports the client's car |
| `AS_LuaReady` | `dummy: uint8_t` | client → server: server scripts loaded |
| `AS_RequestResetCar` | `dummy: uint8_t` | client → server: request car reset |

AssettoServer's plugins add more events, such as `autoModerationFlag`
(doc 9 §9.3).

## 4.5 Chat-encoded messages and extra options

CSP can read binary data hidden in ordinary welcome and chat text. It is used in
two places.

### Server extra options (welcome message)

The `0x51 WelcomeMessage` text may end with:

```
<welcome text> + "\t" × 32 + "$CSP0:" + base64(zlib(extra_options_ini))
```

- `extra_options_ini` is a UTF-8 INI document (the CSP "server extra options" format).
- zlib = RFC 1950 stream; base64 padding (`=`) is stripped.
- On decode, any run of tabs before `$CSP0:` is accepted.

`[AS]` AssettoServer always injects into the INI (in addition to configured content):
`[EXTRA_DATA] CUSTOM_UPDATE_FORMAT` (when custom update is on, §4.2),
`[EXTRA_TWEAKS] VERIFY_STEAM_API_INTEGRITY = 1` (when Steam auth is off) and a default
`[EXTRA_TWEAKS] MIN_TIME_BETWEEN_COLLISIONS = 2`. Server Lua scripts are referenced from
the INI as `[SCRIPT_...]` sections pointing at `http://<server>/api/scripts/<id>`.

**Watch out:** an encoded welcome message longer than **2039 characters** crashes
clients running CSP older than 0.1.77 (build 1937).

### Chat-encoded client messages (server → client)

When broadcasting CSP messages to clients whose CSP consumes chat-encoded packets
(AssettoServer default for its own outgoing typed messages, e.g. car color updates), the
message is wrapped in a normal `0x47 Chat` packet:

```
message = "\t\t\t\t$CSP0:" + base64(data)   // '=' padding stripped, 4 tabs
data    = u16 Type + payload                // same as a 0xAB client message body
```

## 4.6 CSP feature negotiation

The comma-separated `Features` string from the handshake request lists CSP feature flags
(last element = CSP version number). The server compares it against its required feature
set (e.g. weather FX requirements, client messages) and rejects the handshake
(`AuthFailed`) if required features are missing. Feature flags also gate per-client
behavior: `CUSTOM_UPDATE` enables §4.2 custom updates for that client.

The full feature list the server requires is also published to the lobby/CM via the HTTP
API (`Features` arrays, doc 5).

**`CLIENT_MESSAGES` and `CLIENT_UDP_MESSAGES`.** CSP uses the TCP and UDP client
messages of §4.3 only on a server whose list has these. Without `CLIENT_MESSAGES`,
script events travel as `$CSP0:` chat lines (§4.5), each under 175 bytes and at
least 200 ms apart; without `CLIENT_UDP_MESSAGES`, a script asking for a UDP event
gets the error "UDP messages are not available with this server" (CSP Lua SDK,
`ac.OnlineEvent`; the mapping from feature to flag is from CSP's client code as
documented by rustyAC). `[AS]` AssettoServer lists `CLIENT_MESSAGES` as mandatory
when its client messages are on, which turns away players without CSP.

**`EMOJI`.** The list also tells CSP how to write wide strings. A server whose
list lacks `EMOJI` gets every character beyond U+FFFF as `?`; one that lists
it gets the code point in a single cell. With no list at all, CSP assumes
`EMOJI`. `[AS]` AssettoServer always lists it. (From CSP's client code as
documented by rustyAC, not measured.)

**WeatherFX and the vanilla server browser.** With WeatherFX on,
`WEATHERFX_V1` is mandatory. A join from the vanilla in-game server browser
sends no feature list even when CSP is installed, so it is refused with
`Missing CSP features. Please update CSP and/or Content Manager.`, the same
error as a player without CSP. Such a player must join through Content
Manager (reported by players, not measured).
