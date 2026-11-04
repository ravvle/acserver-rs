//! Registration with the Kunos lobby, and keeping it updated (docs/05 §5.8).

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{lap_race, server_name, timed_race, track_name};
use crate::server::Server;

const LOBBY_PRIMARY: &str = "http://93.57.10.21/lobby.ashx";
const LOBBY_FALLBACK: &str = "http://lobby.assettocorsa.net/lobby.ashx";

/// Vanilla's keepalive cadence.
const PING_INTERVAL: Duration = Duration::from_secs(60);
/// How often to check for a session change.
const SESSION_POLL: Duration = Duration::from_secs(5);
/// How many "restart your server" replies in a row trigger a
/// re-registration, so one glitch does not.
const DROPPED_LIMIT: u32 = 2;
/// The bounds of the doubling wait between registration attempts.
const RETRY_MIN: Duration = Duration::from_secs(60);
const RETRY_MAX: Duration = Duration::from_secs(15 * 60);
const LOBBY_TIMEOUT: Duration = Duration::from_secs(15);

/// Whether a keepalive reply says the lobby forgot us: "ERROR - RESTART YOUR
/// SERVER TO REGISTER WITH THE LOBBY". Only "RESTART" is matched, in case the
/// wording changes.
fn lobby_dropped(body: &str) -> bool {
    let b = body.trim().to_uppercase();
    b.starts_with("ERROR") && b.contains("RESTART")
}

/// Whether a registration reply is the lobby's rate-limit refusal.
fn lobby_rate_limited(body: &str) -> bool {
    let b = body.trim().to_uppercase();
    b.starts_with("ERROR") && (b.contains("SLOW DOWN") || b.contains("TOO MANY"))
}

fn bool_str(b: bool) -> &'static str {
    if b { "1" } else { "0" }
}

/// Car models joined with commas, cut to at most 1200 bytes at a comma.
fn truncate_cars(cars: &[String]) -> String {
    let joined = cars.join(",");
    if joined.len() <= 1200 {
        return joined;
    }
    let mut cut = &joined[..1200];
    if let Some(i) = cut.rfind(',') {
        cut = &cut[..i];
    }
    cut.to_string()
}

/// A lobby call's query parameters, in order.
type Query = Vec<(&'static str, String)>;

/// The registrar's state across calls.
struct Registrar {
    server: Arc<Server>,
    base: &'static str,
    /// Consecutive failed keepalives, so an outage warns once.
    fails: u32,
    /// Consecutive "restart your server" replies.
    dropped: u32,
    backoff: Duration,
    retry_at: Option<Instant>,
    /// The lobby's limiter refused us.
    rate_limited: bool,
}

/// Registers, then sends vanilla's two calls until shutdown: a keepalive every
/// minute (LobbyPingRun) and a session update on each change
/// (LobbyUpdateSession).
pub async fn run(server: Arc<Server>) {
    if !server.cfg.server.register_to_lobby {
        tracing::info!(
            reason = "REGISTER_TO_LOBBY=0",
            "lobby registration disabled"
        );
        return;
    }
    let shutdown = server.shutdown.clone();
    let mut l = Registrar {
        server,
        base: LOBBY_PRIMARY,
        fails: 0,
        dropped: 0,
        backoff: Duration::ZERO,
        retry_at: None,
        rate_limited: false,
    };
    while !l.register().await {
        let wait = l.penalize();
        tracing::error!(retry_in = ?wait, rate_limited = l.rate_limited, "not registered with the lobby; server is not listed publicly");
        if shutdown
            .run_until_cancelled(tokio::time::sleep(wait))
            .await
            .is_none()
        {
            return;
        }
    }
    // Vanilla sends a session update right after registering.
    l.update_session().await;
    let (_, mut last_index) = l.server.sessions().current();
    let mut last_occupied = l.server.connected_count();
    let mut last_booking = l.server.booking_version();

    let mut pings =
        tokio::time::interval_at(tokio::time::Instant::now() + PING_INTERVAL, PING_INTERVAL);
    let mut sessions =
        tokio::time::interval_at(tokio::time::Instant::now() + SESSION_POLL, SESSION_POLL);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = pings.tick() => {
                l.ping().await;
                l.recover_if_dropped().await;
            }
            _ = sessions.tick() => {
                let (_, index) = l.server.sessions().current();
                if index != last_index {
                    last_index = index;
                    l.update_session().await;
                }
                // Vanilla sends a subscriptions ping on every join and leave,
                // and on every booking and release (addCar, removeCar).
                let occupied = l.server.connected_count();
                let booking = l.server.booking_version();
                if occupied != last_occupied || booking != last_booking {
                    last_occupied = occupied;
                    last_booking = booking;
                    l.update_subscriptions().await;
                }
            }
        }
    }
}

impl Registrar {
    /// Schedules the next registration attempt and returns the wait.
    fn penalize(&mut self) -> Duration {
        if self.backoff.is_zero() {
            self.backoff = RETRY_MIN;
        } else if self.backoff < RETRY_MAX {
            self.backoff = (self.backoff * 2).min(RETRY_MAX);
        }
        self.retry_at = Some(Instant::now() + self.backoff);
        self.backoff
    }

    async fn register(&mut self) -> bool {
        let server = Arc::clone(&self.server);
        let cfg = &server.cfg.server;
        let sessions = server.sessions().all();
        let wx = server.weather.snapshot();

        let timed = timed_race(&sessions);
        let mut types = Vec::new();
        let mut durations = Vec::new();
        for session in &sessions {
            types.push(session.kind.as_u8().to_string());
            // Laps for a lap race, else seconds, unlike /INFO's minutes (measured).
            if lap_race(session, timed) {
                durations.push(session.laps.to_string());
            } else {
                durations.push((u32::from(session.time) * 60).to_string());
            }
        }
        // MAX_CLIENTS capped by the entry list, unlike /INFO (measured).
        let max_clients = usize::from(cfg.max_clients).min(server.entry_cars().len());
        let track = track_name(&server.cfg);
        let query: Query = vec![
            // [AS] With EnableServerDetails the ℹ<port> suffix, as in /INFO: it is
            // how Content Manager finds /api/details, and with it the CSP features
            // the server requires, for a server picked from the lobby list.
            ("name", server_name(&server.cfg)),
            ("port", cfg.udp_port.to_string()),
            ("tcp_port", cfg.tcp_port.to_string()),
            ("max_clients", max_clients.to_string()),
            ("track", track.clone()),
            ("cars", truncate_cars(&server.cfg.distinct_models())),
            ("timeofday", (wx.sun_angle as i64).to_string()),
            ("sessions", types.join(",")),
            ("durations", durations.join(",")),
            ("password", bool_str(!cfg.password.is_empty()).to_string()),
            ("version", "202".to_string()),
            ("pickup", bool_str(cfg.pickup_mode_enabled).to_string()),
            ("autoclutch", bool_str(cfg.autoclutch_allowed).to_string()),
            ("abs", cfg.abs_allowed.to_string()),
            ("tc", cfg.tc_allowed.to_string()),
            ("stability", bool_str(cfg.stability_allowed).to_string()),
            ("legal_tyres", cfg.legal_tyres.clone()),
            ("fixed_setup", "0".to_string()),
            ("timed", bool_str(timed).to_string()),
            ("extra", bool_str(cfg.race_extra_lap).to_string()),
            ("pit", bool_str(cfg.race_pit_window_end != 0).to_string()),
            ("inverted", cfg.reversed_grid_race_positions.to_string()),
        ];

        self.rate_limited = false;
        for base in [LOBBY_PRIMARY, LOBBY_FALLBACK] {
            tracing::debug!(endpoint = base, params = ?query, "registering with lobby");
            let (body, result) = self.get(&format!("{base}/register"), &query).await;
            if result.is_ok() && body.trim().starts_with("OK") {
                self.base = base;
                self.backoff = Duration::ZERO;
                self.retry_at = None;
                self.rate_limited = false;
                tracing::info!(
                    endpoint = base, name = %cfg.name, track = %track, slots = max_clients,
                    udp = cfg.udp_port, tcp = cfg.tcp_port, http = cfg.http_port,
                    "registered with lobby"
                );
                return true;
            }
            // A 200 can still be a refusal; the body says why.
            if lobby_rate_limited(&body) {
                self.rate_limited = true;
            }
            tracing::warn!(endpoint = base, response = %body.trim(), err = ?result.err(), "lobby registration failed");
        }
        false
    }

    /// The keepalive call: port, tcp_port, timeleft, clients, pickup.
    async fn ping(&mut self) {
        let cfg = &self.server.cfg.server;
        let query: Query = vec![
            ("port", cfg.udp_port.to_string()),
            ("tcp_port", cfg.tcp_port.to_string()),
            (
                "timeleft",
                self.server.sessions().time_left_seconds().to_string(),
            ),
            ("clients", self.server.ready_count().to_string()),
            ("pickup", bool_str(cfg.pickup_mode_enabled).to_string()),
        ];
        self.keepalive("ping", &query).await;
    }

    /// The join/leave call (vanilla LobbyUpdateSubscriptions).
    async fn update_subscriptions(&mut self) {
        let cfg = &self.server.cfg.server;
        let query: Query = vec![
            ("clients", self.server.ready_count().to_string()),
            ("port", cfg.udp_port.to_string()),
            ("tcp_port", cfg.tcp_port.to_string()),
        ];
        self.keepalive("subscriptions update", &query).await;
    }

    /// The session-update call: session, timeleft, port, clients, track,
    /// pickup.
    async fn update_session(&mut self) {
        let cfg = &self.server.cfg.server;
        let (cur, _) = self.server.sessions().current();
        let query: Query = vec![
            // The session type, unlike /INFO's index (measured).
            ("session", cur.kind.as_u8().to_string()),
            (
                "timeleft",
                self.server.sessions().time_left_seconds().to_string(),
            ),
            ("port", cfg.udp_port.to_string()),
            ("clients", self.server.ready_count().to_string()),
            ("track", track_name(&self.server.cfg)),
            ("pickup", bool_str(cfg.pickup_mode_enabled).to_string()),
        ];
        tracing::debug!(session = %cur.name, kind = cur.kind.as_u8(), "lobby session update");
        self.keepalive("session update", &query).await;
    }

    /// One lobby call: the body (even on a non-200, capped at 4 KiB) and
    /// whether it succeeded.
    async fn get(&self, url: &str, query: &Query) -> (String, Result<(), String>) {
        let response = match self
            .server
            .http_client
            .get(url)
            .query(query)
            .timeout(LOBBY_TIMEOUT)
            .send()
            .await
        {
            Ok(response) => response,
            Err(err) => return (String::new(), Err(err.to_string())),
        };
        let status = response.status();
        let bytes = response.bytes().await.unwrap_or_default();
        let body = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]).into_owned();
        if status != reqwest::StatusCode::OK {
            return (body, Err(format!("http {status}")));
        }
        (body, Ok(()))
    }

    /// Sends a `/ping` call, logging one warning per outage and one on
    /// recovery.
    async fn keepalive(&mut self, kind: &str, query: &Query) {
        let url = format!("{}/ping", self.base);
        let (body, result) = self.get(&url, query).await;
        if let Err(err) = result {
            self.fails += 1;
            if self.fails == 1 {
                tracing::warn!(%err, "lobby {kind} failed");
            } else {
                tracing::debug!(%err, consecutive = self.fails, "lobby {kind} failed");
            }
            return;
        }
        if self.fails > 0 {
            tracing::info!(kind, after_failures = self.fails, "lobby reachable again");
            self.fails = 0;
        }
        if lobby_dropped(&body) {
            self.dropped += 1;
            tracing::warn!(kind, response = %body.trim(), consecutive = self.dropped, "lobby dropped our registration");
            return;
        }
        if self.dropped > 0 {
            tracing::info!(
                kind,
                after_dropped = self.dropped,
                "lobby registration accepted again"
            );
            self.dropped = 0;
        }
        tracing::debug!(response = %body.trim(), "lobby {kind}");
    }

    /// Re-registers after `DROPPED_LIMIT` "restart" replies in a row.
    /// Connected players are unaffected.
    async fn recover_if_dropped(&mut self) {
        if self.dropped < DROPPED_LIMIT {
            return;
        }
        if let Some(at) = self.retry_at
            && Instant::now() < at
        {
            // Respect the backoff, or the lobby's rate limiter trips again.
            tracing::debug!(
                rate_limited = self.rate_limited,
                "holding off lobby re-registration"
            );
            return;
        }
        tracing::warn!(
            after_dropped = self.dropped,
            "re-registering with the lobby"
        );
        self.dropped = 0;
        if !self.register().await {
            let wait = self.penalize();
            tracing::error!(retry_in = ?wait, rate_limited = self.rate_limited, "lobby re-registration failed; server stays unlisted");
            return;
        }
        // As after any registration, announce the session.
        self.update_session().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_and_car_list() {
        assert!(lobby_dropped(" error - RESTART YOUR SERVER "));
        assert!(!lobby_dropped("OK"));
        assert!(lobby_rate_limited(
            "ERROR - TOO MANY REGISTRATION ATTEMPTS, SLOW DOWN"
        ));
        let many: Vec<String> = (0..200).map(|i| format!("car_{i:03}")).collect();
        let cut = truncate_cars(&many);
        assert!(cut.len() <= 1200 && !cut.ends_with(','));
    }
}
