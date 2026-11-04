//! Vanilla's main-loop pass every SLEEP_TIME ms (relay, pings, sun, 0x57)
//! and the 1 s ping-timeout check.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::Server;
use super::broadcast::build_race_start;
use super::client::{Client, MAX_SESSION_SLOTS};
use super::entrycar::PositionUpdate;
use crate::proto;
use crate::wire::{Vec3, Writer};

/// Vanilla's main-loop sleep while no client is ready.
const IDLE_RELAY_SLEEP: Duration = Duration::from_millis(50);

/// How long after a session's start 0x57 keeps going out. Measured on v1.15:
/// the last one goes out 4.0–5.0 s after it.
const RACE_START_WINDOW_MS: i64 = 5000;

/// Vanilla's period for both the per-car ping and the 0x57 round: more than
/// a second since the last one, in µs.
const STEP_INTERVAL_US: i64 = 1_000_000;

/// The ping timeout once a pong has arrived ([AS]).
const PING_TIMEOUT_MS: i64 = 15_000;

/// Where a relayed 0x46 carries its timestamp: after the id, the sender's
/// session id and the sequence number (see `build_position_update`).
const POSITION_TIMESTAMP: usize = 3;

/// A ready client as one relay pass sees it.
pub(crate) struct ReadyClient {
    pub(crate) session_id: u8,
    pub(crate) client: Arc<Client>,
    /// Its UDP endpoint; `None` if it has lost it since becoming ready.
    pub(crate) addr: Option<std::net::SocketAddr>,
    /// The clock offset stored with its car's last accepted update.
    pub(crate) offset: i64,
    /// Takes positions as `0xAB 0x03` batches (docs/04 §4.2).
    pub(crate) custom_update: bool,
    /// The car it watches, which distances are measured from: the one it
    /// spectates, else its own. `None` before that car has a position.
    pub(crate) view: Option<PositionUpdate>,
}

/// What a relay pass reads once for every position it sends.
struct PassSettings {
    live: crate::config::LiveExtra,
}

/// The most cars in one `0xAB 0x03`, as AssettoServer sends.
const CUSTOM_UPDATE_BATCH: usize = 20;

/// One receiver's `0xAB 0x03` under construction.
#[derive(Default)]
pub(crate) struct CustomBatch {
    count: u8,
    body: Vec<u8>,
}

/// Appends one car to a `0xAB 0x03` body: the 0x46 fields with the sender
/// first and Gas before PerformanceDelta (docs/04 §4.2).
fn write_custom_update_entry(
    w: &mut Writer,
    session_id: u8,
    ping: u16,
    timestamp: u32,
    p: &PositionUpdate,
) {
    w.u8(session_id);
    w.u8(p.sequence_id);
    w.u32(timestamp);
    w.u16(ping);
    w.vec3(p.position);
    w.vec3(p.rotation);
    w.vec3(p.velocity);
    w.u8(p.tyre_angular_speed[0]);
    w.u8(p.tyre_angular_speed[1]);
    w.u8(p.tyre_angular_speed[2]);
    w.u8(p.tyre_angular_speed[3]);
    w.u8(p.steer_angle);
    w.u8(p.wheel_angle);
    w.u16(p.engine_rpm);
    w.u8(p.gear);
    w.u32(p.status_flags);
    w.u8(p.gas);
    w.i16(p.performance_delta);
}

/// One slot's player car, for ranking by distance.
#[derive(Debug, Clone, Copy)]
pub struct PositionSnapshot {
    pub id: u8,
    /// Its last position; `None` before the first.
    pub position: Option<Vec3>,
    /// Its driver is spectating another car.
    pub spectating: bool,
}

/// The tier below vanilla's lowest: every fifth update. A car outside the
/// network bubble always gets it.
const OUTSIDE_BUBBLE_PRIORITY: u8 = 4;

/// The AI cars' bubble when NetworkBubbleDistance is 0.
const AI_FALLBACK_BUBBLE_METERS: f32 = 500.0;

/// The optimizer never thins out a car this close to the receiver, whatever
/// its rank.
const NEVER_DECIMATE_METERS: f32 = 100.0;

/// How many cars share each optimizer tier. Vanilla's is 5.
const TIER_SIZE: usize = 7;

/// The network bubble's radius in metres for players' cars (0: none) and
/// for AI cars.
#[derive(Debug, Clone, Copy)]
pub struct Bubble {
    pub players: f32,
    pub ai: f32,
}

impl Bubble {
    pub fn new(live: &crate::config::LiveExtra) -> Bubble {
        let players = live.network_bubble_distance.max(0.0);
        let ai = if players > 0.0 {
            players
        } else {
            AI_FALLBACK_BUBBLE_METERS
        };
        Bubble { players, ai }
    }
}

/// Sets one receiver's per-sender priorities (reference
/// Car.updatePriorities, with the changes in `ranked_priority`). Every
/// other car, the players' in `snaps` and the AI's in `ai` as (id, squared
/// distance), is ranked as one list by its distance from `view`; ties keep
/// slot order. A car whose distance is unknown (it, or `view`, has no
/// position yet, as for an empty slot) ranks last but gets every update, so
/// a newcomer's first updates are never thinned out.
pub fn update_position_priorities(
    receiver: u8,
    view: Option<Vec3>,
    priorities: &mut [u8; MAX_SESSION_SLOTS],
    snaps: &[PositionSnapshot],
    ai: &[(u8, f32)],
    bubble: Bubble,
) {
    // (id, squared distance if known, outside the bubble) for every other car.
    let mut others: Vec<(u8, Option<f32>, bool)> = Vec::with_capacity(snaps.len() + ai.len());
    for snap in snaps {
        if snap.id == receiver {
            continue;
        }
        let d2 = match (view, snap.position) {
            (Some(v), Some(p)) => Some(v.distance_squared(p)),
            _ => None,
        };
        let outside = d2.is_some_and(|d2| d2 > bubble.players.powi(2));
        let far = snap.spectating || (bubble.players > 0.0 && outside);
        others.push((snap.id, d2, far));
    }
    for &(id, d2) in ai {
        others.push((id, Some(d2), d2 > bubble.ai.powi(2)));
    }
    // Stable, so ties keep slot order.
    others.sort_by(|a, b| {
        let (a, b) = (a.1.unwrap_or(f32::MAX), b.1.unwrap_or(f32::MAX));
        a.total_cmp(&b)
    });
    for (rank, &(id, d2, far)) in others.iter().enumerate() {
        priorities[usize::from(id)] = ranked_priority(rank, d2, far);
    }
}

/// The tier of the car at `rank` (0 nearest) and squared distance `d2`:
/// `OUTSIDE_BUBBLE_PRIORITY` outside the bubble or when its driver is
/// spectating, 0 when the distance is unknown or within
/// `NEVER_DECIMATE_METERS`, else the nearest `TIER_SIZE` → 0, the next → 1,
/// the next → 2 and the rest → 3.
fn ranked_priority(rank: usize, d2: Option<f32>, far: bool) -> u8 {
    if far {
        return OUTSIDE_BUBBLE_PRIORITY;
    }
    if d2.is_none_or(|d2| d2 <= NEVER_DECIMATE_METERS.powi(2)) {
        return 0;
    }
    (rank / TIER_SIZE).min(3) as u8
}

/// Holds one far car for the next tick's 0x48, replacing an older update
/// from the same car.
fn hold_far_position(held: &mut Vec<MegaEntry>, entry: MegaEntry) {
    match held.iter_mut().find(|e| e.session_id == entry.session_id) {
        Some(e) => *e = entry,
        None => held.push(entry),
    }
}

/// Encodes an outbound 0x46.
pub fn build_position_update(
    session_id: u8,
    ping: u16,
    timestamp: u32,
    p: &PositionUpdate,
) -> Vec<u8> {
    let mut w = Writer::with_capacity(61);
    w.u8(proto::POSITION_UPDATE);
    w.u8(session_id);
    w.u8(p.sequence_id);
    w.u32(timestamp);
    w.u16(ping);
    w.vec3(p.position);
    w.vec3(p.rotation);
    w.vec3(p.velocity);
    w.u8(p.tyre_angular_speed[0]);
    w.u8(p.tyre_angular_speed[1]);
    w.u8(p.tyre_angular_speed[2]);
    w.u8(p.tyre_angular_speed[3]);
    w.u8(p.steer_angle);
    w.u8(p.wheel_angle);
    w.u16(p.engine_rpm);
    w.u8(p.gear);
    w.u32(p.status_flags);
    w.i16(p.performance_delta);
    w.u8(p.gas);
    w.into_bytes()
}

/// The most cars in one 0x48, as AssettoServer sends.
pub(crate) const MEGA_PACKET_BATCH: usize = 20;

/// One car in a 0x48: its sender, the receiver-clock timestamp and the
/// sender's ping, then the position.
#[derive(Debug)]
pub(crate) struct MegaEntry {
    pub(crate) session_id: u8,
    pub(crate) timestamp: u32,
    pub(crate) ping: u16,
    pub(crate) pos: PositionUpdate,
}

/// Encodes a 0x48 MegaPacket (docs/03): the receiver's clock and ping, then
/// up to `MEGA_PACKET_BATCH` cars in the 0x46 layout, ending at StatusFlags.
/// Vanilla never sends it; only AI traffic does, as AssettoServer's does.
pub(crate) fn build_mega_packet(server_time: u32, ping: u16, entries: &[MegaEntry]) -> Vec<u8> {
    debug_assert!(entries.len() <= MEGA_PACKET_BATCH);
    let mut w = Writer::with_capacity(8 + 57 * entries.len());
    w.u8(proto::MEGA_PACKET);
    w.u32(server_time);
    w.u16(ping);
    w.u8(entries.len() as u8);
    for e in entries {
        let p = &e.pos;
        w.u8(e.session_id);
        w.u8(p.sequence_id);
        w.u32(e.timestamp);
        w.u16(e.ping);
        w.vec3(p.position);
        w.vec3(p.rotation);
        w.vec3(p.velocity);
        w.u8(p.tyre_angular_speed[0]);
        w.u8(p.tyre_angular_speed[1]);
        w.u8(p.tyre_angular_speed[2]);
        w.u8(p.tyre_angular_speed[3]);
        w.u8(p.steer_angle);
        w.u8(p.wheel_angle);
        w.u16(p.engine_rpm);
        w.u8(p.gear);
        w.u32(p.status_flags);
    }
    w.into_bytes()
}

/// Encodes a 0xF9 ping.
fn build_ping(server_time: u32, ping: u16) -> Vec<u8> {
    let mut w = Writer::with_capacity(7);
    w.u8(proto::PING_UPDATE);
    w.u32(server_time);
    w.u16(ping);
    w.into_bytes()
}

impl Server {
    /// Vanilla's main loop, on its own thread: each pass sends every car that has a newer
    /// position its latest one, once, in slot order, steps the ping, sun and
    /// race-start timers, then sleeps SLEEP_TIME ms (50 ms while no client is
    /// ready). Updates a car sends within one pass merge into the newest
    /// (docs/03 0x46). SLEEP_TIME below 1 runs as 1: vanilla spins there.
    pub(crate) fn relay_loop(self: Arc<Self>) {
        let busy_sleep = self.cfg.server.sleep_time.max(Duration::from_millis(1));
        while !self.shutdown.is_cancelled() {
            // Who is ready, read once for the whole pass rather than by
            // locking every slot again in each step.
            let ready = self.ready_clients();
            self.relay_pass(&ready);
            let uptime_us = self.uptime_us();
            self.maybe_tick(uptime_us, &ready);
            // The rest of vanilla's Step, in its order: pings, sun, race start.
            self.step_pings(uptime_us, &ready);
            self.step_sun();
            self.step_race_start(uptime_us, &ready);
            let pause = if ready.is_empty() {
                IDLE_RELAY_SLEEP
            } else {
                busy_sleep
            };
            std::thread::sleep(pause);
        }
    }

    /// Once per CLIENT_SEND_INTERVAL_HZ, re-ranks every ready car's view of
    /// the others, which vanilla does every pass with the optimizer on; here
    /// it is always on. With AI traffic the tick also moves and sends the AI,
    /// ranked with the players (`step_traffic`). Each tick is due one interval
    /// after the last was due, not after it ran, so the rate holds exactly;
    /// after a stall the schedule restarts from now.
    fn maybe_tick(&self, uptime_us: i64, ready: &[ReadyClient]) {
        let interval_us = 1_000_000 / i64::from(self.cfg.server.refresh_rate_hz.max(1));
        let due = self.next_tick_us.load(Ordering::SeqCst);
        if uptime_us < due {
            return;
        }
        let next = if uptime_us - due >= interval_us {
            uptime_us + interval_us
        } else {
            due + interval_us
        };
        self.next_tick_us.store(next, Ordering::SeqCst);
        // Every held car goes out in this tick, before the next relay pass,
        // so one moving up a tier at the re-rank cannot have its 0x48 arrive
        // after its next 0x46.
        let far = self.take_far_positions(ready);
        if self.traffic.is_some() {
            // The far players share the AI's 0x48s.
            self.step_traffic(ready, far);
            return;
        }
        let now = self.server_time_millis();
        for (receiver, entries) in ready.iter().zip(&far) {
            self.send_far_positions(receiver, entries, now);
        }
        let snaps = self.position_snapshots();
        let bubble = Bubble::new(&self.live().extra);
        for receiver in ready {
            update_position_priorities(
                receiver.session_id,
                receiver.view.map(|p| p.position),
                &mut receiver.client.decimation.lock().priority,
                &snaps,
                &[],
                bubble,
            );
        }
    }

    /// The ready clients, with what the relay needs from each.
    fn ready_clients(&self) -> Vec<ReadyClient> {
        let mut ready = Vec::new();
        for car in &self.entry_cars {
            if let Some(client) = car.client()
                && client.ready()
            {
                let spectated = client.csp.spectating.load(Ordering::SeqCst);
                let watched = if spectated > 0 {
                    self.entry_cars.get(spectated as usize - 1).unwrap_or(car)
                } else {
                    car
                };
                ready.push(ReadyClient {
                    view: watched.last_position(),
                    session_id: car.session_id,
                    addr: client.udp_addr(),
                    offset: car.relay_offset(),
                    custom_update: client.csp.custom_update.load(Ordering::SeqCst),
                    client,
                });
            }
        }
        ready
    }

    /// Relays each pending car's latest position once, in slot order, at the
    /// tiers the last tick ranked. A pass with nothing pending skips the work.
    fn relay_pass(&self, ready: &[ReadyClient]) {
        let pending = self
            .entry_cars
            .iter()
            .any(|car| car.relay_pending.load(Ordering::SeqCst));
        if !pending {
            return;
        }
        let pass = PassSettings {
            live: self.live().extra,
        };
        let mut batches: Vec<CustomBatch> = Vec::new();
        batches.resize_with(ready.len(), CustomBatch::default);
        for car in &self.entry_cars {
            if !car.relay_pending.swap(false, Ordering::SeqCst) {
                continue;
            }
            let Some(client) = car.client() else { continue };
            if let Some((pos, offset)) = car.pending_relay() {
                self.relay_position(&client, &pos, offset, &pass, ready, &mut batches);
            }
        }
        self.flush_custom_batches(ready, &mut batches);
    }

    /// Adds one car to a custom-update receiver's batch, sending it when
    /// full.
    pub(crate) fn add_custom_update(
        &self,
        receiver: &ReadyClient,
        batch: &mut CustomBatch,
        session_id: u8,
        ping: u16,
        timestamp: u32,
        pos: &PositionUpdate,
    ) {
        let mut w = Writer::with_capacity(60);
        write_custom_update_entry(&mut w, session_id, ping, timestamp, pos);
        batch.body.extend_from_slice(w.bytes());
        batch.count += 1;
        if usize::from(batch.count) == CUSTOM_UPDATE_BATCH {
            self.send_custom_batch(receiver, batch);
        }
    }

    /// Sends what each custom-update receiver's batch holds.
    pub(crate) fn flush_custom_batches(&self, ready: &[ReadyClient], batches: &mut [CustomBatch]) {
        for (receiver, batch) in ready.iter().zip(batches.iter_mut()) {
            if batch.count > 0 {
                self.send_custom_batch(receiver, batch);
            }
        }
    }

    fn send_custom_batch(&self, receiver: &ReadyClient, batch: &mut CustomBatch) {
        if let Some(addr) = receiver.addr {
            let mut w = Writer::with_capacity(3 + batch.body.len());
            w.u8(proto::EXTENDED);
            w.u8(proto::EXT_UDP_CUSTOM_UPDATE);
            w.u8(batch.count);
            w.raw(&batch.body);
            self.send_udp(w.bytes(), addr);
        }
        batch.count = 0;
        batch.body.clear();
    }

    /// Forwards the sender's latest 0x46 to every other ready client, as
    /// vanilla's BroadcastCarUpdate does. A silent car sends nothing (docs/03
    /// 0x46). `offset` is the sender's clock offset stored with the update.
    /// Each receiver's skip counter for the sender advances once, at the
    /// tier the last tick ranked. The packet is built once; only its
    /// timestamp differs per receiver.
    fn relay_position(
        &self,
        sender: &Arc<Client>,
        pos: &PositionUpdate,
        offset: i64,
        pass: &PassSettings,
        ready: &[ReadyClient],
        batches: &mut [CustomBatch],
    ) {
        let live = &pass.live;
        let sender_id = sender.session_id();
        // ForceLights and /forcelights turn the lights flag on (docs/03 §3.3).
        let forced;
        let pos = if live.force_lights
            || self.entry_cars[usize::from(sender_id)]
                .force_lights
                .load(Ordering::SeqCst)
        {
            forced = PositionUpdate {
                status_flags: pos.status_flags | proto::STATUS_LIGHTS,
                ..*pos
            };
            &forced
        } else {
            pos
        };
        let mut packet = build_position_update(sender_id, sender.ping(), 0, pos);
        for (receiver, batch) in ready.iter().zip(batches.iter_mut()) {
            if Arc::ptr_eq(&receiver.client, sender) {
                continue;
            }
            let Some(addr) = receiver.addr else { continue };
            let Some(tier) = receiver.client.relay_tier(sender_id) else {
                continue;
            };
            // Sender clock → server clock → receiver clock, with the offsets
            // stored at each car's last accepted update (docs/03 0x46).
            let timestamp = (i64::from(pos.timestamp) + offset - receiver.offset) as u32;
            if receiver.custom_update {
                self.add_custom_update(receiver, batch, sender_id, sender.ping(), timestamp, pos);
                continue;
            }
            if tier == OUTSIDE_BUBBLE_PRIORITY && live.batch_far_positions {
                let entry = MegaEntry {
                    session_id: sender_id,
                    timestamp,
                    ping: sender.ping(),
                    pos: *pos,
                };
                hold_far_position(&mut receiver.client.far_positions.lock(), entry);
                continue;
            }
            packet[POSITION_TIMESTAMP..POSITION_TIMESTAMP + 4]
                .copy_from_slice(&timestamp.to_le_bytes());
            self.send_udp(&packet, addr);
        }
    }

    /// Takes each receiver's far cars held since the last tick
    /// (`BatchFarPositions`), in `ready`'s order. A car whose driver has left
    /// since is dropped.
    fn take_far_positions(&self, ready: &[ReadyClient]) -> Vec<Vec<MegaEntry>> {
        ready
            .iter()
            .map(|receiver| {
                let mut held = std::mem::take(&mut *receiver.client.far_positions.lock());
                held.retain(|e| {
                    self.entry_cars
                        .get(usize::from(e.session_id))
                        .is_some_and(|car| car.client().is_some())
                });
                held
            })
            .collect()
    }

    /// Sends one receiver its held far cars: up to `MEGA_PACKET_BATCH` to a
    /// 0x48, and a car left on its own as a plain 0x46, which keeps Gas and
    /// PerformanceDelta. `now` is the server clock.
    pub(crate) fn send_far_positions(
        &self,
        receiver: &ReadyClient,
        entries: &[MegaEntry],
        now: i64,
    ) {
        let Some(addr) = receiver.addr else { return };
        // Server clock → receiver clock, as for the AI's 0x48s.
        let server_time = (now - receiver.offset) as u32;
        let ping = receiver.client.ping();
        for chunk in entries.chunks(MEGA_PACKET_BATCH) {
            let packet = match chunk {
                [e] => build_position_update(e.session_id, e.ping, e.timestamp, &e.pos),
                _ => build_mega_packet(server_time, ping, chunk),
            };
            self.send_udp(&packet, addr);
        }
    }

    /// Every slot's last position.
    pub(crate) fn position_snapshots(&self) -> Vec<PositionSnapshot> {
        self.entry_cars
            .iter()
            .map(|car| PositionSnapshot {
                id: car.session_id,
                position: car.relay_position().map(|(pos, _)| pos.position),
                spectating: car
                    .client()
                    .is_some_and(|c| c.csp.spectating.load(Ordering::SeqCst) != 0),
            })
            .collect()
    }

    /// Disconnects clients whose pongs have stopped. Also samples the UDP
    /// send rate each second for /ENTRY's throughput line and steps idle mode.
    pub(crate) fn ping_timeouts(&self) {
        let sent = self.udp_bytes.swap(0, Ordering::Relaxed);
        self.throughput_kbs.store(sent / 1024, Ordering::Relaxed);
        self.step_idle();
        self.forget_idle_sources();
        self.check_afk();
        let now = self.uptime_ms();
        for car in &self.entry_cars {
            let Some(client) = car.client() else { continue };
            if !client.ready() {
                continue;
            }
            let last = client.last_pong_ms();
            if last != 0 && now - last > PING_TIMEOUT_MS {
                tracing::info!(
                    car = client.session_id(),
                    name = client.name(),
                    "ping timeout"
                );
                self.disconnect(&client, "ping timeout");
            }
        }
    }

    /// Pings each ready client once more than a second of uptime has passed
    /// since its last ping, as vanilla's Car.Step checks every pass: a car is
    /// pinged in the pass it becomes ready, then about every second on its
    /// own phase (docs/03 0xF9).
    fn step_pings(&self, uptime_us: i64, ready: &[ReadyClient]) {
        for receiver in ready {
            // Vanilla pings only a ready client (Car.Step checks IsHandshakeDone).
            let Some(addr) = receiver.addr else { continue };
            let client = &receiver.client;
            if uptime_us - client.ping_sent_us.load(Ordering::SeqCst) <= STEP_INTERVAL_US {
                continue;
            }
            client.ping_sent_us.store(uptime_us, Ordering::SeqCst);
            let packet = build_ping(self.server_time_millis() as u32, client.ping());
            self.send_udp(&packet, addr);
        }
    }

    /// Sends 0x57 to every ready client while the session is under
    /// `RACE_START_WINDOW_MS` old, a race's countdown included, once more than
    /// a second has passed since the last round. Vanilla's broadcastRaceStart
    /// keeps that one timer for the server, so the first round follows a
    /// session switch at once (docs/03 0x57).
    fn step_race_start(&self, uptime_us: i64, ready: &[ReadyClient]) {
        // One consistent pair, even across a clock rebase.
        let (start, server_now) = {
            let _clock = self.clock_lock.read();
            (self.sessions.start_time_ms(), self.server_time_millis())
        };
        if server_now - start >= RACE_START_WINDOW_MS
            || uptime_us - self.race_start_sent_us.load(Ordering::SeqCst) <= STEP_INTERVAL_US
        {
            return;
        }
        self.race_start_sent_us.store(uptime_us, Ordering::SeqCst);
        for receiver in ready {
            if let Some(addr) = receiver.addr {
                let client = &receiver.client;
                let packet =
                    build_race_start(start, server_now, client.time_offset(), client.ping());
                self.send_udp(&packet, addr);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(id: u8, x: f32, valid: bool) -> PositionSnapshot {
        PositionSnapshot {
            id,
            position: valid.then_some(Vec3 { x, y: 0.0, z: 0.0 }),
            spectating: false,
        }
    }

    const NO_BUBBLE: Bubble = Bubble {
        players: 0.0,
        ai: f32::MAX,
    };

    /// Ranks for receiver 0, watching from x=0.
    fn rank(
        snaps: &[PositionSnapshot],
        ai: &[(u8, f32)],
        bubble: Bubble,
    ) -> [u8; MAX_SESSION_SLOTS] {
        let mut priorities = [9u8; MAX_SESSION_SLOTS];
        update_position_priorities(0, Some(Vec3::default()), &mut priorities, snaps, ai, bubble);
        priorities
    }

    #[test]
    fn timestamp_offset_matches_the_builder() {
        let packet = build_position_update(7, 0, 0x0403_0201, &PositionUpdate::default());
        assert_eq!(
            &packet[POSITION_TIMESTAMP..POSITION_TIMESTAMP + 4],
            &[1, 2, 3, 4]
        );
    }

    #[test]
    fn mega_packet_layout() {
        let pos = PositionUpdate {
            sequence_id: 9,
            status_flags: 0xAABB_CCDD,
            performance_delta: 0x1234,
            gas: 0x77,
            ..PositionUpdate::default()
        };
        let entry = |session_id| MegaEntry {
            session_id,
            timestamp: 0x0807_0605,
            ping: 0x0B0A,
            pos,
        };
        let packet = build_mega_packet(0x0403_0201, 0x0605, &[entry(3), entry(4)]);
        // Header 8, then 57 per car: the 0x46 body without PerformanceDelta and Gas.
        assert_eq!(packet.len(), 8 + 2 * 57);
        assert_eq!(&packet[..8], &[proto::MEGA_PACKET, 1, 2, 3, 4, 5, 6, 2]);
        assert_eq!(&packet[8..16], &[3, 9, 5, 6, 7, 8, 0x0A, 0x0B]);
        assert_eq!(&packet[61..65], &[0xDD, 0xCC, 0xBB, 0xAA]);
        assert_eq!(packet[65], 4);
        // Each entry is the 0x46 bytes after the id, up to StatusFlags.
        let single = build_position_update(3, 0x0B0A, 0x0807_0605, &pos);
        assert_eq!(&packet[8..65], &single[1..58]);
    }

    #[test]
    fn priorities_rank_by_distance() {
        // Receiver 0 at x=0; 22 others at x=200+i, plus one without a
        // position.
        let mut snaps = vec![snap(0, 0.0, true)];
        for i in 1..=22u8 {
            snaps.push(snap(i, 200.0 + f32::from(i), true));
        }
        snaps.push(snap(23, 0.5, false));
        let priorities = rank(&snaps, &[], NO_BUBBLE);
        assert_eq!(&priorities[1..=7], &[0; 7]);
        assert_eq!(&priorities[8..=14], &[1; 7]);
        assert_eq!(&priorities[15..=21], &[2; 7]);
        assert_eq!(priorities[22], 3, "the rest");
        assert_eq!(priorities[23], 0, "no position yet: every update");
        assert_eq!(priorities[0], 9, "the receiver itself is untouched");
    }

    #[test]
    fn priorities_never_decimate_within_100m() {
        // Receiver 0 at x=0; 20 others at x=5i, so the first 20 are within
        // 100 m whatever their rank, and car 21 at 105 m is not.
        let mut snaps = vec![snap(0, 0.0, true)];
        for i in 1..=21u8 {
            snaps.push(snap(i, 5.0 * f32::from(i), true));
        }
        let priorities = rank(&snaps, &[], NO_BUBBLE);
        assert_eq!(&priorities[1..=20], &[0; 20]);
        assert_eq!(priorities[21], 2, "beyond 100 m its rank (20th) applies");
    }

    #[test]
    fn priorities_rank_ai_with_players() {
        // Players 1–7 at 300+i m; AI 10–16 at 200+i m, nearer, so the AI
        // takes the first tier and the players the second.
        let mut snaps = vec![snap(0, 0.0, true)];
        for i in 1..=7u8 {
            snaps.push(snap(i, 300.0 + f32::from(i), true));
        }
        let ai: Vec<(u8, f32)> = (10..=16u8)
            .map(|id| (id, (200.0 + f32::from(id)).powi(2)))
            .collect();
        let priorities = rank(&snaps, &ai, NO_BUBBLE);
        assert_eq!(&priorities[10..=16], &[0; 7]);
        assert_eq!(&priorities[1..=7], &[1; 7]);
    }

    #[test]
    fn priorities_outside_bubble_or_spectating_are_lowest() {
        // A player at 10 m, one spectating at 20 m, one at 600 m; AI at
        // 50 m and 2 km. Players' bubble 500 m, AI's 1500 m.
        let mut spectator = snap(2, 20.0, true);
        spectator.spectating = true;
        let snaps = [
            snap(0, 0.0, true),
            snap(1, 10.0, true),
            spectator,
            snap(3, 600.0, true),
        ];
        let ai = [(10, 50.0f32.powi(2)), (11, 2000.0f32.powi(2))];
        let bubble = Bubble {
            players: 500.0,
            ai: 1500.0,
        };
        let priorities = rank(&snaps, &ai, bubble);
        assert_eq!(priorities[1], 0);
        assert_eq!(priorities[2], OUTSIDE_BUBBLE_PRIORITY, "spectating");
        assert_eq!(priorities[3], OUTSIDE_BUBBLE_PRIORITY, "beyond 500 m");
        assert_eq!(priorities[10], 0);
        assert_eq!(priorities[11], OUTSIDE_BUBBLE_PRIORITY, "AI beyond 1500 m");
    }

    #[test]
    fn held_far_position_keeps_the_newest_per_car() {
        let entry = |id, timestamp| MegaEntry {
            session_id: id,
            timestamp,
            ping: 0,
            pos: PositionUpdate::default(),
        };
        let mut held = Vec::new();
        hold_far_position(&mut held, entry(3, 10));
        hold_far_position(&mut held, entry(5, 10));
        hold_far_position(&mut held, entry(3, 20));
        let got: Vec<_> = held.iter().map(|e| (e.session_id, e.timestamp)).collect();
        assert_eq!(got, [(3, 20), (5, 10)]);
    }

    #[test]
    fn bubble_zero_keeps_ai_fallback() {
        let mut live = crate::config::LiveExtra {
            network_bubble_distance: 0.0,
            ..Default::default()
        };
        let b = Bubble::new(&live);
        assert_eq!((b.players, b.ai), (0.0, AI_FALLBACK_BUBBLE_METERS));
        live.network_bubble_distance = 800.0;
        let b = Bubble::new(&live);
        assert_eq!((b.players, b.ai), (800.0, 800.0));
    }
}
