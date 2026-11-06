# 5. HTTP API

The HTTP server listens on `HTTP_PORT` and answers in JSON unless noted. A Kunos
replacement needs only the `[K]` parts: `/INFO`, `/JSON|<guid>` and the outbound
lobby registration (§5.8). The rest are Content Manager `[CM]` or AssettoServer
`[AS]` additions.

## 5.1 `GET /INFO` (legacy) `[K]`

The server summary shown in the lobby and Content Manager. Every field is always
present.

```jsonc
{
  "cars": ["ks_mazda_mx5_cup", ...],   // distinct car models in entry list
  "clients": 3,                         // connected player count
  "country": ["Germany", "DE"],         // [name, ISO code] from GeoIP; vanilla without GeoIP
                                        //   sends ["na", "na"]
  "cport": 8081,                        // HTTP port
  "durations": [10, 5, ...],            // [K*] per session: LAPS for a lap race, else TIME
                                        //   in *minutes*. AssettoServer sends minutes*60
                                        //   seconds here; vanilla does not (§5.8 does).
  "extra": false,                       // has extra lap
  "inverted": 0,                        // inverted grid positions
  "ip": "1.2.3.4",                      // public IP
  "json": null,
  "l": false,
  "maxclients": 24,
  "name": "My Server ℹ8081",           // server name; "ℹ<httpPort>" appended when serverDetails enabled
  "pass": false,                        // password protected
  "pickup": true,                       // always true
  "pit": false,                         // RACE_PIT_WINDOW_END set (a valid window)
  "port": 9600,                         // UDP port
  "session": 0,                         // [K*] current session *index* into sessiontypes.
                                        //   AssettoServer sends the session type instead;
                                        //   the §5.8 lobby ping does carry the type.
  "sessiontypes": [1, 2, 3],            // configured session types
  "timed": false,                       // the race runs on TIME (LAPS=0); false with
                                        //   no race, even if every session is timed
  "timeleft": 3599,                     // seconds left in current session
  "timeofday": 16,                      // sun angle, degrees
  "timestamp": 0,
  "tport": 9600,                        // TCP port
  "track": "ks_brands_hatch-gp",       // track, "-config" appended if set
  "poweredBy": "AssettoServer 0.0.55"   // [AS] addition, absent in vanilla
}
```

The `ℹ<httpPort>` suffix on `name` is a `[CM]` convention that tells Content
Manager where to find `/api/details`. Vanilla sends the plain name.

## 5.2 `GET /JSON|<guid>` (legacy) `[K]`

The entry list as the launcher sees it. The path really contains a `|`, as in
`/JSON|76561198000000000`, where the guid is the requesting player's SteamID64.
It decides which slots that player may take.

Vanilla's exact body is `Cars` and nothing else:

```jsonc
{
  "Cars": [
    {
      "Model": "ks_mazda_mx5_cup",
      "Skin": "00_official",
      "DriverName": "Some Driver",   // "" when the slot is empty, never null
      "DriverTeam": "Some Team",     // "" when empty
      "DriverNation": "GBR",         // "" when empty
      "IsConnected": true,
      "IsRequestedGUID": false,      // see below
      "IsEntryList": true            // slot open for the requesting guid (admins: always true)
    }
  ]
}
```

Easy to get wrong:

- **Headers.** Vanilla answers `/INFO` and `/JSON` with Go's default
  `Content-Type: text/plain; charset=utf-8` and no CORS headers (seen on a real
  launcher's requests). The launcher and Content Manager parse the body
  whatever the content type, so `application/json` and
  `Access-Control-Allow-Origin: *` (for browser-based server lists) are safe.
- The top-level `Features` array is an `[AS]`/`[CM]` addition. Vanilla does not
  send it.
- The driver strings are never `null`. They name the slot's occupant, else its
  **last** driver, who stays listed after leaving until someone else takes the
  slot (doc 1 §1.5 "Which slot"). A slot nobody has used reports `""`, even when
  the entry list sets a `TEAM`, unless `GUID=` reserves it (measured on v1.15).
- **`IsEntryList`** with pickup mode on is whether the requester may take the
  slot. With it off it is vanilla's `Car.IsEntryList`: true for every
  entry-list car, reserved for someone else or not, and false for a booked one.
- **`IsRequestedGUID` means "reserved for the requesting guid"** (entry-list
  `GUID=`), not "the slot that guid occupies". A driver asking about their own
  unreserved slot gets `false`.

### Routing `[K*]`

Vanilla answers an unrouted path with `200` and an empty body, and matches
every route by prefix, so `/INFO/extra` returns `/INFO`. No client depends on
the `200`, so `404` is safe.

## 5.2a acServerManager endpoints `[K]`

Vanilla v1.15 also serves endpoints for **booking mode** and idle servers.
Despite the section name, the Kunos acServerManager GUI calls none of them: it
only writes the config files and launches the server. Bookings need another
client (Kunos's own booking tooling, or a script). Vanilla registers a single
handler and routes by path **prefix**, in this order: `/RECONFIG`,
`/INFO`, `/SUB`, `/UNSUB`, `/JSON`, `/CANJOIN`, `/ACAUTH`, `/ENTRY`. A request's
fields are separated by `|`, as in `/JSON`, and every reply is plain text. There
is no `/DTD` in v1.15.

**Booking mode** is pickup mode off (`PICKUP_MODE_ENABLED=0`) plus a `[BOOK]`
session, the first session in the list. A `[BOOK]` with `TIME=0` is ignored
with a warning, and a pickup server with a `[BOOK]` refuses to start. The car
list then holds the entry list's cars plus one per booking, so it may start
empty, and `MAX_CLIENTS` is not lowered to the entry-list size: bookings take
session ids below it.

`/SUB` and `/UNSUB` answer `FAILED_PICKUP` on a pickup server and `CLOSED`
outside the booking session, then check their field count (`SYNTAX_ERROR`).

| Request | Effect | Reply |
|---|---|---|
| `/SUB\|<car>\|<skin>\|<name>\|<team>\|<guid>[\|<password>]` | books a car: updates the guid's booking in place, or adds one with the lowest session id below `MAX_CLIENTS` that no car uses | `OK,<s>`; `SERVER FULL`; `INCORRECT PASSWORD`; `ILLEGAL CAR,<s>`; `BLACKLISTED`; `PLUGIN AUTH FAILED: <reason>` |
| `/UNSUB\|<guid>` | releases the guid's car, whatever list it came from | `OK`; `UNKNOWN GUID` |
| `/CANJOIN\|<guid>` | asks whether the guid has a car | `YES` if a listed car's driver has the guid and the session is **not** the booking session, otherwise `NO`; a malformed request is only logged, with an empty body |

`<s>` is the **seconds left** in the booking session (`TimeLeft / 1000`), not a
session id. `/SUB` checks, in order: the password (server or admin), the car
against `CARS=`, the blacklist, then `AUTH_PLUGIN_ADDRESS`. A new booking takes
`setups/<MODEL>.ini` as its fixed setup if there is one, and booking with the
admin password makes its driver an admin. Every booking and release sends the
lobby a subscriptions ping (§5.8).

Joins: a booking server admits only booked drivers (or `GUID=`-reserved ones),
into their slot whatever car they ask for, under the name and team they booked
with. During the booking session vanilla sends `0x41` with the milliseconds
left, but through its UDP socket to the address of a packet that came over TCP,
so a client hears nothing. When the booking session ends (on time, with no
`0x4B`), an empty car list runs it again ("NO CARS REGISTERED"); otherwise the
next session starts.

**Entry-list reload.** With pickup mode off (booking or not), vanilla empties
its car list and **reads `entry_list.ini` again** whenever the sessions loop
back to the first, which also drops every booking. Edits to the file therefore
take effect at the next loop: changed cars, reservations and BoP, cars added up
to `MAX_CLIENTS`, cars removed. `server_cfg.ini` is not re-read. Vanilla's new
car objects orphan every connected client, so nobody hears anything until they
reconnect. Keeping each driver whose slot still fits them (same model, still
reserved for them) and disconnecting the rest avoids that. With pickup mode off, vanilla also seats nobody as admin from the join's password
(only a booking with the admin password does), so admins use `/admin`.

**`/ACAUTH`** is Kunos's test `AUTH_PLUGIN_ADDRESS`: `OK` for the query value
`GUID=ks1`, otherwise `DENY|Sorry, you are too stupid to join here`.

**`/ENTRY`** returns an XHTML page from Kunos's `ks/khtml`, on one line after
the head: "Asetto Corsa Server: Entry List" (sic), `Session: <TYPE> [<name>],
TIME LEFT mm:ss` (`BOOK`, `PRACTICE`, `QUALIFY`, `RACE`), a table of every
listed car (`ID` from 1, driver, car, team, ping or `DC`, laps, last, best and
total times, `--:--:---` with no lap, guid), the standings by best lap (`POS`
from 0, best as `m:ss:mmm` with `16666:39:999` for none, `+` the gap to the
leader), and `Throughput: <n>kBs`, the game socket's send rate. Vanilla writes
the names into the page verbatim; since a driver picks their own name and team,
they should be HTML-escaped.

**Idle mode.** `[IDLE_MODE]` `ENABLED=1` and `TIMEOUT=<minutes>`. Once no car
has been loaded (doc 1 §1.5) for `TIMEOUT`, vanilla logs `Server is now IDLE`
and sets a flag. Nothing else in v1.15 reads it, and nothing clears it; a
loaded car restarts the count.

**`/RECONFIG?json=<json>`** parses its form value `json` into:

```
{ "Name": str, "TrackName": str, "TrackConfig": str,
  "PracticeTimeMins": int, "QualifyTimeMins": int, "RaceLaps": int,
  "Cars": [ { "Model": str, "Skin": str, "Ballast": int, "Restrictor": int } ] }
```

and answers `OK`, or the JSON decoder's own error text (e.g. `invalid
character 'o' in literal null (expecting 'u')`). In v1.15 the settings are then
**discarded**.

## 5.3 `GET /api/details?guid=<guid>` `[CM]`

Defined by ac-server-wrapper (Content Manager's server wrapper). AssettoServer
always serves it; `EnableServerDetails` (default on) only adds the `ℹ<httpPort>`
name suffix (below). CSP reads the server's `features` from it when the launcher
passed none (doc 4 §4.6). The body is every
`/INFO` field plus:

```jsonc
{
  // ... all /INFO fields ...
  "players": {
    "Cars": [
      {
        // ... all /JSON car fields ... plus:
        "DriverNation": "DEU",
        "ID": "3c9d4d9c..."       // SHA1 hex of "antarcticfurseal" + guid (anonymized player id, [CM] wrapper convention)
      }
    ]
  },
  "until": 1752900000,            // unix seconds when session ends
  "content": { ... },             // CM downloadable content manifest, see §5.4 (null if none)
  "trackBase": "ks_brands_hatch", // track without layout suffix
  "city": "Berlin",
  "frequency": 60,                // RefreshRateHz
  "assists": {
    "absState": 1, "tcState": 1,
    "fuelRate": 100, "damageMultiplier": 100, "tyreWearRate": 100,
    "allowedTyresOut": 2,
    "stabilityAllowed": false, "autoclutchAllowed": true,
    "tyreBlanketsAllowed": false, "forceVirtualMirror": false
  },
  "wrappedPort": 8081,
  "ambientTemperature": 24.0,
  "roadTemperature": 31.5,
  "currentWeatherId": "3_clear",  // WeatherFX type name when WFX active, else graphics name
  "windSpeed": 4,
  "windDirection": 90,
  "description": "…",             // server description (extra_cfg)
  "grip": 96.0,                   // track grip percent
  "gripTransfer": 0.0,
  "features": ["SPECTATING_AWARE", ...],
  "loadingImageUrl": "https://…", // random pick from configured list, or null
  "poweredBy": "AssettoServer 0.0.55",
  "extensions": { }               // plugin-defined extras
}
```

`name` here is the plain name, without the `ℹ` suffix.

## 5.4 Content Manager content downloads `[CM]`

The `content` object in `/api/details` lists content a player can download:

```jsonc
{
  "cars": {
    "<carId>": {
      "url": "https://… or /content/car/<carId>",  // omitted when null
      "version": "1.2",
      "skins": { "<skinId>": { "url": "…" } }
    }
  },
  "track": { "url": "…", "version": "1.0" }
}
```

Local entries are served by the server:

| Endpoint | Returns |
|---|---|
| `GET/HEAD /content/car/{carId}?password=<p>` | `application/zip` |
| `GET/HEAD /content/skin/{carId}/{skinId}?password=<p>` | `application/zip` |
| `GET/HEAD /content/track?password=<p>` | `application/zip` |

`password` is required only when the server has a join password. Its value is
`hex(SHA1("tanidolizedhoatzin" + serverPassword))`, a fixed salt defined by
Content Manager. A wrong or missing password gets `401`, unknown content `404`.

AssettoServer serves zips an admin prepared in advance. A server can instead
build them from its content directory on first request: the manifest's
relative URLs are resolved by CM against the `/api/details` host, and
`version` comes from `ui_car.json` / `ui_track.json`. Only entry-list models
should be served, with skin and track paths checked for traversal.

## 5.5 `GET /api/scripts/{id}` `[AS]`/`[CSP]`

Raw Lua source for the CSP server scripts listed in the extra options (doc 4
§4.5). `404` if `id` is out of range.

## 5.6 `GET /assets/...` `[AS]`

Embedded images (`/assets/logo_42.png`, `/assets/srp-logo-new.png`).

Plugin endpoints (`/automoderation/*.png`, `POST /report`) are in doc 9 §9.9.

## 5.7 `/api/configuration` (authenticated, admin only) `[AS]`

Live configuration for CSP's in-game admin UI. The caller must be a connected
admin, identified by two headers:

| Header | Value |
|---|---|
| `X-Car-Id` | the client's session ID (entry list index) |
| `X-Api-Key` | the per-connection API key the server sent via the `AS_ApiKey` Lua event (32 hex chars, random per connection) |

- `GET /api/configuration` → configuration tree, serialized as Lua
  (`Content-Type: text/x-lua`).
- `POST /api/configuration?key=<path>&value=<v>` → sets one property. The first 5
  characters of `key` are stripped (UI prefix). Response JSON:
  `{ "Status": "OK" }` or `{ "Status": "Error", "ErrorMessage": "…" }`.

## 5.8 Outbound: Kunos lobby registration `[K]`

Needed to appear in the in-game server list. The server sends a GET to
`http://93.57.10.21/lobby.ashx/register` (fallback
`http://lobby.assettocorsa.net/lobby.ashx/register`) with these query parameters:

`name, port (UDP), tcp_port, max_clients` (**`min(MAX_CLIENTS, entry-list size)`**, unlike
`/INFO`, which reports the raw `MAX_CLIENTS`), `track, cars` (comma-separated, truncated to
≤ 1200 chars at a comma boundary), `timeofday` (sun angle int), `sessions`
(comma-separated type ints), `durations` (laps for a lap race, else **seconds** —
`minutes*60`; note `/INFO` reports raw minutes), `password` (0/1), `version=202`, `pickup=1`,
`autoclutch` (0/1), `abs`, `tc`, `stability` (0/1), `legal_tyres`, `fixed_setup` (0/1),
`timed` (0/1), `extra` (0/1), `pit` (0/1), `inverted`.

`[AS]` With `EnableServerDetails`, `name` carries the `ℹ<httpPort>` suffix, as
in `/INFO`, as AssettoServer registers it. Content Manager finds
`/api/details`, and with it the server's required CSP features, through that
suffix, so without it a player who picks the server from the lobby list joins
with an empty feature list and a WeatherFX server refuses them.

A success body starts with `OK`. The server then calls `…/lobby.ashx/ping` in
**three shapes** (vanilla `LobbyPingRun`, `LobbyUpdateSession` and
`LobbyUpdateSubscriptions`):

| Call | Parameters | When |
|---|---|---|
| keepalive | `port`, `tcp_port`, `timeleft` (seconds), `clients`, `pickup=1` | every 60 s |
| session update | `session` (current **type**), `timeleft`, `port`, `clients`, `track`, `pickup=1` | immediately after a successful register, and at every session transition |
| subscriptions update | `clients`, `port`, `tcp_port` | on every join and leave (and booking-mode `/SUB` / `/UNSUB`) |

`clients` is the number of ready clients (doc 1 §1.5) in every shape.

The lobby checks reachability with a UDP `0xC8` probe and a fetch of `/INFO`.

Vanilla quirks:

- After a session's clock runs out, the keepalive `timeleft` underflows to a huge
  unsigned value (`4294967236` for −60 s).
- The lobby can forget a registration while still answering pings with HTTP 200
  and a body of `ERROR - RESTART YOUR SERVER TO REGISTER WITH THE LOBBY`. It also
  rate-limits re-registration (`TOO MANY REGISTRATION ATTEMPTS, SLOW DOWN`), so
  re-register on that reply with backoff.
