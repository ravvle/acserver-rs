//! The first-update snapshot and leaderboard ranking.

use std::cmp::Ordering as CmpOrdering;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::Server;
use super::client::{ChecksumStatus, Client};
use super::entrycar::{CarResult, EntryCar};
use crate::config::AiMode;
use crate::proto::SessionType;

/// One car's result, copied once so sorting takes no locks.
#[derive(Debug, Clone)]
pub struct RankedCar {
    pub car: Arc<EntryCar>,
    pub result: CarResult,
    pub occupied: bool,
}

impl RankedCar {
    fn snapshot(car: &Arc<EntryCar>) -> RankedCar {
        RankedCar {
            car: Arc::clone(car),
            result: car.result(),
            occupied: car.occupied(),
        }
    }
}

/// Sorts a leaderboard. Race: most laps, then least total time. Other
/// sessions: best lap, no-lap last, then occupied before empty. The sort is
/// stable, so ties keep their order.
pub fn sort_ranked(ranked: &mut [RankedCar], session_type: SessionType) {
    ranked.sort_by(|a, b| {
        let (ra, rb) = (&a.result, &b.result);
        if session_type == SessionType::Race {
            if ra.num_laps != rb.num_laps {
                return rb.num_laps.cmp(&ra.num_laps);
            }
            return compare_non_zero(ra.total_time, rb.total_time);
        }
        match compare_non_zero(ra.best_lap, rb.best_lap) {
            CmpOrdering::Equal => b.occupied.cmp(&a.occupied),
            other => other,
        }
    });
}

/// Sorts times ascending, with 0 (no time) last.
fn compare_non_zero(a: u32, b: u32) -> CmpOrdering {
    match (a == 0, b == 0) {
        (true, true) => CmpOrdering::Equal,
        (true, false) => CmpOrdering::Greater,
        (false, true) => CmpOrdering::Less,
        (false, false) => a.cmp(&b),
    }
}

/// The Time field a car contributes to 0x49 / 0x4B.
pub fn leaderboard_time(result: &CarResult, session_type: SessionType) -> u32 {
    if session_type == SessionType::Race {
        return result.total_time;
    }
    result.best_lap_or_none()
}

// The leaderboard order is vanilla's bestLapRank. It starts in slot order
// each session and is re-sorted only on a counted lap, when a new driver takes
// a slot and at the session's end (docs/02 0x49). Between those every 0x49,
// 0x4B and grid reads it as stored, so a driver joining or leaving moves
// nobody.
impl Server {
    /// Occupied and AI slots, in entry-list order.
    pub fn active_cars(&self) -> Vec<Arc<EntryCar>> {
        self.entry_cars
            .iter()
            .filter(|car| car.occupied() || car.state().ai_mode != AiMode::None)
            .cloned()
            .collect()
    }

    /// Every listed slot in the stored leaderboard order, as 0x49 and 0x4B
    /// list them all.
    pub fn leaderboard_entries(&self) -> Vec<Arc<EntryCar>> {
        let rank = self.rank.lock();
        let mut order = Vec::with_capacity(rank.len());
        for &idx in rank.iter() {
            let car = &self.entry_cars[idx];
            if car.listed() {
                order.push(Arc::clone(car));
            }
        }
        order
    }

    /// Every listed slot's result, copied in the stored order.
    pub(crate) fn leaderboard_snapshots(&self) -> Vec<RankedCar> {
        let rank = self.rank.lock();
        let mut out = Vec::with_capacity(rank.len());
        for &idx in rank.iter() {
            let car = &self.entry_cars[idx];
            if car.listed() {
                out.push(RankedCar::snapshot(car));
            }
        }
        out
    }

    /// Restores slot order, as a new session's results do.
    pub(crate) fn reset_ranking(&self) {
        let mut rank = self.rank.lock();
        rank.clear();
        rank.extend(0..self.entry_cars.len());
    }

    /// Sorts the stored order on the current results. The sort is stable, so
    /// tied cars keep their previous order where vanilla's older, unstable
    /// sort may swap them.
    pub(crate) fn resort_ranking(&self) {
        let (cur, _) = self.sessions.current();
        let mut rank = self.rank.lock();
        let mut ranked: Vec<RankedCar> = rank
            .iter()
            .map(|&idx| RankedCar::snapshot(&self.entry_cars[idx]))
            .collect();
        sort_ranked(&mut ranked, cur.kind);
        for (i, entry) in ranked.iter().enumerate() {
            rank[i] = usize::from(entry.car.session_id);
        }
    }

    /// Sends the first-update snapshot in vanilla's order (docs/01 §1.5,
    /// probe FirstUpdate). No 0x54: the angle was in 0x3E. The caller
    /// guarantees it runs once per client.
    pub(crate) fn send_first_update_sequence(self: &Arc<Self>, client: &Arc<Client>) {
        self.send_driver_info(client);
        self.send_weather(client);
        self.send_leaderboard_sync(client);
        // Per slot, vanilla sends the tyre compound any other slot last
        // reported, connected or not, then P2P and mandatory pit.
        let own_id = client.session_id();
        let listed = self.listed_cars();
        for car in &listed {
            let state = car.state();
            let compound = state.tyre_compound;
            if car.session_id != own_id && !compound.is_empty() {
                self.send_tyre_compound(client, car.session_id, &compound);
            }
            self.send_p2p(client, car.session_id, state.p2p_count, false);
            self.send_mandatory_pit(client, car.session_id, state.mandatory_pit_done);
        }
        self.send_bop(client, &self.listed_cars());
        // Then, in vanilla's order (onConnectionFirstUpdate): the plugin's
        // 0x3A, the welcome message, the fixed setup and the DRS zones, each
        // only when there is one.
        self.plugin().on_client_first_update(client);
        if !self.welcome_text(client).is_empty() {
            self.send_welcome(client);
        }
        let setup = self.entry_cars[usize::from(own_id)].state().setup;
        self.send_car_setup(client, &setup);
        self.send_drs_zones(client);

        self.send_csp_handshake(client);
        self.send_traffic_visibility(client);

        client.has_sent_first_update.store(true, Ordering::SeqCst);
        client.set_read_deadline(None);

        // No 0x5A here: it went out at the handshake.
        tracing::info!(
            car = own_id,
            name = client.name(),
            after_secs = client.seconds_since_join(),
            "client loaded"
        );

        // The deferred checksum kick (docs/01 §1.5).
        if client.checksum_status() == ChecksumStatus::Failed {
            self.kick_checksum_fail(client);
        }
        self.arm_checksum_timeout(client);
        self.plugins_first_update(client);
    }
}
