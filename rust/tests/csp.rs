//! CSP extensions (`0xAB`): client messages, handshake, kick message, extra
//! options.

mod common;

use std::io::Read as _;
use std::time::Duration;

use acserver_rs::config::Config;
use acserver_rs::proto;
use acserver_rs::wire::{Reader, Writer};
use base64::Engine as _;
use common::*;

const CSP_FEATURES: Option<&str> = Some("SPECTATING_AWARE,2144");
const ADMIN_PASSWORD: &str = "admin-secret";

fn csp_on(cfg: &mut Config) {
    cfg.extra.enable_csp = true;
    cfg.extra.enable_client_messages = true;
    cfg.extra.enable_udp_client_messages = true;
}

fn is_ext(sub: u8) -> impl Fn(&[u8]) -> bool {
    move |p| p.len() >= 2 && p[0] == proto::EXTENDED && p[1] == sub
}

/// A relayed client message of one type.
fn is_msg(kind: u16) -> impl Fn(&[u8]) -> bool {
    move |p| {
        p.len() >= 5
            && p[0] == proto::EXTENDED
            && p[1] == proto::EXT_TCP_CLIENT_MESSAGE
            && u16::from_le_bytes([p[3], p[4]]) == kind
    }
}

fn client_message(sub: u8, kind: u16, payload: &[u8]) -> Vec<u8> {
    let mut w = Writer::with_id(proto::EXTENDED);
    w.u8(sub);
    w.u16(kind);
    w.raw(payload);
    w.into_bytes()
}

#[test]
fn client_message_relay() {
    let srv = start(csp_on);
    let a = join_client_features(&srv, "A", CSP_FEATURES);
    let mut b = join_client_features(&srv, "B", CSP_FEATURES);
    let mut vanilla = join_client(&srv, "Vanilla");

    // TCP: any other type reaches the other CSP clients, stamped with the sender.
    a.conn.send(&client_message(
        proto::EXT_TCP_CLIENT_MESSAGE,
        15000,
        &[1, 2, 3],
    ));
    let pkt = b.conn.await_packet(is_msg(15000));
    assert_eq!(
        pkt,
        [
            proto::EXTENDED,
            proto::EXT_TCP_CLIENT_MESSAGE,
            a.id,
            0x98,
            0x3A,
            1,
            2,
            3
        ]
    );
    vanilla.conn.no_packet(is_msg(15000));

    // UDP: the same on the datagram transport.
    a.udp
        .send(&client_message(proto::EXT_UDP_CLIENT_MESSAGE, 15000, &[9]))
        .unwrap();
    b.udp
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut buf = [0u8; 1500];
    loop {
        let n = b.udp.recv(&mut buf).expect("udp relay never arrived");
        if buf[0] == proto::EXTENDED {
            assert_eq!(
                &buf[..n],
                [
                    proto::EXTENDED,
                    proto::EXT_UDP_CLIENT_MESSAGE,
                    a.id,
                    0x98,
                    0x3A,
                    9
                ]
            );
            break;
        }
    }
}

/// A client message reaches its sender too, as AssettoServer relays it and
/// the CSP SDK says ("your own messages will arrive to you as well").
#[test]
fn client_message_reaches_sender() {
    let srv = start(csp_on);
    let mut a = join_client_features(&srv, "A", CSP_FEATURES);

    a.conn
        .send(&client_message(proto::EXT_TCP_CLIENT_MESSAGE, 15000, &[1]));
    let pkt = a.conn.await_packet(is_msg(15000));
    assert_eq!(pkt[2], a.id, "stamped with the sender");

    // Targeted at the sender itself: delivered.
    a.conn.send(&client_message(
        proto::EXT_TCP_CLIENT_MESSAGE,
        proto::CSP_LUA_TARGETED,
        &[a.id, 7, 0, 0, 0],
    ));
    let pkt = a.conn.await_packet(is_msg(proto::CSP_LUA_TARGETED));
    assert_eq!(pkt[5..], [a.id, 7, 0, 0, 0]);

    // UDP: the same.
    a.udp
        .send(&client_message(proto::EXT_UDP_CLIENT_MESSAGE, 15000, &[9]))
        .unwrap();
    a.udp
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut buf = [0u8; 1500];
    loop {
        let n = a.udp.recv(&mut buf).expect("udp echo never arrived");
        if buf[0] == proto::EXTENDED {
            assert_eq!(
                &buf[..n],
                [
                    proto::EXTENDED,
                    proto::EXT_UDP_CLIENT_MESSAGE,
                    a.id,
                    0x98,
                    0x3A,
                    9
                ]
            );
            break;
        }
    }
}

#[test]
fn lua_targeted_and_admin_penalty() {
    let srv = start(|cfg| {
        csp_on(cfg);
        cfg.server.admin_password = ADMIN_PASSWORD.into();
    });
    let a = join_client_features(&srv, "A", CSP_FEATURES);
    let mut b = join_client_features(&srv, "B", CSP_FEATURES);
    let mut c = join_client_features(&srv, "C", CSP_FEATURES);

    // Targeted: only B hears it.
    a.conn.send(&client_message(
        proto::EXT_TCP_CLIENT_MESSAGE,
        proto::CSP_LUA_TARGETED,
        &[b.id, 7, 0, 0, 0],
    ));
    let pkt = b.conn.await_packet(is_msg(proto::CSP_LUA_TARGETED));
    assert_eq!(pkt[2], a.id);
    assert_eq!(pkt[5..], [b.id, 7, 0, 0, 0]);
    c.conn.no_packet(is_msg(proto::CSP_LUA_TARGETED));

    // A penalty from a non-admin goes nowhere.
    let penalty = [1, 0, b.id, 0, 0, 0, 0, 0];
    let send_penalty = || {
        a.conn.send(&client_message(
            proto::EXT_TCP_CLIENT_MESSAGE,
            proto::CSP_ADMIN_PENALTY,
            &penalty,
        ))
    };
    send_penalty();
    b.conn.no_packet(is_msg(proto::CSP_ADMIN_PENALTY));

    // An admin's reaches its target from the server.
    a.conn.send(&chat(&format!("/admin {ADMIN_PASSWORD}")));
    let car = srv.server.car_by_id(a.id).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !car.client().is_some_and(|c| c.is_admin()) {
        assert!(
            std::time::Instant::now() < deadline,
            "/admin should promote"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    send_penalty();
    let pkt = b.conn.await_packet(is_msg(proto::CSP_ADMIN_PENALTY));
    assert_eq!(pkt[2], proto::SERVER_SESSION_ID);
    assert_eq!(pkt[5..], penalty);
}

#[test]
fn csp_disabled() {
    let srv = start(|cfg| {
        cfg.extra.enable_csp = false;
        cfg.extra.enable_custom_update = false;
        cfg.extra.enable_client_messages = false;
        cfg.extra.enable_udp_client_messages = false;
    });
    let a = join_client_features(&srv, "A", CSP_FEATURES);
    let mut b = join_client_features(&srv, "B", CSP_FEATURES);
    a.conn
        .send(&client_message(proto::EXT_TCP_CLIENT_MESSAGE, 15000, &[1]));
    b.conn.no_packet(|p| p[0] == proto::EXTENDED);
}

#[test]
fn handshake_and_minimum_version() {
    let srv = start(|cfg| {
        csp_on(cfg);
        cfg.extra.minimum_csp_version = 2000;
    });
    for features in ["", "FOO,1900"] {
        let (_conn, resp) =
            dial_handshake_features(srv.game, "76561198000000501", "Old", Some(features));
        assert_eq!(
            resp[0],
            proto::AUTH_FAILED,
            "features {features:?} should be refused"
        );
    }

    let mut new = join_client_features(&srv, "New", CSP_FEATURES);
    // 0xAB 0x03, the server's id, type 0, MinVersion, RequiresWeatherFx.
    let pkt = new.conn.await_packet(is_ext(proto::EXT_TCP_CLIENT_MESSAGE));
    assert_eq!(pkt, [proto::EXTENDED, 3, 0xFF, 0, 0, 0xD0, 0x07, 0, 0, 0]);
}

#[test]
fn kick_message_and_visibility() {
    let srv = start(csp_on);
    let a = join_client_features(&srv, "A", CSP_FEATURES);
    let mut b = join_client_features(&srv, "B", CSP_FEATURES);

    srv.server.set_car_visibility(a.id, false);
    let pkt = b.conn.await_packet(is_ext(proto::EXT_TCP_CAR_VISIBILITY));
    assert_eq!(
        pkt,
        [proto::EXTENDED, proto::EXT_TCP_CAR_VISIBILITY, a.id, 1]
    );

    assert!(srv.server.kick_car("test", b.id));
    let pkt = b.conn.await_packet(is_ext(proto::EXT_TCP_KICK_MESSAGE));
    let n = u32::from_le_bytes(pkt[2..6].try_into().unwrap()) as usize;
    assert_eq!(n, pkt.len() - 6, "4-byte length prefix");
    assert!(pkt.len() > 6, "a kick message");
    // The kick message precedes the 0x68 on the target's stream.
    b.conn.await_packet(|p| p[0] == proto::KICK_CAR);
}

#[test]
fn extra_options_in_welcome() {
    const INI: &str = "[EXTRA_TWEAKS]\nMIN_TIME_BETWEEN_COLLISIONS = 2\n";
    let srv = start(|cfg| {
        csp_on(cfg);
        cfg.extra.csp_extra_options = INI.into();
    });

    let welcome = |features: &str| -> String {
        let name = format!("W{}", &features[..1]);
        let mut joined = join_client_features(&srv, &name, Some(features));
        joined.conn.set_timeout(Duration::from_millis(500));
        while let Ok(pkt) = joined.conn.read() {
            if pkt[0] == proto::WELCOME_MESSAGE {
                let mut r = Reader::new(&pkt[1..]);
                r.u8();
                return r.wstring16();
            }
        }
        String::new()
    };

    // A CSP client gets the INI after 32 tabs, as zlib in unpadded base64.
    let prefix = format!("{}$CSP0:", "\t".repeat(32));
    let msg = welcome(CSP_FEATURES.unwrap());
    let encoded = msg
        .strip_prefix(&prefix)
        .unwrap_or_else(|| panic!("got {msg:?}"));
    let raw = base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(encoded)
        .unwrap();
    let mut got = String::new();
    flate2::read::ZlibDecoder::new(&raw[..])
        .read_to_string(&mut got)
        .unwrap();
    // The configured options, then the custom-update schema (on by default).
    let want = format!("{}\r\n[EXTRA_DATA]\r\nCUSTOM_UPDATE_FORMAT = '", INI.trim());
    assert!(got.starts_with(&want), "got {got:?}");

    // A vanilla client hears no welcome at all.
    assert!(
        welcome("FOO").is_empty(),
        "a vanilla client must not see the options"
    );
}

/// The extensions need no extra_cfg.yml entry, and chat-encoded CSP lines get
/// through a chat rate limit that drops plain chat.
#[test]
fn csp_on_by_default() {
    let srv = start(|cfg| cfg.extra.live.chat_rate_limit_ms = 60_000);
    let a = join_client_features(&srv, "A", CSP_FEATURES);
    let mut b = join_client_features(&srv, "B", CSP_FEATURES);

    let is_chat = |want: String| move |p: &[u8]| p[0] == proto::CHAT && chat_text(p) == want;
    let enc = "\t\t\t\t$CSP0:AAAA";
    for msg in [
        enc.to_string(),
        format!("{enc}1"),
        "hello".into(),
        "dropped".into(),
    ] {
        a.conn.send(&chat(&msg));
    }
    b.conn.await_packet(is_chat(enc.into()));
    b.conn.await_packet(is_chat(format!("{enc}1")));
    b.conn.await_packet(is_chat("hello".into()));
    b.conn.no_packet(is_chat("dropped".into()));

    a.conn
        .send(&client_message(proto::EXT_TCP_CLIENT_MESSAGE, 15000, &[1]));
    b.conn.await_packet(is_msg(15000));
}
