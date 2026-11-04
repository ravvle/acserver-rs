//! Limits that bound what an unauthenticated or hostile peer can make the
//! server do. They have no vanilla counterpart, so no comparison probe can
//! assert them (README "Abuse limits").

mod common;

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use acserver_rs::proto;
use common::*;

fn wait_until(what: &str, within: Duration, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + within;
    while !cond() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A repeat join on a connected guid drops the old connection and takes its
/// slot, so a player whose link died can rejoin at once, and one guid never
/// holds two slots.
#[test]
fn duplicate_guid_replaces_old() {
    let srv = start(|_| {});
    const GUID: &str = "76561198000000123";
    let (mut first, resp) = dial_handshake(srv.game, GUID, "First");
    assert_eq!(resp[0], proto::NEW_CAR_CONNECTION, "first join admitted");
    let (_second, resp) = dial_handshake(srv.game, GUID, "Second");
    assert_eq!(resp[0], proto::NEW_CAR_CONNECTION, "rejoin admitted");

    // The old connection is closed: reads drain to an error, not a timeout.
    first.set_timeout(Duration::from_secs(3));
    let err = loop {
        if let Err(err) = first.read_byte() {
            break err;
        }
    };
    assert!(!is_timeout(&err), "the old connection should be closed");
    wait_until(
        "one guid must hold one slot",
        Duration::from_secs(1),
        || srv.server.connected_count() == 1,
    );
    let client = srv.server.entry_cars()[0]
        .client()
        .expect("slot 0 occupied");
    assert_eq!(client.name(), "Second", "the rejoin reuses the old slot");
}

/// MAX_CLIENTS caps occupied slots even when the entry list is longer.
#[test]
fn max_clients_enforced() {
    let srv = start(|cfg| cfg.server.max_clients = 1);
    let (_first, resp) = dial_handshake(srv.game, "76561198000000201", "First");
    assert_eq!(resp[0], proto::NEW_CAR_CONNECTION, "first join admitted");
    let (_second, resp) = dial_handshake(srv.game, "76561198000000202", "Second");
    assert_eq!(
        resp[0],
        proto::NO_SLOTS_AVAILABLE,
        "a join past MAX_CLIENTS gets 0x45"
    );
}

/// Connections that never handshake cannot accumulate without bound.
#[test]
fn pending_connections_capped() {
    let srv = start(|_| {});
    // The cap is max(32, 2*slots); open past it and hold every socket silent.
    let limit = (2 * srv.server.entry_cars().len()).max(32);
    let mut held = Vec::new();
    for _ in 0..limit + 8 {
        match std::net::TcpStream::connect(("127.0.0.1", srv.game)) {
            Ok(stream) => held.push(stream),
            Err(_) => break,
        }
    }
    // Past the cap the server closes at once, so a read ends rather than blocks.
    let last = held.last().expect("connections");
    last.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut buf = [0u8; 1];
    let result = std::io::Read::read(&mut &*last, &mut buf);
    match result {
        Ok(0) => {}
        Ok(_) => panic!("unexpected data"),
        Err(err) => assert!(!is_timeout(&err), "closed, not merely idle: {err}"),
    }
}

/// A connection that never sends its 0x3D is dropped near the 10 s deadline.
#[test]
fn handshake_deadline() {
    let srv = start(|_| {});
    let mut conn = Conn::dial(srv.game);
    conn.set_timeout(Duration::from_secs(30));
    let start = Instant::now();
    let err = conn
        .read_byte()
        .expect_err("a silent connection is dropped");
    assert!(!is_timeout(&err), "the server should close it: {err}");
    assert!(
        start.elapsed() < Duration::from_secs(25),
        "dropped near 10 s"
    );
}

/// An oversized length prefix before the handshake must not stop the server.
#[test]
fn pre_handshake_read_error_survives() {
    let srv = start(|_| {});
    let conn = Conn::dial(srv.game);
    conn.write_raw(&[0xFF, 0xFF]).unwrap(); // past MAX_TCP_RECV: a framing error
    drop(conn);
    std::thread::sleep(Duration::from_millis(300));

    let (_good, resp) = dial_handshake(srv.game, "76561198000000401", "Survivor");
    assert_eq!(resp[0], proto::NEW_CAR_CONNECTION, "still accepts joins");
    assert_eq!(srv.server.connected_count(), 1);
}

/// Joins "Victim" over TCP and binds a UDP socket on `local` to associate from.
fn udp_hijack_setup(srv: &TestServer, local: &str) -> Option<(Conn, u8, UdpSocket)> {
    let (conn, resp) = dial_handshake(srv.game, "76561198000000301", "Victim");
    assert_eq!(resp[0], proto::NEW_CAR_CONNECTION);
    let id = srv.session_id_of("Victim").expect("victim slot");
    let Ok(udp) = UdpSocket::bind(local) else {
        eprintln!("cannot bind {local}; skipped");
        return None;
    };
    Some((conn, id, udp))
}

/// With ValidateUDPSource, a 0x4E from outside the peer's /24 is ignored.
#[test]
fn udp_association_from_wrong_source_ignored() {
    let srv = start(|cfg| cfg.extra.validate_udp_source = true);
    let Some((_conn, id, attacker)) = udp_hijack_setup(&srv, "127.1.0.1:0") else {
        return;
    };
    attacker
        .send_to(&[proto::CAR_CONNECT, id], ("127.0.0.1", srv.game))
        .unwrap();
    attacker
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let mut buf = [0u8; 16];
    assert!(
        attacker.recv_from(&mut buf).is_err(),
        "a hijacked association is not acknowledged"
    );
    let client = srv.server.car_by_id(id).unwrap().client().unwrap();
    assert!(client.udp_addr().is_none(), "the slot stays unassociated");
}

/// A source inside the peer's /24 associates, as a CGNAT client's would.
#[test]
fn udp_association_from_same_subnet_allowed() {
    let srv = start(|cfg| cfg.extra.validate_udp_source = true);
    let Some((_conn, id, udp)) = udp_hijack_setup(&srv, "127.0.0.2:0") else {
        return;
    };
    udp.send_to(&[proto::CAR_CONNECT, id], ("127.0.0.1", srv.game))
        .unwrap();
    udp.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    let mut buf = [0u8; 16];
    let (n, _) = udp
        .recv_from(&mut buf)
        .expect("a same-/24 association is acknowledged");
    assert_eq!(n, 1);
    assert_eq!(buf[0], proto::CAR_CONNECT);
}

/// A chat line cannot carry control characters into the log or other chats.
#[test]
fn chat_control_characters_stripped() {
    let srv = start(|_| {});
    let talker = join_client(&srv, "Talker");
    let mut listener = join_client(&srv, "Listener");
    talker.conn.send(&chat("\t\thello\nFAKE: injected\x07"));
    let pkt = listener.conn.await_packet(|p| p[0] == proto::CHAT);
    let msg = chat_text(&pkt);
    assert!(!msg.contains('\n'), "newline stripped");
    assert!(!msg.contains('\x07'), "control character stripped");
    assert!(
        msg.starts_with("\t\thello"),
        "tabs (CSP chat) kept: {msg:?}"
    );
}

/// A millisecond lap cannot claim the best lap, where it would stand
/// unbeatable for the session.
#[test]
fn implausible_lap_time_not_best_lap() {
    let srv = start(|_| {});
    let cheater = join_client(&srv, "Cheater");
    cheater.conn.send(&lap_timed(1));
    cheater.conn.send(&lap_timed(95_000));
    std::thread::sleep(Duration::from_millis(300));
    let car = srv.server.car_by_id(cheater.id).unwrap();
    assert_eq!(
        car.result().best_lap,
        95_000,
        "the 1 ms lap is not the best"
    );
}

/// /admin stops answering after a few failures on one connection.
#[test]
fn admin_brute_force_stops() {
    let srv = start(|cfg| cfg.server.admin_password = "correct-horse".into());
    let mut guesser = join_client(&srv, "Guesser");
    // Spend the budget with wrong passwords; then even the right one fails.
    for i in 0..4 {
        guesser.conn.send(&chat(&format!("/admin wrong{i}")));
        // Chat is rate-limited in the sample config.
        std::thread::sleep(Duration::from_millis(1100));
    }
    guesser.conn.send(&chat("/admin correct-horse"));

    let deadline = Instant::now() + Duration::from_secs(2);
    guesser.conn.set_timeout(Duration::from_secs(2));
    while Instant::now() < deadline {
        let Ok(pkt) = guesser.conn.read() else { break };
        if pkt[0] == proto::CHAT {
            assert_ne!(
                chat_text(&pkt),
                "You are now Admin for this server",
                "the right password must not work once the budget is spent"
            );
        }
    }
}
