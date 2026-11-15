//! The results recorder: laps and collisions for the session in progress.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::PathBuf;

use super::Server;
use super::client::Client;
use super::entrycar::EntryCar;
use crate::proto::SessionType;
use crate::results::{self, Car, Driver, Event, Lap, ResultRow, Session};
use crate::wire::Vec3;

/// The most laps and collisions one car records in a session. A session
/// with no time or lap limit never ends, and a client chooses what it
/// reports, so without a cap the recorder grows until the process runs out
/// of memory. Far above any real session: a car would need a lap under 90 s
/// for a whole day to reach it.
const MAX_LAPS_PER_CAR: usize = 1000;
const MAX_EVENTS_PER_CAR: usize = 500;

#[derive(Debug, Default)]
struct RecorderInner {
    laps: Vec<Lap>,
    events: Vec<Event>,
    /// Laps and events recorded so far, by car id.
    per_car: HashMap<i64, (usize, usize)>,
}

/// Accumulates laps and collisions for the session in progress.
#[derive(Debug, Default)]
pub struct ResultsRecorder {
    inner: Mutex<RecorderInner>,
}

/// Counts one more record against `cap`, warning when it is reached.
fn take_room(count: &mut usize, cap: usize, car_id: i64, what: &str) -> bool {
    if *count >= cap {
        return false;
    }
    *count += 1;
    if *count == cap {
        tracing::warn!(
            car = car_id,
            cap,
            "results: {what} cap reached; later ones this session are not recorded"
        );
    }
    true
}

impl ResultsRecorder {
    /// Records the event `make` builds, unless the car has used its share.
    fn add_event(&self, car_id: i64, make: impl FnOnce() -> Event) {
        let mut inner = self.inner.lock();
        let (_, events) = inner.per_car.entry(car_id).or_default();
        if take_room(events, MAX_EVENTS_PER_CAR, car_id, "collision") {
            inner.events.push(make());
        }
    }

    /// Records the lap `make` builds, unless the car has used its share.
    fn add_lap(&self, car_id: i64, make: impl FnOnce() -> Lap) {
        let mut inner = self.inner.lock();
        let (laps, _) = inner.per_car.entry(car_id).or_default();
        if take_room(laps, MAX_LAPS_PER_CAR, car_id, "lap") {
            inner.laps.push(make());
        }
    }

    fn drain(&self) -> (Vec<Lap>, Vec<Event>) {
        let mut inner = self.inner.lock();
        inner.per_car.clear();
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
        if !self.cfg.extra.write_result_json {
            return;
        }
        let car = &self.entry_cars[usize::from(client.session_id())];
        let car_id = i64::from(client.session_id());
        self.recorder.add_event(car_id, || {
            let (kind, other_car_id, other_driver) = match other {
                Some(other) => (
                    "COLLISION_WITH_CAR",
                    i64::from(other.session_id),
                    results_driver(other),
                ),
                None => ("COLLISION_WITH_ENV", -1, Driver::default()),
            };
            Event {
                kind: kind.to_string(),
                car_id,
                driver: results_driver(car),
                other_car_id,
                other_driver,
                impact_speed: speed,
                world_position: pos.into(),
                rel_position: rel.into(),
            }
        });
    }

    /// Stores a lap for the current session's results file.
    pub(crate) fn record_lap(&self, client: &Client, lap_time: u32, sectors: Vec<u32>, cuts: u8) {
        if !self.cfg.extra.write_result_json {
            return;
        }
        let car = &self.entry_cars[usize::from(client.session_id())];
        let car_id = i64::from(client.session_id());
        self.recorder.add_lap(car_id, || {
            let (car_model, ballast_kg, restrictor) =
                car.read(|s| (s.model.clone(), s.ballast, s.restrictor));
            let id = client.identity();
            Lap {
                car_id,
                driver_name: id.name.clone(),
                driver_guid: id.guid.clone(),
                car_model,
                lap_time,
                sectors: Some(sectors),
                tyre: String::new(),
                ballast_kg,
                restrictor,
                cuts: i64::from(cuts),
                timestamp: self.server_time_millis() as u32,
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorder_caps_each_car_until_the_session_ends() {
        let recorder = ResultsRecorder::default();
        for _ in 0..MAX_LAPS_PER_CAR + 5 {
            recorder.add_lap(1, Lap::default);
            recorder.add_event(1, Event::default);
        }
        recorder.add_lap(2, Lap::default);
        let (laps, events) = recorder.drain();
        assert_eq!(
            laps.len(),
            MAX_LAPS_PER_CAR + 1,
            "car 1 capped, car 2 unaffected"
        );
        assert_eq!(events.len(), MAX_EVENTS_PER_CAR);
        // A new session starts the count again.
        recorder.add_lap(1, Lap::default);
        assert_eq!(recorder.drain().0.len(), 1);
    }
}
