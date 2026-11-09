//! The results recorder: laps and collisions for the session in progress.

use parking_lot::Mutex;
use std::path::PathBuf;

use super::Server;
use super::client::Client;
use super::entrycar::EntryCar;
use crate::proto::SessionType;
use crate::results::{self, Car, Driver, Event, Lap, ResultRow, Session};
use crate::wire::Vec3;

#[derive(Debug, Default)]
struct RecorderInner {
    laps: Vec<Lap>,
    events: Vec<Event>,
}

/// Accumulates laps and collisions for the session in progress.
#[derive(Debug, Default)]
pub struct ResultsRecorder {
    inner: Mutex<RecorderInner>,
}

impl ResultsRecorder {
    fn add_event(&self, event: Event) {
        self.inner.lock().events.push(event);
    }

    fn add_lap(&self, lap: Lap) {
        self.inner.lock().laps.push(lap);
    }

    fn drain(&self) -> (Vec<Lap>, Vec<Event>) {
        let mut inner = self.inner.lock();
        (
            std::mem::take(&mut inner.laps),
            std::mem::take(&mut inner.events),
        )
    }
}

/// A slot's current driver as the results file lists it.
fn results_driver(car: &EntryCar) -> Driver {
    let id = car.identity();
    Driver {
        name: id.name,
        team: id.team.to_string(),
        nation: id.nation.to_string(),
        guid: id.guid,
        guids_list: None,
    }
}

impl Server {
    /// Stores a collision for the current session's results file (vanilla
    /// onClientEvent). `other` is `None` for the environment.
    pub(crate) fn record_collision(
        &self,
        client: &Client,
        other: Option<&EntryCar>,
        speed: f32,
        pos: Vec3,
        rel: Vec3,
    ) {
        let car = &self.entry_cars[usize::from(client.session_id())];
        let (kind, other_car_id, other_driver) = match other {
            Some(other) => (
                "COLLISION_WITH_CAR",
                i64::from(other.session_id),
                results_driver(other),
            ),
            None => ("COLLISION_WITH_ENV", -1, Driver::default()),
        };
        self.recorder.add_event(Event {
            kind: kind.to_string(),
            car_id: i64::from(client.session_id()),
            driver: results_driver(car),
            other_car_id,
            other_driver,
            impact_speed: speed,
            world_position: pos.into(),
            rel_position: rel.into(),
        });
    }

    /// Stores a lap for the current session's results file.
    pub(crate) fn record_lap(&self, client: &Client, lap_time: u32, sectors: Vec<u32>, cuts: u8) {
        let car = &self.entry_cars[usize::from(client.session_id())];
        let state = car.state();
        let id = client.identity();
        self.recorder.add_lap(Lap {
            car_id: i64::from(client.session_id()),
            driver_name: id.name.clone(),
            driver_guid: id.guid.clone(),
            car_model: state.model.clone(),
            lap_time,
            sectors: Some(sectors),
            tyre: String::new(),
            ballast_kg: state.ballast,
            restrictor: state.restrictor,
            cuts: i64::from(cuts),
            timestamp: self.server_time_millis() as u32,
        });
    }

    /// Writes the ending session's results JSON and returns its path, or
    /// `None` when nothing was written. Like vanilla, a session with no laps
    /// writes no file (measured).
    pub(crate) fn write_results_for_current_session(&self) -> Option<PathBuf> {
        let (laps, events) = self.recorder.drain();
        if !self.cfg.extra.write_result_json || laps.is_empty() {
            return None;
        }
        let (cur, _) = self.sessions.current();

        let mut doc = Session {
            track_name: self.cfg.server.track.clone(),
            track_config: self.cfg.server.track_config.clone(),
            kind: cur.kind.results_name().to_string(),
            // Vanilla wrote 0 for a 10-minute practice and a lap race (measured).
            duration_secs: if cur.kind == SessionType::Race {
                i64::from(cur.time) * 60
            } else {
                0
            },
            race_laps: i64::from(cur.laps),
            cars: None,
            result: None,
            laps: None,
            // null when there was no contact.
            events: if events.is_empty() {
                None
            } else {
                Some(events)
            },
        };

        // Every listed slot, empty ones included, as vanilla lists them (measured).
        let mut cars = Vec::new();
        for car in self.listed_cars() {
            let state = car.state();
            // Vanilla's value for a pickup slot (measured).
            let guids = if car.reserved() {
                state.guids.clone()
            } else {
                vec![String::new()]
            };
            cars.push(Car {
                car_id: i64::from(car.session_id),
                driver: Driver {
                    guids_list: Some(guids),
                    ..results_driver(&car)
                },
                model: car.model(),
                skin: state.skin.clone(),
                ballast_kg: state.ballast,
                restrictor: state.restrictor,
            });
        }
        doc.cars = (!cars.is_empty()).then_some(cars);

        let mut rows = Vec::new();
        for car in self.leaderboard_entries() {
            let state = car.state();
            let res = car.result();
            let id = car.identity();
            rows.push(ResultRow {
                driver_name: id.name,
                driver_guid: id.guid,
                car_id: i64::from(car.session_id),
                car_model: car.model(),
                best_lap: res.best_lap_or_none(),
                total_time: res.total_time,
                ballast_kg: state.ballast,
                restrictor: state.restrictor,
            });
        }
        doc.result = (!rows.is_empty()).then_some(rows);

        doc.laps = Some(laps);

        match results::write(&self.cfg.base_dir, cur.kind, &self.cfg.server.name, &doc) {
            Ok(path) => {
                tracing::info!(path = %path.display(), "results written");
                Some(path)
            }
            Err(err) => {
                tracing::error!(%err, "write results");
                None
            }
        }
    }
}
