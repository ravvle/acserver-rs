//! User groups (`UserGroups` in extra_cfg.yml): named files of Steam IDs
//! for the admin, whitelist and blacklist groups and for
//! `UserGroupCommandPermissions`. As in AssettoServer each file holds one
//! id per line, is created if missing, and is re-read when it changes.
//!
//! The blacklist.txt files are read the same way, but never created.

use parking_lot::{Mutex, RwLock};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use super::Server;
use crate::config::{ExtraConfig, parse_steam_id};

/// The ban list's file name.
const BLACKLIST_FILE: &str = "blacklist.txt";

/// How often the files are checked for changes.
pub const RELOAD_INTERVAL_SECS: u64 = 5;

/// One group: its file and the ids in it.
#[derive(Debug)]
pub struct UserGroup {
    pub name: String,
    path: PathBuf,
    /// Write an empty file when there is none (AssettoServer's groups).
    create_if_missing: bool,
    guids: RwLock<HashSet<u64>>,
    /// The file's modification time at the last read.
    modified: Mutex<Option<SystemTime>>,
}

impl UserGroup {
    fn new(name: &str, path: impl Into<PathBuf>, create_if_missing: bool) -> UserGroup {
        UserGroup {
            name: name.to_string(),
            path: path.into(),
            create_if_missing,
            guids: RwLock::new(HashSet::new()),
            modified: Mutex::new(None),
        }
    }

    fn modified_time(&self) -> Option<SystemTime> {
        std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok()
    }

    /// Reads the file. A missing one is created or read as empty. Lines
    /// that are not ids (blanks, "#" comments) are skipped.
    fn load(&self) -> std::io::Result<()> {
        if !self.path.exists() && self.create_if_missing {
            std::fs::write(&self.path, "")?;
        }
        let text = match std::fs::read_to_string(&self.path) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound && !self.create_if_missing => {
                String::new()
            }
            read => read?,
        };
        let mut guids = HashSet::new();
        for line in text.lines() {
            if let Some(guid) = parse_steam_id(line.trim())
                && !guids.insert(guid)
            {
                tracing::warn!(file = %self.path.display(), guid, "duplicate entry");
            }
        }
        tracing::info!(group = %self.name, file = %self.path.display(), entries = guids.len(), "user group loaded");
        *self.guids.write() = guids;
        *self.modified.lock() = self.modified_time();
        Ok(())
    }

    /// Re-reads the file if it changed since the last read.
    fn reload_if_changed(&self) -> bool {
        let now = self.modified_time();
        if now == *self.modified.lock() {
            return false;
        }
        match self.load() {
            Ok(()) => true,
            Err(err) => {
                tracing::error!(group = %self.name, %err, "user group reload failed");
                false
            }
        }
    }

    pub fn contains(&self, guid: u64) -> bool {
        self.guids.read().contains(&guid)
    }

    pub fn is_empty(&self) -> bool {
        self.guids.read().is_empty()
    }

    /// Adds `guid`, appending it to the file. False if it was there.
    pub fn add(&self, guid: u64) -> std::io::Result<bool> {
        if !self.guids.write().insert(guid) {
            return Ok(false);
        }
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(file, "{guid}")?;
        *self.modified.lock() = self.modified_time();
        Ok(true)
    }

    /// Removes `guid`, rewriting the file. False if it was not there.
    pub fn remove(&self, guid: u64) -> std::io::Result<bool> {
        if !self.guids.write().remove(&guid) {
            return Ok(false);
        }
        remove_line(&self.path, guid)?;
        *self.modified.lock() = self.modified_time();
        Ok(true)
    }
}

/// Drops every line that is `guid` from a file of ids.
pub fn remove_line(path: &std::path::Path, guid: u64) -> std::io::Result<()> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(());
    };
    let wanted = guid.to_string();
    let kept: Vec<&str> = text.lines().filter(|l| l.trim() != wanted).collect();
    let mut body = kept.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    std::fs::write(path, body)
}

/// Every configured group, by name.
#[derive(Debug, Default)]
pub struct UserGroups {
    groups: HashMap<String, Arc<UserGroup>>,
    /// blacklist.txt in the working directory, where vanilla reads it, then
    /// in the cfg directory, where /ban_id appends; one when they are the
    /// same file.
    blacklist_files: Vec<UserGroup>,
}

impl UserGroups {
    /// Loads every group in `UserGroups`. A file that cannot be read stops
    /// the server, since the admin and ban lists depend on it.
    /// The blacklist.txt files are read as well.
    pub fn load(extra: &ExtraConfig, base_dir: &Path) -> anyhow::Result<UserGroups> {
        let mut groups = HashMap::new();
        for (name, path) in &extra.user_groups {
            let group = UserGroup::new(name, path, true);
            group
                .load()
                .map_err(|e| anyhow::anyhow!("user group {name} ({path}): {e}"))?;
            groups.insert(name.clone(), Arc::new(group));
        }
        let mut paths = vec![PathBuf::from(BLACKLIST_FILE)];
        let in_cfg = base_dir.join(BLACKLIST_FILE);
        if std::path::absolute(&paths[0]).ok() != std::path::absolute(&in_cfg).ok() {
            paths.push(in_cfg);
        }
        let mut blacklist_files = Vec::new();
        for path in paths {
            let group = UserGroup::new("blacklist", path, false);
            if let Err(err) = group.load() {
                tracing::error!(file = %group.path.display(), %err, "blacklist not read");
            }
            blacklist_files.push(group);
        }
        Ok(UserGroups {
            groups,
            blacklist_files,
        })
    }

    pub fn get(&self, name: &str) -> Option<&Arc<UserGroup>> {
        if name.is_empty() {
            return None;
        }
        self.groups.get(name)
    }

    /// Re-reads changed files; the names of the groups that changed.
    fn reload(&self) -> Vec<String> {
        for file in &self.blacklist_files {
            file.reload_if_changed();
        }
        self.groups
            .values()
            .filter(|g| g.reload_if_changed())
            .map(|g| g.name.clone())
            .collect()
    }
}

impl Server {
    fn group(&self, name: &str) -> Option<&Arc<UserGroup>> {
        self.user_groups.get(name)
    }

    /// Whether a guid is banned: until restart, by a blacklist.txt or by
    /// BlacklistUserGroup.
    pub fn blacklisted(&self, guid: u64) -> bool {
        self.session_bans.read().contains(&guid)
            || self
                .user_groups
                .blacklist_files
                .iter()
                .any(|f| f.contains(guid))
            || self.group_blacklisted(guid)
    }

    /// Lifts a ban: from memory, both blacklist.txt files and the
    /// blacklist group. False if the guid was not banned.
    pub fn unban(&self, guid: u64) -> bool {
        let was = self.blacklisted(guid);
        self.session_bans.write().remove(&guid);
        for group in self
            .user_groups
            .blacklist_files
            .iter()
            .chain(self.blacklist_group().map(|g| &**g))
        {
            if let Err(err) = group.remove(guid) {
                tracing::error!(file = %group.path.display(), %err, "unban");
            }
        }
        was
    }

    /// Bans a guid until restart.
    pub fn add_to_blacklist(&self, guid: u64) {
        self.session_bans.write().insert(guid);
    }

    /// Bans a guid and appends it, once, to <cfg dir>/blacklist.txt.
    pub fn persist_blacklist(&self, guid: u64) -> std::io::Result<()> {
        let cfg_file = self
            .user_groups
            .blacklist_files
            .last()
            .expect("there is always a blacklist file");
        cfg_file.add(guid).map(|_| ())
    }

    /// AdminUserGroup lists `guid`.
    pub(crate) fn group_admin(&self, guid: u64) -> bool {
        self.group(&self.cfg.extra.admin_user_group)
            .is_some_and(|g| g.contains(guid))
    }

    /// WhitelistUserGroup is unset, empty or lists `guid`.
    pub(crate) fn whitelisted(&self, guid: u64) -> bool {
        match self.group(&self.cfg.extra.whitelist_user_group) {
            Some(group) => group.is_empty() || group.contains(guid),
            None => true,
        }
    }

    /// BlacklistUserGroup lists `guid`.
    pub(crate) fn group_blacklisted(&self, guid: u64) -> bool {
        self.group(&self.cfg.extra.blacklist_user_group)
            .is_some_and(|g| g.contains(guid))
    }

    /// The blacklist group, if one is configured.
    pub(crate) fn blacklist_group(&self) -> Option<&Arc<UserGroup>> {
        self.group(&self.cfg.extra.blacklist_user_group)
    }

    /// The whitelist group, if one is configured.
    pub(crate) fn whitelist_group(&self) -> Option<&Arc<UserGroup>> {
        self.group(&self.cfg.extra.whitelist_user_group)
    }

    /// UserGroupCommandPermissions lets `guid` run `cmd`.
    pub(crate) fn group_permits(&self, cmd: &str, guid: u64) -> bool {
        self.cfg
            .extra
            .user_group_command_permissions
            .iter()
            .any(|p| {
                p.commands.iter().any(|c| c.trim_start_matches('/') == cmd)
                    && self.group(&p.user_group).is_some_and(|g| g.contains(guid))
            })
    }

    /// Without Steam session tickets a joining player's Steam ID is not
    /// verified, so a non-empty admin group lets anyone who knows an admin's
    /// id become admin. As AssettoServer, refuse that unless overridden.
    pub(crate) fn check_admin_group_safety(&self) -> anyhow::Result<()> {
        let Some(group) = self.group(&self.cfg.extra.admin_user_group) else {
            return Ok(());
        };
        if group.is_empty() {
            return Ok(());
        }
        let message = "the admin user group is not empty, but Steam IDs are not verified, so anyone can claim an admin's id";
        if self
            .cfg
            .extra
            .ignore_configuration_errors
            .unsafe_admin_whitelist
        {
            tracing::warn!("{message}");
            return Ok(());
        }
        anyhow::bail!(
            "{message}; set IgnoreConfigurationErrors.UnsafeAdminWhitelist to start anyway"
        )
    }

    /// Re-reads changed group files, and kicks connected players the
    /// blacklist group now lists.
    pub(crate) fn reload_user_groups(&self) {
        let changed = self.user_groups.reload();
        if !changed.contains(&self.cfg.extra.blacklist_user_group) {
            return;
        }
        for car in &self.entry_cars {
            let Some(client) = car.client() else { continue };
            if self.group_blacklisted(client.identity().steam_id()) {
                tracing::info!(name = client.name(), "banned after the blacklist reloaded");
                self.admin_kick(None, "blacklist", car, false);
            }
        }
    }

    /// Starts the file watcher.
    pub(crate) fn start_user_groups(self: &Arc<Self>) {
        let server = Arc::clone(self);
        self.spawn_ticker(
            std::time::Duration::from_secs(RELOAD_INTERVAL_SECS),
            move || server.reload_user_groups(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_file_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("admins.txt");
        std::fs::write(&path, "76561198000000001\nnot an id\n76561198000000002\n").expect("write");
        let group = UserGroup::new("admins", &path, true);
        group.load().expect("load");
        assert!(group.contains(76561198000000001));
        assert!(!group.contains(3));
        assert!(group.add(3).expect("add"));
        assert!(!group.add(3).expect("add again"));
        assert!(group.remove(76561198000000001).expect("remove"));
        let text = std::fs::read_to_string(&path).expect("read");
        assert_eq!(text, "not an id\n76561198000000002\n3\n");
        group.load().expect("reload");
        assert!(group.contains(3) && !group.contains(76561198000000001));
    }

    #[test]
    fn missing_file_is_created_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("whitelist.txt");
        let group = UserGroup::new("w", &path, true);
        group.load().expect("load");
        assert!(path.exists());
        assert!(group.is_empty());
    }

    #[test]
    fn blacklist_file_is_read_but_not_created() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("blacklist.txt");
        let group = UserGroup::new("blacklist", &path, false);
        group.load().expect("load");
        assert!(!path.exists());
        assert!(group.is_empty());
        std::fs::write(&path, "# banned\n76561198000000001\n").expect("write");
        assert!(group.reload_if_changed());
        assert!(group.contains(76561198000000001));
    }
}
