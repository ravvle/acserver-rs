//! Session and kick votes (docs/02 0x64–0x67).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::client::Client;
use super::{Server, signed_millis};
use crate::proto;
use crate::wire::{Reader, Writer};

/// The minimum gap between votes one driver opens.
const VOTE_START_COOLDOWN: Duration = Duration::from_secs(30);

/// Which vote is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoteKind {
    NextSession,
    RestartSession,
    KickUser,
}

impl VoteKind {
    fn from_packet(id: u8) -> Option<VoteKind> {
        match id {
            proto::VOTE_NEXT_SESSION => Some(VoteKind::NextSession),
            proto::VOTE_RESTART_SESSION => Some(VoteKind::RestartSession),
            proto::VOTE_KICK_USER => Some(VoteKind::KickUser),
            _ => None,
        }
    }

    fn packet_id(self) -> u8 {
        match self {
            VoteKind::NextSession => proto::VOTE_NEXT_SESSION,
            VoteKind::RestartSession => proto::VOTE_RESTART_SESSION,
            VoteKind::KickUser => proto::VOTE_KICK_USER,
        }
    }

    /// A label for logs.
    fn name(self) -> &'static str {
        match self {
            VoteKind::NextSession => "next_session",
            VoteKind::RestartSession => "restart_session",
            VoteKind::KickUser => "kick_user",
        }
    }
}

/// A running poll.
#[derive(Debug, Clone)]
struct Vote {
    kind: VoteKind,
    target: u8,
    ends_at: Instant,
    /// Session id to vote.
    voters: HashMap<u8, bool>,
    /// The last driver to vote yes (see `broadcast_vote_state`).
    last_voter: u8,
}

impl Vote {
    fn yes_count(&self) -> usize {
        self.voters.values().filter(|&&yes| yes).count()
    }
}

/// The single active vote, and when each driver last opened one.
#[derive(Debug, Default)]
pub struct Votes {
    active: Option<Vote>,
    /// When each session id last opened a vote.
    last_start: HashMap<u8, Instant>,
}

impl Votes {
    /// Drops the running vote silently, as a session change does in vanilla
    /// (no 0x67; probe VoteAcrossSessionChange).
    pub fn discard(&mut self) {
        self.active = None;
    }
}

impl Server {
    /// Processes 0x64/0x65/0x66. Rules: docs/02 "Vote state".
    pub(crate) fn handle_vote(&self, client: &Client, id: u8, r: &mut Reader) {
        let Some(kind) = VoteKind::from_packet(id) else {
            return;
        };
        let target = if kind == VoteKind::KickUser {
            r.u8()
        } else {
            0
        };
        let yes = r.bool();

        let enabled = match kind {
            VoteKind::NextSession | VoteKind::RestartSession => {
                self.live().extra.enable_session_voting
            }
            VoteKind::KickUser => self.live().extra.enable_kick_voting,
        };
        if !enabled {
            return;
        }

        let mut votes = self.votes.lock();
        let now = Instant::now();
        let voter = client.session_id();
        if let Some(active) = &votes.active
            && active.kind != kind
        {
            // One vote at a time; another kind is dropped (probe VoteCompetingKind).
            return;
        }
        if votes.active.is_none() {
            // Deviation: no kick vote against an empty slot or yourself.
            // Checked only on opening, since later 0x66s ignore their target.
            if kind == VoteKind::KickUser {
                let target_occupied = self.car_by_id(target).is_some_and(|car| car.occupied());
                if !target_occupied || target == voter {
                    return;
                }
            }
            // Opening a vote broadcasts to everyone, so it is rate-limited;
            // voting in a running poll is not.
            if let Some(last) = votes.last_start.get(&voter)
                && last.elapsed() < VOTE_START_COOLDOWN
            {
                return;
            }
            votes.last_start.insert(voter, Instant::now());
            let duration = self.cfg.server.vote_duration;
            votes.active = Some(Vote {
                kind,
                target,
                ends_at: now + duration,
                voters: HashMap::new(),
                last_voter: 0,
            });
            tracing::info!(
                kind = kind.name(),
                target,
                quorum = self.vote_quorum(kind),
                connected = self.ready_count(),
                by = voter,
                name = client.name(),
                "vote started"
            );
        }
        // A same-kind packet votes in the running poll whatever its target
        // (probe VoteKickRetarget).
        let vote = votes
            .active
            .as_mut()
            .expect("a vote was just opened or found");
        vote.voters.insert(voter, yes);
        if yes {
            vote.last_voter = voter;
        }

        // Decided only at expiry (`expire_stale_vote`), as in vanilla's StepVote.
        let snapshot = vote.clone();
        self.broadcast_vote_state(&snapshot, now);
    }

    /// The yes count a vote must exceed, from the live count of ready clients
    /// (reference GetQuorumForCurrentVote); a driver still loading does not
    /// count. A kick excludes its target. Truncated, so a lone driver's
    /// quorum is 0.
    fn vote_quorum(&self, kind: VoteKind) -> u8 {
        let mut voters = self.ready_count() as i64;
        let mut percent = self.cfg.server.voting_quorum;
        if kind == VoteKind::KickUser {
            voters -= 1;
            percent = self.cfg.server.kick_quorum;
        }
        // Saturates: a quorum above 255 cannot be exceeded.
        u8::try_from(voters.max(0) * i64::from(percent) / 100).unwrap_or(u8::MAX)
    }

    /// Sends the tally to everyone. LastVote is last_voter's current vote,
    /// not the last vote cast (probes VoteAgainst, VoteChangedMind). The time
    /// left is whole milliseconds from now, so a new vote reads exactly
    /// VOTE_DURATION.
    fn broadcast_vote_state(&self, vote: &Vote, now: Instant) {
        let last_vote = vote.voters.get(&vote.last_voter).copied().unwrap_or(false);
        let remaining = signed_millis(now, vote.ends_at) as u32;
        let mut w = Writer::with_id(vote.kind.packet_id());
        w.u8(vote.target);
        w.u8(self.vote_quorum(vote.kind));
        w.u8(vote.yes_count() as u8);
        w.u32(remaining);
        w.u8(vote.last_voter);
        w.bool(last_vote);
        self.broadcast_tcp(w.bytes(), None);
    }

    /// Acts on a passed vote. A session change announces nothing; a kick
    /// announces itself before the 0x68, like an admin kick (probe
    /// VoteKickPasses).
    fn execute_vote(self: &Arc<Self>, vote: &Vote) {
        match vote.kind {
            VoteKind::NextSession => {
                let server = Arc::clone(self);
                self.spawn_task(async move {
                    server.next_session();
                });
            }
            VoteKind::RestartSession => {
                let server = Arc::clone(self);
                self.spawn_task(async move {
                    server.restart_session();
                });
            }
            VoteKind::KickUser => {
                // Vanilla's StepVote calls KickUser, so BLACKLIST_MODE applies.
                if let Some(car) = self.car_by_id(vote.target) {
                    self.admin_kick(None, "vote", &car, false);
                }
            }
        }
    }

    /// Settles a vote whose time is up (vanilla StepVote): yes > quorum
    /// passes, else 0x67 carrying the just-wrapped countdown, as vanilla
    /// writes it.
    pub(crate) fn expire_stale_vote(self: &Arc<Self>) {
        let now = Instant::now();
        let (vote, yes, quorum, remaining) = {
            let mut votes = self.votes.lock();
            let Some(vote) = votes.active.clone() else {
                return;
            };
            if now <= vote.ends_at {
                return;
            }
            votes.active = None;
            let yes = vote.yes_count();
            let quorum = usize::from(self.vote_quorum(vote.kind));
            let remaining = signed_millis(now, vote.ends_at) as u32;
            (vote, yes, quorum, remaining)
        };

        if yes > quorum {
            tracing::info!(
                kind = vote.kind.name(),
                target = vote.target,
                yes,
                quorum,
                "vote passed"
            );
            self.execute_vote(&vote);
            return;
        }
        tracing::info!(
            kind = vote.kind.name(),
            target = vote.target,
            yes,
            quorum,
            "vote expired without quorum"
        );
        let mut w = Writer::with_id(proto::VOTE_QUORUM_NOT_REACHED);
        w.u32(remaining);
        self.broadcast_tcp(w.bytes(), None);
    }
}
