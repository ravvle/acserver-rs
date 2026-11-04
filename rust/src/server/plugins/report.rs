//! ReportPlugin, without its Discord webhook: CSP players press Ctrl+Shift+S
//! to upload a replay clip, which is saved under `reports/` with a log of
//! the last ClipDurationSeconds of joins, leaves and chat; `/report <reason>`
//! then records the reason with it for moderators.

use parking_lot::Mutex;
use smart_default::SmartDefault;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use super::{Plugin, PluginRequest, PluginResponse, Server};
use crate::config::Config;
use crate::server::client::Client;

/// `!ReportConfiguration`.
#[derive(Debug, SmartDefault, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct ReportConfiguration {
    #[default(60)]
    pub clip_duration_seconds: u64,
    /// Ignored: reports are not posted to Discord.
    pub webhook_url: Option<String>,
    /// The address CSP uploads clips to, "host" or "host:port". Default:
    /// the server's public IP and HTTP_PORT, as in AssettoServer.
    pub server_address: Option<String>,
}

const REPORTS_DIR: &str = "reports";

/// Where AssettoServer looks up the server's public IP; `query` holds it.
const IP_API_URL: &str = "http://ip-api.com/json";

/// A clip must be this far apart from the same player's last one.
const MIN_REPORT_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
struct AuditClient {
    name: String,
    steam_id: u64,
    session_id: u8,
    car_model: String,
    skin: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
struct AuditEvent {
    /// Unix seconds.
    timestamp: u64,
    event_type: &'static str,
    client: AuditClient,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct AuditLog {
    timestamp: u64,
    entry_list: Vec<AuditClient>,
    events: Vec<AuditEvent>,
}

/// A player's last uploaded clip.
struct Replay {
    id: String,
    at: SystemTime,
    submitted: bool,
}

pub struct Report {
    cfg: ReportConfiguration,
    /// In the upload URL, so only CSP (told it in the extra options) uploads.
    key: String,
    http_port: u16,
    /// `Config::public_ip`: the upload host without a ServerAddress.
    public_ip: Option<std::net::IpAddr>,
    events: Mutex<VecDeque<AuditEvent>>,
    /// By guid.
    replays: Mutex<HashMap<u64, Replay>>,
}

fn unix(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn audit_client(server: &Server, client: &Client) -> AuditClient {
    let id = client.identity();
    let state = server.entry_cars[usize::from(id.session_id)].state();
    AuditClient {
        name: id.name.clone(),
        steam_id: id.steam_id(),
        session_id: id.session_id,
        car_model: state.model,
        skin: state.skin,
    }
}

impl Report {
    pub fn new(server_cfg: &Config, cfg: ReportConfiguration) -> anyhow::Result<Report> {
        if cfg.webhook_url.as_deref().is_some_and(|u| !u.is_empty()) {
            tracing::warn!(
                "ReportPlugin: WebhookUrl is ignored; reports are saved under reports/ only"
            );
        }
        if !server_cfg.extra.enable_csp {
            anyhow::bail!("ReportPlugin needs EnableCSP");
        }
        std::fs::create_dir_all(REPORTS_DIR)?;
        Ok(Report {
            cfg,
            key: format!("{:032x}", rand::random::<u128>()),
            http_port: server_cfg.server.http_port,
            public_ip: server_cfg.public_ip,
            events: Mutex::new(VecDeque::new()),
            replays: Mutex::new(HashMap::new()),
        })
    }

    /// The host and port CSP uploads to. CSP fills `{ServerIP}` into
    /// `[SCRIPT_*]` URLs but not into `[REPLAY_CLIPS]`, so the placeholder is
    /// only the last resort.
    fn upload_host(&self) -> String {
        match (&self.cfg.server_address, self.public_ip) {
            (Some(address), _) if address.contains(':') => address.clone(),
            (Some(address), _) => format!("{address}:{}", self.http_port),
            (None, Some(ip)) => std::net::SocketAddr::new(ip, self.http_port).to_string(),
            (None, None) => "{ServerIP}:{ServerHTTPPort}".to_string(),
        }
    }

    fn record(&self, event: AuditEvent) {
        let mut events = self.events.lock();
        events.push_back(event);
        let horizon = unix(SystemTime::now()).saturating_sub(self.cfg.clip_duration_seconds);
        while events.front().is_some_and(|e| e.timestamp < horizon) {
            events.pop_front();
        }
    }

    fn upload(&self, server: &Server, request: &PluginRequest) -> PluginResponse {
        let id = request
            .headers
            .get("x-car-index")
            .and_then(|v| v.trim().parse::<u8>().ok());
        let Some(client) = id.and_then(|id| server.client_in(id)) else {
            return PluginResponse::status(400);
        };
        let key_ok = request
            .query
            .get("key")
            .is_some_and(|k| k.eq_ignore_ascii_case(&self.key));
        let same_host = request
            .peer
            .is_some_and(|ip| ip.is_loopback() || ip == client.peer.ip());
        if !key_ok || !same_host {
            return PluginResponse::status(403);
        }
        let guid = client.identity().steam_id();
        let now = SystemTime::now();
        if let Some(last) = self.replays.lock().get(&guid)
            && now.duration_since(last.at).unwrap_or_default() < MIN_REPORT_INTERVAL
        {
            server.send_chat_to(
                &client,
                "Please wait a moment before submitting another replay.",
            );
            return PluginResponse::status(429);
        }
        let replay_id = format!("{:032x}", rand::random::<u128>());
        let base = std::path::Path::new(REPORTS_DIR);
        let log = AuditLog {
            timestamp: unix(now),
            entry_list: server
                .connected_clients()
                .iter()
                .map(|c| audit_client(server, c))
                .collect(),
            events: self.events.lock().iter().cloned().collect(),
        };
        let written = std::fs::write(base.join(format!("{replay_id}.zip")), &request.body)
            .and_then(|_| {
                let json = serde_json::to_string_pretty(&log).unwrap_or_default();
                std::fs::write(base.join(format!("{replay_id}.json")), json)
            });
        if let Err(err) = written {
            tracing::error!(%err, "saving a replay clip failed");
            return PluginResponse::status(500);
        }
        self.replays.lock().insert(
            guid,
            Replay {
                id: replay_id.clone(),
                at: now,
                submitted: false,
            },
        );
        tracing::info!(name = client.name(), id = %replay_id, "replay received");
        server.send_chat_to(&client, "Replay received.");
        server.send_chat_to(
            &client,
            "Use /report <reason> to submit this replay to moderators.",
        );
        PluginResponse::status(200)
    }
}

impl Plugin for Report {
    fn name(&self) -> &'static str {
        "ReportPlugin"
    }

    fn extra_options(&self) -> String {
        let host = self.upload_host();
        format!(
            "\r\n[REPLAY_CLIPS]\r\nUPLOAD_URL = 'http://{host}/report?key={}'\r\nDURATION = {}\r\n",
            self.key, self.cfg.clip_duration_seconds
        )
    }

    fn on_first_update(&self, server: &Arc<Server>, client: &Arc<Client>) {
        self.record(AuditEvent {
            timestamp: unix(SystemTime::now()),
            event_type: "PlayerConnected",
            client: audit_client(server, client),
            message: None,
        });
    }

    fn on_disconnect(&self, server: &Server, client: &Client) {
        self.record(AuditEvent {
            timestamp: unix(SystemTime::now()),
            event_type: "PlayerDisconnected",
            client: audit_client(server, client),
            message: None,
        });
        self.replays.lock().remove(&client.identity().steam_id());
    }

    fn on_chat(&self, server: &Arc<Server>, client: &Arc<Client>, message: &str) -> bool {
        self.record(AuditEvent {
            timestamp: unix(SystemTime::now()),
            event_type: "ChatMessage",
            client: audit_client(server, client),
            message: Some(message.to_string()),
        });
        true
    }

    fn on_command(
        &self,
        _server: &Arc<Server>,
        client: &Arc<Client>,
        cmd: &str,
        args: &[&str],
    ) -> Option<String> {
        if cmd != "report" {
            return None;
        }
        let reason = args.join(" ");
        let mut replays = self.replays.lock();
        let Some(replay) = replays.get_mut(&client.identity().steam_id()) else {
            return Some(
                "No replay submitted! Press Ctrl+Shift+S to send a replay (CSP 0.1.76+ required)"
                    .to_string(),
            );
        };
        if replay.submitted {
            return Some("You have already submitted your last replay.".to_string());
        }
        let path = std::path::Path::new(REPORTS_DIR).join(format!("{}.report.txt", replay.id));
        let body = format!(
            "Reporter: {} ({})\nReason: {reason}\n",
            client.name(),
            client.identity().guid
        );
        if let Err(err) = std::fs::write(&path, body) {
            tracing::error!(%err, "saving a report failed");
            return Some("Your report could not be saved.".to_string());
        }
        replay.submitted = true;
        tracing::info!(name = client.name(), id = %replay.id, reason, "report received");
        Some("Your report has been submitted.".to_string())
    }

    fn owns_path(&self, path: &str) -> bool {
        path == "/report"
    }

    fn http(&self, server: &Arc<Server>, request: &PluginRequest) -> Option<PluginResponse> {
        if request.path != "/report" {
            return None;
        }
        if request.method != "POST" {
            return Some(PluginResponse::status(405));
        }
        Some(self.upload(server, request))
    }
}

/// The server's public IP from ip-api.com, as AssettoServer finds it.
pub(super) async fn lookup_public_ip() -> Option<std::net::IpAddr> {
    #[derive(Deserialize)]
    struct IpApi {
        query: std::net::IpAddr,
    }
    let lookup = async {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()?;
        client
            .get(IP_API_URL)
            .send()
            .await?
            .error_for_status()?
            .json::<IpApi>()
            .await
    };
    match lookup.await {
        Ok(reply) => {
            tracing::info!(ip = %reply.query, "ReportPlugin: clips upload to the public IP");
            Some(reply.query)
        }
        Err(err) => {
            tracing::warn!(
                %err,
                "ReportPlugin: public IP lookup failed; set ServerAddress in !ReportConfiguration or clip uploads may fail"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(server_address: Option<&str>, public_ip: Option<&str>) -> Report {
        Report {
            cfg: ReportConfiguration {
                server_address: server_address.map(str::to_string),
                ..ReportConfiguration::default()
            },
            key: String::new(),
            http_port: 8081,
            public_ip: public_ip.map(|ip| ip.parse().expect("ip")),
            events: Mutex::new(VecDeque::new()),
            replays: Mutex::new(HashMap::new()),
        }
    }

    #[test]
    fn upload_host_prefers_server_address_then_public_ip() {
        let host = |address, ip| report(address, ip).upload_host();
        assert_eq!(host(Some("lan:9000"), Some("1.2.3.4")), "lan:9000");
        assert_eq!(host(Some("lan"), Some("1.2.3.4")), "lan:8081");
        assert_eq!(host(None, Some("1.2.3.4")), "1.2.3.4:8081");
        assert_eq!(host(None, Some("2001:db8::1")), "[2001:db8::1]:8081");
        assert_eq!(host(None, None), "{ServerIP}:{ServerHTTPPort}");
    }
}
