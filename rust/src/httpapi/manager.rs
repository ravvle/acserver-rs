//! The acServerManager endpoints vanilla serves beside /INFO and /JSON
//! (docs/05 §5.2a). Requests separate their fields with '|', as /JSON does,
//! and every reply is plain text.

use std::sync::Arc;

use std::collections::HashMap;

use axum::http::header;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use super::{form_urlencoded_pairs, text};
use crate::proto::SessionType;
use crate::server::Server;

/// Answers /SUB|<car>|<skin>|<name>|<team>|<guid>[|<password>] (vanilla
/// subscribe): a booking, in a booking session only.
pub(super) async fn handle_subscribe(server: &Arc<Server>, path: &str) -> Response {
    if let Some(refusal) = booking_closed(server) {
        return text(refusal);
    }
    let f: Vec<&str> = path.split('|').collect();
    if f.len() != 6 && f.len() != 7 {
        return text("SYNTAX_ERROR");
    }
    let password = f.get(6).copied().unwrap_or("");
    text(
        server
            .subscribe(f[1], f[2], f[3], f[4], f[5], password)
            .await,
    )
}

/// Answers /UNSUB|<guid> (vanilla unsubscribe).
pub(super) fn handle_unsubscribe(server: &Arc<Server>, path: &str) -> Response {
    if let Some(refusal) = booking_closed(server) {
        return text(refusal);
    }
    let f: Vec<&str> = path.split('|').collect();
    if f.len() != 2 {
        return text("SYNTAX_ERROR");
    }
    text(server.unsubscribe(f[1]))
}

/// FAILED_PICKUP on a pickup server and CLOSED outside a booking session, or
/// `None` when a booking request may go on.
fn booking_closed(server: &Server) -> Option<String> {
    if server.cfg.server.pickup_mode_enabled {
        return Some("FAILED_PICKUP".to_string());
    }
    let (cur, _) = server.sessions().current();
    if cur.kind != SessionType::Booking {
        return Some("CLOSED".to_string());
    }
    None
}

/// Answers /CANJOIN|<guid> with YES or NO (vanilla canJoin). A malformed
/// request is only logged, and its body is empty.
pub(super) fn handle_can_join(server: &Server, path: &str) -> Response {
    let f: Vec<&str> = path.split('|').collect();
    if f.len() != 2 {
        tracing::info!(path, "malformed CANJOIN request");
        return text("");
    }
    text(server.can_join(f[1]))
}

/// Vanilla's test AUTH_PLUGIN_ADDRESS: it admits only the guid "ks1".
pub(super) fn handle_ac_auth(query: &HashMap<String, String>) -> Response {
    if query.get("GUID").is_some_and(|guid| guid == "ks1") {
        return text("OK");
    }
    text("DENY|Sorry, you are too stupid to join here")
}

/// /RECONFIG's json value (vanilla idleReconfig).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
#[allow(dead_code)] // decoded only to validate it, as vanilla discards it
struct ReconfigRequest {
    #[serde(default)]
    name: String,
    #[serde(default)]
    track_name: String,
    #[serde(default)]
    track_config: String,
    #[serde(default)]
    practice_time_mins: i64,
    #[serde(default)]
    qualify_time_mins: i64,
    #[serde(default)]
    race_laps: i64,
    #[serde(default)]
    cars: Vec<ReconfigCar>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
#[allow(dead_code)] // decoded only to validate it
struct ReconfigCar {
    #[serde(default)]
    model: String,
    #[serde(default)]
    skin: String,
    #[serde(default)]
    ballast: i64,
    #[serde(default)]
    restrictor: i64,
}

/// Answers /RECONFIG?json=<json>. Vanilla v1.15 parses the settings and then
/// discards them, so it does too; a bad value gets the decoder's error text.
/// A posted form's value wins over the query's.
pub(super) fn handle_reconfig(query: &HashMap<String, String>, body: &[u8]) -> Response {
    let form = form_urlencoded_pairs(body);
    let value = form
        .get("json")
        .or_else(|| query.get("json"))
        .map(String::as_str)
        .unwrap_or("");
    match serde_json::from_str::<ReconfigRequest>(value) {
        Ok(request) => {
            tracing::info!(name = %request.name, "reconfigure request ignored, as vanilla ignores it");
            text("OK")
        }
        Err(err) => text(err.to_string()),
    }
}

/// Serves /ENTRY, vanilla's HTML status page (serveEntryList, ks/khtml): the
/// session and its time left, the car list with each car's ping and times,
/// the standings by best lap, and the send throughput.
pub(super) fn handle_entry_page(server: &Server) -> Response {
    let (cur, _) = server.sessions().current();
    let left = server.sessions().time_left_ms() / 1000;

    let mut b = String::new();
    b.push_str(concat!(
        r#"<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Transitional//EN" "http://www.w3.org/TR/xhtml1/DTD/xhtml1-transitional.dtd">"#,
        "\n",
        r#"<html xmlns="http://www.w3.org/1999/xhtml" >"#,
        "\n<head>\n    <title>Asetto Corsa Server: Entry List</title><body>"
    ));
    b.push_str(&format!(
        "<p>Session: {} [{}], TIME LEFT {:02}:{:02}</p>",
        cur.kind.results_name(),
        html_escape(&cur.name),
        left / 60,
        left % 60
    ));
    b.push_str("<p>Entry List</p>");

    b.push_str("<table>");
    row(
        &mut b,
        &[
            "ID", "Driver", "Car", "Team", "Ping", "Laps", "Last", "Best", "Total", "Guid",
        ],
    );
    for (i, car) in server.listed_cars().iter().enumerate() {
        let id = car.identity();
        let ping = match car.client() {
            Some(client) => client.ping().to_string(),
            None => "DC".to_string(),
        };
        let res = car.result();
        let none = "--:--:---".to_string();
        let (last, best, total) = if res.num_laps != 0 {
            (
                kunos_time(res.last_lap_time),
                kunos_time(res.best_lap_or_none()),
                kunos_time(res.total_time),
            )
        } else {
            (none.clone(), none.clone(), none)
        };
        row(
            &mut b,
            &[
                &(i + 1).to_string(),
                &id.name,
                &car.model(),
                &id.team.to_string(),
                &ping,
                &res.num_laps.to_string(),
                &last,
                &best,
                &total,
                &id.guid,
            ],
        );
    }
    b.push_str("</table>");

    b.push_str("<table>");
    row(
        &mut b,
        &["POS", "Driver", "Car", "Team", "Laps", "Time", "Diff"],
    );
    let order = server.leaderboard_entries();
    let leader_best = order
        .first()
        .map(|car| car.result().best_lap_or_none())
        .unwrap_or(0);
    for (i, car) in order.iter().enumerate() {
        let id = car.identity();
        let res = car.result();
        let diff = if i > 0 {
            // Unsigned, as vanilla subtracts.
            format!(
                "+{}",
                kunos_time(res.best_lap_or_none().wrapping_sub(leader_best))
            )
        } else {
            String::new()
        };
        row(
            &mut b,
            &[
                &i.to_string(),
                &id.name,
                &car.model(),
                &id.team.to_string(),
                &res.num_laps.to_string(),
                &kunos_time(res.best_lap_or_none()),
                &diff,
            ],
        );
    }
    b.push_str("</table>");

    b.push_str(&format!(
        "<p>Throughput: {}kBs</p></body>\n</html>",
        server.throughput()
    ));
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], b).into_response()
}

/// One khtml table row. Vanilla writes each cell verbatim; this server
/// escapes it, since a driver picks their own name and team (deviation,
/// probe HTTPEntryEscaped).
fn row(b: &mut String, cells: &[&str]) {
    b.push_str("<tr>");
    for cell in cells {
        b.push_str("<td>");
        b.push_str(&html_escape(cell));
        b.push_str("</td>");
    }
    b.push_str("</tr>");
}

/// Escapes text for HTML.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '\'' => out.push_str("&#39;"),
            '"' => out.push_str("&#34;"),
            _ => out.push(c),
        }
    }
    out
}

/// Milliseconds as vanilla's ks.TimeToString: m:ss:mmm, minutes unpadded.
fn kunos_time(ms: u32) -> String {
    format!("{}:{:02}:{:03}", ms / 60000, ms / 1000 % 60, ms % 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_html() {
        assert_eq!(
            html_escape(r#"<b>A&B</b> "it's""#),
            "&lt;b&gt;A&amp;B&lt;/b&gt; &#34;it&#39;s&#34;"
        );
        assert_eq!(html_escape("Ünïcode"), "Ünïcode");
    }
}
