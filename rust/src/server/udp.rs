//! UDP receive: association (0x4E), positions (0x46), pongs (0xF8), the
//! lobby and time probes, and the per-source limiter.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::Server;
use super::client::Client;
use super::entrycar::PositionUpdate;
use crate::proto;
use crate::wire::{Reader, Writer};

/// The minimum gap, in ms, between "unexpected source" warnings for one
/// client.
const UDP_SOURCE_WARN_INTERVAL_MS: i64 = 10_000;

/// The floor of the per-client 0x46 budget per second.
const MIN_POSITION_RATE_LIMIT: u32 = 100;

/// Whether two addresses plausibly belong to one peer. Compares by /24 (IPv6
/// /48) rather than exactly, because carrier-grade NAT can put a client's two
/// flows on different addresses from one pool.
pub fn same_host(tcp: SocketAddr, udp: SocketAddr) -> bool {
    let t = tcp.ip().to_canonical();
    let u = udp.ip().to_canonical();
    match (t, u) {
        (IpAddr::V4(t), IpAddr::V4(u)) => t.octets()[..3] == u.octets()[..3],
        (IpAddr::V6(t), IpAddr::V6(u)) => t.segments()[..3] == u.segments()[..3],
        _ => false,
    }
}

/// [AS] hardening: NaN or out-of-range values can crash other clients.
fn valid_position(p: &PositionUpdate) -> bool {
    [p.position, p.rotation, p.velocity]
        .iter()
        .all(|v| v.is_finite())
        && p.position.abs().max_element() <= 100_000.0
        && p.velocity.abs().max_element() <= 500.0
}

/// Caps replies to unassociated probes per source IP: 20 at once, refilled
/// at 20 a second. Sources idle for a second are dropped every second
/// (`forget_idle_sources`), so spoofed sources cannot grow the table past one
/// second of traffic. The budget sits far above legitimate use, so players
/// sharing a NAT never lose a reply.
pub type SourceLimiter = governor::DefaultKeyedRateLimiter<IpAddr>;

pub fn source_limiter() -> SourceLimiter {
    governor::RateLimiter::keyed(governor::Quota::per_second(
        std::num::NonZeroU32::new(20).expect("non-zero"),
    ))
}

impl Server {
    /// Writes a datagram, counting it for /ENTRY's throughput. A full socket
    /// buffer makes it wait briefly.
    pub(crate) fn send_udp(&self, payload: &[u8], addr: SocketAddr) {
        let Some(socket) = self.udp_socket.get() else {
            return;
        };
        match socket.send_to(payload, addr) {
            Ok(_) => {
                self.udp_bytes
                    .fetch_add(payload.len() as i64, Ordering::Relaxed);
            }
            Err(err) => tracing::debug!(%addr, %err, "udp write failed"),
        }
    }

    /// Sends a datagram to a client's associated endpoint, if it has one.
    pub(crate) fn send_udp_to_client(&self, client: &Client, payload: &[u8]) {
        if let Some(addr) = client.udp_addr() {
            self.send_udp(payload, addr);
        }
    }

    /// Handles one datagram.
    pub(crate) fn handle_udp(self: &Arc<Self>, packet: &[u8], addr: SocketAddr) {
        let Some((&id, body)) = packet.split_first() else {
            return;
        };
        let mut r = Reader::new(body);

        match id {
            proto::LOBBY_CHECK => {
                // Answered without association, which makes it a reflector for
                // spoofed sources, so replies are capped per source IP (docs/03
                // 0xC8).
                if self.lobby_probes.check_key(&addr.ip()).is_err() {
                    return;
                }
                let mut w = Writer::with_id(proto::LOBBY_CHECK);
                w.u16(self.cfg.server.http_port);
                self.send_udp(w.bytes(), addr);
                return;
            }
            proto::CAR_CONNECT => {
                self.handle_udp_association(&mut r, addr);
                return;
            }
            proto::PING_UPDATE => {
                // A client-sent 0xF9 is answered at once, from any source, as
                // vanilla's socket layer does (docs/03 0xF9). The same per-IP
                // cap as 0xC8 keeps it from being a reflector.
                self.handle_time_request(&mut r, addr);
                return;
            }
            _ => {}
        }

        // All other packets require an associated client.
        let Some(client) = self.client_by_udp(addr) else {
            return;
        };
        match id {
            proto::POSITION_UPDATE => self.handle_position_update(&client, &mut r),
            proto::PING_PONG => self.handle_pong(&client, &mut r),
            proto::SESSION_REQUEST => self.handle_session_request(&client, &mut r),
            proto::EXTENDED => self.handle_extended_udp(&client, &mut r),
            proto::PULSE => {
                // Vanilla marks the connection ready on 0x4C without sending
                // the first update (docs/03 0x4C).
                client.ready.store(true, Ordering::SeqCst);
            }
            _ => {}
        }
    }

    /// Answers a client's 0xF9 with 0xF8: its u64 echoed, then the server
    /// clock as a u32.
    fn handle_time_request(&self, r: &mut Reader, addr: SocketAddr) {
        let token = r.u64();
        if r.overflowed() || self.time_probes.check_key(&addr.ip()).is_err() {
            return;
        }
        let mut w = Writer::with_id(proto::PING_PONG);
        w.u64(token);
        w.u32(self.server_time_millis() as u32);
        self.send_udp(w.bytes(), addr);
    }

    /// Warns about a refused association, at most once per interval.
    fn warn_udp_source(&self, client: &Client, message: &str, id: u8, addr: SocketAddr) {
        let now = self.uptime_ms();
        let last = client.udp_source_warned_ms.load(Ordering::SeqCst);
        if now.saturating_sub(last) >= UDP_SOURCE_WARN_INTERVAL_MS {
            client.udp_source_warned_ms.store(now, Ordering::SeqCst);
            tracing::warn!(car = id, tcp = %client.peer, udp = %addr, old = ?client.udp_addr(), "{message}");
        }
    }

    /// Binds a UDP endpoint to a connected client (0x4E).
    fn handle_udp_association(&self, r: &mut Reader, addr: SocketAddr) {
        let id = r.u8();
        let Some(car) = self.car_by_id(id) else {
            return;
        };
        let Some(client) = car.client() else { return };
        // Stops another host claiming an unassociated slot (docs/03 0x4E).
        // Off by default because carrier-grade NAT can still split a real
        // client's flows.
        if self.cfg.extra.validate_udp_source && !same_host(client.peer, addr) {
            self.warn_udp_source(&client, "udp association from unexpected source", id, addr);
            return;
        }
        // Vanilla answers every 0x4E and moves the client to the newest
        // endpoint, which is how a player survives a NAT rebinding (docs/03
        // 0x4E).
        let old = client.udp_addr();
        if old == Some(addr) {
            self.send_udp(&[proto::CAR_CONNECT], addr);
            return;
        }
        // [AS] hardening: a move must stay on the old address or the TCP
        // peer's host, so a third party cannot take over a live player's UDP.
        if let Some(old) = old
            && old.ip() != addr.ip()
            && !same_host(client.peer, addr)
        {
            self.warn_udp_source(
                &client,
                "udp re-association from another host refused",
                id,
                addr,
            );
            return;
        }
        client.set_udp_addr(Some(addr));
        {
            let mut map = self.udp_clients.write();
            if let Some(old) = old {
                remove_if_same(&mut map, old, &client);
            }
            map.insert(addr, Arc::clone(&client));
        }
        match old {
            Some(old) => tracing::info!(car = id, from = %old, to = %addr, "udp endpoint moved"),
            None => tracing::debug!(car = id, %addr, "udp associated"),
        }
        // Disconnect sets the flag before unregistering, so if it ran
        // meanwhile this is the mapping it missed.
        if client.disconnected.load(Ordering::SeqCst) {
            let mut map = self.udp_clients.write();
            remove_if_same(&mut map, addr, &client);
            return;
        }
        self.send_udp(&[proto::CAR_CONNECT], addr);
    }

    /// The client that owns a UDP source endpoint.
    fn client_by_udp(&self, addr: SocketAddr) -> Option<Arc<Client>> {
        self.udp_clients.read().get(&addr).cloned()
    }

    /// Removes the client's endpoint mapping, unless a newer client reused the
    /// same address and port.
    pub(crate) fn unregister_udp(&self, client: &Arc<Client>) {
        if let Some(addr) = client.udp_addr() {
            let mut map = self.udp_clients.write();
            remove_if_same(&mut map, addr, client);
        }
    }

    /// Decodes an inbound 0x46, validates it, stores it, and triggers the
    /// first-update sequence.
    fn handle_position_update(self: &Arc<Self>, client: &Arc<Client>, r: &mut Reader) {
        // Not gated on the checksum, as in vanilla: a bad checksum is a
        // deferred kick after the first update (docs/01 §1.5).
        let mut p = PositionUpdate {
            sequence_id: r.u8(),
            timestamp: r.u32(),
            position: r.vec3(),
            rotation: r.vec3(),
            velocity: r.vec3(),
            tyre_angular_speed: [r.u8(), r.u8(), r.u8(), r.u8()],
            steer_angle: r.u8(),
            wheel_angle: r.u8(),
            engine_rpm: r.u16(),
            gear: r.u8(),
            status_flags: r.u32(),
            performance_delta: r.i16(),
            gas: r.u8(),
            normalized_pos: 0.0,
        };
        // 57 bytes so far; NormalizedPosition makes 61 (62 with the id).
        if r.overflowed() {
            return;
        }
        if r.remaining() >= 4 {
            p.normalized_pos = r.f32();
        }

        if self.cfg.extra.validate_position_updates && !valid_position(&p) {
            tracing::debug!(
                car = client.session_id(),
                name = client.name(),
                "position update rejected: implausible values"
            );
            return;
        }

        // [AS] relay budget, far above any client's send rate (docs/03 0x46).
        if client.position_budget.check().is_err() {
            return;
        }

        // The first 0x46 makes the client ready and triggers the snapshot,
        // before the timestamp check, as vanilla's Car.OnMessage does. The
        // snapshot runs on its own task so the receive loop keeps draining
        // the socket.
        client.ready.store(true, Ordering::SeqCst);
        if !client.has_sent_first_update.load(Ordering::SeqCst)
            && client
                .first_update_pending
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            let server = Arc::clone(self);
            let first = Arc::clone(client);
            self.spawn_task(async move {
                server.send_first_update_sequence(&first);
            });
        }

        // Vanilla slews the clock offset on every 0x46, stale or not, then
        // drops an update whose timestamp does not advance (docs/03 0x46). An
        // accepted one waits for the next relay pass. The clock lock keeps a
        // rebase from landing between the slew and the stored offset.
        let car = &self.entry_cars[usize::from(client.session_id())];
        let previous = car.last_position();
        let accepted = {
            let _clock = self.clock_lock.read();
            client.slew_offset();
            car.set_position(p, client.time_offset())
        };
        if accepted {
            car.relay_pending.store(true, Ordering::SeqCst);
            self.note_activity(client, previous, &p);
            self.plugins_position(client, previous, &p);
        }
    }

    /// The per-client 0x46 budget: twice the configured client send rate,
    /// and never below `MIN_POSITION_RATE_LIMIT`.
    pub(crate) fn position_rate_limit(&self) -> u32 {
        MIN_POSITION_RATE_LIMIT.max(2 * u32::from(self.cfg.server.refresh_rate_hz))
    }

    /// Drops probe sources that have been quiet for a second.
    pub(crate) fn forget_idle_sources(&self) {
        self.lobby_probes.retain_recent();
        self.time_probes.retain_recent();
    }

    /// Resyncs a late joiner whose believed session differs from the current
    /// one (0x4F).
    fn handle_session_request(&self, client: &Client, r: &mut Reader) {
        let believed = r.u8();
        let (cur, _) = self.sessions.current();
        if believed != cur.kind.as_u8() {
            self.send_current_session(client);
        }
    }

    /// Feeds a 0xF8 round trip to `record_pong`.
    fn handle_pong(&self, client: &Arc<Client>, r: &mut Reader) {
        let sent_time = r.i32();
        let client_time = r.i32();
        // Held so a clock rebase cannot land between the reading and the store.
        let _clock = self.clock_lock.read();
        let now = self.server_time_millis() as i32;
        client.record_pong(
            now.wrapping_sub(sent_time),
            now,
            client_time,
            self.uptime_ms(),
        );
        drop(_clock);
        self.check_ping(client);
    }
}

/// Removes `addr` from the map only if it still points at `client`.
fn remove_if_same(
    map: &mut HashMap<SocketAddr, Arc<Client>>,
    addr: SocketAddr,
    client: &Arc<Client>,
) {
    if map
        .get(&addr)
        .is_some_and(|current| Arc::ptr_eq(current, client))
    {
        map.remove(&addr);
    }
}
