//! FIXED_SETUP: the locked setup sent as 0x52.

use std::path::{Path, PathBuf};

use super::Server;
use super::client::Client;
use crate::config::{IniFile, parse_float};
use crate::proto;
use crate::wire::Writer;

/// One locked setup value. File order is kept so 0x52 is stable; vanilla's
/// order is arbitrary, and the client keys values by name.
#[derive(Debug, Clone, PartialEq)]
pub struct SetupEntry {
    pub name: String,
    pub value: f32,
}

/// The file under setups/ a slot's FIXED_SETUP names, as vanilla's
/// loadEntryList builds it: a named setup verbatim ("setups/%s", so "fast"
/// and "fast.ini" are different files), and an empty value `<MODEL>.ini`.
pub fn setup_file(fixed_setup: &str, named: bool) -> String {
    if named {
        return fixed_setup.trim().to_string();
    }
    format!("{fixed_setup}.ini")
}

/// Reads setups/<file> as vanilla's LoadSetup does: every section is a setup
/// key, worth its VALUE=<float> (e.g. [PRESSURE_LF] VALUE=26) or 0 without
/// one. A missing or unreadable file gives an empty list (no 0x52).
pub fn load_fixed_setup(base_dir: &Path, file: &str) -> Vec<SetupEntry> {
    if file.is_empty() {
        return Vec::new();
    }
    // Vanilla's location (the working directory) first, then fallbacks.
    let mut candidates: Vec<PathBuf> = vec![Path::new("setups").join(file)];
    if !base_dir.as_os_str().is_empty() {
        let parent = base_dir.parent().unwrap_or(Path::new("."));
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        candidates.push(parent.join("setups").join(file));
        candidates.push(base_dir.join("setups").join(file));
    }
    let Some(path) = candidates.into_iter().find(|p| p.exists()) else {
        return Vec::new();
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return Vec::new();
    };
    let ini = IniFile::parse(&String::from_utf8_lossy(&bytes));

    let mut entries = Vec::new();
    for section in ini.sections() {
        let value = ini
            .get(section.name(), "VALUE")
            .and_then(|v| parse_float(&v))
            .unwrap_or(0.0);
        entries.push(SetupEntry {
            name: section.name().to_string(),
            value: value as f32,
        });
    }
    entries
}

/// Encodes 0x52 CarSetup.
pub fn build_car_setup(setup: &[SetupEntry]) -> Vec<u8> {
    let mut w = Writer::with_id(proto::CAR_SETUP);
    w.bool(true); // Fixed
    w.u8(setup.len() as u8);
    for entry in setup {
        w.string8(&entry.name);
        w.f32(entry.value);
    }
    w.into_bytes()
}

impl Server {
    /// Sends 0x52, or nothing if the slot has no fixed setup.
    pub(crate) fn send_car_setup(&self, client: &Client, setup: &[SetupEntry]) {
        if setup.is_empty() {
            return;
        }
        self.send_tcp(client, &build_car_setup(setup));
    }
}
