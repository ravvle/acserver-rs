//! Chat, car list, P2P, pit, tyres, damage, sectors, laps and client events.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::Server;
use super::client::Client;
use crate::proto::{self, SessionType};
use crate::wire::{Reader, Writer};

/// The shortest lap that may be a best lap, far below any real layout.
const MIN_PLAUSIBLE_LAP_MS: u32 = 1000;

/// Drops control characters, which would let a player forge log lines. Tabs
/// stay: CSP prefixes its chat-encoded messages with them so that clients
/// hide the line (docs/04 §4.5).
pub fn sanitize_chat(msg: &str) -> String {
    msg.chars()
        .filter(|&c| !c.is_control() || c == '\t')
        .collect()
}

/// Whether a chat line is CSP data: "$CSP0:" after any leading tabs.
fn is_csp_chat(msg: &str) -> bool {
    msg.trim_start_matches('\t').starts_with("$CSP0:")
}

impl Server {
    /// Processes a 0x47. The claimed sender id is replaced by the real one.
    /// Vanilla has no rate limit, so ChatRateLimitMs defaults to off.
    pub(crate) fn handle_chat(self: &Arc<Self>, client: &Arc<Client>, r: &mut Reader) {
        let _claimed_id = r.u8();
        let msg = r.wstring8();
        self.note_chat_activity(client);

        // CSP carries data between clients as chat lines (docs/04 §4.5), so
        // it gets the client-message budget instead of the chat limit, which
        // would drop them. A client without CSP is limited as any other.
        let csp_chat =
            client.is_csp() && self.cfg.extra.enable_client_messages && is_csp_chat(&msg);
        if csp_chat {
            if !client.csp.allow(false) {
                return;
            }
        } else if self.live().extra.chat_rate_limit_ms > 0 {
            let now = self.uptime_ms();
            let last = client.last_chat_ms.load(Ordering::SeqCst);
            if now.saturating_sub(last) < self.live().extra.chat_rate_limit_ms {
                return;
            }
            client.last_chat_ms.store(now, Ordering::SeqCst);
        }

        let msg = sanitize_chat(&msg);
        if msg.trim().is_empty() {
            return;
        }

        if msg.starts_with('/') {
            // Vanilla reports a command to the plugin as chat before acting on
            // it, /admin's password included (measured; docs/06 0x39).
            self.plugin().on_chat(client.session_id(), &msg);
            self.handle_command(client, &msg);
            return;
        }

        // EnablePlugins' filters (WordFilterPlugin) may drop it.
        if !is_csp_chat(&msg) && !self.plugins_allow_chat(client, &msg) {
            return;
        }

        // CSP exchanges data as chat lines, several a second while it is
        // active, so they are logged only at debug level.
        if is_csp_chat(&msg) {
            tracing::debug!(car = client.session_id(), name = client.name(), msg = %msg, "chat");
        } else {
            tracing::info!(car = client.session_id(), name = client.name(), msg = %msg, "chat");
        }
        self.broadcast_chat(client.session_id(), &msg);
        self.plugin().on_chat(client.session_id(), &msg);
    }

    /// Sends a 0x47 chat line from `session_id` to all clients.
    pub(crate) fn broadcast_chat(&self, session_id: u8, msg: &str) {
        let mut w = Writer::with_id(proto::CHAT);
        w.u8(session_id);
        w.wstring8(msg);
        self.broadcast_tcp(w.bytes(), None);
    }

    /// Delivers a server chat message (session id 255) to one client.
    pub fn send_chat_to(&self, client: &Client, msg: &str) {
        let mut w = Writer::with_id(proto::CHAT);
        w.u8(proto::SERVER_SESSION_ID);
        w.wstring8(msg);
        self.send_tcp(client, w.bytes());
    }

    /// Sends a server chat message to all clients.
    pub fn broadcast_server_chat(&self, msg: &str) {
        self.broadcast_chat(proto::SERVER_SESSION_ID, msg);
    }

    /// Answers 0x3F with up to 10 cars starting at PageIndex, a position in
    /// the car list (`listed_cars`).
    pub(crate) fn handle_car_list_request(&self, client: &Client, r: &mut Reader) {
        let cars = self.listed_cars();
        let requested = usize::from(r.u8());
        let end = (requested + 10).min(cars.len());
        let page = requested.min(cars.len());

        let mut w = Writer::with_id(proto::CAR_LIST);
        w.u8(page as u8);
        w.u8((end - page) as u8);
        for car in &cars[page..end] {
            w.u8(car.session_id);
            w.string8(&car.model());
            let state = car.state();
            w.string8(&state.skin);
            let id = car.identity();
            w.string8(&id.name);
            w.narrow8(&id.team);
            w.narrow8(&id.nation);
            w.bool(state.spectator_mode);
            for level in state.damage {
                w.f32(level);
            }
        }
        self.send_tcp(client, w.bytes());
    }

    /// Processes push-to-pass (0x0D).
    pub(crate) fn handle_p2p(&self, client: &Client, r: &mut Reader) {
        let count = r.i16();
        let active = r.bool();
        let id = client.session_id();
        let car = &self.entry_cars[usize::from(id)];

        if count == -1 {
            // Query: reply only to the sender with the current count.
            self.send_p2p(client, id, car.state().p2p_count, false);
            return;
        }

        // Client-authoritative: vanilla relays the reported count (docs/02 0x0D).
        if !self.cfg.extra.enable_unlimited_p2p {
            car.update(|s| s.p2p_count = count);
        }
        let mut w = Writer::with_id(proto::P2P_UPDATE);
        w.u8(id);
        w.i16(count);
        w.bool(active);
        self.broadcast_tcp(w.bytes(), None);
    }

    /// Stores and broadcasts the mandatory-pit flag (0x0E); a later joiner's
    /// snapshot reports the stored value.
    pub(crate) fn handle_mandatory_pit(&self, client: &Client, r: &mut Reader) {
        let done = r.bool();
        let id = client.session_id();
        self.entry_cars[usize::from(id)].update(|s| s.mandatory_pit_done = done);
        let mut w = Writer::with_id(proto::MANDATORY_PIT_UPDATE);
        w.u8(id);
        w.bool(done);
        self.broadcast_tcp(w.bytes(), None);
    }

    /// Stores a 0x50 for later joiners' snapshots and relays it to the other
    /// cars; vanilla does not echo it.
    pub(crate) fn handle_tyre_compound(&self, client: &Client, r: &mut Reader) {
        let name = r.narrow8();
        let id = client.session_id();
        self.entry_cars[usize::from(id)].update(|s| s.tyre_compound = name.clone());
        let mut w = Writer::with_id(proto::TYRE_COMPOUND_CHANGE);
        w.u8(id);
        w.narrow8(&name);
        self.broadcast_tcp(w.bytes(), Some(id));
    }

    /// Stores a 0x56 (reported in 0x40) and relays it to the others.
    pub(crate) fn handle_damage(&self, client: &Client, r: &mut Reader) {
        let mut damage = [0.0f32; 5];
        for level in damage.iter_mut() {
            *level = r.f32();
        }
        let id = client.session_id();
        self.entry_cars[usize::from(id)].update(|s| s.damage = damage);

        let mut w = Writer::with_id(proto::DAMAGE_UPDATE);
        w.u8(id);
        for level in damage {
            w.f32(level);
        }
        self.broadcast_tcp(w.bytes(), Some(id));
    }

    /// Forwards a sector split (0x58) to the other cars; vanilla's
    /// onSectorSplit does not echo it (probe EchoSectorSplit).
    pub(crate) fn handle_sector_split(&self, client: &Client, r: &mut Reader) {
        let split_index = r.u8();
        let split_time = r.u32();
        let cuts = r.u8();
        let id = client.session_id();
        let mut w = Writer::with_id(proto::SECTOR_SPLIT);
        w.u8(id);
        w.u8(split_index);
        w.u32(split_time);
        w.u8(cuts);
        self.broadcast_tcp(w.bytes(), Some(id));
    }

    /// Records a lap and broadcasts the leaderboard (0x49).
    pub(crate) fn handle_lap_completed(self: &Arc<Self>, client: &Client, r: &mut Reader) {
        let timestamp = r.u32();
        let lap_time = r.u32();
        let split_count = usize::from(r.u8());
        let mut sectors: Vec<u32> = Vec::with_capacity(split_count);
        for _ in 0..split_count {
            sectors.push(r.u32());
        }
        let cuts = r.u8();
        let num_lap = r.u8();

        let id = client.session_id();
        let car = Arc::clone(&self.entry_cars[usize::from(id)]);

        // Gate 1 (vanilla Car.onLapCompleted): a lap whose Timestamp does not
        // advance is dropped outright, with no grip gain.
        if !car.accept_lap_timestamp(timestamp) {
            tracing::warn!(
                car = id,
                name = client.name(),
                timestamp,
                "laptime discarded: timestamp not newer than last"
            );
            return;
        }

        // Every lap past Gate 1 adds grip, including cut laps and laps Gate 2
        // refuses, as in vanilla.
        self.weather.add_lap_grip(self.cfg.dynamic.lap_gain);

        // Gate 2 (vanilla SessionResult.OnLapCompleted): a lap after the car
        // has completed its last lap, or a race lap before the start or after
        // the race is over, is neither recorded nor relayed.
        if self.lap_refused(&car) {
            tracing::info!(
                car = id,
                name = client.name(),
                "lap refused (race not started, car finished or race over)"
            );
            return;
        }

        let (cur, _) = self.sessions.current();
        let race = cur.kind == SessionType::Race;
        let elapsed = self.elapsed_ms();
        let valid = cuts == 0;
        let plausible = lap_time >= MIN_PLAUSIBLE_LAP_MS;
        if !plausible {
            // Otherwise a client could claim an unbeatable best lap. The lap
            // still counts; it just cannot be the best.
            tracing::warn!(
                car = id,
                name = client.name(),
                time = lap_time,
                "implausible lap time ignored for best lap"
            );
        }
        let half_ping = i64::from(client.ping() / 2);
        car.update(|s| {
            let res = &mut s.result;
            // Vanilla counts laps itself and ignores the client's NumLap:
            // every race lap, cut or not, and only uncut laps otherwise
            // (measured). A counted lap also sets TotalTime to the session
            // time less half the ping, as vanilla's
            // SessionResult.OnLapCompleted: the race total, and the results
            // file's TotalTime in other sessions.
            if valid || race {
                res.num_laps = res.num_laps.wrapping_add(1);
                res.total_time = (elapsed - half_ping).max(0) as u32;
            }
            res.last_lap_time = lap_time;
            // In a race a cut lap can still be the best lap (measured).
            if (valid || race) && plausible && (res.best_lap == 0 || lap_time < res.best_lap) {
                res.best_lap = lap_time;
            }
        });

        self.record_lap(client, lap_time, sectors, cuts);
        // Vanilla re-sorts only on a lap it counts (docs/02 0x49).
        if valid || race {
            self.resort_ranking();
        }

        tracing::info!(car = id, time = lap_time, cuts, client_lap = num_lap, "lap");
        // The finish is decided before the leaderboard goes out, so the lap
        // that finishes a car already carries its flag, and 0x4B follows the
        // 0x49.
        if race {
            self.on_race_lap(&car);
        } else {
            self.on_timed_lap(&car);
        }
        let payload = self.build_leaderboard(id, lap_time, cuts);
        self.broadcast_tcp(&payload, None);
        self.plugin().on_lap_completed(&payload);
        if race {
            self.end_race_if_all_finished();
        }
    }

    /// Processes collisions and penalties (0x82).
    pub(crate) fn handle_client_event(&self, client: &Client, r: &mut Reader) {
        let count = r.i16();
        let id = client.session_id();
        for _ in 0..count.max(0) {
            let kind = r.u8();
            // The count is client-declared; stop at the end of the data rather
            // than report zeroed collisions to the plugin.
            if r.overflowed() {
                return;
            }
            match kind {
                proto::EVENT_COLLISION_WITH_CAR => {
                    let target = r.u8();
                    let speed = r.f32();
                    let pos = r.vec3();
                    let rel = r.vec3();
                    if r.overflowed() {
                        return;
                    }
                    if let Some(other) = self.car_by_id(target) {
                        self.record_collision(client, Some(&other), speed, pos, rel);
                    }
                    self.traffic_collision(&self.entry_cars[usize::from(id)], target);
                    self.plugin()
                        .on_collision_with_car(id, target, speed, pos, rel);
                }
                proto::EVENT_COLLISION_WITH_ENV => {
                    let speed = r.f32();
                    let pos = r.vec3();
                    let rel = r.vec3();
                    if r.overflowed() {
                        return;
                    }
                    self.record_collision(client, None, speed, pos, rel);
                    self.plugin().on_collision_with_env(id, speed, pos, rel);
                }
                proto::EVENT_JUMP_START => {
                    let mut w = Writer::with_id(proto::CLIENT_EVENT);
                    w.u8(proto::EVENT_JUMP_START);
                    w.u8(id);
                    self.broadcast_tcp(w.bytes(), None);
                }
                _ => return, // unknown event: stop parsing this batch
            }
        }
    }
}
