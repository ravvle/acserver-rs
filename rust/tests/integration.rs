//! End-to-end joins, races and lobby checks against a server in process.

mod common;

use std::time::{Duration, Instant};

use acserver_rs::proto;
use acserver_rs::wire::Reader;
use common::*;

#[test]
fn handshake_and_first_update() {
    let srv = start(|_| {});
    let (mut conn, resp) = dial_handshake(srv.game, "76561198000000001", "TestDriver");
    assert_eq!(resp[0], proto::NEW_CAR_CONNECTION, "expected 0x3E");
    let mut r = Reader::new(&resp[1..]);
    assert!(!r.wstring8().is_empty(), "empty server name");
    assert_eq!(r.u16(), srv.game, "udp port");

    let id = srv
        .session_id_of("TestDriver")
        .expect("client not registered in a slot");

    let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    udp.connect(("127.0.0.1", srv.game)).unwrap();
    udp.send(&[proto::CAR_CONNECT, id]).unwrap();
    udp.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut buf = [0u8; 1500];
    let n = udp.recv(&mut buf).expect("udp association reply");
    assert!(n >= 1);
    assert_eq!(buf[0], proto::CAR_CONNECT, "udp association reply id");

    // Empty content: a single 16-byte data.acd digest is accepted.
    let mut checksum = acserver_rs::wire::Writer::with_id(proto::CHECKSUM_PACKET);
    checksum.raw(&[0u8; 16]);
    conn.send(checksum.bytes());
    write_position(&udp, 1, 0.1);

    // The first-update burst. No 0x51: WELCOME_MESSAGE is unset.
    conn.set_timeout(Duration::from_secs(3));
    let (mut driver_info, mut leaderboard) = (false, false);
    for _ in 0..12 {
        if driver_info && leaderboard {
            break;
        }
        let Ok(pkt) = conn.read() else { break };
        match pkt[0] {
            proto::DRIVER_INFO_UPDATE => driver_info = true,
            proto::LAP_COMPLETED => leaderboard = true,
            _ => {}
        }
    }
    assert!(driver_info, "no 0x5B driver info after the first update");
    assert!(
        leaderboard,
        "no 0x49 leaderboard sync after the first update"
    );

    let info = srv.info();
    assert!(info["clients"].as_i64().unwrap() >= 1, "/INFO clients");
    assert_eq!(
        info["port"].as_u64().unwrap(),
        u64::from(srv.game),
        "/INFO port"
    );
    assert_eq!(info["pickup"], true, "/INFO pickup");
}

#[test]
fn unsupported_protocol_rejected() {
    let srv = start(|_| {});
    let mut conn = Conn::dial(srv.game);
    let mut hs = handshake("76561198000000001", "X", "", None);
    hs[1..3].copy_from_slice(&1u16.to_le_bytes()); // wrong version
    conn.send(&hs);
    conn.set_timeout(Duration::from_secs(5));
    let pkt = conn.expect();
    assert_eq!(pkt[0], proto::UNSUPPORTED_PROTOCOL, "expected 0x42");
    assert_eq!(
        Reader::new(&pkt[1..]).u16(),
        proto::CLIENT_PROTOCOL_VERSION,
        "the required version"
    );
}

#[test]
fn locked_shared_entry_admission() {
    const SHARED: &str = "76561198000999999";
    let srv = start(|cfg| {
        cfg.server.locked_entry_list = true;
        cfg.server.password = "ordinary-password".into();
        cfg.server.admin_password = "admin123".into();
        let car = &mut cfg.entry_list[0];
        car.guid = format!("76561198000888888;{SHARED}");
        car.guids = vec!["76561198000888888".into(), SHARED.into()];
        car.driver_name = "Shared Driver".into();
        car.team = "Shared Team".into();
    });

    // A listed shared guid bypasses the join password. A blank name is valid
    // because the entry-list identity replaces it.
    let mut conn = Conn::dial(srv.game);
    conn.send(&handshake(SHARED, "", "", None));
    conn.set_timeout(Duration::from_secs(5));
    assert_eq!(conn.expect()[0], proto::NEW_CAR_CONNECTION);

    let occupant = srv
        .server
        .entry_cars()
        .iter()
        .find(|car| car.reserved_for(SHARED))
        .and_then(|car| car.client())
        .expect("occupant");
    assert_eq!(occupant.name(), "Shared Driver");
    assert_eq!(occupant.identity().team, "Shared Team");

    // Even the admin password does not let an unlisted guid past a locked
    // entry list: "only players already included" is the guide's rule.
    let mut other = Conn::dial(srv.game);
    other.send(&handshake(
        "76561198000777777",
        "Unlisted",
        "admin123",
        None,
    ));
    other.set_timeout(Duration::from_millis(500));
    assert!(
        other.read().is_err(),
        "unlisted admin-password join should be dropped"
    );
}

#[test]
fn race_over_on_final_lap() {
    let srv = start(|cfg| {
        cfg.sessions = one_lap_race();
        cfg.server.result_screen_time = 1;
        cfg.server.loop_mode = false;
    });
    let mut racer = join_client(&srv, "Racer");
    racer.conn.send(&lap(1));

    // A full-grid first-update snapshot may still be queued ahead of it.
    let pkt = await_race_over(&mut racer.conn, Duration::from_secs(3))
        .expect("no 0x4B RaceOver after the final lap");
    let (entries, pickup) = race_over_entries(&pkt);
    assert_eq!(
        entries.len(),
        srv.server.entry_cars().len(),
        "one result entry per entry-list slot"
    );
    assert!(pickup, "PickupMode should be true");
}

/// Vanilla's straggler rule: when the leader finishes, the race does not end
/// while another car is still on track; it ends when RACE_OVER_TIME expires.
#[test]
fn race_over_time_cap() {
    let srv = start(|cfg| {
        cfg.sessions = one_lap_race();
        cfg.server.race_over_time = 1;
        cfg.server.result_screen_time = 1;
        cfg.server.loop_mode = false;
    });
    let mut leader = join_client(&srv, "Leader");
    let _straggler = join_client(&srv, "Straggler");

    let start = Instant::now();
    leader.conn.send(&lap(1));
    let pkt = await_race_over(&mut leader.conn, Duration::from_secs(4))
        .expect("no RaceOver via the time cap");
    assert!(
        start.elapsed() >= Duration::from_millis(800),
        "the straggler was cut off instead of getting the cap"
    );

    // Vanilla classifies every entry-list slot, ranked: the leader first.
    let (entries, _) = race_over_entries(&pkt);
    assert_eq!(entries.len(), srv.server.entry_cars().len());
    assert_eq!(entries[0].2, 1, "the leader ranks first with 1 lap");
}

/// The race ends at once, before the cap, when every running car finished.
#[test]
fn race_over_all_finish() {
    let srv = start(|cfg| {
        cfg.sessions = one_lap_race();
        cfg.server.race_over_time = 30; // must not be needed
        cfg.server.result_screen_time = 1;
        cfg.server.loop_mode = false;
    });
    let mut a = join_client(&srv, "CarA");
    let b = join_client(&srv, "CarB");

    let start = Instant::now();
    a.conn.send(&lap(1));
    b.conn.send(&lap(1));
    await_race_over(&mut a.conn, Duration::from_secs(4))
        .expect("no RaceOver after all cars finished");
    assert!(
        start.elapsed() < Duration::from_secs(25),
        "the race waited for the cap"
    );
}

#[test]
fn lobby_check_udp() {
    let srv = start(|_| {});
    let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    udp.connect(("127.0.0.1", srv.game)).unwrap();
    udp.send(&[proto::LOBBY_CHECK]).unwrap();
    udp.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut buf = [0u8; 16];
    let n = udp.recv(&mut buf).expect("lobby check reply");
    assert!(n >= 3, "reply too short");
    assert_eq!(buf[0], proto::LOBBY_CHECK, "expected 0xC8");
    assert_eq!(Reader::new(&buf[1..n]).u16(), srv.http, "http port");
}

/// With pickup mode off a loop back re-reads `entry_list.ini`. An edit that
/// leaves the car list as clients loaded it (only BoP here) keeps its
/// drivers; one that changes it disconnects them all, since the game never
/// asks for the list again and reads 0x49, 0x4A and 0x4B one row per car it
/// loaded (docs/05 §5.2a).
#[test]
fn entry_list_reload_keeps_drivers_only_while_the_car_list_holds() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("entry_list.ini");
    // Pickup mode off admits only listed guids.
    const GUID: &str = "76561198000777777";
    let original = std::fs::read_to_string("../cfg/entry_list.ini")
        .expect("entry list")
        .replacen("GUID=\n", &format!("GUID={GUID}\n"), 1);
    std::fs::write(&path, &original).expect("write entry list");
    let srv = start(|cfg| {
        cfg.server.pickup_mode_enabled = false;
        cfg.server.max_clients = 13;
        cfg.sessions.truncate(1);
        cfg.entry_list_path = path.clone();
        cfg.entry_list[0].guid = GUID.into();
        cfg.entry_list[0].guids = vec![GUID.into()];
    });
    let driver = join_guid(&srv, GUID, "Kept", None).expect("join");
    let deadline = Instant::now() + Duration::from_secs(2);
    while !srv
        .server
        .car_by_id(driver.id)
        .and_then(|c| c.client())
        .is_some_and(|c| c.ready())
    {
        assert!(Instant::now() < deadline, "the driver never became ready");
        std::thread::sleep(Duration::from_millis(5));
    }
    let car = srv.server.car_by_id(driver.id).expect("slot");

    std::fs::write(&path, original.replace("BALLAST=0", "BALLAST=20")).expect("edit BoP");
    assert!(srv.server.next_session(), "loop refused");
    assert!(car.client().is_some(), "a BoP edit disconnected the driver");

    let added = format!("{original}\n[CAR_12]\nMODEL=ks_mazda_mx5_cup\nSKIN=00_official\n");
    std::fs::write(&path, added).expect("add a car");
    assert!(srv.server.next_session(), "loop refused");
    assert!(
        car.client().is_none(),
        "a driver kept a car list that changed"
    );
    assert_eq!(srv.server.listed_cars().len(), 13);
}
