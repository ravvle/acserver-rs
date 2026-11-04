# Assetto Corsa Multiplayer Protocol

A language-agnostic reference for the Assetto Corsa 1.x multiplayer network
protocol (game protocol **202**, plugin protocol ACSP **v4**). It is written so a
compatible server, client or admin tool can be built without reading any
existing source.

The layouts were documented from
[AssettoServer](https://github.com/compujuckel/AssettoServer) (`master`, July
2026) and checked against the original Kunos `acServer` v1.15, packet by
packet. Wherever the two differ, both behaviours are given.

## Documents

| # | Document | Covers |
|---|---|---|
| 1 | [Overview](01-overview.md) | Wire types, framing, packet-id registry, connection lifecycle, sessions |
| 2 | [TCP protocol](02-tcp-protocol.md) | Every reliable game packet, chat commands, votes |
| 3 | [UDP protocol](03-udp-protocol.md) | Association, positions, ping, lobby probe |
| 4 | [CSP extensions](04-csp-extensions.md) | `0xAB` packets, Lua events, chat-encoded data |
| 5 | [HTTP API](05-http-api.md) | `/INFO`, `/JSON`, Content Manager endpoints, lobby registration |
| 6 | [Plugin API](06-udp-plugin-api.md) | ACSP v4 over UDP |
| 7 | [RCON](07-rcon.md) | Source-style remote console |
| 8 | [Kunos compatibility](08-kunos-compatibility.md) | What a drop-in replacement must implement, and every known AssettoServer ↔ vanilla difference |
| 9 | [AssettoServer features](09-assettoserver-features.md) | AssettoServer's own features: extra options, Lua events, WeatherFX, AI traffic, kicks, user groups, plugins |

The [official Kunos server guide](assettoserverguideofficial.md) is kept as the
configuration reference.

## Ports

| Surface | Transport | Config key |
|---|---|---|
| Game (reliable) | TCP | `TCP_PORT` |
| Game (realtime) | UDP | `UDP_PORT` (usually the same number as TCP) |
| HTTP | TCP | `HTTP_PORT` |
| Plugin API | UDP | `UDP_PLUGIN_ADDRESS` (send), `UDP_PLUGIN_LOCAL_PORT` (listen) |
| RCON | TCP | `RconPort` (AssettoServer `extra_cfg.yml`) |

## Origin tags

Each packet, field and behaviour is tagged by where it comes from:

| Tag | Origin | Needed for a Kunos drop-in? |
|---|---|---|
| `[K]` | Original Kunos `acServer` | Yes |
| `[K*]` | Kunos feature where AssettoServer behaves differently. The text gives both | Yes, the vanilla behaviour |
| `[AS]` | Added by AssettoServer | No |
| `[CSP]` | Custom Shaders Patch client mod | No. Vanilla clients never send it and CSP clients cope without it |
| `[CM]` | Content Manager / ac-server-wrapper conventions | No |

Untagged content in docs 1–3 and 6 is `[K]`.
