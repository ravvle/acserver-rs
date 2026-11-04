//! WordFilterPlugin: refuses names, drops chat lines and bans for chat
//! lines matching configured patterns (case-insensitive regular
//! expressions).

use std::sync::Arc;

use regex::{Regex, RegexBuilder};
use serde::Deserialize;

use super::{Plugin, Server};
use crate::server::client::Client;

/// `!WordFilterConfiguration`.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct WordFilterConfiguration {
    pub prohibited_username_patterns: Vec<String>,
    pub prohibited_chat_patterns: Vec<String>,
    pub bannable_chat_patterns: Vec<String>,
}

pub struct WordFilter {
    usernames: Vec<Regex>,
    chat: Vec<Regex>,
    bannable: Vec<Regex>,
}

fn compile(patterns: &[String]) -> anyhow::Result<Vec<Regex>> {
    patterns
        .iter()
        .map(|p| {
            RegexBuilder::new(p)
                .case_insensitive(true)
                .build()
                .map_err(|e| anyhow::anyhow!("WordFilterPlugin pattern {p:?}: {e}"))
        })
        .collect()
}

impl WordFilter {
    pub fn new(cfg: WordFilterConfiguration) -> anyhow::Result<WordFilter> {
        Ok(WordFilter {
            usernames: compile(&cfg.prohibited_username_patterns)?,
            chat: compile(&cfg.prohibited_chat_patterns)?,
            bannable: compile(&cfg.bannable_chat_patterns)?,
        })
    }
}

impl Plugin for WordFilter {
    fn name(&self) -> &'static str {
        "WordFilterPlugin"
    }

    fn reject_join(&self, name: &str) -> Option<String> {
        self.usernames.iter().any(|r| r.is_match(name)).then(|| {
            "Prohibited username. Change your Online Name in Settings > Content Manager > Drive > Online Name."
                .to_string()
        })
    }

    fn on_chat(&self, server: &Arc<Server>, client: &Arc<Client>, message: &str) -> bool {
        if self.bannable.iter().any(|r| r.is_match(message)) {
            tracing::info!(
                car = client.session_id(),
                name = client.name(),
                message,
                "chat filtered, banned"
            );
            server.ban_with_reason(None, client, "prohibited language");
            return false;
        }
        if self.chat.iter().any(|r| r.is_match(message)) {
            tracing::info!(
                car = client.session_id(),
                name = client.name(),
                message,
                "chat filtered"
            );
            return false;
        }
        true
    }
}
