//! The chat and RCON commands of the AssettoServer features. Those of
//! WeatherFX and AI traffic exist while their feature is on; the rest
//! (AssettoServer's AdminModule and GeneralModule) with
//! `EnableAdminCommands`. Vanilla's own commands keep their vanilla replies
//! in commands.rs.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::Server;
use super::client::Client;
use super::weatherfx;
use crate::config::parse_steam_id;
use crate::proto;

/// A player by session id, else by name: exact, then ignoring case, then
/// the shortest name containing it, then the same ignoring case (as
/// AssettoServer's ACClientTypeParser). A leading '@' is ignored.
pub fn find_player(players: &[Arc<Client>], arg: &str) -> Option<Arc<Client>> {
    if let Ok(id) = arg.parse::<u8>()
        && let Some(c) = players.iter().find(|c| c.session_id() == id)
    {
        return Some(Arc::clone(c));
    }
    let wanted = arg.strip_prefix('@').unwrap_or(arg);
    let lower = wanted.to_lowercase();
    let shortest = |matches: &dyn Fn(&str) -> bool| {
        players
            .iter()
            .filter(|c| matches(c.name()))
            .min_by_key(|c| c.name().len())
            .cloned()
    };
    shortest(&|n| n == wanted)
        .or_else(|| shortest(&|n| n.to_lowercase() == lower))
        .or_else(|| shortest(&|n| n.contains(wanted)))
        .or_else(|| shortest(&|n| n.to_lowercase().contains(&lower)))
}

/// The command's text after its first `skip` arguments.
fn rest(args: &[&str], skip: usize) -> String {
    args.get(skip..).unwrap_or_default().join(" ")
}

impl Server {
    /// /ping and /time, for every player, with EnableAdminCommands.
    pub(crate) fn player_command(&self, client: &Client, cmd: &str) -> Option<String> {
        if !self.cfg.extra.enable_admin_commands {
            return None;
        }
        match cmd {
            "ping" => Some(format!("Pong! {}ms.", client.ping())),
            "time" => Some(self.time_reply()),
            _ => None,
        }
    }

    /// The /time reply: the in-game time of day.
    fn time_reply(&self) -> String {
        let time = match &self.weatherfx {
            Some(fx) => fx.local_time(),
            None => {
                let sun = f64::from(self.weather.snapshot().sun_angle);
                let secs = weatherfx::seconds_from_sun_angle(sun).rem_euclid(86400.0) as u32;
                format!("{}:{:02}", secs / 3600, secs / 60 % 60)
            }
        };
        format!("It is currently {time}.")
    }

    /// An admin command of the AssettoServer features, from chat (`by`) or
    /// RCON and ACSP (`None`). `None` when `cmd` is not one, or its feature
    /// is off. Replies may span several lines.
    pub(crate) fn extension_command(
        &self,
        by: Option<&Arc<Client>>,
        cmd: &str,
        args: &[&str],
    ) -> Option<String> {
        if let Some(reply) = self.weather_command(cmd, args) {
            return Some(reply);
        }
        // /time needs no player, so RCON and ACSP get it too; /ping does.
        if cmd == "time" && by.is_none() && self.cfg.extra.enable_admin_commands {
            return Some(self.time_reply());
        }
        if cmd == "set"
            && let Some(reply) = self.set_command(args)
        {
            return Some(reply);
        }
        if cmd == "setaioverbooking" && self.traffic.is_some() {
            let Some(count) = args.first().and_then(|a| a.parse::<usize>().ok()) else {
                return Some("Usage: /setaioverbooking <count>".to_string());
            };
            self.set_ai_overbooking(count);
            return Some(format!("AI overbooking set to {count}"));
        }
        if !self.cfg.extra.enable_admin_commands {
            return None;
        }
        let players = self.connected_clients();
        let player = |i: usize| args.get(i).and_then(|a| find_player(&players, a));
        let no_player = || Some("Player not found.".to_string());
        match cmd {
            "ban" => {
                let Some(target) = player(0) else {
                    return no_player();
                };
                Some(self.ban_with_reason(by, &target, &rest(args, 1)))
            }
            "unban" => {
                let Some(guid) = args.first().and_then(|a| parse_steam_id(a)) else {
                    return Some("Usage: /unban <steam id>".to_string());
                };
                if self.unban(guid) {
                    tracing::info!(guid, "unbanned");
                    Some(format!("SteamID {guid} was unbanned."))
                } else {
                    Some(format!("SteamID {guid} is not banned."))
                }
            }
            "whitelist" => {
                let Some(guid) = args.first().and_then(|a| parse_steam_id(a)) else {
                    return Some("Usage: /whitelist <steam id>".to_string());
                };
                let Some(group) = self.whitelist_group() else {
                    return Some("No WhitelistUserGroup is configured.".to_string());
                };
                match group.add(guid) {
                    Ok(_) => Some(format!("SteamID {guid} was added to the whitelist")),
                    Err(err) => Some(format!("Could not update the whitelist: {err}")),
                }
            }
            "pit" => {
                let Some(target) = player(0) else {
                    return no_player();
                };
                // A fresh 0x4A puts the car back in the pits.
                self.send_current_session(&target);
                self.send_chat_to(&target, "You have been teleported to the pits.");
                Some(format!(
                    "{} has been teleported to the pits.",
                    target.name()
                ))
            }
            "whois" => {
                let Some(target) = player(0) else {
                    return no_player();
                };
                let id = target.identity();
                let mut lines = vec![
                    format!("IP: {}", target.peer.ip()),
                    format!("Profile: https://steamcommunity.com/profiles/{}", id.guid),
                    format!("Ping: {}ms", target.ping()),
                ];
                if let Some(pos) = self.entry_cars[usize::from(id.session_id)].last_position() {
                    let p = pos.position;
                    let speed = pos.velocity.length_squared().sqrt() * 3.6;
                    lines.push(format!("Position: <{:.1}, {:.1}, {:.1}>", p.x, p.y, p.z));
                    lines.push(format!("Velocity: {}kmh", speed as i32));
                }
                Some(lines.join("\n"))
            }
            "distance" => {
                let (Some(me), Some(target)) = (by, player(0)) else {
                    return no_player();
                };
                let at = |c: &Client| self.entry_cars[usize::from(c.session_id())].last_position();
                match (at(me), at(&target)) {
                    (Some(a), Some(b)) => Some(format!(
                        "{:.1}",
                        a.position.distance_squared(b.position).sqrt()
                    )),
                    _ => Some("No position yet.".to_string()),
                }
            }
            "forcelights" => {
                let on = match args.first() {
                    Some(&"on") => true,
                    Some(&"off") => false,
                    _ => return Some("Usage: /forcelights <on|off> <player>".to_string()),
                };
                let Some(target) = args
                    .get(1..)
                    .and_then(|a| find_player(&players, &a.join(" ")))
                else {
                    return no_player();
                };
                self.entry_cars[usize::from(target.session_id())]
                    .force_lights
                    .store(on, Ordering::SeqCst);
                let will = if on { "will" } else { "will not" };
                Some(format!("{}'s lights {will} be forced on.", target.name()))
            }
            "restrict" => {
                let usage =
                    || Some("Usage: /restrict <player> <restrictor> <ballast kg>".to_string());
                let Some(target) = player(0) else {
                    return no_player();
                };
                let (Some(restrictor), Some(ballast)) = (args.get(1), args.get(2)) else {
                    return usage();
                };
                let id = target.session_id().to_string();
                let a = self.admin_set_bop(&[id.as_str(), restrictor], true);
                let b = self.admin_set_bop(&[id.as_str(), ballast], false);
                Some(format!("{a}\n{b}"))
            }
            "say" => {
                self.broadcast_server_chat(&format!("CONSOLE: {}", args.join(" ")));
                Some(String::new())
            }
            "setweather" => {
                let Some(index) = args.first().and_then(|a| a.parse::<usize>().ok()) else {
                    return Some("Usage: /setweather <weather id>".to_string());
                };
                if index >= self.cfg.weathers.len() {
                    return Some("There is no weather configuration with this id.".to_string());
                }
                self.set_weather_config(index);
                Some("Weather configuration has been set.".to_string())
            }
            "setgrip" => {
                let Some(grip) = args.first().and_then(|a| a.parse::<f32>().ok()) else {
                    return Some("Usage: /setgrip <grip, 0-1>".to_string());
                };
                self.weather.set_grip(grip.clamp(0.0, 1.0));
                Some(format!("Track grip set to {:.2}.", grip.clamp(0.0, 1.0)))
            }
            _ => None,
        }
    }

    /// The WeatherFX commands, while WeatherFX runs.
    fn weather_command(&self, cmd: &str, args: &[&str]) -> Option<String> {
        let fx = self.weatherfx.as_ref()?;
        match cmd {
            "cspweather" => {
                let names: Vec<&str> = weatherfx::WEATHER_TYPES.iter().map(|t| t.name).collect();
                Some(format!("Available weathers: {}", names.join(", ")))
            }
            "setcspweather" => {
                let usage = "Usage: /setcspweather <type name or id> [transition seconds]";
                let Some(kind) = args.first().and_then(|a| weatherfx::find_weather_type(a)) else {
                    return Some(format!("{usage}. /cspweather lists the types."));
                };
                let seconds = match args.get(1) {
                    Some(s) => match s.parse::<u32>() {
                        Ok(n) => n,
                        Err(_) => return Some(usage.to_string()),
                    },
                    None => 30,
                };
                fx.set_upcoming(kind, seconds);
                tracing::info!(weather = kind.name, seconds, "csp weather set");
                Some(format!(
                    "Weather changing to {} over {seconds} s",
                    kind.name
                ))
            }
            "setrain" => {
                let values: Vec<f32> = args.iter().filter_map(|a| a.parse().ok()).collect();
                let [intensity, wetness, water] = values[..] else {
                    return Some(
                        "Usage: /setrain <intensity> <wetness> <water>, each 0-1".to_string(),
                    );
                };
                fx.set_rain(intensity, wetness, water);
                Some("Rain has been set.".to_string())
            }
            "settime" => {
                let usage = "Invalid time format. Usage: /settime 15:31";
                let Some((h, m)) = args.first().and_then(|a| a.split_once(':')) else {
                    return Some(usage.to_string());
                };
                let (Ok(h), Ok(m)) = (h.parse::<u32>(), m.parse::<u32>()) else {
                    return Some(usage.to_string());
                };
                if h > 23 || m > 59 {
                    return Some(usage.to_string());
                }
                fx.set_time_of_day(f64::from(h * 3600 + m * 60));
                self.broadcast_server_chat("Time has been set.");
                Some(format!("Time set to {}", fx.local_time()))
            }
            _ => None,
        }
    }

    /// Applies [WEATHER_<index>] now, to WeatherFX or as a 0x78.
    fn set_weather_config(&self, index: usize) {
        self.weather.roll(&self.cfg, index);
        let wx = self.weather.snapshot();
        match &self.weatherfx {
            Some(fx) => {
                let kind = weatherfx::graphics_weather_type(&wx.graphics)
                    .unwrap_or(weatherfx::WEATHER_TYPES[15]);
                fx.set_weather(
                    kind,
                    wx.ambient,
                    wx.road,
                    f32::from(wx.wind_speed),
                    f32::from(wx.wind_direction),
                );
                for client in self.broadcast_clients() {
                    self.send_weatherfx(&client);
                }
            }
            None => self.broadcast_weather(),
        }
        tracing::info!(index, graphics = %wx.graphics, "weather set");
    }

    /// AssettoServer's /ban: refuses self and admins, tells everyone why,
    /// shows the reason on the banned player's kick screen (CSP), and adds
    /// them to blacklist.txt and the blacklist group.
    pub(crate) fn ban_with_reason(
        &self,
        by: Option<&Arc<Client>>,
        target: &Arc<Client>,
        reason: &str,
    ) -> String {
        if by.is_some_and(|b| Arc::ptr_eq(b, target)) {
            return "You cannot ban yourself.".to_string();
        }
        if target.is_admin() {
            return "You cannot ban an administrator.".to_string();
        }
        let id = target.identity().clone();
        let (to_player, to_all) = if reason.is_empty() {
            (
                "You have been banned from the server".to_string(),
                format!("{} has been banned from the server.", id.name),
            )
        } else {
            (
                format!("You have been banned for {reason}"),
                format!("{} has been banned from the server for {reason}.", id.name),
            )
        };
        if let Err(err) = self.persist_blacklist(id.steam_id()) {
            tracing::error!(guid = %id.guid, %err, "persist blacklist");
            self.add_to_blacklist(id.steam_id());
        }
        if let Some(group) = self.blacklist_group()
            && let Err(err) = group.add(id.steam_id())
        {
            tracing::error!(guid = %id.guid, %err, "add to the blacklist group");
        }
        let by_name = by.map_or("rcon".to_string(), |b| b.name().to_string());
        tracing::info!(car = id.session_id, name = %id.name, guid = %id.guid, by = %by_name, reason, "admin banned");
        self.kick_with_message(
            target,
            proto::KICK_VOTE_BLACKLISTED,
            Some(&to_player),
            Some(&to_all),
        );
        format!(
            "Steam profile of {}: https://steamcommunity.com/profiles/{}",
            id.name, id.guid
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rest_joins_the_tail() {
        assert_eq!(rest(&["7", "too", "fast"], 1), "too fast");
        assert_eq!(rest(&["7"], 1), "");
        assert_eq!(rest(&[], 1), "");
    }
}
