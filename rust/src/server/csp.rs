//! Custom Shaders Patch extensions (docs/04), switched on in extra_cfg.yml.

use std::io::Write as _;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use base64::Engine as _;

use super::Server;
use super::client::Client;
use super::scripts;
use crate::config::parse_digits;
use crate::proto;
use crate::wire::{self, Reader, Writer};

/// Bounds a relayed 0xAB message body. Each one fans out to every other CSP
/// client, so it is capped on top of the rate limit.
const MAX_CSP_MESSAGE: usize = 2048;
/// The most messages a client may send at once, and its refill rate per
/// The tab run that hides extra options after the welcome text (docs/04 §4.5).
const CSP_EXTRA_TABS: usize = 32;
/// The encoded welcome length that crashes a CSP older than
/// `CSP_LONG_WELCOME_BUILD`.
const CSP_LONG_WELCOME_CHARS: usize = 2039;
const CSP_LONG_WELCOME_BUILD: u32 = 1937;

/// Reads the build from a Features list such as
/// "SPECTATING_AWARE,CUSTOM_UPDATE,2144": its last element. 0 if absent.
pub fn parse_csp_version(features: &str) -> u32 {
    let last = features.rsplit(',').next().unwrap_or_default();
    parse_digits(last.trim()).unwrap_or(0)
}

/// The position fields of `0xAB 0x03`, in wire order, in the schema CSP
/// reads from the extra options (docs/04 §4.2).
pub const CUSTOM_UPDATE_FORMAT: &str = "packet:
  group:
    sessionID
    pakSequenceID
    timestamp
    ping: ushort
    pos: float3
    rotation: float3
    velocity: float3
    tyreAngularSpeedFL: byte, -100, tyreAngularSpeed
    tyreAngularSpeedFR: byte, -100, tyreAngularSpeed
    tyreAngularSpeedRL: byte, -100, tyreAngularSpeed
    tyreAngularSpeedRR: byte, -100, tyreAngularSpeed
    steerAngle: byte, -127
    wheelAngle: byte, -127, /2
    engineRPM: ushort
    gear: byte
    statusBytes
    gas: byte, /255
    performanceDelta: short";

/// The INI document sent as extra options: the configured CSPExtraOptions,
/// then the sections the AssettoServer features need. Empty when there is
/// nothing to send.
pub fn extra_options_ini(
    extra: &crate::config::ExtraConfig,
    scripts: &[scripts::ServerScript],
) -> String {
    let mut ini = extra.csp_extra_options.trim().to_string();
    if extra.enable_custom_update {
        ini.push_str(&format!(
            "\r\n[EXTRA_DATA]\r\nCUSTOM_UPDATE_FORMAT = '{CUSTOM_UPDATE_FORMAT}'\r\n"
        ));
    }
    ini.push_str(&scripts::script_sections(scripts));
    ini
}

/// The text that follows the welcome message to carry an INI document to
/// CSP: a tab run, "$CSP0:" and the base64 of its zlib stream, unpadded
/// (docs/04 §4.5). The compressed bytes can differ from Go's compress/zlib;
/// CSP decompresses either to the same document.
pub fn encode_csp_extra_options(ini: &str) -> std::io::Result<String> {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(ini.as_bytes())?;
    let compressed = encoder.finish()?;
    let encoded = base64::engine::general_purpose::STANDARD_NO_PAD.encode(compressed);
    Ok(format!("{}$CSP0:{encoded}", "\t".repeat(CSP_EXTRA_TABS)))
}

/// Encodes a client message: 0xAB, the transport's subtype, the originating
/// car (0xFF for the server), the type and its payload.
fn build_csp_message(udp: bool, sender: u8, kind: u16, payload: &[u8]) -> Vec<u8> {
    let mut w = Writer::with_capacity(5 + payload.len());
    w.u8(proto::EXTENDED);
    if udp {
        w.u8(proto::EXT_UDP_CLIENT_MESSAGE);
    } else {
        w.u8(proto::EXT_TCP_CLIENT_MESSAGE);
    }
    w.u8(sender);
    w.u16(kind);
    w.raw(payload);
    w.into_bytes()
}

/// A CSP feature the server offers, by its handshake and Content Manager
/// name. A mandatory one refuses clients that do not announce it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CspFeature {
    pub name: &'static str,
    pub mandatory: bool,
}

/// The CSP features advertised in /api/details: EMOJI always, then the ones
/// an `extra_cfg.yml` turns on (client messages by default).
///
/// EMOJI, as AssettoServer has it: CSP writes a character beyond U+FFFF as
/// `?` to a server whose list lacks it, and as one wstring cell to one that
/// has it (docs/04 §4.6). The wire reader takes either.
pub fn csp_features(extra: &crate::config::ExtraConfig) -> Vec<CspFeature> {
    let mut features = vec![CspFeature {
        name: "EMOJI",
        mandatory: false,
    }];
    // Without these CSP sends script events as $CSP0: chat lines and has no
    // UDP events at all (docs/04 §4.6). Not mandatory, unlike AssettoServer:
    // they are on by default, and a player without CSP must still get in.
    if extra.enable_client_messages {
        features.push(CspFeature {
            name: "CLIENT_MESSAGES",
            mandatory: false,
        });
    }
    if extra.enable_udp_client_messages {
        features.push(CspFeature {
            name: "CLIENT_UDP_MESSAGES",
            mandatory: false,
        });
    }
    if extra.enable_custom_update {
        features.push(CspFeature {
            name: "CUSTOM_UPDATE",
            mandatory: false,
        });
    }
    if extra.enable_weather_fx {
        features.push(CspFeature {
            name: "WEATHERFX_V1",
            mandatory: true,
        });
    }
    features
}

/// Splits a handshake Features list, dropping the build number at its end.
fn feature_names(features: &str) -> Vec<&str> {
    features
        .split(',')
        .map(str::trim)
        .filter(|f| !f.is_empty() && !f.bytes().all(|b| b.is_ascii_digit()))
        .collect()
}

impl Server {
    /// Whether a handshake's Features list has every mandatory feature.
    pub(crate) fn csp_features_ok(&self, features: &str) -> bool {
        let announced = feature_names(features);
        csp_features(&self.cfg.extra)
            .iter()
            .all(|f| !f.mandatory || announced.contains(&f.name))
    }

    /// Records the optional features a joining client announced.
    pub(crate) fn note_csp_features(&self, client: &Client, features: &str) {
        let custom = self.cfg.extra.enable_custom_update
            && feature_names(features).contains(&"CUSTOM_UPDATE");
        client.csp.custom_update.store(custom, Ordering::SeqCst);
    }

    /// The rejection text when MinimumCSPVersion excludes `client`.
    pub(crate) fn csp_too_old(&self, client: &Client) -> Option<String> {
        let min = self.cfg.extra.minimum_csp_version;
        if min <= 0 || i64::from(client.csp_version()) >= min {
            return None;
        }
        Some(format!("CSP build {min} or newer is required."))
    }

    /// The ready CSP clients. A client message goes to its sender too, as
    /// AssettoServer relays it: CSP scripts receive their own messages
    /// (docs/04 §4.3).
    fn csp_receivers(&self) -> Vec<Arc<Client>> {
        self.broadcast_clients()
            .into_iter()
            .filter(|c| c.is_csp())
            .collect()
    }

    /// Dispatches a client's 0xAB on TCP (docs/04 §4.1).
    pub(crate) fn handle_extended_tcp(&self, client: &Arc<Client>, r: &mut Reader) {
        if !client.is_csp() {
            return;
        }
        match r.u8() {
            proto::EXT_TCP_SPECTATE_CAR if self.cfg.extra.enable_csp => {
                self.handle_spectate_car(client, r);
            }
            proto::EXT_TCP_CLIENT_MESSAGE
                if self.cfg.extra.enable_client_messages && client.csp.allow(false) =>
            {
                self.handle_client_message(client, r, false);
            }
            _ => {}
        }
    }

    /// Dispatches a client's 0xAB on UDP.
    pub(crate) fn handle_extended_udp(&self, client: &Arc<Client>, r: &mut Reader) {
        if !client.is_csp() || !self.cfg.extra.enable_udp_client_messages {
            return;
        }
        if r.u8() == proto::EXT_UDP_CLIENT_MESSAGE && client.csp.allow(true) {
            self.handle_client_message(client, r, true);
        }
    }

    /// Records the car a client watches. Its own id or one out of range
    /// clears it (docs/02 0xAB).
    fn handle_spectate_car(&self, client: &Client, r: &mut Reader) {
        let id = r.u8();
        let _camera_mode = r.u8();
        if r.overflowed() {
            return;
        }
        let target_occupied = self.car_by_id(id).is_some_and(|car| car.occupied());
        if id == client.session_id() || !target_occupied {
            client.csp.spectating.store(0, Ordering::SeqCst);
            return;
        }
        client
            .csp
            .spectating
            .store(i32::from(id) + 1, Ordering::SeqCst);
    }

    /// Routes one CSP client message (docs/04 §4.3). Anything without special
    /// handling goes to every CSP client, the sender included, on the
    /// transport it arrived on, stamped with the sender's id.
    fn handle_client_message(&self, client: &Arc<Client>, r: &mut Reader, udp: bool) {
        if r.remaining() < 2 || r.remaining() > MAX_CSP_MESSAGE {
            return;
        }
        let kind = r.u16();
        let payload = r.remaining_bytes();
        let id = client.session_id();

        // A Lua event for the server itself is answered, not relayed.
        if matches!(kind, proto::CSP_LUA_MESSAGE | proto::CSP_LUA_TARGETED)
            && let Some(lua_type) = scripts::lua_packet_type(kind, &payload)
            && self.handle_server_lua_event(client, lua_type)
        {
            return;
        }

        match kind {
            proto::CSP_HANDSHAKE_OUT => {
                if !udp {
                    handle_csp_handshake_out(client, &payload);
                }
            }
            proto::CSP_ADMIN_PENALTY => {
                if !udp {
                    self.handle_admin_penalty(client, &payload);
                }
            }
            proto::CSP_LUA_TARGETED => {
                if let Some(&target) = payload.first() {
                    let to = self.csp_target(target);
                    self.send_csp_message(to.as_deref(), udp, id, kind, &payload);
                }
            }
            proto::CSP_LUA_RANGED | proto::CSP_LUA_RANGED_TARGET => {
                if udp {
                    self.relay_ranged(client, kind, &payload);
                }
            }
            _ => {
                let receivers = self.csp_receivers();
                self.broadcast_csp_message(&receivers, udp, id, kind, &payload);
            }
        }
    }

    /// Forwards an admin's penalty to its target as if the server sent it.
    /// Only admins may send one.
    fn handle_admin_penalty(&self, client: &Client, payload: &[u8]) {
        if !client.is_admin() {
            return;
        }
        let mut r = Reader::new(payload);
        let _mode = r.u16();
        let car = r.u16();
        if r.overflowed() || car > u16::from(u8::MAX) {
            return;
        }
        tracing::info!(by = client.name(), target = car, "csp admin penalty");
        let to = self.csp_target(car as u8);
        self.send_csp_message(
            to.as_deref(),
            false,
            proto::SERVER_SESSION_ID,
            proto::CSP_ADMIN_PENALTY,
            payload,
        );
    }

    /// The ready CSP client in slot `id`, the sender's own included.
    fn csp_target(&self, id: u8) -> Option<Arc<Client>> {
        let target = self.car_by_id(id)?.client()?;
        (target.ready() && target.is_csp()).then_some(target)
    }

    /// Forwards a Lua message to the cars within its range of the sender:
    /// type 60002 to all of them as a plain 60000, 60003 to one as a 60001.
    /// payload: f16 range, [u8 target], then the Lua key and data.
    fn relay_ranged(&self, client: &Client, kind: u16, payload: &[u8]) {
        let mut r = Reader::new(payload);
        let range = r.f16();
        let target = if kind == proto::CSP_LUA_RANGED_TARGET {
            r.u8()
        } else {
            0
        };
        if r.overflowed() || range.is_nan() || range <= 0.0 || range.is_infinite() {
            return;
        }
        let id = client.session_id();
        let Some(from) = self.entry_cars[usize::from(id)].last_position() else {
            return;
        };
        let range2 = f64::from(range) * f64::from(range);
        let near = |t: &Client| -> bool {
            let pos = self.entry_cars[usize::from(t.session_id())].last_position();
            pos.is_some_and(|pos| {
                (from.position - pos.position).as_dvec3().length_squared() <= range2
            })
        };
        let data = r.remaining_bytes();
        if kind == proto::CSP_LUA_RANGED_TARGET {
            if let Some(t) = self.csp_target(target)
                && near(&t)
            {
                let mut body = Vec::with_capacity(1 + data.len());
                body.push(target);
                body.extend_from_slice(&data);
                self.send_csp_message(Some(&t), true, id, proto::CSP_LUA_TARGETED, &body);
            }
            return;
        }
        let receivers: Vec<Arc<Client>> = self
            .csp_receivers()
            .into_iter()
            .filter(|t| near(t))
            .collect();
        self.broadcast_csp_message(&receivers, true, id, proto::CSP_LUA_MESSAGE, &data);
    }

    /// Sends one client message to one client, if there is one.
    fn send_csp_message(
        &self,
        to: Option<&Client>,
        udp: bool,
        sender: u8,
        kind: u16,
        payload: &[u8],
    ) {
        let Some(to) = to else { return };
        let msg = build_csp_message(udp, sender, kind, payload);
        if udp {
            self.send_udp_to_client(to, &msg);
        } else {
            self.send_tcp(to, &msg);
        }
    }

    /// Encodes a client message once and sends it to each client.
    fn broadcast_csp_message(
        &self,
        to: &[Arc<Client>],
        udp: bool,
        sender: u8,
        kind: u16,
        payload: &[u8],
    ) {
        if to.is_empty() {
            return;
        }
        let msg = build_csp_message(udp, sender, kind, payload);
        if udp {
            for client in to {
                self.send_udp_to_client(client, &msg);
            }
            return;
        }
        let frame = wire::frame_tcp(&msg);
        for client in to {
            self.queue_frame(client, frame.clone());
        }
    }

    /// Asks a CSP client to identify itself, at the end of its first update,
    /// requiring WeatherFX when the server runs it.
    pub(crate) fn send_csp_handshake(&self, client: &Client) {
        if !self.cfg.extra.enable_client_messages || !client.is_csp() {
            return;
        }
        let mut w = Writer::with_id(proto::EXTENDED);
        w.u8(proto::EXT_TCP_CLIENT_MESSAGE);
        w.u8(proto::SERVER_SESSION_ID);
        w.u16(proto::CSP_HANDSHAKE_IN);
        w.u32(self.cfg.extra.minimum_csp_version.max(0) as u32);
        w.bool(self.cfg.extra.enable_weather_fx); // RequiresWeatherFx
        self.send_tcp(client, w.bytes());
    }

    /// Replaces the client's generic kick screen text, just before its 0x68.
    pub(crate) fn send_csp_kick_message(&self, client: &Client, reason: u8) {
        if !self.cfg.extra.enable_csp || !client.is_csp() {
            return;
        }
        let msg = match reason {
            proto::KICK_VOTE_KICKED => "You have been kicked by a vote.",
            proto::KICK_VOTE_BANNED => "You have been banned by a vote.",
            proto::KICK_VOTE_BLACKLISTED => "You have been blacklisted from the server.",
            proto::KICK_CHECKSUM_FAILED => "Your game files do not match the server's.",
            _ => "You have been kicked from the server.",
        };
        let mut w = Writer::with_id(proto::EXTENDED);
        w.u8(proto::EXT_TCP_KICK_MESSAGE);
        w.u32(msg.len() as u32); // a 4-byte length, unlike every other string
        w.raw(msg.as_bytes());
        self.send_tcp(client, w.bytes());
    }

    /// Paints car `id` in `rgba` for one CSP client: a CarColorChange
    /// message from that car (docs/04 §4.3). Needs EnableClientMessages.
    pub(crate) fn send_car_color(&self, client: &Client, id: u8, rgba: [u8; 4]) {
        if !self.cfg.extra.enable_client_messages || !client.is_csp() {
            return;
        }
        self.send_csp_message(Some(client), false, id, proto::CSP_CAR_COLOR_CHANGE, &rgba);
    }

    /// Shows or hides a car for one CSP client (0xAB 0x02).
    pub(crate) fn send_car_visibility(&self, client: &Client, id: u8, visible: bool) {
        let mut w = Writer::with_id(proto::EXTENDED);
        w.u8(proto::EXT_TCP_CAR_VISIBILITY);
        w.u8(id);
        w.bool(!visible); // 0 visible, 1 invisible
        self.send_tcp(client, w.bytes());
    }

    /// Shows or hides a car for every CSP client (0xAB 0x02).
    pub fn set_car_visibility(&self, id: u8, visible: bool) {
        if !self.cfg.extra.enable_csp {
            return;
        }
        let mut w = Writer::with_id(proto::EXTENDED);
        w.u8(proto::EXT_TCP_CAR_VISIBILITY);
        w.u8(id);
        w.bool(!visible); // 0 visible, 1 invisible
        let frame = wire::frame_tcp(w.bytes());
        for client in self.csp_receivers() {
            self.queue_frame(&client, frame.clone());
        }
    }

    /// The 0x51 text for `client`: the welcome message and, for a CSP client,
    /// the extra options. A CSP too old for a long welcome gets none.
    pub(crate) fn welcome_text(&self, client: &Client) -> String {
        let msg = &self.cfg.server.welcome_message;
        if self.csp_extra.is_empty() || !client.is_csp() {
            return msg.clone();
        }
        let full = format!("{msg}{}", self.csp_extra);
        if client.csp_version() < CSP_LONG_WELCOME_BUILD
            && full.chars().count() > CSP_LONG_WELCOME_CHARS
        {
            tracing::warn!(
                car = client.session_id(),
                build = client.csp_version(),
                "csp extra options skipped: welcome too long for this CSP build"
            );
            return msg.clone();
        }
        full
    }
}

/// Records the client's answer to HandshakeIn.
fn handle_csp_handshake_out(client: &Client, payload: &[u8]) {
    let mut r = Reader::new(payload);
    let version = r.u32();
    let weather_fx = r.bool();
    let input = r.u8();
    let rain_fx = r.bool();
    let _padding = r.u8();
    let _hardware_key = r.u64(); // not retained
    if r.overflowed() {
        return;
    }
    client.csp.hello.store(true, Ordering::SeqCst);
    tracing::debug!(
        car = client.session_id(),
        version,
        weather_fx,
        input,
        rain_fx,
        "csp handshake"
    );
}
