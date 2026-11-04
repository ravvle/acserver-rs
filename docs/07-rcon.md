# 7. RCON Protocol

**`[AS]` — an AssettoServer addition.** Vanilla acServer has no RCON; it is
administered through the plugin API (doc 6) and admin chat commands. A Kunos
replacement can skip this document.

A TCP console following Valve's Source RCON format. Enable it with `EnableRcon`
and `RconPort` in `extra_cfg.yml`.

## 7.1 Framing

Every RCON packet (both directions) is framed as:

```
i32 length     // payload length, excluding these 4 bytes
i32 request_id
i32 type
...body
u8  0x00       // trailing null appended by the framing layer (outbound)
```

Types:

| Direction | Type | Name |
|---|---|---|
| client → server | 3 | Auth |
| client → server | 2 | ExecCommand |
| server → client | 2 | AuthResponse |
| server → client | 0 | ResponseValue |

## 7.2 Client → server

### Auth (type 3)

Body: the RCON password as bytes. The server reads
`payload[8 .. length-2]` — i.e. it expects **two** trailing null bytes after the
password (standard Source RCON encoding: null-terminated string + empty string).

The password is checked against the server's **admin password**. Success →
`AuthResponse` echoing the request ID. Failure → `AuthResponse` with request ID `-1`,
then the connection is closed. Any packet before successful auth that is not Auth closes
the connection.

### ExecCommand (type 2)

Body: the command text (same two-trailing-null encoding), for example `kick 3`.

The command runs with admin rights, as a chat command without its `/`.
Vanilla has no RCON, so the command set is the server's own (AssettoServer's:
doc 8 §8.3).

## 7.3 Server → client

### AuthResponse (type 2)

Body: single `0x00` byte (plus the framing null → two nulls total on the wire).

### ResponseValue (type 0)

Sent after each command. The body is command output followed by `0x00` (plus the
framing null); a command with no output has an empty body. One ResponseValue
is sent per command.
