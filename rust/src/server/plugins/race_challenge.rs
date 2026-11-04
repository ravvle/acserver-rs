//! RaceChallengePlugin: challenge the car in front by flashing your lights
//! three times, or anyone with `/race <player>`; `/accept` (or hazards, for
//! a flash challenge) starts a rolling race that ends when one car pulls
//! 750 m clear or nobody overtakes for a minute.

use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::{Plugin, Server, bearing, heading};
use crate::config::Config;
use crate::server::admin_commands::find_player;
use crate::server::client::Client;
use crate::server::entrycar::PositionUpdate;

const LIGHTS: u32 = 0x20;
const HAZARDS: u32 = 0x2000;
const HIGH_BEAMS_OFF: u32 = 0x4000;

/// One race between two slots.
struct Race {
    challenger: u8,
    challenged: u8,
    /// A /race challenge must line up side by side; a flash one rolls.
    line_up_required: bool,
    started: std::sync::atomic::AtomicBool,
}

/// One slot's flash counter and race.
#[derive(Default)]
struct Slot {
    flashes: u32,
    last_flash_ms: i64,
    last_challenge_ms: i64,
    race: Option<Arc<Race>>,
}

pub struct RaceChallenge {
    races: Arc<Races>,
}

/// Every slot's state; shared so races can run as tasks.
struct Races {
    slots: Mutex<Vec<Slot>>,
}

impl RaceChallenge {
    pub fn new(_cfg: &Config) -> RaceChallenge {
        let mut slots = Vec::with_capacity(256);
        slots.resize_with(256, Slot::default);
        RaceChallenge {
            races: Arc::new(Races {
                slots: Mutex::new(slots),
            }),
        }
    }
}

impl Races {
    fn lock(&self) -> parking_lot::MutexGuard<'_, Vec<Slot>> {
        self.slots.lock()
    }

    /// Challenges `target` on behalf of `challenger`; the reply for the
    /// challenger.
    fn challenge(
        self: &Arc<Self>,
        server: &Arc<Server>,
        challenger: &Arc<Client>,
        target: &Arc<Client>,
        line_up: bool,
    ) -> Option<String> {
        let (me, them) = (challenger.session_id(), target.session_id());
        let race = {
            let mut slots = self.lock();
            if let Some(race) = &slots[usize::from(me)].race {
                return Some(if race.started.load(Ordering::SeqCst) {
                    "You are currently in a race.".to_string()
                } else {
                    "You have a pending race request.".to_string()
                });
            }
            if me == them {
                return Some("You cannot challenge yourself to a race.".to_string());
            }
            if let Some(race) = &slots[usize::from(them)].race {
                return Some(if race.started.load(Ordering::SeqCst) {
                    "This car is currently in a race.".to_string()
                } else {
                    "This car has a pending race request.".to_string()
                });
            }
            let race = Arc::new(Race {
                challenger: me,
                challenged: them,
                line_up_required: line_up,
                started: false.into(),
            });
            slots[usize::from(me)].race = Some(Arc::clone(&race));
            slots[usize::from(them)].race = Some(Arc::clone(&race));
            race
        };
        let accept = if line_up {
            "Send /accept within 10 seconds to accept."
        } else {
            "Flash your hazard lights or send /accept within 10 seconds to accept."
        };
        server.send_chat_to(
            target,
            &format!(
                "{} has challenged you to a race. {accept}",
                challenger.name()
            ),
        );
        let plugin = Arc::clone(self);
        let s = Arc::clone(server);
        server.spawn_after(Duration::from_secs(10), move || {
            if race.started.load(Ordering::SeqCst) {
                return;
            }
            plugin.clear(&race);
            for id in [race.challenger, race.challenged] {
                if let Some(c) = s.client_in(id) {
                    s.send_chat_to(&c, "Race request has timed out.");
                }
            }
        });
        Some(format!("You have challenged {} to a race.", target.name()))
    }

    fn clear(&self, race: &Arc<Race>) {
        let mut slots = self.lock();
        for id in [race.challenger, race.challenged] {
            let slot = &mut slots[usize::from(id)];
            if slot.race.as_ref().is_some_and(|r| Arc::ptr_eq(r, race)) {
                slot.race = None;
            }
        }
    }

    fn start(self: &Arc<Self>, server: &Arc<Server>, race: Arc<Race>) {
        if race.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let plugin = Arc::clone(self);
        let s = Arc::clone(server);
        server.spawn_task(async move {
            run_race(&s, &race).await;
            plugin.clear(&race);
        });
    }

    /// The car in front of `me`, within 30 m.
    fn car_ahead(server: &Server, me: &Client) -> Option<Arc<Client>> {
        let mine = server.entry_cars[usize::from(me.session_id())].last_position()?;
        let mut found = None;
        for car in &server.entry_cars {
            let Some(client) = car.client() else { continue };
            if client.session_id() == me.session_id() {
                continue;
            }
            let Some(theirs) = car.last_position() else {
                continue;
            };
            let angle = bearing(mine.position, heading(&theirs), theirs.position);
            if angle > 110.0
                && angle < 250.0
                && theirs.position.distance_squared(mine.position) < 30.0 * 30.0
            {
                found = Some(client);
            }
        }
        found
    }
}

/// A slot's position while its original driver is still in it.
fn position_of(server: &Server, id: u8) -> Option<PositionUpdate> {
    server.client_in(id)?;
    server.entry_cars[usize::from(id)].last_position()
}

fn tell(server: &Server, race: &Race, message: &str) {
    for id in [race.challenger, race.challenged] {
        if let Some(c) = server.client_in(id) {
            server.send_chat_to(&c, message);
        }
    }
}

/// Sends `message` to both, the higher ping first, so both see it at once.
async fn tell_timed(server: &Server, race: &Race, message: &str) {
    let (Some(a), Some(b)) = (
        server.client_in(race.challenger),
        server.client_in(race.challenged),
    ) else {
        return;
    };
    let (slow, fast) = if a.ping() > b.ping() { (a, b) } else { (b, a) };
    server.send_chat_to(&slow, message);
    tokio::time::sleep(Duration::from_millis(u64::from(slow.ping() - fast.ping()))).await;
    server.send_chat_to(&fast, message);
}

/// Side by side within 10 m, both facing the same way (a /race challenge),
/// or within 30 m (a flash challenge).
fn lined_up(server: &Server, race: &Race) -> bool {
    let (Some(a), Some(b)) = (
        position_of(server, race.challenger),
        position_of(server, race.challenged),
    ) else {
        return false;
    };
    let d2 = a.position.distance_squared(b.position);
    if !race.line_up_required {
        return d2 <= 900.0;
    }
    if d2 > 100.0 {
        return false;
    }
    let beside = |angle: f32| (75.0..=105.0).contains(&angle) || (255.0..=285.0).contains(&angle);
    if !beside(bearing(b.position, heading(&a), a.position))
        || !beside(bearing(a.position, heading(&b), b.position))
    {
        return false;
    }
    let diff = (heading(&a) - heading(&b) + 180.0 + 360.0) % 360.0 - 180.0;
    diff.abs() <= 5.0
}

async fn run_race(server: &Server, race: &Race) {
    let name = |id: u8| {
        server
            .client_in(id)
            .map(|c| c.name().to_string())
            .unwrap_or_default()
    };
    let (challenger_name, challenged_name) = (name(race.challenger), name(race.challenged));
    if server.client_in(race.challenger).is_none() || server.client_in(race.challenged).is_none() {
        tell(server, race, "Opponent has disconnected.");
        return;
    }
    if race.line_up_required && !lined_up(server, race) {
        tell(server, race, "You have 15 seconds to line up.");
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while !lined_up(server, race) {
            if std::time::Instant::now() > deadline {
                tell(
                    server,
                    race,
                    "You did not line up in time. The race has been cancelled.",
                );
                return;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }
    for signal in ["Ready...", "Set...", "Go!"] {
        if !lined_up(server, race) {
            tell(
                server,
                race,
                "You went out of line. The race has been cancelled.",
            );
            return;
        }
        tell_timed(server, race, signal).await;
        if signal != "Go!" {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    let mut leader = race.challenger;
    let mut follower = race.challenged;
    let mut last_overtake = server.server_time_millis();
    let mut last_leader_pos = position_of(server, leader)
        .map(|p| p.position)
        .unwrap_or_default();
    loop {
        let (Some(a), Some(b)) = (
            position_of(server, race.challenger),
            position_of(server, race.challenged),
        ) else {
            // Whoever left loses.
            if position_of(server, race.challenger).is_none() {
                leader = race.challenged;
            } else {
                leader = race.challenger;
            }
            break;
        };
        // An overtake: the car behind is faster and within 50 m.
        let speed = |p: &PositionUpdate| p.velocity.length_squared().max(0.077_160_62);
        let d2 = a.position.distance_squared(b.position);
        let old = leader;
        let a_behind = bearing(a.position, heading(&a), b.position);
        let b_behind = bearing(b.position, heading(&b), a.position);
        if a_behind > 90.0
            && a_behind < 275.0
            && leader != race.challenger
            && speed(&a) > speed(&b)
            && d2 < 2500.0
        {
            (leader, follower) = (race.challenger, race.challenged);
        } else if b_behind > 90.0
            && b_behind < 275.0
            && leader != race.challenged
            && speed(&b) > speed(&a)
            && d2 < 2500.0
        {
            (leader, follower) = (race.challenged, race.challenger);
        }
        let leader_pos = if leader == race.challenger {
            a.position
        } else {
            b.position
        };
        if old != leader {
            tell(
                server,
                race,
                &format!("{} has overtaken {}", name(leader), name(old)),
            );
            last_overtake = server.server_time_millis();
            last_leader_pos = leader_pos;
        }
        // A leader that jumps 200 m in a check teleported: the follower wins.
        if last_leader_pos.distance_squared(leader_pos) > 40_000.0 {
            std::mem::swap(&mut leader, &mut follower);
            break;
        }
        last_leader_pos = leader_pos;
        if d2 > 562_500.0 || server.server_time_millis() - last_overtake > 60_000 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let (winner, loser) = if leader == race.challenger {
        (challenger_name, challenged_name)
    } else {
        (challenged_name, challenger_name)
    };
    server.broadcast_server_chat(&format!("{winner} just beat {loser} in a race."));
    tracing::info!(winner, loser, "race won");
}

impl Plugin for RaceChallenge {
    fn name(&self) -> &'static str {
        "RaceChallengePlugin"
    }

    fn on_position(
        &self,
        server: &Arc<Server>,
        client: &Arc<Client>,
        previous: Option<PositionUpdate>,
        p: &PositionUpdate,
    ) {
        let Some(old) = previous else { return };
        let now = server.server_time_millis();
        let id = usize::from(client.session_id());
        let rising = |flag: u32| old.status_flags & flag == 0 && p.status_flags & flag != 0;
        let races = &self.races;
        let (accept, challenge) = {
            let mut slots = races.lock();
            let slot = &mut slots[id];
            if rising(LIGHTS) || rising(HIGH_BEAMS_OFF) {
                slot.last_flash_ms = now;
                slot.flashes += 1;
            }
            let accept = slot.race.as_ref().filter(|r| {
                rising(HAZARDS)
                    && usize::from(r.challenged) == id
                    && !r.started.load(Ordering::SeqCst)
                    && !r.line_up_required
            });
            let accept = accept.cloned();
            if now - slot.last_flash_ms > 3000 && slot.flashes > 0 {
                slot.flashes = 0;
            }
            let mut challenge = false;
            if slot.flashes == 3 {
                slot.flashes = 0;
                if now - slot.last_challenge_ms > 20_000 {
                    slot.last_challenge_ms = now;
                    challenge = true;
                }
            }
            (accept, challenge)
        };
        if let Some(race) = accept {
            races.start(server, race);
        }
        if challenge
            && let Some(target) = Races::car_ahead(server, client)
            && let Some(reply) = races.challenge(server, client, &target, false)
        {
            server.send_chat_to(client, &reply);
        }
    }

    fn on_command(
        &self,
        server: &Arc<Server>,
        client: &Arc<Client>,
        cmd: &str,
        args: &[&str],
    ) -> Option<String> {
        let races = &self.races;
        match cmd {
            "race" => {
                let target = find_player(&server.connected_clients(), &args.join(" "));
                let Some(target) = target else {
                    return Some("Player not found.".to_string());
                };
                races.challenge(server, client, &target, true)
            }
            "accept" => {
                let race = races.lock()[usize::from(client.session_id())].race.clone();
                match race {
                    None => Some("You do not have a pending race request.".to_string()),
                    Some(r) if r.started.load(Ordering::SeqCst) => {
                        Some("This race has already started.".to_string())
                    }
                    Some(r) => {
                        races.start(server, r);
                        Some(String::new())
                    }
                }
            }
            _ => None,
        }
    }

    fn on_disconnect(&self, _server: &Server, client: &Client) {
        let mut slots = self.races.lock();
        let slot = &mut slots[usize::from(client.session_id())];
        slot.flashes = 0;
        slot.race = None;
    }
}
