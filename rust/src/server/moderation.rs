//! AssettoServer's automatic kicks, each off unless configured: AFK
//! (`EnableAntiAfk`), high ping (`MaxPing`) and a missing checksum reply
//! (`PlayerChecksumTimeoutSeconds`). Kicks go out as AssettoServer sends
//! them: a chat line saying why, the reason on CSP's kick screen, then 0x68.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::Server;
use super::client::{ChecksumStatus, Client};
use super::entrycar::PositionUpdate;
use crate::config::AfkKickBehavior;
use crate::proto;
use crate::wire::Writer;

/// Under AfkKickBehavior MinimumSpeed, inputs count only above this, m/s.
const AFK_MIN_SPEED: f32 = 20.0 / 3.6;

/// The warning goes out this long before an AFK kick, ms.
const AFK_WARNING_MS: i64 = 60_000;

impl Server {
    /// Kicks `target` as AssettoServer's KickAsync does: `to_all` to every
    /// player, `to_player` on the target's CSP kick screen, then 0x68 with
    /// `code`.
    pub(crate) fn kick_with_message(
        &self,
        target: &Arc<Client>,
        code: u8,
        to_player: Option<&str>,
        to_all: Option<&str>,
    ) {
        if target.disconnected.load(Ordering::SeqCst) {
            return;
        }
        if let Some(text) = to_all {
            self.broadcast_server_chat(text);
        }
        if let Some(text) = to_player
            && self.cfg.extra.enable_csp
            && target.is_csp()
        {
            let mut w = Writer::with_id(proto::EXTENDED);
            w.u8(proto::EXT_TCP_KICK_MESSAGE);
            w.u32(text.len() as u32);
            w.raw(text.as_bytes());
            self.send_tcp(target, w.bytes());
        }
        target.kicked.store(true, Ordering::SeqCst);
        let mut w = Writer::with_id(proto::KICK_CAR);
        w.u8(target.session_id());
        w.u8(code);
        self.broadcast_tcp(w.bytes(), None);
        // The target may still be loading, so it gets its own copy.
        if !target.ready() {
            self.send_tcp(target, w.bytes());
        }
        self.disconnect(target, "kicked");
    }

    /// Kicks `target` "for <reason>", as AssettoServer words it.
    pub(crate) fn kick_for(&self, target: &Arc<Client>, reason: &str) {
        tracing::info!(
            car = target.session_id(),
            name = target.name(),
            reason,
            "kicked"
        );
        self.kick_with_message(
            target,
            proto::KICK_KICKED,
            Some(&format!("You have been kicked for {reason}")),
            Some(&format!(
                "{} has been kicked from the server for {reason}.",
                target.name()
            )),
        );
    }

    /// Marks a player active (EnableAntiAfk).
    pub(crate) fn mark_active(&self, client: &Client) {
        client
            .last_active_ms
            .store(self.uptime_ms(), Ordering::SeqCst);
        client.afk_warned.store(false, Ordering::SeqCst);
    }

    /// A new position counts as activity when the lights, gas or steering
    /// changed (and, under MinimumSpeed, the car is moving).
    pub(crate) fn note_activity(
        &self,
        client: &Client,
        previous: Option<PositionUpdate>,
        p: &PositionUpdate,
    ) {
        if !self.live().extra.enable_anti_afk {
            return;
        }
        let changed = previous.is_none_or(|old| {
            old.status_flags != p.status_flags
                || old.gas != p.gas
                || old.steer_angle != p.steer_angle
        });
        let fast_enough = self.live().extra.afk_kick_behavior != AfkKickBehavior::MinimumSpeed
            || p.velocity.length_squared() > AFK_MIN_SPEED * AFK_MIN_SPEED;
        if changed && fast_enough {
            self.mark_active(client);
        }
    }

    /// A chat line is activity under PlayerInput.
    pub(crate) fn note_chat_activity(&self, client: &Client) {
        if self.live().extra.enable_anti_afk
            && self.live().extra.afk_kick_behavior == AfkKickBehavior::PlayerInput
        {
            self.mark_active(client);
        }
    }

    /// Warns, then kicks, loaded non-admin players who have been AFK too
    /// long. Runs every second.
    pub(crate) fn check_afk(&self) {
        if !self.live().extra.enable_anti_afk {
            return;
        }
        let max_ms = self.live().extra.max_afk_time_minutes * 60_000;
        let now = self.uptime_ms();
        for client in self.connected_clients() {
            if client.is_admin() || !client.has_sent_first_update.load(Ordering::SeqCst) {
                continue;
            }
            let idle = now - client.last_active_ms.load(Ordering::SeqCst);
            if idle > max_ms {
                self.kick_for(&client, "being AFK");
            } else if max_ms - idle < AFK_WARNING_MS
                && !client.afk_warned.swap(true, Ordering::SeqCst)
            {
                self.send_chat_to(&client, "You will be kicked in 1 minute for being AFK.");
            }
        }
    }

    /// Counts pongs above MaxPing (about one a second) and kicks a player
    /// whose ping stays high for more than MaxPingSeconds.
    pub(crate) fn check_ping(&self, client: &Arc<Client>) {
        let max = self.live().extra.max_ping;
        if max <= 0 {
            return;
        }
        let ping = i64::from(client.ping());
        if ping <= max {
            client.high_ping_seconds.store(0, Ordering::SeqCst);
            return;
        }
        let seconds = client.high_ping_seconds.fetch_add(1, Ordering::SeqCst) + 1;
        if i64::from(seconds) > self.live().extra.max_ping_seconds {
            client.high_ping_seconds.store(0, Ordering::SeqCst);
            self.kick_for(client, &format!("high ping ({ping}ms)"));
        }
    }

    /// After the first update, gives the player PlayerChecksumTimeoutSeconds
    /// to answer the checksum request.
    pub(crate) fn arm_checksum_timeout(self: &Arc<Self>, client: &Arc<Client>) {
        let seconds = self.live().extra.player_checksum_timeout_seconds;
        if seconds <= 0 || client.checksum_status() != ChecksumStatus::Pending {
            return;
        }
        let server = Arc::clone(self);
        let client = Arc::clone(client);
        self.spawn_after(Duration::from_secs(seconds as u64), move || {
            if client.checksum_status() == ChecksumStatus::Pending
                && !client.disconnected.load(Ordering::SeqCst)
            {
                tracing::info!(
                    car = client.session_id(),
                    name = client.name(),
                    "checksum timeout"
                );
                let text = format!("{} did not send the requested checksums.", client.name());
                server.kick_with_message(&client, proto::KICK_CHECKSUM_FAILED, None, Some(&text));
            }
        });
    }
}
