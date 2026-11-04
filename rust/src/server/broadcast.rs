//! Every server-to-client packet builder and broadcast.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::Server;
use super::client::{Client, Frame};
use super::entrycar::EntryCar;
use super::firstupdate::leaderboard_time;
use crate::proto::{self, SessionType};
use crate::wire::{self, Writer};

impl Server {
    /// The ready clients (see `Client::ready`).
    pub(crate) fn broadcast_clients(&self) -> Vec<Arc<Client>> {
        let mut out = Vec::new();
        for car in &self.entry_cars {
            if let Some(client) = car.client()
                && client.ready()
            {
                out.push(client);
            }
        }
        out
    }

    /// Sends a payload to every ready client except session id `except`
    /// (`None` for none), as vanilla's BroadcastTCP skips a client still
    /// loading. It is framed once and shared.
    pub(crate) fn broadcast_tcp(&self, payload: &[u8], except: Option<u8>) {
        let frame: Frame = wire::frame_tcp(payload);
        for car in &self.entry_cars {
            let Some(client) = car.client() else { continue };
            if Some(client.session_id()) == except || !client.ready() {
                continue;
            }
            self.queue_frame(&client, frame.clone());
        }
    }

    /// Announces a newly admitted driver (0x5A).
    pub(crate) fn broadcast_car_connected(&self, car: &EntryCar) {
        let Some(client) = car.client() else { return };
        let id = client.identity();
        let mut w = Writer::with_id(proto::CAR_CONNECTED);
        w.u8(car.session_id);
        w.string8(&id.name);
        // The occupant's own nation.
        w.string8(&id.nation);
        self.broadcast_tcp(w.bytes(), Some(car.session_id));
    }

    /// Announces a car that has no client, such as AI traffic (0x5A).
    pub(crate) fn broadcast_named_car_connected(&self, id: u8, name: &str) {
        let mut w = Writer::with_id(proto::CAR_CONNECTED);
        w.u8(id);
        w.string8(name);
        w.string8("");
        self.broadcast_tcp(w.bytes(), None);
    }

    /// Announces a departure (0x4D).
    pub(crate) fn broadcast_car_disconnected(&self, id: u8) {
        let mut w = Writer::with_id(proto::CAR_DISCONNECTED);
        w.u8(id);
        self.broadcast_tcp(w.bytes(), None);
    }

    /// Sends 0x68 to everyone, the target included, and suppresses the
    /// target's 0x4D.
    pub(crate) fn broadcast_kick(&self, id: u8, reason: u8) {
        if let Some(car) = self.car_by_id(id)
            && let Some(client) = car.client()
        {
            client.kicked.store(true, Ordering::SeqCst);
            self.send_csp_kick_message(&client, reason);
        }
        let mut w = Writer::with_id(proto::KICK_CAR);
        w.u8(id);
        w.u8(reason);
        self.broadcast_tcp(w.bytes(), None);
    }

    /// Sends 0x4A to every handshaked client, including one still loading:
    /// vanilla's broadcastSession goes to every connection, not through the
    /// ready-gated BroadcastTCP. Each copy carries that client's clock.
    pub(crate) fn broadcast_current_session(&self) {
        for car in &self.entry_cars {
            if let Some(client) = car.client()
                && client.handshaked.load(Ordering::SeqCst)
            {
                self.send_current_session(&client);
            }
        }
    }

    /// Sends 0x4A to one client (also the 0x4F resync).
    pub(crate) fn send_current_session(&self, client: &Client) {
        let payload = self.build_current_session(client.time_offset());
        self.send_tcp(client, &payload);
    }

    /// Encodes a 0x4A whose StartTime is in the receiver's clock: the
    /// server-clock start minus its offset (docs/02 0x4A, vanilla
    /// sendSessionToClient).
    pub(crate) fn build_current_session(&self, offset: i64) -> Vec<u8> {
        let (cur, cur_idx) = self.sessions.current();
        let mut w = Writer::with_id(proto::CURRENT_SESSION_UPDATE);
        w.string8(&cur.name);
        w.u8(cur_idx as u8);
        w.u8(cur.kind.as_u8());
        w.u16(cur.time);
        w.u16(cur.laps);
        w.f32(self.weather.grip());
        for id in self.grid() {
            w.u8(id);
        }
        w.i64(self.sessions.start_time_ms() - offset);
        w.into_bytes()
    }

    /// Sends 0x78 to one client; with WeatherFX, `0xAB 0x01` instead.
    pub(crate) fn send_weather(&self, client: &Client) {
        if self.weatherfx.is_some() {
            self.send_weatherfx(client);
            return;
        }
        let wx = self.weather.snapshot();
        let mut w = Writer::with_id(proto::WEATHER_UPDATE);
        w.temperature(wx.ambient);
        w.temperature(wx.road);
        w.wstring8(&wx.graphics);
        w.i16(wx.wind_speed);
        w.i16(wx.wind_direction);
        self.send_tcp(client, w.bytes());
    }

    /// Sends 0x54 to everyone (docs/02 0x54). WeatherFX sets the time
    /// itself, so it sends none.
    pub(crate) fn broadcast_sun_angle(&self) {
        if self.weatherfx.is_some() {
            return;
        }
        let mut w = Writer::with_id(proto::SUN_ANGLE_UPDATE);
        w.f32(self.weather.snapshot().sun_angle);
        self.broadcast_tcp(w.bytes(), None);
    }

    /// Sends 0x78 to everyone.
    pub(crate) fn broadcast_weather(&self) {
        for client in self.broadcast_clients() {
            self.send_weather(&client);
        }
    }

    /// Sends 0x5B: every slot, with the name of its occupant or last driver,
    /// as vanilla.
    pub(crate) fn send_driver_info(&self, client: &Client) {
        let cars = self.listed_cars();
        let mut w = Writer::with_id(proto::DRIVER_INFO_UPDATE);
        w.u8(cars.len() as u8);
        for car in &cars {
            w.u8(car.session_id);
            w.wstring8(&car.identity().name);
        }
        self.send_tcp(client, w.bytes());
    }

    /// Sends one slot's 0x50 to a client.
    pub(crate) fn send_tyre_compound(&self, client: &Client, id: u8, compound: &str) {
        let mut w = Writer::with_id(proto::TYRE_COMPOUND_CHANGE);
        w.u8(id);
        w.string8(compound);
        self.send_tcp(client, w.bytes());
    }

    /// Sends 0x0E for one car to a client.
    pub(crate) fn send_mandatory_pit(&self, client: &Client, id: u8, done: bool) {
        let mut w = Writer::with_id(proto::MANDATORY_PIT_UPDATE);
        w.u8(id);
        w.bool(done);
        self.send_tcp(client, w.bytes());
    }

    /// Sends 0x0D for one car to a client.
    pub(crate) fn send_p2p(&self, client: &Client, id: u8, count: i16, active: bool) {
        let mut w = Writer::with_id(proto::P2P_UPDATE);
        w.u8(id);
        w.i16(count);
        w.bool(active);
        self.send_tcp(client, w.bytes());
    }

    /// Sends one 0x70 listing `cars`.
    pub(crate) fn send_bop(&self, client: &Client, cars: &[Arc<EntryCar>]) {
        let mut w = Writer::with_id(proto::BOP_UPDATE);
        w.u8(cars.len() as u8);
        for car in cars {
            let state = car.state();
            w.u8(car.session_id);
            w.f32(state.ballast);
            w.f32(state.restrictor);
        }
        self.send_tcp(client, w.bytes());
    }

    /// Sends every slot's BoP to everyone, as vanilla does after /ballast or
    /// /restrictor.
    pub(crate) fn broadcast_bop(&self) {
        for client in self.broadcast_clients() {
            self.send_bop(&client, &self.listed_cars());
        }
    }

    /// Sends 0x51 to a client.
    pub(crate) fn send_welcome(&self, client: &Client) {
        let mut w = Writer::with_id(proto::WELCOME_MESSAGE);
        w.u8(0);
        w.wstring16(&self.welcome_text(client));
        self.send_tcp(client, w.bytes());
    }

    /// Builds a 0x49. `session_id`, `lap_time` and `cuts` describe the lap
    /// just completed (0xFF, 0, 0 for a plain sync).
    pub(crate) fn build_leaderboard(&self, session_id: u8, lap_time: u32, cuts: u8) -> Vec<u8> {
        let (cur, _) = self.sessions.current();
        let entries = self.leaderboard_snapshots();

        // Header (8), one 8-byte result per slot, and grip (4).
        let mut w = Writer::with_capacity(12 + 8 * entries.len());
        w.u8(proto::LAP_COMPLETED);
        w.u8(session_id);
        w.u32(lap_time);
        w.u8(cuts);
        w.u8(entries.len() as u8);
        for entry in &entries {
            let res = &entry.result;
            w.u8(entry.car.session_id);
            w.u32(leaderboard_time(res, cur.kind));
            w.u16(res.num_laps);
            // Set once the car has finished the race; always 0 outside one.
            w.bool(cur.kind == SessionType::Race && res.has_completed_last_lap);
        }
        w.f32(self.weather.grip());
        w.into_bytes()
    }

    /// Sends a plain leaderboard sync to one client.
    pub(crate) fn send_leaderboard_sync(&self, client: &Client) {
        let payload = self.build_leaderboard(proto::SERVER_SESSION_ID, 0, 0);
        self.send_tcp(client, &payload);
    }

    /// Sends 0x4B at the end of any session: a 7-byte result per slot, then
    /// PickupMode.
    pub(crate) fn broadcast_race_over(&self) {
        // Vanilla's sun timer counts from process start, before the session
        // loads, so a 0x54 due by now has already gone out ahead of the
        // session's end. The session ticker is coarser than the relay loop,
        // so catch it up here.
        self.step_sun();
        let (cur, _) = self.sessions.current();
        let entries = self.leaderboard_snapshots();

        // Packet id, one 7-byte result per slot, and PickupMode.
        let mut w = Writer::with_capacity(2 + 7 * entries.len());
        w.u8(proto::RACE_OVER);
        for entry in &entries {
            let res = &entry.result;
            w.u8(entry.car.session_id);
            w.u32(leaderboard_time(res, cur.kind));
            w.u16(res.num_laps);
        }
        w.bool(self.cfg.server.pickup_mode_enabled);
        self.broadcast_tcp(w.bytes(), None);
    }
}

/// Encodes a 0x57: the session start and the current time, both in the
/// receiver's clock (server clock minus offset, docs/03 0x57).
pub fn build_race_start(start: i64, now: i64, offset: i64, ping: u16) -> Vec<u8> {
    let mut w = Writer::with_id(proto::RACE_START);
    w.i32((start - offset) as i32);
    w.u32((now - offset) as u32);
    w.u16(ping);
    w.into_bytes()
}
