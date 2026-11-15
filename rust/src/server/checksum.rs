//! Checksum paths from --content, 0x44 verification and the deferred kick.

use std::io::Read as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use md5::{Digest, Md5};

use super::Server;
use super::client::{ChecksumStatus, Client};
use crate::proto;
use crate::wire::Reader;

/// One model's checksum paths and expected MD5s. Only files present under
/// the content dir are listed; without it the list is empty and every client
/// passes.
#[derive(Debug, Default)]
pub struct ChecksumInfo {
    pub paths: Vec<String>,
    /// The MD5 per path, same order.
    digests: Vec<[u8; 16]>,
    /// The expected MD5 of content/cars/<model>/data.acd; `None` accepts any.
    data_acd: Option<[u8; 16]>,
}

/// The read buffer for hashing.
const HASH_BUFFER_BYTES: usize = 1 << 20;

/// Streams `path` into `hasher` through `buf`, so a large track kn5 is never
/// held in memory whole.
fn hash_into(hasher: &mut Md5, path: &Path, buf: &mut [u8]) -> std::io::Result<()> {
    let mut file = std::fs::File::open(path)?;
    loop {
        match file.read(buf)? {
            0 => return Ok(()),
            n => hasher.update(&buf[..n]),
        }
    }
}

fn md5_file(path: &Path) -> std::io::Result<[u8; 16]> {
    let mut hasher = Md5::new();
    hash_into(&mut hasher, path, &mut vec![0; HASH_BUFFER_BYTES])?;
    Ok(hasher.finalize().into())
}

/// Vanilla's GetFolderChecksum: one MD5 over the contents of every file
/// under `dir`, concatenated in order of lower-cased path.
fn md5_folder(dir: &Path) -> std::io::Result<[u8; 16]> {
    // Regular files only: symlinks are neither followed nor counted.
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(dir) {
        let entry = entry?;
        if entry.file_type().is_file() {
            files.push(entry.into_path());
        }
    }
    if files.is_empty() {
        return Err(std::io::ErrorKind::NotFound.into());
    }
    files.sort_by_key(|path| path.to_string_lossy().to_lowercase());
    let mut hasher = Md5::new();
    let mut buf = vec![0; HASH_BUFFER_BYTES];
    for file in &files {
        hash_into(&mut hasher, file, &mut buf)?;
    }
    Ok(hasher.finalize().into())
}

impl Server {
    /// Hashes every model a driver can join with, as vanilla's
    /// InitChecksums does at start, so no join waits on gigabytes of track
    /// files.
    pub(crate) fn warm_checksums(&self) {
        if self.cfg.content_dir.is_none() {
            return;
        }
        let started = std::time::Instant::now();
        let mut models = self.cfg.distinct_models();
        for model in &self.cfg.server.cars {
            if !models.contains(model) {
                models.push(model.clone());
            }
        }
        for model in &models {
            self.checksum_info_for(model);
        }
        tracing::info!(
            models = models.len(),
            secs = started.elapsed().as_secs_f32(),
            "checksums computed"
        );
    }

    /// A model's checksum info, built on first use. The hashing runs outside
    /// the cache lock, so it holds up no lookup of another model.
    pub(crate) fn checksum_info_for(&self, model: &str) -> Arc<ChecksumInfo> {
        if let Some(info) = self.checksum_cache.lock().get(model) {
            return Arc::clone(info);
        }
        let info = Arc::new(self.build_checksum_info(model));
        let mut cache = self.checksum_cache.lock();
        Arc::clone(cache.entry(model.to_string()).or_insert(info))
    }

    fn build_checksum_info(&self, model: &str) -> ChecksumInfo {
        let mut info = ChecksumInfo::default();
        let Some(content) = &self.cfg.content_dir else {
            return info;
        };
        for (path, digest) in self.shared_checksums(content) {
            info.paths.push(path.clone());
            info.digests.push(*digest);
        }

        // The car collider.
        let collider = format!("content/cars/{model}/collider.kn5");
        if let Ok(digest) = md5_file(&content.join(&collider)) {
            info.paths.push(collider);
            info.digests.push(digest);
        }

        // The expected final digest: data.acd, or, for a car without one,
        // vanilla's checksum of its unpacked data folder.
        let car_dir = content.join("content").join("cars").join(model);
        info.data_acd = md5_file(&car_dir.join("data.acd"))
            .or_else(|_| md5_folder(&car_dir.join("data")))
            .ok();
        info
    }

    /// The entries every model shares, the system and track files, hashed
    /// once: a large track has gigabytes of kn5.
    fn shared_checksums(&self, content: &Path) -> &[(String, [u8; 16])] {
        self.shared_checksums
            .get_or_init(|| self.build_shared_checksums(content))
    }

    fn build_shared_checksums(&self, content: &Path) -> Vec<(String, [u8; 16])> {
        let mut entries = Vec::new();
        let server = &self.cfg.server;

        let mut add = |rel: &str| {
            let Ok(digest) = md5_file(&content.join(rel)) else {
                return; // missing server-side: don't request it
            };
            entries.push((rel.to_string(), digest));
        };

        add("system/data/surfaces.ini");

        // Track data files.
        let track_dir = format!("content/tracks/{}", server.track);
        if server.track_config.is_empty() {
            add(&format!("{track_dir}/data/surfaces.ini"));
        } else {
            add(&format!(
                "{track_dir}/{}/data/surfaces.ini",
                server.track_config
            ));
        }

        // Every *.kn5 and models*.ini directly in the track folder, as
        // vanilla's InitChecksums globs them (all layouts' models files, not
        // only this one's). A root surfaces.ini is ours.
        let abs_track = content.join("content").join("tracks").join(&server.track);
        if let Ok(entries) = std::fs::read_dir(&abs_track) {
            let mut extra: Vec<String> = Vec::new();
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                let models = name.starts_with("models") && name.ends_with(".ini");
                if name.ends_with(".kn5") || models || name == "surfaces.ini" {
                    extra.push(format!("{track_dir}/{name}"));
                }
            }
            extra.sort();
            for path in &extra {
                add(path);
            }
        }
        entries
    }

    /// The ordered relative paths sent in 0x3E.
    pub(crate) fn checksum_paths(&self, model: &str) -> Vec<String> {
        self.checksum_info_for(model).paths.clone()
    }

    /// Verifies a 0x44. A failure is kicked after the first update.
    pub(crate) fn handle_checksum(&self, client: &Arc<Client>, r: &mut Reader) {
        let info = self.checksum_info_for(&client.identity().car_model);
        let blob = r.remaining_bytes();

        let expected_len = 16 * (info.paths.len() + 1);
        let mut ok = blob.len() == expected_len;
        let mut mismatch = "length".to_string();
        if ok {
            for (i, want) in info.digests.iter().enumerate() {
                if &blob[i * 16..i * 16 + 16] != want {
                    ok = false;
                    mismatch = info.paths[i].clone();
                    break;
                }
            }
            // The final digest is data.acd.
            if ok
                && let Some(want) = &info.data_acd
                && &blob[info.paths.len() * 16..] != want
            {
                ok = false;
                mismatch = "data.acd".to_string();
            }
        }

        if ok {
            client.set_checksum_status(ChecksumStatus::Succeeded);
            tracing::debug!(car = client.session_id(), "checksum ok");
        } else {
            client.set_checksum_status(ChecksumStatus::Failed);
            tracing::info!(car = client.session_id(), name = client.name(), file = %mismatch, "checksum failed");
            if client.has_sent_first_update.load(Ordering::SeqCst) {
                self.kick_checksum_fail(client);
            }
        }
    }

    pub(crate) fn kick_checksum_fail(&self, client: &Arc<Client>) {
        self.broadcast_kick(client.session_id(), proto::KICK_CHECKSUM_FAILED);
        self.disconnect(client, "checksum failed");
    }
}
