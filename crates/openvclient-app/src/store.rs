//! Saved workspace (profiles, folders, preferences) and keychain secrets.

use std::path::{Path, PathBuf};

use openvclient_core::ConnectionProfile;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const KEYCHAIN_SERVICE: &str = "dev.openvclient.client";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeChoice {
    #[default]
    System,
    Dark,
    Light,
    /// Old single setting for the cartoon look; read from older workspaces
    /// and turned into `style: Cartoon` with the system mode on load.
    #[serde(rename = "Cartoon")]
    LegacyCartoon,
}

/// Visual style, chosen separately from the light/dark mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeStyle {
    /// The original look.
    #[default]
    Original,
    /// The playful look of openvclient.xyz.
    Cartoon,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub theme: ThemeChoice,
    #[serde(default)]
    pub style: ThemeStyle,
    #[serde(default = "default_rows")]
    pub max_rows: usize,
    #[serde(default = "default_page")]
    pub page_size: u64,
    #[serde(default = "default_font")]
    pub editor_font_size: f32,
    #[serde(default = "default_true")]
    pub confirm_production: bool,
    #[serde(default = "default_scroll")]
    pub scroll_speed: f32,
    /// Look for a newer release on GitHub at startup.
    #[serde(default = "default_true")]
    pub check_updates: bool,
    /// Whole-app zoom (also ⌘= / ⌘−).
    #[serde(default = "default_zoom")]
    pub ui_zoom: f32,
    /// How NULL cells are shown in grids.
    #[serde(default = "default_null")]
    pub null_display: String,
    /// Drop fractional seconds from timestamps in grids (copy keeps the full value).
    #[serde(default = "default_true")]
    pub short_timestamps: bool,
    /// Cancel a SQL run after this many seconds (0 = never).
    #[serde(default)]
    pub query_timeout_secs: u64,
    /// Ask for the typed name on Staging connections too.
    #[serde(default)]
    pub confirm_staging: bool,
    /// Home groups connection cards by sidebar folder instead of most recent first.
    #[serde(default)]
    pub home_by_folder: bool,
}

fn default_zoom() -> f32 {
    1.0
}

fn default_null() -> String {
    "NULL".into()
}

fn default_scroll() -> f32 {
    0.6
}

fn default_rows() -> usize {
    10_000
}
fn default_page() -> u64 {
    200
}
fn default_font() -> f32 {
    13.5
}
fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::System,
            style: ThemeStyle::Original,
            max_rows: default_rows(),
            page_size: default_page(),
            editor_font_size: default_font(),
            confirm_production: true,
            scroll_speed: default_scroll(),
            check_updates: true,
            ui_zoom: default_zoom(),
            null_display: default_null(),
            short_timestamps: true,
            query_timeout_secs: 0,
            confirm_staging: false,
            home_by_folder: false,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Workspace {
    #[serde(default)]
    pub profiles: Vec<ConnectionProfile>,
    /// Folders that exist even when empty.
    #[serde(default)]
    pub folders: Vec<String>,
    #[serde(default)]
    pub settings: Settings,
    /// Recently run SQL per connection, newest first.
    #[serde(default)]
    pub history: Vec<HistoryEntry>,
    /// Saved SQL snippets, shared by every connection.
    #[serde(default)]
    pub snippets: Vec<Snippet>,
    /// Tabs open when the app last closed, reopened at start-up.
    #[serde(default)]
    pub session: Vec<SavedTab>,
    /// Connections pinned to Home, in the order they were starred.
    #[serde(default)]
    pub favorites: Vec<Uuid>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub profile: Uuid,
    pub sql: String,
    pub at: String,
    pub ok: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snippet {
    pub name: String,
    pub sql: String,
}

/// A tab that can be reopened after a restart.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "tab", rename_all = "snake_case")]
pub enum SavedTab {
    Sql { conn: Uuid, database: Option<String>, text: String },
    Table { conn: Uuid, kind: openvclient_core::sql::ObjectKind, path: openvclient_core::sql::ObjPath },
    Monitor { conn: Uuid },
    Redis { conn: Uuid, db: u32 },
}

impl SavedTab {
    pub fn conn(&self) -> Uuid {
        match self {
            SavedTab::Sql { conn, .. } | SavedTab::Table { conn, .. } | SavedTab::Monitor { conn } | SavedTab::Redis { conn, .. } => *conn,
        }
    }

    pub fn into_cmd(self) -> crate::app::Cmd {
        use crate::app::Cmd;
        match self {
            SavedTab::Sql { conn, database, text } => Cmd::NewSql { conn, database, text },
            SavedTab::Table { conn, kind, path } => Cmd::OpenTable { conn, kind, path },
            SavedTab::Monitor { conn } => Cmd::OpenMonitor(conn),
            SavedTab::Redis { conn, db } => Cmd::OpenRedis { conn, db },
        }
    }
}

pub struct Store {
    path: PathBuf,
    pub ws: Workspace,
    pub load_error: Option<String>,
}

/// Owner-only permissions: the workspace holds SQL history and hosts.
pub fn restrict(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    // Windows: the config dir lives under %APPDATA%, whose inherited ACL already
    // limits access to this account (plus SYSTEM and Administrators).
    #[cfg(not(unix))]
    let _ = path;
}

pub fn config_dir() -> PathBuf {
    if let Ok(p) = std::env::var("OPENVCLIENT_CONFIG_DIR") {
        return PathBuf::from(p);
    }
    directories::ProjectDirs::from("dev", "OpenVClient", "OpenVClient").map(|d| d.config_dir().to_path_buf()).unwrap_or_else(|| PathBuf::from(".openvclient"))
}

/// Decrypt sealed values in place; returns how many could not be opened.
fn unseal(ws: &mut Workspace) -> usize {
    let mut lost = 0;
    ws.history.retain_mut(|h| match crate::vault::open(&h.sql) {
        Some(s) => {
            h.sql = s;
            true
        }
        None => {
            lost += 1;
            false
        }
    });
    for p in &mut ws.profiles {
        for v in p.params.values_mut() {
            if crate::vault::is_sealed(v) {
                match crate::vault::open(v) {
                    Some(s) => *v = s,
                    None => {
                        lost += 1;
                        v.clear();
                    }
                }
            }
        }
    }
    lost
}

impl Store {
    pub fn load() -> Self {
        let path = config_dir().join("workspace.json");
        // One-time move from the pre-rename location ("OpenV").
        if !path.exists()
            && std::env::var("OPENVCLIENT_CONFIG_DIR").is_err()
            && let Some(old) = directories::ProjectDirs::from("dev", "OpenV", "OpenV").map(|d| d.config_dir().join("workspace.json"))
            && old.exists()
        {
            let _ = std::fs::create_dir_all(path.parent().unwrap_or(std::path::Path::new(".")));
            let _ = std::fs::copy(&old, &path);
        }
        let (ws, load_error) = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Workspace>(&bytes) {
                Ok(mut ws) => {
                    if ws.settings.theme == ThemeChoice::LegacyCartoon {
                        ws.settings.theme = ThemeChoice::System;
                        ws.settings.style = ThemeStyle::Cartoon;
                    }
                    let lost = unseal(&mut ws);
                    let warn =
                        (lost > 0).then(|| format!("{lost} encrypted item(s) in the workspace could not be decrypted (the key in the system keychain is missing or different) and were left out."));
                    (ws, warn)
                }
                Err(e) => {
                    // Keep the unreadable file so nothing is lost.
                    let backup = path.with_extension(format!("broken-{}.json", chrono::Utc::now().timestamp()));
                    let _ = std::fs::copy(&path, &backup);
                    (Workspace::default(), Some(format!("Could not read {}: {e}. A copy was kept at {}", path.display(), backup.display())))
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Workspace::default(), None),
            Err(e) => (Workspace::default(), Some(format!("Could not read {}: {e}", path.display()))),
        };
        Self { path, ws, load_error }
    }

    /// Atomic write: temp file then rename.
    pub fn save(&self) -> Result<(), String> {
        let dir = self.path.parent().ok_or("bad config path")?;
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let tmp = self.path.with_extension("json.tmp");
        // History and URIs with embedded passwords are encrypted at rest.
        let mut disk = self.ws.clone();
        for h in &mut disk.history {
            h.sql = crate::vault::seal(&h.sql);
        }
        for p in &mut disk.profiles {
            for v in p.params.values_mut() {
                if crate::vault::param_has_secret(v) {
                    *v = crate::vault::seal(v);
                }
            }
        }
        let data = serde_json::to_vec_pretty(&disk).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, data).map_err(|e| e.to_string())?;
        restrict(&tmp);
        std::fs::rename(&tmp, &self.path).map_err(|e| e.to_string())
    }

    pub fn profile(&self, id: Uuid) -> Option<&ConnectionProfile> {
        self.ws.profiles.iter().find(|p| p.id == id)
    }

    pub fn profile_mut(&mut self, id: Uuid) -> Option<&mut ConnectionProfile> {
        self.ws.profiles.iter_mut().find(|p| p.id == id)
    }

    pub fn upsert(&mut self, p: ConnectionProfile) {
        match self.ws.profiles.iter_mut().find(|x| x.id == p.id) {
            Some(slot) => *slot = p,
            None => self.ws.profiles.push(p),
        }
    }

    /// Write every connection, without secrets, to `path`. Passwords live in
    /// the keychain; user:pass@ in connection strings is redacted too.
    pub fn export_profiles(&self, path: &Path) -> Result<usize, String> {
        let mut out = self.ws.profiles.clone();
        for p in &mut out {
            for v in p.params.values_mut() {
                if v.contains("://") {
                    *v = openvclient_core::redact(v);
                }
            }
        }
        let json = serde_json::to_vec_pretty(&serde_json::json!({ "openvclient_connections": 1, "profiles": out })).map_err(|e| e.to_string())?;
        std::fs::write(path, json).map_err(|e| e.to_string())?;
        Ok(out.len())
    }

    /// Add connections from an exported file. Existing ids are left alone.
    pub fn import_profiles(&mut self, path: &Path) -> Result<(usize, usize), String> {
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        let v: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| format!("Not a connections file: {e}"))?;
        let list = v.get("profiles").cloned().ok_or("Not a connections file (no \"profiles\")")?;
        let profiles: Vec<ConnectionProfile> = serde_json::from_value(list).map_err(|e| format!("Not a connections file: {e}"))?;
        let (mut added, mut skipped) = (0, 0);
        for p in profiles {
            if self.ws.profiles.iter().any(|x| x.id == p.id) {
                skipped += 1;
            } else {
                self.ws.profiles.push(p);
                added += 1;
            }
        }
        Ok((added, skipped))
    }

    pub fn remove(&mut self, id: Uuid) {
        self.ws.profiles.retain(|p| p.id != id);
        self.ws.history.retain(|h| h.profile != id);
        self.ws.favorites.retain(|f| *f != id);
    }

    pub fn is_favorite(&self, id: Uuid) -> bool {
        self.ws.favorites.contains(&id)
    }

    pub fn toggle_favorite(&mut self, id: Uuid) {
        if self.is_favorite(id) {
            self.ws.favorites.retain(|f| *f != id);
        } else {
            self.ws.favorites.push(id);
        }
    }

    /// Every folder path, including parents of nested folders.
    pub fn all_folders(&self) -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        let mut add = |f: &str| {
            let mut acc = String::new();
            for part in f.split('/').filter(|p| !p.is_empty()) {
                if !acc.is_empty() {
                    acc.push('/');
                }
                acc.push_str(part);
                if !v.contains(&acc) {
                    v.push(acc.clone());
                }
            }
        };
        for f in &self.ws.folders {
            add(f);
        }
        for p in &self.ws.profiles {
            add(&p.folder);
        }
        v.sort_by_key(|s| s.to_lowercase());
        v
    }

    pub fn push_history(&mut self, profile: Uuid, sql: &str, ok: bool) {
        let sql = sql.trim();
        if sql.is_empty() {
            return;
        }
        self.ws.history.retain(|h| !(h.profile == profile && h.sql == sql));
        self.ws.history.insert(0, HistoryEntry { profile, sql: sql.to_string(), at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(), ok });
        self.ws.history.truncate(500);
    }
}

/// OS keychain (macOS Keychain, Windows Credential Manager, Secret Service).
pub mod secrets {
    use super::*;

    fn entry(id: Uuid) -> Result<keyring::Entry, String> {
        keyring::Entry::new(KEYCHAIN_SERVICE, &id.to_string()).map_err(|e| format!("Keychain unavailable: {e}"))
    }

    pub fn get(id: Uuid) -> Result<Option<String>, String> {
        match entry(id)?.get_password() {
            Ok(p) => Ok(Some(p)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(format!("Keychain read failed: {e}")),
        }
    }

    pub fn set(id: Uuid, secret: &str) -> Result<(), String> {
        entry(id)?.set_password(secret).map_err(|e| format!("Keychain write failed: {e}"))
    }

    /// Additional secrets (SSH password, proxy password…) for a profile.
    pub fn get_named(id: Uuid, name: &str) -> Result<Option<String>, String> {
        match keyring::Entry::new(KEYCHAIN_SERVICE, &format!("{id}:{name}")).map_err(|e| e.to_string())?.get_password() {
            Ok(p) => Ok(Some(p)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(format!("Keychain read failed: {e}")),
        }
    }

    pub fn set_named(id: Uuid, name: &str, secret: &str) -> Result<(), String> {
        let e = keyring::Entry::new(KEYCHAIN_SERVICE, &format!("{id}:{name}")).map_err(|e| e.to_string())?;
        if secret.is_empty() {
            match e.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(err) => Err(err.to_string()),
            }
        } else {
            e.set_password(secret).map_err(|err| format!("Keychain write failed: {err}"))
        }
    }

    /// Remove only the main password (network-path secrets stay).
    pub fn delete_main(id: Uuid) -> Result<(), String> {
        match entry(id)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(format!("Keychain delete failed: {e}")),
        }
    }

    /// Remove every secret of a connection (when it is deleted).
    pub fn delete(id: Uuid) -> Result<(), String> {
        for n in [openvclient_net::keys::SECRET_SSH_PASSWORD, openvclient_net::keys::SECRET_SSH_PASSPHRASE, openvclient_net::keys::SECRET_PROXY_PASSWORD] {
            let _ = set_named(id, n, "");
        }
        match entry(id)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(format!("Keychain delete failed: {e}")),
        }
    }
}
