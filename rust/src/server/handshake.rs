//! 0x3D validation in order, slot allocation, 0x3E and AUTH_PLUGIN_ADDRESS.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::Server;
use super::client::{Client, ClientIdentity};
use super::csp::parse_csp_version;
use super::entrycar::EntryCar;
use crate::config::parse_steam_id;
use crate::proto::{self, SessionType};
use crate::wire::{Reader, Writer};

/// The wstring8 ceiling: any name a client can encode survives intact, as in
/// vanilla, while the stored string stays bounded.
const MAX_DRIVER_NAME_CHARS: usize = 255;

/// Bounds the wait on the auth service. The reference sets no timeout, which
/// would let a hung service stall joins forever.
const AUTH_TIMEOUT: Duration = Duration::from_secs(5);

/// Compares passwords in constant time.
pub fn secret_equal(got: &str, want: &str) -> bool {
    use subtle::ConstantTimeEq as _;
    got.as_bytes().ct_eq(want.as_bytes()).into()
}

/// What a 0x3D asks for.
struct JoinRequest {
    version: u16,
    guid: String,
    name: String,
    team: String,
    nation: String,
    requested_car: String,
    password: String,
}

/// Logs a refused join: without it "why can't I join?" has no answer.
fn log_rejection(client: &Client, req: &JoinRequest, reason: &str, detail: String) {
    tracing::info!(reason, name = %req.name, guid = %req.guid, addr = %client.peer, detail = %detail, "handshake rejected");
}

impl Server {
    /// Validates a 0x3D request and, on success, assigns a slot and sends
    /// 0x3E. Returns false when the connection should be closed.
    pub(crate) async fn handle_handshake(
        self: &Arc<Self>,
        client: &Arc<Client>,
        r: &mut Reader<'_>,
    ) -> bool {
        let req = JoinRequest {
            version: r.u16(),
            guid: r.string8(),
            name: r.wstring8(),
            team: r.string8(),
            nation: r.string8(),
            requested_car: r.string8(),
            password: r.string8(),
        };
        // [CSP] The optional feature list, ending in the CSP build. Vanilla
        // stops reading here, as does the Steam ticket after it.
        let mut features = String::new();
        if r.remaining() >= 2 {
            features = r.string16();
            let version = parse_csp_version(&features);
            client.csp.version.store(version, Ordering::SeqCst);
        }

        let reject = |reason: &str, detail: String| log_rejection(client, &req, reason, detail);

        // 1. Protocol version.
        if req.version != proto::CLIENT_PROTOCOL_VERSION {
            let mut w = Writer::with_id(proto::UNSUPPORTED_PROTOCOL);
            w.u16(proto::CLIENT_PROTOCOL_VERSION);
            self.send_tcp(client, w.bytes());
            reject(
                "protocol mismatch",
                format!(
                    "client={} server={}",
                    req.version,
                    proto::CLIENT_PROTOCOL_VERSION
                ),
            );
            return false;
        }

        // An unparseable guid is 0, refused below.
        let guid = parse_steam_id(req.guid.trim()).unwrap_or(0);
        let guid_str = guid.to_string();

        // 2. [AS] Steam guid required. Vanilla admits a zero guid (docs/01 §1.5).
        if guid == 0 {
            self.send_auth_failed(
                client,
                "AC Pro is not supported. Please use the Steam version.",
            );
            reject("missing steam guid", String::new());
            return false;
        }

        // 3. Blacklist.
        if self.blacklisted(guid) {
            self.send_tcp(client, &[proto::BLACKLISTED]);
            reject("blacklisted", String::new());
            return false;
        }

        // 4. Admission and password. With a locked entry list, reserved owners
        // skip the join password (official guide). With pickup mode off
        // vanilla seats a driver as admin only if they booked as one
        // (createNewCarConnection's BookedAsAdmin); the join's password does
        // not count, though /admin still works.
        let server_cfg = &self.cfg.server;
        let is_admin = server_cfg.pickup_mode_enabled
            && ((!server_cfg.admin_password.is_empty()
                && secret_equal(&req.password, &server_cfg.admin_password))
                || self.group_admin(guid));

        // A booking server admits only the drivers it has booked, whatever car
        // they ask for, and none before the booking session is over (vanilla
        // onNewConnection). Their checks ran at /SUB. Vanilla's 0x41 with the
        // time left goes to the UDP address of a packet that came over TCP,
        // which has none, so a driver who is too early hears nothing (probe
        // BookingFlow).
        if self.booking {
            let Some(booked) = self.car_by_driver_guid(&guid_str) else {
                reject("not booked", String::new());
                return false;
            };
            let (cur, _) = self.sessions.current();
            if cur.kind == SessionType::Booking {
                reject(
                    "still in the booking session",
                    format!(
                        "car={} msLeft={}",
                        booked.session_id,
                        self.sessions.time_left_ms()
                    ),
                );
                return false;
            }
            if !self.check_csp(client, &req, &features) {
                return false;
            }
            return self.admit_booked(client, &booked, guid, &guid_str, &req.nation, is_admin);
        }

        let locked = !server_cfg.pickup_mode_enabled || server_cfg.locked_entry_list;
        let reserved_owner = self.has_reserved_slot(&guid_str, &req.requested_car);
        if locked && !reserved_owner {
            reject(
                "entry list is locked",
                format!("requestedCar={}", req.requested_car),
            );
            return false;
        }
        let password_exempt = reserved_owner && locked;
        if !server_cfg.password.is_empty()
            && !is_admin
            && !password_exempt
            && !secret_equal(&req.password, &server_cfg.password)
        {
            self.send_tcp(client, &[proto::WRONG_PASSWORD]);
            reject("wrong password", String::new());
            return false;
        }

        // 5. IS_OPEN, per the official guide (see join_open).
        if !self.join_open() {
            self.send_tcp(client, &[proto::SESSION_CLOSED]);
            reject("session closed", String::new());
            return false;
        }

        // 6. [AS] Driver name required; vanilla admits a blank one.
        let mut name = req.name.trim().to_string();
        if name.is_empty() && !self.has_reserved_driver_name(&guid_str, &req.requested_car) {
            self.send_auth_failed(client, "Driver name cannot be empty.");
            reject("empty driver name", String::new());
            return false;
        }
        name = name.chars().take(MAX_DRIVER_NAME_CHARS).collect();

        // 7. [CSP] Minimum CSP build, then the required features.
        if !self.check_csp(client, &req, &features) {
            return false;
        }

        // [AS] WhitelistUserGroup, once it lists anyone.
        if !self.whitelisted(guid) {
            self.send_auth_failed(client, "You are not whitelisted on this server");
            reject("not whitelisted", String::new());
            return false;
        }

        // [AS] EnablePlugins' name filters (WordFilterPlugin).
        if let Some(reason) = self.plugin_rejects_join(&name) {
            self.send_auth_failed(client, &reason);
            reject("name refused by a plugin", String::new());
            return false;
        }

        // One connection per guid, or one client can take the whole grid. A
        // repeat join replaces the old connection in claim_slot, after the
        // auth plugin, so a player whose link died can rejoin at once.

        // 8. AUTH_PLUGIN_ADDRESS, after the cheap checks and before a slot is
        // taken (reference addCar → DoPluginAuth).
        if let Err(reason) = self.plugin_auth(&guid_str, &req.password).await {
            self.send_auth_failed(client, &format!("Access denied: {reason}"));
            reject("auth plugin denied", format!("authReason={reason}"));
            return false;
        }

        // 9. Slot allocation. claim_slot re-checks the guid under the lock.
        let Some(car) = self.claim_slot(client, &req, guid, &name, is_admin) else {
            self.send_tcp(client, &[proto::NO_SLOTS_AVAILABLE]);
            reject(
                "no slot available",
                format!("requestedCar={}", req.requested_car),
            );
            return false;
        };

        tracing::info!(car = car.session_id, name = %name, model = %car.model(), admin = is_admin, "client handshake");
        self.complete_handshake(client, &car);
        true
    }

    /// [CSP] The minimum build, then the features the server requires
    /// (AssettoServer's order). Rejects the client and returns false when
    /// either is missing.
    fn check_csp(&self, client: &Client, req: &JoinRequest, features: &str) -> bool {
        if let Some(reason) = self.csp_too_old(client) {
            self.send_auth_failed(client, &reason);
            let detail = format!("csp={}", client.csp_version());
            log_rejection(client, req, &reason, detail);
            return false;
        }
        if !self.csp_features_ok(features) {
            self.send_auth_failed(
                client,
                "Missing CSP features. Please update CSP and/or Content Manager.",
            );
            let detail = format!("features={features}");
            log_rejection(client, req, "missing csp features", detail);
            return false;
        }
        self.note_csp_features(client, features);
        true
    }

    /// Seats a booked driver in their slot, under the name and team they
    /// booked with, as admin if they booked with the admin password.
    fn admit_booked(
        self: &Arc<Self>,
        client: &Arc<Client>,
        car: &Arc<EntryCar>,
        guid: u64,
        guid_str: &str,
        nation: &str,
        is_admin: bool,
    ) -> bool {
        {
            let _alloc = self.alloc.lock();
            if let Some(old) = self.client_by_guid(guid) {
                self.disconnect(&old, "guid rejoined");
            }
            if car.occupied() || !car.listed() {
                return false;
            }
            let booking = car.state().driver;
            client.set_identity(ClientIdentity {
                session_id: car.session_id,
                guid: guid_str.to_string(),
                name: booking.name,
                team: booking.team,
                nation: nation.to_string(),
                car_model: car.model(),
            });
            client.set_admin(is_admin || car.booked_as_admin.load(Ordering::SeqCst));
            car.seat(client);
            car.update(|s| s.client = Some(Arc::clone(client)));
        }

        tracing::info!(car = car.session_id, name = client.name(), model = %car.model(), admin = client.is_admin(), booked = true, "client handshake");
        self.complete_handshake(client, car);
        true
    }

    /// Sends 0x3E and announces the joined car.
    fn complete_handshake(&self, client: &Arc<Client>, car: &EntryCar) {
        self.mark_active(client);
        self.send_handshake_response(client, car);
        // Broadcasts may reach the client only once its 0x3E is queued.
        client.handshaked.store(true, Ordering::SeqCst);
        // AI stops driving the slot first, so its 0x4D precedes the 0x5A.
        self.traffic_on_join(car);
        // Vanilla sends 0x5A at the handshake, not the first update.
        self.broadcast_car_connected(car);
        // A /ballast or /restrictor does not outlive the connection: vanilla
        // restores the entry-list BoP for every join and, if that changed it,
        // tells the loaded clients after the 0x5A (docs/02 0x70).
        if car.reset_bop() {
            self.broadcast_bop();
        }
        self.plugin().on_client_connected(client);
    }

    /// The connected client holding a Steam guid.
    pub(crate) fn client_by_guid(&self, guid: u64) -> Option<Arc<Client>> {
        let mut clients = self.entry_cars.iter().filter_map(|car| car.client());
        clients.find(|client| client.identity().steam_id() == guid)
    }

    /// Finds a free slot for the model. An empty model matches nothing, as in
    /// vanilla. It follows vanilla's onNewConnectionPickup (docs/01 §1.5):
    /// first a slot reserved for guid, or one whose last driver was guid,
    /// which keeps its results; then the first free slot no driver has used,
    /// else the first free one, whose results the caller resets (fresh).
    /// Returns the slot and whether it is fresh.
    fn allocate_slot(&self, guid: u64, model: &str) -> Option<(Arc<EntryCar>, bool)> {
        if model.is_empty() {
            return None;
        }
        // MAX_CLIENTS may be below the entry-list size; /INFO advertises it.
        if self.connected_count() >= usize::from(self.cfg.server.max_clients)
            || self.traffic_player_limit()
        {
            return None;
        }
        let free = |car: &EntryCar| {
            !self.ai_only(car) && !car.occupied() && car.model().eq_ignore_ascii_case(model)
        };
        let guid_str = guid.to_string();
        for car in &self.entry_cars {
            if !free(car) {
                continue;
            }
            if car.reserved_for(&guid_str)
                || (!car.reserved() && car.state().driver.guid == guid_str)
            {
                return Some((Arc::clone(car), false));
            }
        }
        if !self.cfg.server.pickup_mode_enabled || self.cfg.server.locked_entry_list {
            return None;
        }
        let mut fallback: Option<Arc<EntryCar>> = None;
        for car in &self.entry_cars {
            if !free(car) || car.reserved() {
                continue;
            }
            if car.state().driver.guid.is_empty() {
                return Some((Arc::clone(car), true));
            }
            if fallback.is_none() {
                fallback = Some(Arc::clone(car));
            }
        }
        fallback.map(|car| (car, true))
    }

    fn has_reserved_slot(&self, guid: &str, model: &str) -> bool {
        self.entry_cars.iter().any(|car| {
            !self.ai_only(car)
                && !car.occupied()
                && car.model().eq_ignore_ascii_case(model)
                && car.reserved_for(guid)
        })
    }

    fn has_reserved_driver_name(&self, guid: &str, model: &str) -> bool {
        self.entry_cars.iter().any(|car| {
            car.reserved_for(guid)
                && car.model().eq_ignore_ascii_case(model)
                && !car.state().driver_name.trim().is_empty()
        })
    }

    /// Finds a slot and seats `client` in it under the alloc lock, so
    /// concurrent joins cannot take the same one. If the guid is already
    /// connected, that connection is dropped first and its slot reused when
    /// the model matches, so a rejoin keeps its results. `name` is the
    /// request's, trimmed and capped.
    fn claim_slot(
        &self,
        client: &Arc<Client>,
        req: &JoinRequest,
        guid: u64,
        name: &str,
        is_admin: bool,
    ) -> Option<Arc<EntryCar>> {
        let model = req.requested_car.as_str();
        let _alloc = self.alloc.lock();

        let mut chosen: Option<(Arc<EntryCar>, bool)> = None;
        if let Some(old) = self.client_by_guid(guid) {
            tracing::info!(
                car = old.session_id(),
                name = old.name(),
                "guid rejoined, dropping the old connection"
            );
            self.disconnect(&old, "guid rejoined"); // frees the slot synchronously
            let previous = &self.entry_cars[usize::from(old.session_id())];
            if !previous.occupied() && previous.model().eq_ignore_ascii_case(model) {
                chosen = Some((Arc::clone(previous), false));
            }
        }
        if chosen.is_none() {
            chosen = self.allocate_slot(guid, model);
        }
        let (car, fresh) = chosen?;
        if fresh {
            // Vanilla's ResetCarResults re-sorts while the slot is still empty.
            car.reset_laps();
            self.resort_ranking();
        }

        let reserved = car.reserved();
        let state = car.state();
        let reserved_name = state.driver_name;
        let reserved_team = state.team;
        client.set_identity(ClientIdentity {
            session_id: car.session_id,
            guid: guid.to_string(),
            name: if reserved && !reserved_name.is_empty() {
                reserved_name
            } else {
                name.to_string()
            },
            team: if reserved && !reserved_team.is_empty() {
                reserved_team
            } else {
                req.team.clone()
            },
            nation: req.nation.clone(),
            car_model: car.model(),
        });
        client.set_admin(is_admin);
        car.seat(client);
        // Publishes the client; every field above is set first.
        car.update(|s| s.client = Some(Arc::clone(client)));
        Some(car)
    }

    fn send_auth_failed(&self, client: &Client, reason: &str) {
        let mut w = Writer::with_id(proto::AUTH_FAILED);
        w.wstring8(reason);
        self.send_tcp(client, w.bytes());
    }

    /// Builds and sends 0x3E for the assigned car.
    fn send_handshake_response(&self, client: &Client, car: &EntryCar) {
        let cfg = &self.cfg.server;
        let (cur, cur_idx) = self.sessions.current();
        let sessions = self.sessions.all();
        let weather = self.weather.snapshot();
        let model = car.model();
        let paths = self.checksum_paths(&model);

        let mut w = Writer::with_id(proto::NEW_CAR_CONNECTION);
        w.wstring8(&cfg.name);
        w.u16(cfg.udp_port);
        w.u8(cfg.refresh_rate_hz);
        w.string8(&cfg.track);
        w.string8(&cfg.track_config);
        w.string8(&model);
        w.string8(&car.state().skin);
        w.f32(weather.sun_angle);
        w.i16(cfg.allowed_tyres_out);
        w.bool(cfg.tyre_blankets_allowed);
        w.u8(cfg.tc_allowed);
        w.u8(cfg.abs_allowed);
        w.bool(cfg.stability_allowed);
        w.bool(cfg.autoclutch_allowed);
        w.u8(cfg.start_rule); // JumpStartPenaltyMode
        // MechanicalDamageRate: DAMAGE_MULTIPLIER as a fraction.
        w.f32(f32::from(cfg.damage_multiplier) / 100.0);
        w.f32(f32::from(cfg.fuel_rate) / 100.0);
        w.f32(f32::from(cfg.tyre_wear_rate) / 100.0);
        w.bool(cfg.force_virtual_mirror);
        w.u8(cfg.max_contacts_per_km);
        w.i32(i32::from(cfg.race_over_time) * 1000);
        w.i32(i32::from(cfg.result_screen_time) * 1000);
        w.bool(cfg.race_extra_lap);
        w.bool(cfg.race_gas_penalty_disabled);
        w.i16(cfg.race_pit_window_start);
        w.i16(cfg.race_pit_window_end);
        w.i16(cfg.reversed_grid_race_positions);
        w.u8(car.session_id);
        w.u8(sessions.len() as u8);
        for session in &sessions {
            w.u8(session.kind.as_u8());
            w.u16(session.laps);
            w.u16(session.time);
        }
        // Current session block.
        w.string8(&cur.name);
        w.u8(cur_idx as u8);
        w.u8(cur.kind.as_u8());
        w.u16(cur.time);
        w.u16(cur.laps);
        w.f32(weather.track_grip);
        w.u8(car.session_id); // SpawnPosition (pit box index)
        w.i64(self.elapsed_ms());
        w.u8(paths.len() as u8);
        for path in &paths {
            w.string8(path);
        }
        w.string8(&cfg.legal_tyres);
        w.i32(self.random_seed); // RandomSeed, shared by every client
        w.i32(self.server_time_millis() as i32); // CurrentTime: the server clock now

        self.send_tcp(client, w.bytes());
    }

    /// Asks the configured service whether a player may join:
    ///
    ///   http://<AUTH_PLUGIN_ADDRESS>&GUID=<guid>&PSW=<password>
    ///
    /// A `|`-split reply of OK admits; DENY[|reason] rejects with the reason.
    /// Errors and anything else admit, as vanilla does.
    pub(crate) async fn plugin_auth(&self, guid: &str, password: &str) -> Result<(), String> {
        let address = &self.cfg.server.auth_plugin_address;
        if address.is_empty() {
            return Ok(());
        }
        let url = format!("http://{address}&GUID={guid}&PSW={password}");
        tracing::info!(url = %address, "calling auth plugin");

        let body = match self.fetch_auth(&url).await {
            Ok(body) => body,
            Err(err) => {
                tracing::warn!(%err, "auth plugin error, admitting player by default");
                return Ok(());
            }
        };
        let mut fields = body.split('|').map(str::trim);
        match fields.next().unwrap_or("") {
            "OK" => {
                tracing::info!(guid, "auth plugin authorized player");
                Ok(())
            }
            "DENY" => {
                let reason = match fields.next() {
                    Some(reason) if !reason.is_empty() => reason.to_string(),
                    _ => "UNKNOWN".to_string(),
                };
                tracing::info!(guid, reason = %reason, "auth plugin denied player");
                Err(reason)
            }
            _ => Ok(()),
        }
    }

    async fn fetch_auth(&self, url: &str) -> Result<String, reqwest::Error> {
        let request = self.http_client.get(url).timeout(AUTH_TIMEOUT);
        request.send().await?.text().await
    }
}
