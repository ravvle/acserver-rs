//! `/` chat commands and `execute_admin_command` (shared with RCON and ACSP).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::Server;
use super::client::Client;
use super::entrycar::EntryCar;
use super::handshake::secret_equal;
use crate::config::{BlacklistMode, parse_int};
use crate::proto;

/// Vanilla's /help reply, verbatim.
const HELP_TEXT: &str = "/admin /next_session /restart_session /kick /client_list /kick_id /ban_id /ballast /restrictor";

/// How many wrong /admin passwords one connection may try.
const MAX_ADMIN_ATTEMPTS: u32 = 3;

/// The session id in the first argument; callers wrap it to a byte.
fn parse_id(args: &[&str]) -> Option<i64> {
    parse_int(args.first()?)
}

impl Server {
    /// Runs a "/" chat command. Commands, arguments and replies match
    /// acServer v1.15 (docs/02 §2.1). RCON and the plugin use
    /// `execute_admin_command`.
    pub(crate) fn handle_command(self: &Arc<Self>, client: &Arc<Client>, msg: &str) {
        let fields: Vec<&str> = msg
            .strip_prefix('/')
            .unwrap_or(msg)
            .split_whitespace()
            .collect();
        let Some((&cmd, args)) = fields.split_first() else {
            return;
        };
        // Case-sensitive, as in vanilla: "/HELP" is not recognized.
        let reply = |text: &str| self.send_chat_to(client, text);
        let unknown = || reply(&format!("Command:{msg} not recognized"));

        match cmd {
            "help" => {
                reply(HELP_TEXT);
                return;
            }
            "admin" => {
                if client.admin_failures.load(Ordering::SeqCst) >= MAX_ADMIN_ATTEMPTS {
                    reply("Command refused");
                    return;
                }
                let admin_password = &self.cfg.server.admin_password;
                let correct = match args.first() {
                    Some(given) => {
                        !admin_password.is_empty() && secret_equal(given, admin_password)
                    }
                    None => false,
                };
                if !correct {
                    // Counted per connection, so only reconnecting (which is
                    // logged) gets past the cap.
                    let attempt = client.admin_failures.fetch_add(1, Ordering::SeqCst) + 1;
                    let id = client.identity();
                    tracing::warn!(car = id.session_id, name = %id.name, guid = %id.guid, attempt, of = MAX_ADMIN_ATTEMPTS, "failed /admin attempt");
                    reply("Command refused");
                    return;
                }
                client.set_admin(true);
                let id = client.identity();
                tracing::info!(car = id.session_id, name = %id.name, guid = %id.guid, "admin authenticated");
                reply("You are now Admin for this server");
                return;
            }
            _ => {}
        }

        // EnableAdminCommands' and the plugins' commands for every player.
        if let Some(text) = self
            .player_command(client, cmd)
            .or_else(|| self.plugin_command(client, cmd, args))
        {
            for line in text.lines() {
                reply(line);
            }
            return;
        }

        // Everything below is admin-only, or granted to a user group.
        if !client.is_admin() && !self.group_permits(cmd, client.identity().steam_id()) {
            reply("You are not admin on this server");
            return;
        }

        match cmd {
            "next_session" | "ksns" => {
                if !self.next_session() {
                    reply("Can't skip session. New player connection is in process");
                    return;
                }
                reply("OK. Moving to next session");
            }
            "restart_session" | "ksrs" => {
                if !self.restart_session() {
                    reply("Can't restart session. New player connection is in process");
                    return;
                }
                reply("OK. Restarting session");
            }
            "kick" => {
                // /kick takes a driver name; /kick_id takes a session id.
                if args.is_empty() {
                    unknown();
                    return;
                }
                let name = args.join(" ");
                let Some(car) = self.car_by_driver_name(&name) else {
                    reply(&format!("ERROR: Cannot find driver named {name}"));
                    return;
                };
                self.admin_kick(Some(client), "", &car, false);
            }
            "kick_id" | "ban_id" => {
                let Some(id) = parse_id(args) else {
                    unknown();
                    return;
                };
                let car = self.car_by_id(id as u8);
                let Some(car) = car.filter(|car| car.occupied()) else {
                    // Vanilla "kicks" an empty slot; this server refuses
                    // (deviation, probe CmdKickIdEmptySlot).
                    reply(&format!("ERROR: Cannot find driver id {id}"));
                    return;
                };
                self.admin_kick(Some(client), "", &car, cmd == "ban_id");
            }
            "ballast" | "restrictor" => {
                reply(&self.admin_set_bop(args, cmd == "restrictor"));
            }
            "client_list" => {
                // Vanilla advertises this but answers "not recognized".
                // Implemented here (deviation, probe CmdClientList).
                for line in self.client_list() {
                    reply(&line);
                }
            }
            _ => match self.extension_command(Some(client), cmd, args) {
                Some(text) => {
                    for line in text.lines() {
                        reply(line);
                    }
                }
                None => unknown(),
            },
        }
    }

    /// One "<id>: <name> (<guid>) <model>" line per driver. Separate lines
    /// keep each chat message inside the wstring8 limit.
    pub(crate) fn client_list(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for car in &self.entry_cars {
            let Some(client) = car.client() else { continue };
            let id = client.identity();
            lines.push(format!(
                "{}: {} ({}) {}",
                car.session_id,
                id.name,
                id.guid,
                car.model()
            ));
        }
        if lines.is_empty() {
            return vec!["No drivers connected".to_string()];
        }
        lines
    }

    /// An occupied slot by its driver's name (case-insensitive).
    fn car_by_driver_name(&self, name: &str) -> Option<Arc<EntryCar>> {
        for car in &self.entry_cars {
            if let Some(client) = car.client()
                && client.name().to_lowercase() == name.to_lowercase()
            {
                return Some(Arc::clone(car));
            }
        }
        None
    }

    /// Runs an admin command for RCON and the plugin API. With no vanilla set
    /// to match, it takes a superset of chat: kick by session id, plus ban and
    /// say. `line` is the command and its arguments, without the slash;
    /// `source` ("rcon" or "plugin") labels the log.
    pub fn execute_admin_command(self: &Arc<Self>, source: &str, line: &str) -> String {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let Some((cmd, args)) = fields.split_first() else {
            return String::new();
        };
        tracing::info!(source, cmd = line.trim(), "admin command");
        let cmd = cmd.to_lowercase();
        let (cmd, args) = (cmd.as_str(), args);
        match cmd {
            "kick" | "kick_id" | "ban" | "ban_id" => {
                let Some(id) = parse_id(args) else {
                    return format!("Usage: /{cmd} <sessionId>");
                };
                let Some(car) = self.car_by_id(id as u8) else {
                    return "No player in that slot.".to_string();
                };
                let Some(client) = car.client() else {
                    return "No player in that slot.".to_string();
                };
                let name = client.name().to_string();
                let ban = cmd == "ban" || cmd == "ban_id";
                self.admin_kick(None, source, &car, ban);
                if ban {
                    format!("Banned {name}.")
                } else {
                    format!("Kicked {name}.")
                }
            }
            "next_session" => {
                let server = Arc::clone(self);
                self.spawn_task(async move {
                    server.next_session();
                });
                "Advancing to next session.".to_string()
            }
            "restart_session" => {
                let server = Arc::clone(self);
                self.spawn_task(async move {
                    server.restart_session();
                });
                "Restarting session.".to_string()
            }
            "ballast" => self.admin_set_bop(args, false),
            "restrictor" => self.admin_set_bop(args, true),
            "client_list" => self.client_list().join("\n"),
            "say" => {
                self.broadcast_server_chat(&args.join(" "));
                String::new()
            }
            _ => self
                .extension_command(None, cmd, args)
                .unwrap_or_else(|| "Unknown admin command.".to_string()),
        }
    }

    /// Kicks a slot's client as vanilla's KickUser does (see `admin_kick`).
    /// Serves ACSP 0xCE and a client-sent 0x3B; `source` labels the kick log.
    pub fn kick_car(&self, source: &str, id: u8) -> bool {
        let Some(car) = self.car_by_id(id) else {
            return false;
        };
        if !car.occupied() {
            return false;
        }
        self.admin_kick(None, source, &car, false);
        true
    }

    /// Handles /ballast and /restrictor.
    pub(crate) fn admin_set_bop(&self, args: &[&str], restrictor: bool) -> String {
        let kind = if restrictor { "restrictor" } else { "ballast" };
        // Exactly two numbers; any other shape is the same syntax error.
        let syntax = || {
            let unit = if restrictor { "0-100" } else { "kg" };
            format!("SYNTAX ERROR: Use '{kind} [driver numeric id] [{unit}]'")
        };
        if args.len() != 2 {
            return syntax();
        }
        let Some(id) = parse_int(args[0]) else {
            return syntax();
        };
        let Ok(value) = args[1].parse::<f32>() else {
            return syntax();
        };
        // Out-of-range values are refused, not clamped, before the car lookup.
        if value < 0.0 {
            if restrictor {
                return "ERROR: Restrictor cannot be negative'".to_string();
            }
            return "ERROR: Ballasts cannot be negative'".to_string();
        }
        if restrictor && value > 100.0 {
            return "ERROR: Restrictors cannot be higher than 100'".to_string();
        }
        let max_ballast = f32::from(self.cfg.server.max_ballast_kg);
        if !restrictor && value > max_ballast {
            return format!("ERROR: Ballasts cannot be higher than {max_ballast:.6}Kg'");
        }
        let Some(car) = self.car_by_id(id as u8) else {
            return format!("ERROR: Cannot find car to add {kind}");
        };
        // An empty slot has its own message (probe CmdBallastBadCar).
        let Some(client) = car.client() else {
            return format!("ERROR: Cannot add {kind} to disconnected car");
        };
        car.update(|s| {
            if restrictor {
                s.restrictor = value;
            } else {
                s.ballast = value;
            }
        });
        self.broadcast_bop();

        let ident = client.identity();
        if restrictor {
            return format!(
                "OK, Restrictor set to {value:.1} for {} [{}]",
                ident.name, ident.team
            );
        }
        format!(
            "OK, Ballast set to {value:.1} Kg for {} [{}]",
            ident.name, ident.team
        )
    }

    /// Sends vanilla's kick or ban sequence: a chat announcement, the 0x68,
    /// the disconnect, and a confirmation to the admin (skipped when `by` is
    /// `None`: RCON, plugin, vote or a client's 0x3B). With no admin client,
    /// `source` names the origin in the log.
    ///
    /// A kick is vanilla's KickUser: the reason byte is BLACKLIST_MODE itself
    /// and the chat always says "kicked"; mode 1 also blacklists the guid in
    /// memory, mode 2 in blacklist.txt as well. A ban is BanUser: reason 2,
    /// "banned", and always persisted.
    pub(crate) fn admin_kick(
        &self,
        by: Option<&Arc<Client>>,
        source: &str,
        car: &EntryCar,
        ban: bool,
    ) {
        let Some(client) = car.client() else { return };
        let ident = client.identity().clone();
        let (verb, confirm, reason, blacklist, persist) = if ban {
            let confirm = format!("OK. {} BANNED", ident.name);
            ("banned", confirm, proto::KICK_VOTE_BLACKLISTED, true, true)
        } else {
            let mode = self.cfg.server.blacklist_mode;
            let confirm = format!("OK. Kicking {}", ident.name);
            let blacklist = mode != BlacklistMode::Kick;
            (
                "kicked",
                confirm,
                mode as u8,
                blacklist,
                mode == BlacklistMode::Ban,
            )
        };
        if persist {
            if let Err(err) = self.persist_blacklist(ident.steam_id()) {
                tracing::error!(guid = %ident.guid, %err, "persist blacklist");
                self.add_to_blacklist(ident.steam_id());
            }
            if let Some(group) = self.blacklist_group()
                && let Err(err) = group.add(ident.steam_id())
            {
                tracing::error!(guid = %ident.guid, %err, "add to the blacklist group");
            }
        } else if blacklist {
            self.add_to_blacklist(ident.steam_id());
        }

        let by_name = match by {
            Some(admin) => admin.name().to_string(),
            None => source.to_string(),
        };
        tracing::info!(car = car.session_id, name = %ident.name, guid = %ident.guid, by = %by_name, reason, "admin {verb}");

        self.broadcast_server_chat(&format!("{} {verb} from the server", ident.name));
        self.broadcast_kick(car.session_id, reason);
        self.disconnect(&client, verb);
        if let Some(admin) = by {
            self.send_chat_to(admin, &confirm);
        }
    }
}
