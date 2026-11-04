//! The optional Source-RCON console (docs/07), an AssettoServer addition.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use tokio::net::TcpStream;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use crate::server::{Server, secret_equal};
use crate::wire::{Reader, Writer};

// RCON packet types.
const TYPE_AUTH: i32 = 3;
const TYPE_EXEC_COMMAND: i32 = 2;
const TYPE_AUTH_RESPONSE: i32 = 2;
const TYPE_RESPONSE: i32 = 0;

/// Bounds how long a connection may stay unauthenticated.
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);

/// Caps simultaneously open RCON connections.
const MAX_CONNECTIONS: usize = 8;

/// Accepts RCON connections until shutdown, if RCON is enabled.
pub async fn run(server: Arc<Server>) -> anyhow::Result<()> {
    let extra = &server.cfg.extra;
    if !extra.enable_rcon || extra.rcon_port == 0 {
        return Ok(());
    }
    let port = extra.rcon_port as u16;
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!(port, "rcon listening");
    // One permit per open connection, authenticated or not.
    let connections = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    loop {
        let Some(accepted) = server.shutdown.run_until_cancelled(listener.accept()).await else {
            return Ok(());
        };
        let Ok((stream, addr)) = accepted else {
            continue;
        };
        let Ok(permit) = Arc::clone(&connections).try_acquire_owned() else {
            tracing::warn!(%addr, "rcon connection refused: too many open");
            continue;
        };
        let server = Arc::clone(&server);
        tokio::spawn(async move {
            handle(&server, stream).await;
            drop(permit);
        });
    }
}

/// RCON framing: an i32 little-endian length, then the request id, type,
/// body and two NULs.
type Packets = Framed<TcpStream, LengthDelimitedCodec>;

async fn handle(server: &Arc<Server>, stream: TcpStream) {
    let addr = stream.peer_addr().ok();
    let mut packets = LengthDelimitedCodec::builder()
        .length_field_type::<u32>()
        .little_endian()
        .max_frame_length(4096)
        .new_framed(stream);
    let mut authed = false;
    loop {
        let packet = if authed {
            // Authenticated consoles may idle.
            read_packet(&mut packets).await
        } else {
            match tokio::time::timeout(AUTH_TIMEOUT, read_packet(&mut packets)).await {
                Ok(packet) => packet,
                Err(_) => return,
            }
        };
        let Some((request_id, kind, body)) = packet else {
            return;
        };
        if !authed {
            if kind != TYPE_AUTH {
                return; // any pre-auth non-auth packet closes the connection
            }
            let password = &server.cfg.server.admin_password;
            if !password.is_empty() && secret_equal(&body, password) {
                authed = true;
                tracing::info!(?addr, "rcon authenticated");
                write_packet(&mut packets, request_id, TYPE_AUTH_RESPONSE, "").await;
            } else {
                // Logged: the connection cap is the only bound on guessing.
                tracing::warn!(?addr, "rcon authentication failed");
                write_packet(&mut packets, -1, TYPE_AUTH_RESPONSE, "").await;
                return;
            }
            continue;
        }
        if kind == TYPE_EXEC_COMMAND {
            let out = server.execute_admin_command("rcon", &body);
            write_packet(&mut packets, request_id, TYPE_RESPONSE, &out).await;
        }
    }
}

/// Reads one request: (request id, type, body). A frame under 8 bytes
/// closes the connection.
async fn read_packet(packets: &mut Packets) -> Option<(i32, i32, String)> {
    let payload = packets.next().await?.ok()?;
    if payload.len() < 8 {
        return None;
    }
    let mut r = Reader::new(&payload);
    let request_id = r.i32();
    let kind = r.i32();
    // The body, without its two trailing NULs.
    let body = &payload[8..(payload.len() - 2).max(8)];
    Some((request_id, kind, String::from_utf8_lossy(body).into_owned()))
}

/// Writes a response.
async fn write_packet(packets: &mut Packets, request_id: i32, kind: i32, body: &str) {
    let mut w = Writer::with_capacity(10 + body.len());
    w.i32(request_id);
    w.i32(kind);
    w.raw(body.as_bytes());
    w.raw(&[0, 0]);
    let _ = packets.send(bytes::Bytes::from(w.into_bytes())).await;
}
