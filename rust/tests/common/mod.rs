//! Shared helpers for the integration tests: a server running in process on
//! free ports, and a minimal blocking game client.

#![allow(dead_code)]

use std::io::{self, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use acserver_rs::config::{self, Config, IsOpen, SessionConfig};
use acserver_rs::proto::{self, SessionType};
use acserver_rs::server::{PluginBridge, Server};
use acserver_rs::wire::{Reader, Vec3, Writer, frame_tcp};

/// A server running in process. Dropping it shuts the server down.
pub struct TestServer {
    pub server: Arc<Server>,
    pub game: u16,
    pub http: u16,
    thread: Option<JoinHandle<()>>,
}

impl TestServer {
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.http)
    }

    /// The `clients` count in /INFO (ready clients, as vanilla counts).
    pub fn info_clients(&self) -> i64 {
        self.info()["clients"].as_i64().expect("clients")
    }

    pub fn info(&self) -> serde_json::Value {
        get_json(&format!("{}/INFO", self.base_url()))
    }

    /// Waits up to 5 s for /INFO to report `want` clients.
    pub fn require_info_clients(&self, want: i64, msg: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut got = self.info_clients();
        while got != want && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
            got = self.info_clients();
        }
        assert_eq!(got, want, "{msg}");
    }

    /// The session id of the connected client called `name`.
    pub fn session_id_of(&self, name: &str) -> Option<u8> {
        self.server.entry_cars().iter().find_map(|car| {
            car.client()
                .filter(|c| c.name() == name)
                .map(|_| car.session_id)
        })
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.server.shutdown.cancel();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Loads the sample `../cfg`, applies `mutate`, binds it to free ports and
/// runs it. Vanilla's 20 s race wait, 30 s race cap and 10 s result screen
/// would dominate these tests, so they are zeroed; a test that needs them sets
/// them back.
pub fn start(mutate: impl FnOnce(&mut Config)) -> TestServer {
    start_with_plugin(mutate, None)
}

/// `start` with a plugin bridge, set before the server runs.
pub fn start_with_plugin(
    mutate: impl FnOnce(&mut Config),
    plugin: Option<Arc<dyn PluginBridge>>,
) -> TestServer {
    let mut cfg = config::load("../cfg", "").expect("load ../cfg");
    cfg.server.min_race_wait = 0;
    cfg.server.min_race_over = 0;
    cfg.server.min_result_screen = 0;
    mutate(&mut cfg);
    let game = free_port();
    let http = free_port();
    cfg.server.tcp_port = game;
    cfg.server.udp_port = game;
    cfg.server.http_port = http;
    cfg.server.register_to_lobby = false;

    let server = Server::new(cfg).expect("new server");
    if let Some(plugin) = plugin {
        server.set_plugin(plugin);
    }
    let running = Arc::clone(&server);
    let thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async move {
            let http = Arc::clone(&running);
            tokio::spawn(async move {
                let _ = acserver_rs::httpapi::serve(http).await;
            });
            running.run().await.expect("server run");
        });
    });
    let srv = TestServer {
        server,
        game,
        http,
        thread: Some(thread),
    };
    wait_for_port(game);
    wait_for_port(http);
    srv
}

fn wait_for_port(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("port {port} never opened");
}

/// A port below the ephemeral range, free for both TCP and UDP, so an outgoing
/// socket cannot take it before the server binds.
pub fn free_port() -> u16 {
    static NEXT: AtomicU16 = AtomicU16::new(0);
    let _ = NEXT.compare_exchange(
        0,
        20_000 + (std::process::id() % 200) as u16 * 50,
        Ordering::SeqCst,
        Ordering::SeqCst,
    );
    loop {
        let port = NEXT.fetch_add(1, Ordering::SeqCst);
        let tcp = TcpListener::bind(("127.0.0.1", port));
        let udp = UdpSocket::bind(("127.0.0.1", port));
        if tcp.is_ok() && udp.is_ok() {
            return port;
        }
    }
}

/// A single-lap race as the only session.
pub fn one_lap_race() -> Vec<SessionConfig> {
    vec![SessionConfig {
        name: "Race".into(),
        laps: 1,
        is_open: IsOpen::Open,
        ..SessionConfig::new(SessionType::Race)
    }]
}

/// Replaces the sample entry list with `n` identical slots and opens a
/// single-lap race that fills every one of them.
pub fn fill_grid(cfg: &mut Config, n: usize) {
    let template = cfg.entry_list[0].clone();
    cfg.entry_list = (0..n)
        .map(|_| config::EntryCar {
            model: "ks_mazda_mx5_cup".into(),
            skin: String::new(),
            ..template.clone()
        })
        .collect();
    cfg.server.max_clients = n as u8;
    cfg.sessions = one_lap_race();
    cfg.server.race_over_time = 5; // a safety cap for any straggler
    cfg.server.result_screen_time = 1;
    cfg.server.loop_mode = false;
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

pub fn get_json(url: &str) -> serde_json::Value {
    reqwest::blocking::get(url)
        .unwrap_or_else(|e| panic!("GET {url}: {e}"))
        .json()
        .unwrap_or_else(|e| panic!("decode {url}: {e}"))
}

// ---------------------------------------------------------------------------
// Game client
// ---------------------------------------------------------------------------

/// One TCP connection, reading length-prefixed packets.
pub struct Conn {
    stream: TcpStream,
    reader: BufReader<TcpStream>,
}

impl Conn {
    pub fn dial(port: u16) -> Conn {
        let stream = TcpStream::connect(("127.0.0.1", port)).expect("dial tcp");
        let reader = BufReader::new(stream.try_clone().expect("clone tcp"));
        Conn { stream, reader }
    }

    pub fn send(&self, payload: &[u8]) {
        let _ = (&self.stream).write_all(&frame_tcp(payload));
    }

    /// A handle that writes to this connection, for another thread.
    pub fn sender(&self) -> Sender {
        Sender(self.stream.try_clone().expect("clone tcp"))
    }

    pub fn write_raw(&self, bytes: &[u8]) -> io::Result<()> {
        (&self.stream).write_all(bytes)
    }

    pub fn set_timeout(&self, timeout: Duration) {
        self.stream
            .set_read_timeout(Some(timeout))
            .expect("set timeout");
    }

    /// The next packet, without its length prefix.
    pub fn read(&mut self) -> io::Result<Vec<u8>> {
        let mut len = [0u8; 2];
        self.reader.read_exact(&mut len)?;
        let mut payload = vec![0u8; usize::from(u16::from_le_bytes(len))];
        self.reader.read_exact(&mut payload)?;
        Ok(payload)
    }

    /// The next packet, failing the test on an error.
    pub fn expect(&mut self) -> Vec<u8> {
        self.read().expect("read tcp")
    }

    /// Reads raw bytes, for checking that the server closed the connection.
    pub fn read_byte(&mut self) -> io::Result<u8> {
        let mut b = [0u8; 1];
        match self.reader.read(&mut b)? {
            0 => Err(io::ErrorKind::UnexpectedEof.into()),
            _ => Ok(b[0]),
        }
    }

    /// Reads until a packet satisfies `matches`, within 3 s.
    pub fn await_packet(&mut self, matches: impl Fn(&[u8]) -> bool) -> Vec<u8> {
        self.set_timeout(Duration::from_secs(3));
        loop {
            let pkt = self.read().expect("expected packet never arrived");
            if matches(&pkt) {
                return pkt;
            }
        }
    }

    /// Fails if a packet satisfying `matches` arrives within 300 ms.
    pub fn no_packet(&mut self, matches: impl Fn(&[u8]) -> bool) {
        self.set_timeout(Duration::from_millis(300));
        while let Ok(pkt) = self.read() {
            assert!(!matches(&pkt), "unexpected packet {pkt:02x?}");
        }
    }
}

/// The write side of a `Conn`.
pub struct Sender(TcpStream);

impl Sender {
    pub fn send(&self, payload: &[u8]) {
        let _ = (&self.0).write_all(&frame_tcp(payload));
    }

    /// Closes both directions, which ends a reader blocked on the `Conn`.
    pub fn close(&self) {
        let _ = self.0.shutdown(std::net::Shutdown::Both);
    }
}

pub fn is_timeout(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// A 0x3D join request.
pub fn handshake(guid: &str, name: &str, password: &str, features: Option<&str>) -> Vec<u8> {
    let mut w = Writer::with_id(proto::REQUEST_NEW_CONNECTION);
    w.u16(proto::CLIENT_PROTOCOL_VERSION);
    w.string8(guid);
    w.wstring8(name);
    w.string8("");
    w.string8("GBR");
    w.string8("ks_mazda_mx5_cup");
    w.string8(password);
    if let Some(features) = features {
        w.string16(features);
    }
    w.into_bytes()
}

/// Opens a connection, sends a 0x3D and returns the server's first reply.
pub fn dial_handshake(port: u16, guid: &str, name: &str) -> (Conn, Vec<u8>) {
    dial_handshake_features(port, guid, name, None)
}

pub fn dial_handshake_features(
    port: u16,
    guid: &str,
    name: &str,
    features: Option<&str>,
) -> (Conn, Vec<u8>) {
    let mut conn = Conn::dial(port);
    conn.send(&handshake(guid, name, "", features));
    conn.set_timeout(Duration::from_secs(5));
    let resp = conn.expect();
    (conn, resp)
}

/// Each join gets a distinct Steam guid: the server admits one connection per
/// guid and drops the older session on a repeat.
static JOIN_GUID: AtomicU64 = AtomicU64::new(0);

pub fn next_guid() -> String {
    (76_561_198_000_000_009 + JOIN_GUID.fetch_add(1, Ordering::SeqCst) + 1).to_string()
}

/// A client after a full join: handshake, UDP association, checksum and a
/// first position update.
pub struct Joined {
    pub conn: Conn,
    pub udp: UdpSocket,
    pub id: u8,
}

/// Joins and waits until the server counts the client as ready.
pub fn join_client(srv: &TestServer, name: &str) -> Joined {
    join_client_features(srv, name, None)
}

pub fn join_client_features(srv: &TestServer, name: &str, features: Option<&str>) -> Joined {
    let joined = join(srv, name, features).unwrap_or_else(|e| panic!("join {name}: {e}"));
    let car = srv.server.car_by_id(joined.id).expect("joined slot");
    let deadline = Instant::now() + Duration::from_secs(2);
    while !car.client().is_some_and(|c| c.ready()) {
        assert!(Instant::now() < deadline, "{name} never became ready");
        std::thread::sleep(Duration::from_millis(5));
    }
    joined
}

/// A join that returns an error instead of failing, for joins from many
/// threads. It does not wait for readiness.
pub fn join(srv: &TestServer, name: &str, features: Option<&str>) -> Result<Joined, String> {
    let mut conn = Conn::dial(srv.game);
    conn.send(&handshake(&next_guid(), name, "", features));
    conn.set_timeout(Duration::from_secs(5));
    let resp = conn
        .read()
        .map_err(|e| format!("read handshake reply: {e}"))?;
    if resp[0] != proto::NEW_CAR_CONNECTION {
        return Err(format!("expected 0x3E, got 0x{:02X}", resp[0]));
    }
    let id = srv
        .session_id_of(name)
        .ok_or_else(|| format!("{name} not registered in a slot"))?;

    let udp = UdpSocket::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    udp.connect(("127.0.0.1", srv.game))
        .map_err(|e| e.to_string())?;
    udp.send(&[proto::CAR_CONNECT, id])
        .map_err(|e| e.to_string())?;
    udp.set_read_timeout(Some(Duration::from_secs(3)))
        .map_err(|e| e.to_string())?;
    let mut buf = [0u8; 64];
    udp.recv(&mut buf)
        .map_err(|e| format!("udp association reply: {e}"))?;

    let mut checksum = Writer::with_id(proto::CHECKSUM_PACKET);
    checksum.raw(&[0u8; 16]);
    conn.send(checksum.bytes());
    write_position(&udp, 1, 0.1);
    conn.set_timeout(Duration::from_secs(30));
    Ok(Joined { conn, udp, id })
}

/// One 0x46 position update.
pub fn write_position(udp: &UdpSocket, seq: u8, normalized_pos: f32) {
    let mut w = Writer::with_id(proto::POSITION_UPDATE);
    w.u8(seq);
    w.u32(1000);
    w.vec3(Vec3::ZERO);
    w.vec3(Vec3::ZERO);
    w.vec3(Vec3::ZERO);
    w.raw(&[0u8; 4]); // tyre speeds
    w.u8(127); // steer
    w.u8(127); // wheel
    w.u16(3000); // rpm
    w.u8(2); // gear
    w.u32(0); // status
    w.i16(0); // performance delta
    w.u8(0); // gas
    w.f32(normalized_pos);
    let _ = udp.send(w.bytes());
}

/// A 0x49 lap with timestamp 50000, a 95 s lap and no splits.
pub fn lap(num_lap: u8) -> Vec<u8> {
    lap_at(50_000, 95_000, num_lap)
}

pub fn lap_at(timestamp: u32, lap_time: u32, num_lap: u8) -> Vec<u8> {
    let mut w = Writer::with_id(proto::LAP_COMPLETED);
    w.u32(timestamp);
    w.u32(lap_time);
    w.u8(0); // split count
    w.u8(0); // cuts
    w.u8(num_lap);
    w.into_bytes()
}

/// A lap with a rising timestamp: two laps in one millisecond would fail the
/// lap-timestamp gate.
pub fn lap_timed(lap_time: u32) -> Vec<u8> {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    lap_at(SEQ.fetch_add(1, Ordering::SeqCst) + 1, lap_time, 1)
}

pub fn chat(msg: &str) -> Vec<u8> {
    let mut w = Writer::with_id(proto::CHAT);
    w.u8(0);
    w.wstring8(msg);
    w.into_bytes()
}

/// The text of a 0x47 chat packet.
pub fn chat_text(pkt: &[u8]) -> String {
    let mut r = Reader::new(&pkt[1..]);
    r.u8();
    r.wstring8()
}

/// Reads until a 0x4B RaceOver, or returns `None` after `within`.
pub fn await_race_over(conn: &mut Conn, within: Duration) -> Option<Vec<u8>> {
    let deadline = Instant::now() + within;
    conn.set_timeout(within);
    while Instant::now() < deadline {
        let pkt = conn.read().ok()?;
        if pkt[0] == proto::RACE_OVER {
            return Some(pkt);
        }
    }
    None
}

/// The (session id, time, laps) entries of a 0x4B and its PickupMode flag.
pub fn race_over_entries(pkt: &[u8]) -> (Vec<(u8, u32, u16)>, bool) {
    let mut r = Reader::new(&pkt[1..]);
    let mut entries = Vec::new();
    while r.remaining() >= 8 {
        entries.push((r.u8(), r.u32(), r.u16()));
    }
    (entries, r.bool())
}
