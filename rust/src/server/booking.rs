//! Booking mode (docs/05 §5.2a): vanilla's addCar, removeCar and canJoin.
//!
//! With pickup mode off and a [BOOK] session, the car list holds the entry
//! list's slots plus one per booking: /SUB books a free session id below
//! MAX_CLIENTS, /UNSUB releases it, and booked drivers join once the booking
//! session is over. The slot array is sized for MAX_CLIENTS up front so a
//! session id stays its index; an unlisted slot is simply not in the list.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::Server;
use super::entrycar::{EntryCar, Identity};
use super::handshake::secret_equal;
use super::setup::{load_fixed_setup, setup_file};
use crate::config::parse_steam_id;
use crate::proto::SessionType;
use crate::wire::Narrow;

impl Server {
    /// Whether the server is in booking mode.
    pub fn booking(&self) -> bool {
        self.booking
    }

    /// Changes on every booking and release, so the lobby can send its
    /// subscriptions ping, as vanilla's addCar and removeCar do.
    pub fn booking_version(&self) -> i64 {
        self.booking_version.load(Ordering::SeqCst)
    }

    /// Vanilla's car list: every slot, or a booking server's listed ones.
    pub fn listed_cars(&self) -> Vec<Arc<EntryCar>> {
        if !self.dynamic {
            return self.entry_cars.clone();
        }
        self.entry_cars
            .iter()
            .filter(|car| car.listed())
            .cloned()
            .collect()
    }

    /// Vanilla's GetCarByDriverGuid: the listed slot reserved for `guid` or
    /// whose driver (booked or last seated) has it.
    pub(crate) fn car_by_driver_guid(&self, guid: &str) -> Option<Arc<EntryCar>> {
        self.listed_cars()
            .into_iter()
            .find(|car| car.reserved_for(guid) || car.state().driver.guid == guid)
    }

    /// The current session's time left in seconds, which vanilla's booking
    /// replies carry (Session.TimeLeft / 1000).
    fn booking_seconds_left(&self) -> i64 {
        self.sessions.time_left_ms() / 1000
    }

    /// Books a car for a driver (vanilla addCar via /SUB) and returns the
    /// reply: the checks a join runs, then an update of the guid's booking or
    /// a new slot with the lowest free session id.
    pub async fn subscribe(
        &self,
        model: &str,
        skin: &str,
        name: &str,
        team: &str,
        guid: &str,
        password: &str,
    ) -> String {
        let cfg = &self.cfg.server;
        let is_admin =
            !cfg.admin_password.is_empty() && secret_equal(password, &cfg.admin_password);
        if !cfg.password.is_empty() && !is_admin && !secret_equal(password, &cfg.password) {
            return "INCORRECT PASSWORD".to_string();
        }
        if !self.car_legal(model) {
            return format!("ILLEGAL CAR,{}", self.booking_seconds_left());
        }
        if parse_steam_id(guid).is_some_and(|n| self.blacklisted(n)) {
            return "BLACKLISTED".to_string();
        }
        if let Err(reason) = self.plugin_auth(guid, password).await {
            return format!("PLUGIN AUTH FAILED: {reason}");
        }

        let _alloc = self.alloc.lock();
        let driver = Identity {
            name: name.to_string(),
            team: team.into(),
            nation: Narrow::default(),
            guid: guid.to_string(),
        };
        if let Some(car) = self.car_by_driver_guid(guid) {
            car.update(|s| {
                s.model = model.to_string();
                s.skin = skin.to_string();
                s.driver = driver;
            });
            car.booked_as_admin.store(is_admin, Ordering::SeqCst);
            tracing::info!(car = car.session_id, name, model, guid, "booking updated");
            self.booking_version.fetch_add(1, Ordering::SeqCst);
            return format!("OK,{}", self.booking_seconds_left());
        }
        let Some(car) = self.free_booking_slot() else {
            return "SERVER FULL".to_string();
        };
        let setup = load_fixed_setup(&self.cfg.base_dir, &setup_file(model, false));
        car.update(|s| {
            s.model = model.to_string();
            s.skin = skin.to_string();
            s.setup = setup;
            s.driver = driver;
        });
        car.booked_as_admin.store(is_admin, Ordering::SeqCst);
        car.set_listed(true);
        tracing::info!(car = car.session_id, name, model, guid, "booking added");
        self.booking_version.fetch_add(1, Ordering::SeqCst);
        format!("OK,{}", self.booking_seconds_left())
    }

    /// Vanilla's findAvailableSessionID: the unlisted slot with the lowest
    /// session id below MAX_CLIENTS. The caller holds the alloc lock.
    fn free_booking_slot(&self) -> Option<Arc<EntryCar>> {
        for car in &self.entry_cars {
            if car.session_id >= self.cfg.server.max_clients {
                break;
            }
            if !car.listed() {
                return Some(Arc::clone(car));
            }
        }
        None
    }

    /// Releases a guid's booking (vanilla removeCar via /UNSUB).
    pub fn unsubscribe(&self, guid: &str) -> String {
        let car = {
            let _alloc = self.alloc.lock();
            let car = self.car_by_driver_guid(guid);
            if let Some(car) = &car {
                self.unlist(car);
            }
            car
        };
        let Some(car) = car else {
            return "UNKNOWN GUID".to_string();
        };
        tracing::info!(car = car.session_id, guid, "booking removed");
        self.booking_version.fetch_add(1, Ordering::SeqCst);
        "OK".to_string()
    }

    /// Takes a slot out of the car list, dropping its booking and any
    /// occupant. The caller holds the alloc lock.
    fn unlist(&self, car: &EntryCar) {
        car.clear_listing();
        if let Some(client) = car.client() {
            self.disconnect(&client, "slot unlisted");
        }
    }

    /// Answers /CANJOIN (vanilla canJoin): YES when a listed slot is the
    /// guid's and the booking session is over, NO otherwise.
    pub fn can_join(&self, guid: &str) -> String {
        let (cur, _) = self.sessions.current();
        if cur.kind == SessionType::Booking || self.car_by_driver_guid(guid).is_none() {
            return "NO".to_string();
        }
        "YES".to_string()
    }

    /// Reads entry_list.ini again when the sessions loop back with pickup
    /// mode off, as vanilla's NextSession empties its car list and reloads the
    /// file: bookings are dropped and edits take effect. Vanilla orphans every
    /// connected client; here a driver keeps their seat unless their slot
    /// changed under them (another model, a reservation that leaves them out,
    /// or no car any more). A file that fails to load or check keeps the
    /// previous list.
    pub(crate) fn reload_entry_list(&self) {
        let reloaded = match self.cfg.reload_entry_list() {
            Ok(entries) => {
                tracing::info!(cars = entries.len(), "entry list reloaded");
                Some(entries)
            }
            Err(err) => {
                tracing::error!(%err, "entry list reload failed; keeping the previous list");
                None
            }
        };
        let mut bop_changed = false;
        {
            let mut alloc = self.alloc.lock();
            let loaded_list = self.car_list_layout();
            let entries = match reloaded {
                Some(entries) => entries,
                None => alloc.loaded_entries.clone(),
            };
            for (i, car) in self.entry_cars.iter().enumerate() {
                let Some(entry) = entries.get(i) else {
                    if car.listed() {
                        self.unlist(car);
                    }
                    continue;
                };
                let occupant = car.client();
                car.configure(entry);
                let keep = match &occupant {
                    Some(client) => {
                        let id = client.identity();
                        id.car_model.eq_ignore_ascii_case(&entry.model)
                            && (!car.reserved() || car.reserved_for(&id.guid))
                    }
                    None => false,
                };
                if let Some(client) = &occupant
                    && !keep
                {
                    tracing::info!(
                        car = car.session_id,
                        name = client.name(),
                        "entry list reload took the slot; disconnecting"
                    );
                    self.disconnect(client, "entry list reload");
                }
                let setup = load_fixed_setup(
                    &self.cfg.base_dir,
                    &setup_file(&entry.fixed_setup, entry.fixed_setup_named),
                );
                car.booked_as_admin.store(false, Ordering::SeqCst);
                let changed = car.update(|s| {
                    s.setup = setup;
                    let changed = s.ballast != entry.ballast || s.restrictor != entry.restrictor;
                    s.ballast = entry.ballast;
                    s.restrictor = entry.restrictor;
                    changed
                });
                bop_changed = bop_changed || (changed && keep);
                // Vanilla's cars are new, so no slot remembers a driver from
                // before; a kept driver is listed from their connection until
                // they leave.
                car.reset_driver(entry);
            }
            // A client never asks for the car list again, and the game reads
            // 0x49, 0x4A and 0x4B one row per car of the list it loaded, not
            // by their counts. If the list changed, nobody's still fits.
            if self.car_list_layout() != loaded_list {
                for car in &self.entry_cars {
                    if let Some(client) = car.client() {
                        tracing::info!(
                            car = car.session_id,
                            name = client.name(),
                            "entry list reload changed the car list; disconnecting"
                        );
                        self.disconnect(&client, "entry list reload");
                    }
                }
            }
            alloc.loaded_entries = entries;
        }
        self.booking_version.fetch_add(1, Ordering::SeqCst);
        if bop_changed {
            self.broadcast_bop();
        }
    }

    /// The listed cars as a joining client loads them from 0x40: slot,
    /// model, skin and whether it is a spectator's.
    fn car_list_layout(&self) -> Vec<(u8, String, String, bool)> {
        self.listed_cars()
            .iter()
            .map(|car| {
                let state = car.state();
                (
                    car.session_id,
                    car.model(),
                    state.skin,
                    state.spectator_mode,
                )
            })
            .collect()
    }

    /// Whether `model` is in [SERVER] CARS (vanilla isCarLegal).
    pub(crate) fn car_legal(&self, model: &str) -> bool {
        self.cfg
            .server
            .cars
            .iter()
            .any(|m| m.eq_ignore_ascii_case(model))
    }
}
