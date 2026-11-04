//! CSP server scripts (`CSPServerScripts`, docs/04 §4.5) and the Lua online
//! events the server answers itself (docs/04 §4.4).

use std::path::Path;

use anyhow::Context as _;

use super::Server;
use super::client::Client;
use crate::proto;
use crate::wire::{Reader, Writer};

/// One `CSPServerScripts` entry.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerScript {
    /// A file from the cfg directory, served at /api/scripts/<index>.
    Local { name: String, source: Vec<u8> },
    /// A URL CSP fetches itself.
    Remote { url: String },
}

/// Reads every `CSPServerScripts` entry: an http(s) URL is kept as it is,
/// anything else is a file relative to `base_dir`, read now so a missing
/// script stops the server at start rather than failing on a client.
pub fn load_scripts(entries: &[String], base_dir: &Path) -> anyhow::Result<Vec<ServerScript>> {
    let mut scripts = Vec::with_capacity(entries.len());
    for entry in entries {
        let entry = entry.trim();
        if entry.starts_with("http://") || entry.starts_with("https://") {
            scripts.push(ServerScript::Remote {
                url: entry.to_string(),
            });
            continue;
        }
        let path = base_dir.join(entry);
        let source = std::fs::read(&path)
            .with_context(|| format!("CSPServerScripts: read {}", path.display()))?;
        let name = Path::new(entry)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| entry.to_string());
        scripts.push(ServerScript::Local { name, source });
    }
    Ok(scripts)
}

/// The extra-options sections that make CSP load `scripts`. CSP fills in
/// `{ServerIP}` and `{ServerHTTPPort}` itself. The section numbering starts
/// at 10, as AssettoServer's does.
pub fn script_sections(scripts: &[ServerScript]) -> String {
    let mut ini = String::new();
    for (i, script) in scripts.iter().enumerate() {
        let (label, url) = match script {
            ServerScript::Local { name, .. } => (
                name.clone(),
                format!("http://{{ServerIP}}:{{ServerHTTPPort}}/api/scripts/{i}"),
            ),
            ServerScript::Remote { url } => (format!("remote{i}"), url.clone()),
        };
        ini.push_str(&format!(
            "\r\n[SCRIPT_{}-{label}]\r\nSCRIPT = '{url}'\r\n",
            10 + i
        ));
    }
    ini
}

/// The `LuaPacketType` of `AS_ApiKey { char key[32]; }`: the CSP key hash
/// (xxHash3 0.7.0, folded to 32 bits) of `server_scriptcharkey[32];//AS_ApiKey`
/// (docs/04 §4.4), computed once with the reference library.
pub const LUA_API_KEY: u32 = 0x2761_2FAB;

/// The length of `AS_ApiKey.key`.
const API_KEY_LEN: usize = 32;

impl Server {
    /// A server script's source, if `index` names a local one.
    pub fn script_source(&self, index: usize) -> Option<(String, Vec<u8>)> {
        match self.scripts.get(index)? {
            ServerScript::Local { name, source } => Some((name.clone(), source.clone())),
            ServerScript::Remote { .. } => None,
        }
    }

    /// Answers a Lua event addressed to the server, returning false for any
    /// other so it is relayed. Only `AS_ApiKey` is known: a script asks for
    /// the client's HTTP API key and gets it back.
    pub(crate) fn handle_server_lua_event(&self, client: &Client, lua_type: u32) -> bool {
        if lua_type != LUA_API_KEY || self.scripts.is_empty() {
            return false;
        }
        let mut w = Writer::with_id(proto::EXTENDED);
        w.u8(proto::EXT_TCP_CLIENT_MESSAGE);
        w.u8(proto::SERVER_SESSION_ID);
        w.u16(proto::CSP_LUA_MESSAGE);
        w.u32(LUA_API_KEY);
        // The last field may go out unpadded; the key fills it anyway.
        let key = client.api_key();
        let mut field = [0u8; API_KEY_LEN];
        let n = key.len().min(API_KEY_LEN);
        field[..n].copy_from_slice(&key.as_bytes()[..n]);
        w.raw(&field);
        self.send_tcp(client, w.bytes());
        true
    }
}

/// The `LuaPacketType` of a Lua message body, if it has one: after the
/// optional target byte of a targeted message.
pub fn lua_packet_type(kind: u16, payload: &[u8]) -> Option<u32> {
    let mut r = Reader::new(payload);
    if kind == proto::CSP_LUA_TARGETED {
        let _target = r.u8();
    }
    let lua_type = r.u32();
    (!r.overflowed()).then_some(lua_type)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_number_from_ten() {
        let scripts = vec![
            ServerScript::Local {
                name: "a.lua".into(),
                source: Vec::new(),
            },
            ServerScript::Remote {
                url: "https://example.com/b.lua".into(),
            },
        ];
        let ini = script_sections(&scripts);
        assert!(ini.contains(
            "[SCRIPT_10-a.lua]\r\nSCRIPT = 'http://{ServerIP}:{ServerHTTPPort}/api/scripts/0'"
        ));
        assert!(ini.contains("[SCRIPT_11-remote1]\r\nSCRIPT = 'https://example.com/b.lua'"));
    }

    #[test]
    fn lua_type_skips_the_target() {
        let body = [7u8, 0xAB, 0x2F, 0x61, 0x27];
        assert_eq!(
            lua_packet_type(proto::CSP_LUA_TARGETED, &body),
            Some(LUA_API_KEY)
        );
        assert_eq!(
            lua_packet_type(proto::CSP_LUA_MESSAGE, &body[1..]),
            Some(LUA_API_KEY)
        );
        assert_eq!(lua_packet_type(proto::CSP_LUA_MESSAGE, &[1, 2]), None);
    }
}
