//! A race modelled on a real one rather than a stress burst: staggered joins,
//! three laps with sectors, contacts and pit stops, a staggered finish and
//! departures, on a compressed timeline.

mod common;

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, Instant};

use acserver_rs::proto;
use acserver_rs::server::PluginBridge;
use acserver_rs::wire::{Vec3, Writer};
use common::*;
use rand::SeedableRng as _;
use rand::seq::SliceRandom as _;
use rand::{RngExt as _, rngs::StdRng};

/// Each race loads the machine; they run one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

#[test]
fn normal_race() {
    run_normal_race(Spec {
        clients: 32,
        connect_window: Duration::from_secs(5),
        race_duration: Duration::from_secs(30),
        finish_window: Duration::from_secs(5),
        result_wait: Duration::from_secs(5),
        quit_window: Duration::from_secs(3),
    });
}

/// A normal race at the u8 session-id ceiling: 255 drivers join in random
/// order over 10 s, race 45 s and three laps, finish over 10 s, stay at least
/// 5 s after the last finisher, then leave over another 5 s.
#[test]
fn normal_race_255_players() {
    run_normal_race(Spec {
        clients: 255,
        connect_window: Duration::from_secs(10),
        race_duration: Duration::from_secs(45),
        finish_window: Duration::from_secs(10),
        result_wait: Duration::from_secs(5),
        quit_window: Duration::from_secs(5),
    });
}

struct Spec {
    clients: usize,
    connect_window: Duration,
    race_duration: Duration,
    finish_window: Duration,
    result_wait: Duration,
    quit_window: Duration,
}

/// Counts the contacts that reach the plugin API.
#[derive(Default)]
struct Recorder {
    car_collisions: AtomicI64,
    env_collisions: AtomicI64,
}

impl PluginBridge for Recorder {
    fn on_collision_with_car(&self, _: u8, _: u8, _: f32, _: Vec3, _: Vec3) {
        self.car_collisions.fetch_add(1, Ordering::SeqCst);
    }
    fn on_collision_with_env(&self, _: u8, _: f32, _: Vec3, _: Vec3) {
        self.env_collisions.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct Seen {
    race_over: AtomicI64,
    sector: AtomicI64,
    damage: AtomicI64,
    tyre: AtomicI64,
    pit: AtomicI64,
    jump: AtomicI64,
}

/// A uniformly random duration in [lo, hi).
fn rand_dur(rng: &mut StdRng, lo: Duration, hi: Duration) -> Duration {
    if hi <= lo {
        return lo;
    }
    lo + Duration::from_nanos(rng.random_range(0..(hi - lo).as_nanos() as u64))
}

fn permutation(rng: &mut StdRng, n: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..n).collect();
    order.shuffle(rng);
    order
}

#[derive(Clone, Default)]
struct Schedule {
    join: Duration,
    laps: [Duration; 3],
    quit_delay: Duration,
    car_collision: Option<Duration>,
    env_collision: Option<Duration>,
    jump_start: Option<Duration>,
    pit: Option<Duration>,
}

fn run_normal_race(spec: Spec) {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    const LAPS: u16 = 3;
    let n = spec.clients;

    // Phase boundaries from the start of the test.
    let race_start = spec.connect_window;
    let race_end = race_start + spec.race_duration;
    let finish_end = race_end + spec.finish_window;
    // A margin above the asserted minimum, so a timer waking exactly on the
    // boundary cannot make the measured wait a few nanoseconds short.
    let earliest_quit = finish_end + spec.result_wait + Duration::from_millis(500);

    let recorder = Arc::new(Recorder::default());
    let srv = start_with_plugin(
        |cfg| {
            fill_grid(cfg, n);
            cfg.sessions[0].laps = LAPS;
            // The cap starts at the first finisher: allow the whole staggered
            // field plus five seconds of headroom.
            cfg.server.race_over_time = spec.finish_window.as_secs() as u16 + 5;
            cfg.server.result_screen_time = 30;
            cfg.server.race_pit_window_start = 1;
            cfg.server.race_pit_window_end = 2;
        },
        Some(recorder.clone() as Arc<dyn PluginBridge>),
    );

    // A deterministic schedule per driver (offsets from the test's start).
    let mut rng = StdRng::seed_from_u64(1);
    let pct = |p: u32| race_start + spec.race_duration * p / 100;
    let mut plan = vec![Schedule::default(); n];
    for s in &mut plan {
        s.join = rand_dur(&mut rng, Duration::ZERO, spec.connect_window);
        // The first two crossings near one third and two thirds distance.
        s.laps[0] = rand_dur(&mut rng, pct(28), pct(36));
        s.laps[1] = rand_dur(&mut rng, pct(61), pct(69));
    }
    // Distinct finish times across the window, so never a dead heat.
    for (place, driver) in permutation(&mut rng, n).into_iter().enumerate() {
        plan[driver].laps[2] = race_end + spec.finish_window * (place as u32 + 1) / n as u32;
    }
    for (place, driver) in permutation(&mut rng, n).into_iter().enumerate() {
        plan[driver].quit_delay = if n == 1 {
            Duration::ZERO
        } else {
            spec.quit_window * place as u32 / (n as u32 - 1)
        };
    }
    let car_collisions = n.min(n.div_ceil(4).max(4));
    let env_collisions = n.min((n / 32).max(1));
    let jump_starts = n.min((n / 128).max(1));
    let pits = n.min((n / 10).max(2));
    for driver in permutation(&mut rng, n).into_iter().take(car_collisions) {
        plan[driver].car_collision = Some(rand_dur(
            &mut rng,
            race_start + Duration::from_secs(2),
            race_start + Duration::from_secs(10),
        ));
    }
    for driver in permutation(&mut rng, n).into_iter().take(env_collisions) {
        plan[driver].env_collision = Some(rand_dur(&mut rng, pct(40), pct(80)));
    }
    for driver in permutation(&mut rng, n).into_iter().take(jump_starts) {
        plan[driver].jump_start = Some(rand_dur(
            &mut rng,
            race_start + Duration::from_millis(500),
            race_start + Duration::from_secs(2),
        ));
    }
    for driver in permutation(&mut rng, n).into_iter().take(pits) {
        let s = &mut plan[driver];
        s.pit = Some(s.laps[0] + (s.laps[1] - s.laps[0]) / 2);
    }

    let seen = Arc::new(Seen::default());
    let race_over_at = Arc::new(AtomicI64::new(0)); // ms from start to the first 0x4B
    let quit_gate = Barrier::new(n + 1);
    let mid_count = AtomicI64::new(-1);
    let wait_count = AtomicI64::new(-1);
    let quit_phase_at = AtomicI64::new(0);
    let start = Instant::now();
    let sleep_until = |at: Duration| {
        if let Some(left) = (start + at).checked_duration_since(Instant::now()) {
            std::thread::sleep(left);
        }
    };
    let since_start = || start.elapsed().as_millis() as i64;

    let outcomes: Vec<Result<(Instant, Instant), String>> = std::thread::scope(|scope| {
        scope.spawn(|| {
            sleep_until(race_start + spec.race_duration / 2);
            mid_count.store(srv.info_clients(), Ordering::SeqCst);
        });
        scope.spawn(|| {
            sleep_until(earliest_quit);
            wait_count.store(srv.info_clients(), Ordering::SeqCst);
            quit_phase_at.store(since_start(), Ordering::SeqCst);
            quit_gate.wait();
        });

        let drivers: Vec<_> = plan
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let (srv, seen, race_over_at, quit_gate) = (&srv, &seen, &race_over_at, &quit_gate);
                scope.spawn(move || {
                    sleep_until(s.join);
                    let joined = match join(srv, &driver_name(i), None) {
                        Ok(joined) => joined,
                        Err(e) => {
                            quit_gate.wait();
                            return Err(e);
                        }
                    };
                    let Joined { mut conn, udp, id } = joined;
                    let tcp = conn.sender();

                    // Drain TCP for the whole session and count what arrives.
                    let seen = Arc::clone(seen);
                    let race_over_at = Arc::clone(race_over_at);
                    conn.set_timeout(Duration::from_secs(600));
                    let drain = std::thread::spawn({
                        move || {
                            while let Ok(pkt) = conn.read() {
                                let counter = match pkt[0] {
                                    proto::RACE_OVER => {
                                        let at = start.elapsed().as_millis() as i64;
                                        let _ = race_over_at.compare_exchange(
                                            0,
                                            at,
                                            Ordering::SeqCst,
                                            Ordering::SeqCst,
                                        );
                                        &seen.race_over
                                    }
                                    proto::SECTOR_SPLIT => &seen.sector,
                                    proto::DAMAGE_UPDATE => &seen.damage,
                                    proto::TYRE_COMPOUND_CHANGE => &seen.tyre,
                                    proto::MANDATORY_PIT_UPDATE => &seen.pit,
                                    proto::CLIENT_EVENT => &seen.jump,
                                    _ => continue,
                                };
                                counter.fetch_add(1, Ordering::SeqCst);
                            }
                        }
                    });

                    let finished = std::thread::scope(|inner| {
                        // Positions at the tick rate until this car finishes.
                        let finish = start + s.laps[2];
                        let udp = &udp;
                        inner.spawn(move || {
                            let (mut seq, mut norm) = (2u8, 0.1f32);
                            while Instant::now() < finish {
                                std::thread::sleep(Duration::from_secs(1) / 18);
                                write_position(udp, seq, norm);
                                seq = seq.wrapping_add(1);
                                norm += 0.02;
                                if norm >= 1.0 {
                                    norm = 0.0;
                                }
                            }
                        });

                        // Incidents and pit reports between sector and lap
                        // packets; each framed write stays whole.
                        let tcp = &tcp;
                        if let Some(at) = s.car_collision {
                            inner.spawn(move || {
                                sleep_until(at);
                                let target = if id + 1 == proto::SERVER_SESSION_ID {
                                    0
                                } else {
                                    id + 1
                                };
                                tcp.send(&car_collision(target, 18.0 + (i % 20) as f32));
                                std::thread::sleep(Duration::from_millis(100));
                                tcp.send(&damage(1.0 + (i % 8) as f32));
                            });
                        }
                        if let Some(at) = s.env_collision {
                            inner.spawn(move || {
                                sleep_until(at);
                                tcp.send(&env_collision(25.0 + (i % 30) as f32));
                                std::thread::sleep(Duration::from_millis(100));
                                tcp.send(&damage(4.0 + (i % 12) as f32));
                            });
                        }
                        if let Some(at) = s.jump_start {
                            inner.spawn(move || {
                                sleep_until(at);
                                tcp.send(&jump_start());
                            });
                        }
                        if let Some(at) = s.pit {
                            inner.spawn(move || {
                                sleep_until(at);
                                tcp.send(&tyre_compound("SM"));
                                tcp.send(&mandatory_pit());
                            });
                        }

                        // Three laps with the two sector crossings a client
                        // reports on the way; lap timestamps strictly rise.
                        let mut previous = race_start;
                        for (lap, &crossing) in s.laps.iter().enumerate() {
                            let lap_len = crossing - previous;
                            sleep_until(previous + lap_len / 3);
                            tcp.send(&sector_split(0, 31_000));
                            sleep_until(previous + lap_len * 2 / 3);
                            tcp.send(&sector_split(1, 63_000));
                            sleep_until(crossing);
                            tcp.send(&race_lap(lap as u8 + 1, (lap as u32 + 1) * 40_000));
                            previous = crossing;
                        }
                        Instant::now()
                    });

                    // Stay for the result wait, then leave at this driver's
                    // offset within the shared quit window.
                    quit_gate.wait();
                    std::thread::sleep(s.quit_delay);
                    let quit = Instant::now();
                    tcp.close();
                    drop(udp);
                    let _ = drain.join();
                    Ok((finished, quit))
                })
            })
            .collect();
        drivers.into_iter().map(|h| h.join().unwrap()).collect()
    });
    eprintln!("all drivers done in {:?}", start.elapsed());

    let times: Vec<(Instant, Instant)> = outcomes
        .into_iter()
        .enumerate()
        .map(|(i, r)| r.unwrap_or_else(|e| panic!("driver {i} failed to join: {e}")))
        .collect();
    let n64 = n as i64;
    assert_eq!(
        mid_count.load(Ordering::SeqCst),
        n64,
        "the whole grid mid-race"
    );
    assert_eq!(
        wait_count.load(Ordering::SeqCst),
        n64,
        "the whole grid stays through the post-finish wait"
    );
    for car in srv.server.entry_cars() {
        assert_eq!(
            car.result().num_laps,
            LAPS,
            "car {} completed all three laps",
            car.session_id
        );
    }
    let count = |c: &AtomicI64| c.load(Ordering::SeqCst);
    let others = n64 - 1;
    assert_eq!(
        count(&recorder.car_collisions),
        car_collisions as i64,
        "car contacts reach the plugin"
    );
    assert_eq!(
        count(&recorder.env_collisions),
        env_collisions as i64,
        "env contacts reach the plugin"
    );
    assert_eq!(
        count(&seen.sector),
        n64 * i64::from(LAPS) * 2 * others,
        "sectors to every other driver"
    );
    assert_eq!(
        count(&seen.damage),
        (car_collisions + env_collisions) as i64 * others,
        "damage to every other driver"
    );
    assert_eq!(
        count(&seen.tyre),
        pits as i64 * others,
        "tyre changes to every other driver"
    );
    assert_eq!(
        count(&seen.pit),
        n64 * n64 + pits as i64 * n64,
        "mandatory-pit state to the full field"
    );
    assert_eq!(
        count(&seen.jump),
        jump_starts as i64 * n64,
        "jump starts to the full field"
    );
    assert_eq!(count(&seen.race_over), n64, "every driver got the 0x4B");

    let race_over = Duration::from_millis(race_over_at.load(Ordering::SeqCst) as u64);
    assert!(race_over >= race_end, "the race ended before the last laps");
    assert!(
        (race_over.as_millis() as i64) < quit_phase_at.load(Ordering::SeqCst),
        "RaceOver arrives before the quit window"
    );
    let last_finish = times.iter().map(|t| t.0).max().unwrap();
    let first_quit = times.iter().map(|t| t.1).min().unwrap();
    let last_quit = times.iter().map(|t| t.1).max().unwrap();
    // race_over is whole milliseconds, so allow one.
    assert!(
        start + race_over + Duration::from_millis(1) >= last_finish,
        "RaceOver not before the last driver crosses the line"
    );
    assert!(
        first_quit - last_finish >= spec.result_wait,
        "everyone waits after the last car crosses the line"
    );
    let spread = (last_quit - first_quit).as_secs_f64();
    assert!(
        (spread - spec.quit_window.as_secs_f64()).abs() <= 2.0,
        "departures spread across the quit window ({spread:.1} s)"
    );
}

/// A distinct, stable name per slot.
fn driver_name(i: usize) -> String {
    format!("Driver-{}{}", (b'A' + (i / 10) as u8) as char, i % 10)
}

fn race_lap(num_lap: u8, timestamp: u32) -> Vec<u8> {
    let mut w = Writer::with_id(proto::LAP_COMPLETED);
    w.u32(timestamp);
    w.u32(95_000);
    w.u8(2); // sector count
    w.u32(31_000);
    w.u32(63_000);
    w.u8(0); // cuts
    w.u8(num_lap);
    w.into_bytes()
}

fn sector_split(index: u8, time: u32) -> Vec<u8> {
    let mut w = Writer::with_id(proto::SECTOR_SPLIT);
    w.u8(index);
    w.u32(time);
    w.u8(0); // cuts
    w.into_bytes()
}

fn car_collision(target: u8, speed: f32) -> Vec<u8> {
    let mut w = Writer::with_id(proto::CLIENT_EVENT);
    w.i16(1);
    w.u8(proto::EVENT_COLLISION_WITH_CAR);
    w.u8(target);
    w.f32(speed);
    w.vec3(Vec3::new(10.0, 0.5, 20.0));
    w.vec3(Vec3::new(0.8, 0.2, -1.1));
    w.into_bytes()
}

fn env_collision(speed: f32) -> Vec<u8> {
    let mut w = Writer::with_id(proto::CLIENT_EVENT);
    w.i16(1);
    w.u8(proto::EVENT_COLLISION_WITH_ENV);
    w.f32(speed);
    w.vec3(Vec3::new(100.0, 1.0, 250.0));
    w.vec3(Vec3::new(-0.9, 0.1, 0.4));
    w.into_bytes()
}

fn damage(base: f32) -> Vec<u8> {
    let mut w = Writer::with_id(proto::DAMAGE_UPDATE);
    for i in 0..5 {
        w.f32(base + i as f32 / 2.0);
    }
    w.into_bytes()
}

fn jump_start() -> Vec<u8> {
    let mut w = Writer::with_id(proto::CLIENT_EVENT);
    w.i16(1);
    w.u8(proto::EVENT_JUMP_START);
    w.into_bytes()
}

fn tyre_compound(compound: &str) -> Vec<u8> {
    let mut w = Writer::with_id(proto::TYRE_COMPOUND_CHANGE);
    w.string8(compound);
    w.into_bytes()
}

fn mandatory_pit() -> Vec<u8> {
    let mut w = Writer::with_id(proto::MANDATORY_PIT_UPDATE);
    w.bool(true);
    w.into_bytes()
}
