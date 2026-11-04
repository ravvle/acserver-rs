//! The session list, the session clock, transitions and grids.

use parking_lot::{Mutex, MutexGuard};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use super::entrycar::EntryCar;
use super::{Server, signed_millis};
use crate::config::{IsOpen, SessionConfig};
use crate::proto::SessionType;

/// The speed (m/s) above which a car on its last lap keeps a practice or
/// qualifying session open (vanilla Step).
const LAST_LAP_MIN_SPEED: f64 = 5.0;

/// The mutable session state, under one lock.
#[derive(Debug)]
struct SessionState {
    sessions: Vec<SessionConfig>,
    current_index: usize,
    /// The server-clock time (ms) when the current session's timed portion
    /// begins. For a race with WAIT_TIME it is in the future while the grid
    /// forms.
    start_time_ms: i64,
    /// When the current timed session ends; `None` for a lap race.
    ends_at: Option<Instant>,
    /// RaceOver has been sent.
    finished: bool,
    // Race finish, as vanilla's SessionResult.OnLapCompleted keeps it (docs/02
    // "End of a race"). time_up: a timed race's TIME has run out
    // (sessionOverFlag). leader_laps: the most laps any car has completed.
    // leader_done: the leader has finished. target_lap: with RACE_EXTRA_LAP,
    // the lap the leader must reach after time-up. race_end_at: when the
    // first car finished.
    time_up: bool,
    leader_laps: u16,
    leader_done: bool,
    target_lap: u16,
    race_end_at: Option<Instant>,
    /// The reversed-grid repeat of the last race has run (vanilla
    /// IsLastRaceInverted).
    last_race_inverted: bool,
    /// A practice or qualifying session's time has run out and it waits for
    /// cars on their last lap (see `session_tick`).
    overtime: bool,
    /// The current session's starting order (see `set_grid`); `None` for
    /// entry-list order.
    grid: Option<Vec<u8>>,
}

/// Owns the ordered session list and the current-session clock. The logic
/// that needs the rest of the server is in `impl Server` below.
#[derive(Debug)]
pub struct SessionManager {
    state: Mutex<SessionState>,
}

impl SessionManager {
    pub fn new(sessions: Vec<SessionConfig>) -> SessionManager {
        SessionManager {
            state: Mutex::new(SessionState {
                sessions,
                current_index: 0,
                start_time_ms: 0,
                ends_at: None,
                finished: false,
                time_up: false,
                leader_laps: 0,
                leader_done: false,
                target_lap: 0,
                race_end_at: None,
                last_race_inverted: false,
                overtime: false,
                grid: None,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, SessionState> {
        self.state.lock()
    }

    /// The active session and its index.
    pub fn current(&self) -> (SessionConfig, usize) {
        let st = self.lock();
        (st.sessions[st.current_index].clone(), st.current_index)
    }

    /// The number of configured sessions.
    pub fn count(&self) -> usize {
        self.lock().sessions.len()
    }

    /// The configured session list.
    pub fn all(&self) -> Vec<SessionConfig> {
        self.lock().sessions.clone()
    }

    /// The current session's start time in the server clock.
    pub fn start_time_ms(&self) -> i64 {
        self.lock().start_time_ms
    }

    /// The current timed session's time left in ms, which goes negative once
    /// it is up (vanilla Session.TimeLeft); 0 for a lap race.
    pub fn time_left_ms(&self) -> i64 {
        match self.lock().ends_at {
            None => 0,
            Some(ends_at) => signed_millis(Instant::now(), ends_at),
        }
    }

    /// The seconds left in the current timed session, 0 for a lap race.
    pub fn time_left_seconds(&self) -> i64 {
        match self.lock().ends_at {
            None => 0,
            Some(ends_at) => {
                let left = ends_at.saturating_duration_since(Instant::now());
                left.as_secs() as i64
            }
        }
    }

    /// Overwrites the definition at `index` (ACSP 0xCD). As vanilla's
    /// udpOnSetSessionInfo it checks only index < len(sessions), and it does
    /// not switch sessions; the current session picks up the new values.
    pub fn set_session_info(
        &self,
        index: usize,
        name: &str,
        kind: SessionType,
        laps: u16,
        time_min: u16,
        wait_time: u16,
    ) -> bool {
        let mut st = self.lock();
        if index >= st.sessions.len() {
            return false;
        }
        st.sessions[index] = SessionConfig {
            kind,
            name: name.to_string(),
            time: time_min,
            laps,
            wait_time,
            is_open: IsOpen::Closed,
        };
        true
    }

    /// Whether the current practice or qualifying is in its final-lap window.
    fn overtime(&self) -> bool {
        self.lock().overtime
    }
}

/// The delay from a session's switch to its start. Vanilla honours WAIT_TIME
/// only for a race, with a 20 s floor; practice and qualifying start at once
/// (docs/01 §1.6, measured on v1.15).
fn session_wait_seconds(session: &SessionConfig, min_race_wait: u16) -> i64 {
    if session.kind != SessionType::Race {
        return 0;
    }
    i64::from(session.wait_time.max(min_race_wait))
}

/// Orders the cars that finished the race first, then the rest, and reverses
/// the first REVERSED_GRID_RACE_POSITIONS of the finishers (-1: all of them),
/// with vanilla's index arithmetic (broadcastSession): positions one above
/// the finisher count also swaps in the first car that did not finish.
pub fn reverse_race_grid(order: Vec<Arc<EntryCar>>, positions: i64) -> Vec<Arc<EntryCar>> {
    if positions == 0 {
        return order;
    }
    let mut grid: Vec<Arc<EntryCar>> = Vec::with_capacity(order.len());
    let mut rest: Vec<Arc<EntryCar>> = Vec::new();
    for car in order {
        if car.result().has_completed_last_lap {
            grid.push(car);
        } else {
            rest.push(car);
        }
    }
    let finished = grid.len() as i64;
    grid.extend(rest);
    let mut last = positions - 1;
    if last < 0 || finished < last {
        last = finished - 1;
    }
    // Vanilla indexes past the grid here when every car finished; clamp.
    last = last.min(grid.len() as i64 - 1);
    if last > 0 {
        grid[..=last as usize].reverse();
    }
    grid
}

impl Server {
    /// Selects the first session and starts the 1 s session tick.
    pub(crate) fn start_sessions(self: &Arc<Self>) {
        self.set_session(0, true);
        let server = Arc::clone(self);
        self.spawn_ticker(Duration::from_secs(1), move || server.session_tick());
    }

    /// Milliseconds since the current session's timed start (negative during
    /// the race wait).
    pub fn elapsed_ms(&self) -> i64 {
        let start = self.sessions.start_time_ms();
        self.server_time_millis() - start
    }

    /// Logs a session event at info while anyone is connected and at debug on
    /// an empty server, so the idle session cycle does not fill the log.
    fn log_session(&self, message: String) {
        if self.connected_count() == 0 {
            tracing::debug!("{message}");
        } else {
            tracing::info!("{message}");
        }
    }

    fn session_tick(self: &Arc<Self>) {
        let (cur, idx, ends_at, overtime, finished) = {
            let st = self.sessions.lock();
            (
                st.sessions[st.current_index].clone(),
                st.current_index,
                st.ends_at,
                st.overtime,
                st.finished,
            )
        };
        let now = Instant::now();
        match cur.kind {
            SessionType::Race => {
                self.tick_race(&cur, idx, ends_at, finished, now);
                return;
            }
            SessionType::Booking => {
                // A booking session just ends when its time is up: no 0x4B,
                // no result screen (vanilla Session.IsOver).
                if let Some(ends_at) = ends_at
                    && now > ends_at
                    && !self.switch_stalled()
                {
                    self.advance_from(idx);
                }
                return;
            }
            SessionType::Practice | SessionType::Qualify | SessionType::Other(_) => {}
        }

        // Nothing expires during the result screen.
        let Some(mut ends_at) = ends_at else {
            return;
        };
        if finished || cur.time == 0 {
            return;
        }

        // Practice and qualifying end alike (vanilla Step, docs/02 "End of a
        // session"): when time runs out, cars on their last lap get up to
        // QUALIFY_MAX_WAIT_PERC of the fastest lap to finish it.
        if !overtime {
            if now <= ends_at {
                return;
            }
            let wait_perc = u32::from(self.cfg.server.qualify_max_wait_perc);
            let grace = Duration::from_millis(u64::from(self.fastest_lap())) * wait_perc / 100;
            {
                let mut st = self.sessions.lock();
                if st.current_index != idx || st.overtime {
                    return;
                }
                st.overtime = true;
                st.ends_at = Some(now + grace);
                ends_at = now + grace;
            }
            self.log_session(format!(
                "session time up; final-lap window name={} duration={grace:?}",
                cur.name
            ));
        }

        // It ends when the window closes, or at once when no ready car is
        // still on its last lap at more than 5 m/s.
        if now >= ends_at || !self.car_still_on_last_lap() {
            self.end_session(idx);
        }
    }

    /// Whether any ready car has not completed its last lap and is moving
    /// faster than `LAST_LAP_MIN_SPEED`.
    fn car_still_on_last_lap(&self) -> bool {
        for car in &self.entry_cars {
            let Some(client) = car.client() else { continue };
            if !client.ready() || car.result().has_completed_last_lap {
                continue;
            }
            if let Some(pos) = car.last_position() {
                let v = pos.velocity;
                let speed = f64::from(v.length_squared()).sqrt();
                if speed > LAST_LAP_MIN_SPEED {
                    return true;
                }
            }
        }
        false
    }

    /// After a practice or qualifying lap is recorded: once time is up, that
    /// lap is the car's last (vanilla SessionResult.OnLapCompleted).
    pub(crate) fn on_timed_lap(&self, car: &EntryCar) {
        if self.sessions.overtime() {
            car.update(|s| s.result.has_completed_last_lap = true);
        }
    }

    fn fastest_lap(&self) -> u32 {
        let laps = self.entry_cars.iter().map(|car| car.result().best_lap);
        laps.filter(|&lap| lap != 0).min().unwrap_or(0)
    }

    /// A race's share of vanilla's Step: the empty-race check
    /// (Session.IsOver), the time-up flag of a timed race, and the race's end
    /// once every loaded car has finished.
    fn tick_race(
        self: &Arc<Self>,
        cur: &SessionConfig,
        idx: usize,
        ends_at: Option<Instant>,
        finished: bool,
        now: Instant,
    ) {
        if self.race_abandoned(cur) {
            if !self.switch_stalled() {
                self.log_session(format!(
                    "race has too few cars; moving to the next session session={}",
                    cur.name
                ));
                self.advance_from(idx);
            }
            return;
        }
        if finished {
            return;
        }
        if cur.time > 0
            && cur.laps == 0
            && let Some(ends_at) = ends_at
            && now > ends_at
        {
            let mut st = self.sessions.lock();
            if st.current_index == idx && st.race_end_at.is_none() {
                st.time_up = true;
            }
        }
        self.end_race_if_all_finished();
    }

    /// Vanilla's Session.IsOver for a race, checked every pass: it moves on at
    /// once, with no 0x4B, when IS_OPEN=0 leaves fewer than two loaded cars,
    /// when IS_OPEN=2 does once the race has started, or when no car is
    /// loaded after the start (docs/02 "End of a race").
    fn race_abandoned(&self, cur: &SessionConfig) -> bool {
        let loaded = self.ready_count();
        let started = self.elapsed_ms() > 0;
        if cur.is_open == IsOpen::Closed && loaded < 2 {
            return true;
        }
        if cur.is_open == IsOpen::ClosesAtStart && started && loaded < 2 {
            return true;
        }
        loaded == 0 && started
    }

    /// After a race lap is recorded, decides whether it was the car's last
    /// (vanilla SessionResult.OnLapCompleted, docs/02 "End of a race"). In a
    /// lap race a car finishes on reaching LAPS, and once one has, every car
    /// finishes on its next lap. In a timed race, after TIME runs out, the
    /// next car to cross as leader finishes (with RACE_EXTRA_LAP, one lap
    /// after that first crossing), and every other car on its next lap after
    /// the leader. The first finish starts the RACE_OVER_TIME cap.
    pub(crate) fn on_race_lap(self: &Arc<Self>, car: &EntryCar) {
        let (cur, idx) = self.sessions.current();
        let laps = car.result().num_laps;
        let extra_lap = self.cfg.server.race_extra_lap;

        let (done, first) = {
            let mut st = self.sessions.lock();
            if st.current_index != idx {
                return;
            }
            let as_leader = laps > st.leader_laps;
            if as_leader {
                st.leader_laps = laps;
            }
            let mut done = false;
            if cur.laps > 0 {
                done = i64::from(laps) >= i64::from(cur.laps) || st.race_end_at.is_some();
            } else if !st.time_up {
                // Not over yet.
            } else if !as_leader {
                done = st.leader_done;
            } else if !extra_lap {
                st.leader_done = true;
                done = true;
            } else if st.target_lap == 0 {
                st.target_lap = laps + 1;
            } else if laps >= st.target_lap {
                st.leader_done = true;
                done = true;
            }
            let first = done && st.race_end_at.is_none();
            if first {
                st.race_end_at = Some(Instant::now());
            }
            (done, first)
        };

        if done {
            car.update(|s| s.result.has_completed_last_lap = true);
        }
        if first {
            self.arm_race_over(&cur, idx);
        }
    }

    /// Ends the race max(RACE_OVER_TIME, 30) s after the first car finished,
    /// unless every loaded car finishes first (measured).
    fn arm_race_over(self: &Arc<Self>, cur: &SessionConfig, idx: usize) {
        let cap_seconds = self
            .cfg
            .server
            .race_over_time
            .max(self.cfg.server.min_race_over);
        tracing::info!(session = %cur.name, cap_seconds, "race finishing");
        let delay = Duration::from_secs(cap_seconds.into());
        let server = Arc::clone(self);
        self.spawn_after(delay, move || server.end_session(idx));
    }

    /// Sends 0x4B once a car has finished and every loaded car has, without
    /// waiting for the RACE_OVER_TIME cap (vanilla Step).
    pub(crate) fn end_race_if_all_finished(self: &Arc<Self>) {
        let (idx, started) = {
            let st = self.sessions.lock();
            (st.current_index, st.race_end_at.is_some())
        };
        if !started {
            return;
        }
        for car in &self.entry_cars {
            if let Some(client) = car.client()
                && client.ready()
                && !car.result().has_completed_last_lap
            {
                return;
            }
        }
        self.end_session(idx);
    }

    /// Rejects a lap from a car that has completed its last lap (any session
    /// type), and a race lap before the start or after the race is over.
    /// Vanilla drops each without a 0x49.
    pub(crate) fn lap_refused(&self, car: &EntryCar) -> bool {
        if car.result().has_completed_last_lap {
            return true;
        }
        let (cur, finished) = {
            let st = self.sessions.lock();
            (st.sessions[st.current_index].clone(), st.finished)
        };
        if cur.kind != SessionType::Race {
            return false;
        }
        finished || self.elapsed_ms() < 0
    }

    /// Sends 0x4B once and advances after max(RESULT_SCREEN_TIME, 10)
    /// seconds. Vanilla does this for every session type.
    pub(crate) fn end_session(self: &Arc<Self>, idx: usize) {
        let name = {
            let mut st = self.sessions.lock();
            if st.current_index != idx || st.finished {
                return;
            }
            st.finished = true;
            st.sessions[idx].name.clone()
        };
        self.log_session(format!(
            "session over session={name} resultScreenSeconds={}",
            self.cfg.server.result_screen_time
        ));
        self.broadcast_race_over();

        // Vanilla's floor is 10 s (measured with 3, 5, 7 and 20).
        let seconds = self
            .cfg
            .server
            .result_screen_time
            .max(self.cfg.server.min_result_screen);
        let delay = Duration::from_secs(seconds.into());
        let server = Arc::clone(self);
        self.spawn_task(async move {
            let wait = tokio::time::sleep(delay);
            if server.shutdown.run_until_cancelled(wait).await.is_some() {
                server.wait_loaded().await;
                server.advance_from(idx);
            }
        });
    }

    /// Whether any client is still loading (vanilla StallSessionSwitch). An
    /// automatic switch waits for it; an admin one is refused.
    pub(crate) fn switch_stalled(&self) -> bool {
        for car in &self.entry_cars {
            if let Some(client) = car.client()
                && !client.has_sent_first_update.load(Ordering::SeqCst)
            {
                return true;
            }
        }
        false
    }

    /// Waits until no client is loading. It is bounded by the loading timeout.
    async fn wait_loaded(&self) {
        while self.switch_stalled() {
            let pause = tokio::time::sleep(Duration::from_millis(500));
            if self.shutdown.run_until_cancelled(pause).await.is_none() {
                return;
            }
        }
    }

    /// Moves to the next session, as vanilla's NextSession: after the last
    /// session it loops to the first with LOOP_MODE, and otherwise shuts the
    /// server down. A last race with REVERSED_GRID_RACE_POSITIONS first runs
    /// again on a reversed grid (docs/02 0x4A).
    pub(crate) fn advance_from(self: &Arc<Self>, from: usize) -> bool {
        if self.sessions.lock().current_index != from {
            return false;
        }

        // Vanilla's NextSession sorts the finished session's results once more
        // before writing them and taking the grid from them.
        self.resort_ranking();

        // Vanilla sends 0x37 only when it wrote a results file (measured).
        if let Some(path) = self.write_results_for_current_session() {
            self.plugin().on_end_session(&path);
        }

        let Some((next, invert, wrapped)) = self.next_index(from) else {
            tracing::info!("last session over and LOOP_MODE is off: shutting down");
            self.shutdown.cancel();
            return false;
        };

        // Take the grid from the finished session before set_session clears
        // its results. Unlike a restart, advancing also transfers (or, into
        // index 0, resets) grip and re-rolls the weather.
        let mut order = self.leaderboard_entries();
        if invert {
            order = reverse_race_grid(order, self.cfg.server.reversed_grid_race_positions.into());
        }
        self.set_grid(&order);
        if wrapped && self.dynamic {
            // Vanilla's NextSession reloads the entry list on a loop back with
            // pickup mode off, which also drops the bookings.
            self.reload_entry_list();
        }

        if next == 0 {
            // Vanilla resets grip whenever session index 0 starts (a loop wrap).
            self.weather.reset_grip(&self.cfg);
        } else {
            self.weather.transfer_grip(&self.cfg);
        }
        self.weather.reroll(&self.cfg);
        let wx = self.weather.snapshot();
        tracing::debug!(
            graphics = %wx.graphics, ambient = wx.ambient, road = wx.road,
            wind_speed = wx.wind_speed, wind_dir = wx.wind_direction, grip = wx.track_grip,
            "weather re-rolled for new session"
        );

        self.set_session(next, true);
        true
    }

    /// Records the next session's starting order, one id per slot.
    fn set_grid(&self, order: &[Arc<EntryCar>]) {
        let ids: Vec<u8> = order.iter().map(|car| car.session_id).collect();
        self.sessions.lock().grid = Some(ids);
    }

    /// Picks the session after `from`, as vanilla's NextSession does: `None`
    /// when there is none. After the last session it wraps to the first with
    /// LOOP_MODE; without, the server stops, unless the last session is a race
    /// owed its reversed-grid repeat. Returns (next, invert, wrapped): invert
    /// is a reversed grid (the repeat of a last race, or every run of a race
    /// that is the only session); wrapped is a loop back to the first.
    fn next_index(&self, from: usize) -> Option<(usize, bool, bool)> {
        // Read before taking the session lock, which listed_cars never needs.
        let no_cars_listed = self.listed_cars().is_empty();
        let mut st = self.sessions.lock();
        let cur = st.sessions[from].clone();
        if cur.kind == SessionType::Booking && no_cars_listed {
            // Vanilla: "NO CARS REGISTERED: RESTARTING BOOKING SESSION".
            tracing::info!("no cars registered: restarting the booking session");
            return Some((0, false, false));
        }
        let reversible =
            cur.kind == SessionType::Race && self.cfg.server.reversed_grid_race_positions != 0;
        let mut next = from + 1;
        let mut invert = false;
        let mut wrapped = false;
        if from == st.sessions.len() - 1 {
            if !self.cfg.server.loop_mode && (!reversible || st.last_race_inverted) {
                return None;
            }
            if reversible && st.sessions.len() < 2 {
                invert = true;
            } else if reversible && !st.last_race_inverted {
                invert = true;
                st.last_race_inverted = true;
                next = from;
            }
        }
        if next >= st.sessions.len() {
            // Only LOOP_MODE's wrap counts; a lone reversed race repeats in place.
            next = 0;
            wrapped = self.cfg.server.loop_mode;
        }
        if st.sessions[next].kind != SessionType::Race {
            st.last_race_inverted = false;
        }
        Some((next, invert, wrapped))
    }

    /// The current starting order, or entry-list order for the first session
    /// and after a restart.
    pub fn grid(&self) -> Vec<u8> {
        let grid = self.sessions.lock().grid.clone();
        let cars = self.listed_cars();
        if let Some(grid) = grid
            && grid.len() == cars.len()
        {
            return grid;
        }
        cars.iter().map(|car| car.session_id).collect()
    }

    /// Switches to session `index`, clears results and, if `notify`,
    /// announces it.
    pub(crate) fn set_session(&self, index: usize, notify: bool) {
        let session = {
            let _clock = self.clock_lock.write();
            let mut st = self.sessions.lock();
            self.rebase_clock_locked();
            st.current_index = index;
            st.finished = false;
            st.time_up = false;
            st.leader_laps = 0;
            st.leader_done = false;
            st.target_lap = 0;
            st.race_end_at = None;
            st.overtime = false;
            let session = st.sessions[index].clone();
            let now = self.server_time_millis();
            let wait = session_wait_seconds(&session, self.cfg.server.min_race_wait);
            st.start_time_ms = now + wait * 1000;
            st.ends_at = if session.time > 0 {
                let seconds = wait + i64::from(session.time) * 60;
                Some(Instant::now() + Duration::from_secs(seconds.max(0) as u64))
            } else {
                None
            };
            session
        };

        for car in &self.entry_cars {
            car.reset_result();
        }
        self.reset_ranking();
        self.votes.lock().discard();

        self.plugin().on_new_session();

        self.log_session(format!(
            "session started index={index} name={} type={} laps={} timeMinutes={} waitSeconds={}",
            session.name,
            session.kind.as_u8(),
            session.laps,
            session.time,
            session.wait_time
        ));

        if notify {
            self.broadcast_current_session();
            // Vanilla follows every session change with a 0x78. The 0x57
            // comes from the relay loop.
            self.broadcast_weather();
        }
        // Vanilla zeroes lastSunAngleUpdate here, so its next Step sends the
        // angle, recomputed from the new session's start, once the server is a
        // minute old, and the minute restarts from it.
        self.sun_sent_us.store(0, Ordering::SeqCst);
        self.step_sun();
    }

    /// The official guide's IS_OPEN rules.
    pub fn join_open(&self) -> bool {
        let (cur, _) = self.sessions.current();
        match cur.is_open {
            IsOpen::Closed => false,
            IsOpen::ClosesAtStart => {
                if cur.kind != SessionType::Race {
                    return true;
                }
                -self.elapsed_ms() > 20_000
            }
            IsOpen::Open => true,
        }
    }

    /// Restarts the current session, reporting whether it happened. Like
    /// vanilla, it is refused while a client is loading.
    pub fn restart_session(&self) -> bool {
        if self.switch_stalled() {
            tracing::warn!("session restart stalled: a client is still loading");
            return false;
        }
        let (_, idx) = self.sessions.current();
        // A restart reverts the grid to entry-list order and drops the
        // session's rubber (docs/02 0x4A).
        self.sessions.lock().grid = None;
        self.weather.reset_lap_grip();
        self.set_session(idx, true);
        true
    }

    /// Advances to the next session, reporting whether it happened. Refused
    /// while a client is loading.
    pub fn next_session(self: &Arc<Self>) -> bool {
        if self.switch_stalled() {
            tracing::warn!("session skip stalled: a client is still loading");
            return false;
        }
        let (_, idx) = self.sessions.current();
        self.advance_from(idx);
        true
    }
}
