//! A full grid joining, racing and finishing at once.

mod common;

use std::sync::Mutex;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use acserver_rs::proto;
use common::*;

/// The tests in this file each load the machine; they run one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

/// Joins `n` clients at once from as many threads.
fn join_all(srv: &TestServer, n: usize, prefix: &str) -> Vec<Joined> {
    let results: Vec<Result<Joined, String>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..n)
            .map(|i| scope.spawn(move || join(srv, &format!("{prefix}-{i:03}"), None)))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    results
        .into_iter()
        .enumerate()
        .map(|(i, r)| r.unwrap_or_else(|e| panic!("client {i} failed to join: {e}")))
        .collect()
}

/// Streams position updates from every client for `duration`.
fn race(clients: &[std::net::UdpSocket], duration: Duration, every: Duration) {
    std::thread::scope(|scope| {
        for udp in clients {
            scope.spawn(move || {
                let deadline = Instant::now() + duration;
                let (mut seq, mut norm) = (2u8, 0.1f32);
                while Instant::now() < deadline {
                    write_position(udp, seq, norm);
                    seq = seq.wrapping_add(1);
                    norm += 0.01;
                    if norm >= 1.0 {
                        norm = 0.0;
                    }
                    std::thread::sleep(every);
                }
            });
        }
    });
}

/// Every client keeps draining its socket: an unread client would fill its
/// send queue and be dropped. Client 0 reports the RaceOver.
fn drain(
    clients: Vec<Joined>,
) -> (
    Vec<Sender>,
    Vec<std::net::UdpSocket>,
    mpsc::Receiver<Vec<u8>>,
) {
    let (tx, rx) = mpsc::channel();
    let mut senders = Vec::new();
    let mut sockets = Vec::new();
    for (i, Joined { mut conn, udp, .. }) in clients.into_iter().enumerate() {
        senders.push(conn.sender());
        sockets.push(udp);
        let tx = tx.clone();
        conn.set_timeout(Duration::from_secs(600));
        std::thread::spawn(move || {
            while let Ok(pkt) = conn.read() {
                if i == 0 && pkt[0] == proto::RACE_OVER {
                    let _ = tx.send(pkt);
                }
            }
        });
    }
    (senders, sockets, rx)
}

const GRID: usize = 32;

/// Fills the grid, streams positions from every car, has all finish, and
/// checks the RaceOver classifies the field.
#[test]
fn load_max_grid_race() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let srv = start(|cfg| fill_grid(cfg, GRID));
    let clients = join_all(&srv, GRID, "Load");
    let (senders, sockets, race_over) = drain(clients);

    srv.require_info_clients(GRID as i64, "/INFO should report a full grid");
    assert_eq!(srv.info()["maxclients"], GRID);

    race(
        &sockets,
        Duration::from_millis(1500),
        Duration::from_millis(20),
    );
    assert_eq!(
        srv.info_clients(),
        GRID as i64,
        "the grid is intact after the position-update storm"
    );

    for sender in &senders {
        sender.send(&lap(1));
    }
    let pkt = race_over
        .recv_timeout(Duration::from_secs(10))
        .expect("no 0x4B RaceOver after the field finished");
    let (entries, pickup) = race_over_entries(&pkt);
    assert_eq!(entries.len(), GRID, "one entry per slot");
    assert!(pickup, "PickupMode should be true");
    senders.iter().for_each(Sender::close);
}

/// Every addressable slot: session ids are u8 and 0xFF is the broadcast id,
/// so 255 slots (ids 0..254) is the wire's ceiling.
const EXTREME: usize = 255;

/// 255 clients join and stream positions at once, and the server keeps the
/// whole grid. No synchronised finish: on top of ~65k datagrams per tick the
/// O(cars²) leaderboard burst makes RaceOver timing unreliable;
/// `load_max_grid_race` covers the finish at 32 cars.
#[test]
fn load_extreme_grid_max() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let srv = start(|cfg| fill_grid(cfg, EXTREME));
    let start = Instant::now();
    let clients = join_all(&srv, EXTREME, "X");
    eprintln!("joined {EXTREME} clients in {:?}", start.elapsed());
    let (senders, sockets, _race_over) = drain(clients);

    srv.require_info_clients(EXTREME as i64, "/INFO should report a full 255-car grid");
    race(
        &sockets,
        Duration::from_millis(1500),
        Duration::from_millis(25),
    );
    assert_eq!(
        srv.info_clients(),
        EXTREME as i64,
        "the grid is intact after the position-update storm"
    );
    senders.iter().for_each(Sender::close);
}
