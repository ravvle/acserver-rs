//! One TCP connection: accept, the read and write tasks, dispatch and
//! disconnect.
//!
//! Each connection has two tasks:
//! - the client loop, which reads framed packets with the client's read
//!   deadline and dispatches each one;
//! - a writer, the client's only TCP writer, draining its send queue.
//!
//! Packets are read with `tokio_util`'s `FramedRead`, whose reads can be
//! abandoned without losing a half-read packet, so a deadline can change
//! while the loop waits (the handshake, then loading, then none).

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use futures_util::StreamExt as _;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{OwnedSemaphorePermit, mpsc};
use tokio_util::codec::{FramedRead, LengthDelimitedCodec};
use tokio_util::sync::CancellationToken;

use super::Server;
use super::client::{Client, Frame, SEND_QUEUE_DEPTH, SendError};
use crate::proto;
use crate::wire::{self, Reader};

/// Bounds the wait for 0x3D. Real clients send it at once.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long one write may take before the peer is treated as dead.
const TCP_WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// The write timeout while flushing after a disconnect, so teardown cannot
/// hang on a peer that stopped reading.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// The most frames batched into one write. Batching only frames that are
/// already queued adds no latency.
const WRITE_BATCH_FRAMES: usize = 64;

/// The TCP framing: a u16 little-endian length, then the payload (docs/01
/// §1.3). A longer declared length than `MAX_TCP_RECV` closes the connection.
type Packets = FramedRead<OwnedReadHalf, LengthDelimitedCodec>;

fn packets(read_half: OwnedReadHalf) -> Packets {
    LengthDelimitedCodec::builder()
        .length_field_type::<u16>()
        .little_endian()
        .max_frame_length(wire::MAX_TCP_RECV)
        .new_read(read_half)
}

impl Server {
    /// Frames a payload and queues it for the client's writer. A client that
    /// falls `SEND_QUEUE_DEPTH` frames behind is dropped, so one stuck peer
    /// can neither block broadcasts nor grow its buffer without bound.
    pub(crate) fn send_tcp(&self, client: &Client, payload: &[u8]) {
        let result = client.queue_payload(payload);
        self.handle_send_result(client, result);
    }

    /// Queues an already-framed packet (shared by a broadcast).
    pub(crate) fn queue_frame(&self, client: &Client, frame: Frame) {
        let result = client.queue_frame(frame);
        self.handle_send_result(client, result);
    }

    fn handle_send_result(&self, client: &Client, result: Result<(), SendError>) {
        if result == Err(SendError::QueueFull) {
            tracing::warn!(
                car = client.session_id(),
                name = client.name(),
                "tcp send queue full, dropping client"
            );
            if let Some(owner) = self.client_arc(client) {
                self.disconnect(&owner, "send queue full");
            } else {
                // Not yet in a slot: just stop its writer.
                client.disconnected.store(true, Ordering::SeqCst);
                client.close_queue();
            }
        }
    }

    /// The `Arc` the server holds for a client, found through its slot.
    fn client_arc(&self, client: &Client) -> Option<Arc<Client>> {
        let car = self.entry_cars.get(usize::from(client.session_id()))?;
        let owner = car.client()?;
        std::ptr::eq(Arc::as_ptr(&owner), client).then_some(owner)
    }

    /// Accepts connections until shutdown.
    pub(crate) async fn accept_loop(self: Arc<Self>, listener: tokio::net::TcpListener) {
        loop {
            let Some(accepted) = self.shutdown.run_until_cancelled(listener.accept()).await else {
                return;
            };
            let Ok((stream, peer)) = accepted else {
                continue;
            };
            // Taken here, in arrival order, and held until the handshake
            // completes or the connection closes.
            let Ok(pending) = Arc::clone(&self.pending).try_acquire_owned() else {
                tracing::warn!(addr = %peer, "connection refused: too many pending handshakes");
                continue;
            };
            let server = Arc::clone(&self);
            self.spawn_task(async move {
                server.handle_connection(stream, peer, pending).await;
            });
        }
    }

    /// Drives one TCP connection through the handshake and the packet loop.
    async fn handle_connection(
        self: Arc<Self>,
        stream: TcpStream,
        peer: SocketAddr,
        pending: OwnedSemaphorePermit,
    ) {
        // Vanilla's socket options.
        let _ = stream.set_nodelay(true);
        set_keepalive(&stream);

        let (read_half, write_half) = stream.into_split();
        let (frame_tx, frame_rx) = mpsc::channel::<Frame>(SEND_QUEUE_DEPTH);
        let client = Arc::new(Client::new(peer, frame_tx, self.position_rate_limit()));

        // Cancelled when the writer has closed the socket, to stop the loop.
        let closed = CancellationToken::new();
        // Start the writer first so a handshake reply or rejection can be sent.
        let writer_server = Arc::clone(&self);
        let writer_client = Arc::clone(&client);
        let writer_closed = closed.clone();
        self.spawn_task(async move {
            writer_server
                .write_loop(writer_client, write_half, frame_rx)
                .await;
            writer_closed.cancel();
        });

        client.set_read_deadline(Some(Instant::now() + HANDSHAKE_TIMEOUT));
        let reason = self
            .client_loop(&client, packets(read_half), closed, pending)
            .await;
        self.disconnect(&client, reason);
    }

    /// Reads and dispatches packets until the connection closes, and returns
    /// why it stopped.
    async fn client_loop(
        self: &Arc<Self>,
        client: &Arc<Client>,
        mut packets: Packets,
        closed: CancellationToken,
        pending: OwnedSemaphorePermit,
    ) -> &'static str {
        let mut pending = Some(pending);
        let mut handshake_done = false;
        loop {
            let deadline = client.read_deadline();
            let next = tokio::select! {
                // The writer closed the socket.
                _ = closed.cancelled() => return "read error",
                next = read_before(&mut packets, deadline) => next,
            };
            let next = match next {
                Some(next) => next,
                // The deadline may have moved or been cleared while we
                // waited; only an unchanged one has expired.
                None if client.read_deadline() != deadline => continue,
                None if !handshake_done => {
                    // A socket that never sent 0x3D (a scanner or probe) is
                    // not a driver.
                    tracing::debug!(remote = %client.peer, "handshake timed out");
                    return "handshake timeout";
                }
                None => {
                    tracing::info!(
                        car = client.session_id(),
                        name = client.name(),
                        loaded = client.has_sent_first_update.load(Ordering::SeqCst),
                        after_secs = client.seconds_since_join(),
                        "client timed out"
                    );
                    return "timed out";
                }
            };
            let payload = match next {
                Some(Ok(payload)) => payload,
                // The peer closed the connection.
                None => return "connection closed",
                Some(Err(err)) => {
                    if !client.disconnected.load(Ordering::SeqCst) {
                        tracing::debug!(car = client.session_id(), remote = %client.peer, %err, "tcp read");
                    }
                    return "read error";
                }
            };
            // A zero length ends the connection, as end of stream does.
            let Some((&id, body)) = payload.split_first() else {
                return "connection closed";
            };
            let mut r = Reader::new(body);

            if !handshake_done {
                // Nothing but 0x3D before the handshake.
                if id != proto::REQUEST_NEW_CONNECTION {
                    tracing::debug!(id, remote = %client.peer, "pre-handshake packet dropped");
                    return "pre-handshake packet";
                }
                if !self.handle_handshake(client, &mut r).await {
                    return "handshake rejected";
                }
                handshake_done = true;
                pending.take();
                // Now the loading deadline applies.
                let minutes = self.cfg.extra.player_loading_timeout_minutes.max(0) as u64;
                client.set_read_deadline(Some(Instant::now() + Duration::from_secs(minutes * 60)));
                continue;
            }

            self.dispatch_tcp(client, id, &mut r);
        }
    }

    /// Routes a post-handshake TCP packet to its handler.
    fn dispatch_tcp(self: &Arc<Self>, client: &Arc<Client>, id: u8, r: &mut Reader) {
        match id {
            proto::CLEAN_EXIT_DRIVE => self.disconnect(client, "quit"),
            // Vanilla kicks a client that sends 0x3B (docs/02 0x3B).
            proto::BLACKLISTED => {
                self.kick_car("client 0x3B", client.session_id());
            }
            proto::CHECKSUM_PACKET => self.handle_checksum(client, r),
            proto::CHAT => self.handle_chat(client, r),
            proto::CAR_LIST_REQUEST => self.handle_car_list_request(client, r),
            proto::P2P_UPDATE => self.handle_p2p(client, r),
            proto::MANDATORY_PIT_UPDATE => self.handle_mandatory_pit(client, r),
            proto::TYRE_COMPOUND_CHANGE => self.handle_tyre_compound(client, r),
            proto::DAMAGE_UPDATE => self.handle_damage(client, r),
            proto::SECTOR_SPLIT => self.handle_sector_split(client, r),
            proto::LAP_COMPLETED => self.handle_lap_completed(client, r),
            proto::VOTE_NEXT_SESSION | proto::VOTE_RESTART_SESSION | proto::VOTE_KICK_USER => {
                self.handle_vote(client, id, r);
            }
            proto::CLIENT_EVENT => self.handle_client_event(client, r),
            proto::EXTENDED => self.handle_extended_tcp(client, r),
            _ => tracing::debug!(id, car = client.session_id(), "unhandled tcp packet"),
        }
    }

    /// Tears down the client, frees its slot, and notifies others once. The
    /// first caller's reason is the one logged.
    pub(crate) fn disconnect(&self, client: &Arc<Client>, reason: &str) {
        if client.disconnected.swap(true, Ordering::SeqCst) {
            return;
        }
        // The writer flushes and closes the socket; closing it here would
        // race a final kick or rejection frame.
        client.close_queue();
        self.unregister_udp(client);

        let id = client.session_id();
        let Some(car) = self.entry_cars.get(usize::from(id)) else {
            return; // never handshaked
        };
        // Only if this client still held the slot.
        if !car.release_client(client) {
            return;
        }
        // Results stay with the slot until the session changes, as in vanilla.
        car.reset_occupant_state();

        tracing::info!(
            car = id,
            name = client.name(),
            reason,
            loaded = client.has_sent_first_update.load(Ordering::SeqCst),
            after_secs = client.seconds_since_join(),
            "client disconnected"
        );
        // A kick is announced by its 0x68 alone; vanilla sends no 0x4D.
        if !client.kicked.load(Ordering::SeqCst) {
            self.broadcast_car_disconnected(id);
        }
        self.traffic_on_leave(car);
        self.plugins_disconnect(client);
        self.plugin().on_client_disconnected(client);
    }

    /// The client's only TCP writer. After `disconnect` takes the queue's
    /// sender away it flushes what is queued (so a kick or rejection sent
    /// just before still goes out) and closes the connection.
    async fn write_loop(
        &self,
        client: Arc<Client>,
        mut socket: OwnedWriteHalf,
        mut frames: mpsc::Receiver<Frame>,
    ) {
        let mut batch: Vec<u8> = Vec::with_capacity(4096);
        loop {
            let first = tokio::select! {
                // Shutdown: dropping the socket also stops the reader.
                _ = self.shutdown.cancelled() => return,
                frame = frames.recv() => frame,
            };
            // None: the queue is closed and drained, so we are done.
            let Some(first) = first else {
                let _ = socket.shutdown().await;
                return;
            };
            batch.clear();
            batch.extend_from_slice(&first);
            for _ in 1..WRITE_BATCH_FRAMES {
                match frames.try_recv() {
                    Ok(frame) => batch.extend_from_slice(&frame),
                    Err(_) => break,
                }
            }
            let timeout = if client.disconnected.load(Ordering::SeqCst) {
                FLUSH_TIMEOUT
            } else {
                TCP_WRITE_TIMEOUT
            };
            let written = tokio::time::timeout(timeout, socket.write_all(&batch)).await;
            if !matches!(written, Ok(Ok(()))) {
                if !client.disconnected.load(Ordering::SeqCst) {
                    tracing::debug!(
                        car = client.session_id(),
                        name = client.name(),
                        "tcp write failed"
                    );
                }
                self.disconnect(&client, "write error");
                return;
            }
        }
    }
}

/// The next packet, or `None` if `deadline` passes first.
async fn read_before(
    packets: &mut Packets,
    deadline: Option<Instant>,
) -> Option<Option<std::io::Result<bytes::BytesMut>>> {
    match deadline {
        None => Some(packets.next().await),
        Some(deadline) => tokio::time::timeout_at(deadline.into(), packets.next())
            .await
            .ok(),
    }
}

/// Vanilla's keepalive: on, every 60 s.
fn set_keepalive(stream: &TcpStream) {
    let socket = socket2::SockRef::from(stream);
    let keepalive = socket2::TcpKeepalive::new().with_time(Duration::from_secs(60));
    let _ = socket.set_tcp_keepalive(&keepalive);
}
