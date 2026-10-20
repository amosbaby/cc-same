//! `config.json` (what the user chose) and `state.json` (what we remember between runs).
//! Both keep the key names of the original Python prototype, so an existing install upgrades
//! in place.

use crate::fsx;
use crate::model::{is_uuid, Surface};
use crate::paths::Paths;
use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fs;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    /// `<account>` or `<account>/<org>` entries that stay separate.
    pub exclude: Vec<String>,
    /// When not empty, only these `<account>` or `<account>/<org>` entries take part; every
    /// other account and org stays separate, including ones that appear later.
    pub only: Vec<String>,
    /// Accounts and orgs that appear later join automatically.
    pub auto_join_new: bool,
    /// Rolling snapshots kept (the baseline is kept forever).
    #[serde(deserialize_with = "de_count")]
    pub keep_snapshots: usize,
    #[serde(deserialize_with = "de_number")]
    pub snapshot_every_seconds: f64,
    #[serde(deserialize_with = "de_number")]
    pub trash_days: f64,
    /// Keep a just-left account read-only while Desktop flushes it.
    #[serde(deserialize_with = "de_number")]
    pub switch_grace_seconds: f64,
    pub notify: bool,
    /// Desktop app: `system`, `light` or `dark`.
    pub appearance: String,
    /// Desktop app: `system` or a language code such as `zh-CN`.
    pub language: String,
    /// Desktop app: show the menu bar / notification area icon.
    pub tray: bool,
    /// Desktop app: look for a new version on GitHub once a day.
    pub check_updates: bool,
    /// Desktop app: download new versions in the background and install them while the window
    /// is closed.
    pub auto_update: bool,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            exclude: Vec::new(),
            only: Vec::new(),
            // Off: an account signed in to later (a personal one, say) must be added on purpose.
            auto_join_new: false,
            keep_snapshots: 20,
            snapshot_every_seconds: 3600.0,
            trash_days: 30.0,
            switch_grace_seconds: 30.0,
            notify: true,
            appearance: "system".into(),
            language: "system".into(),
            tray: true,
            // Off: updates are installed by hand after reading what changed.
            check_updates: false,
            auto_update: false,
        }
    }
}

impl Config {
    pub fn load(paths: &Paths) -> Result<Config> {
        let path = paths.config_file();
        let raw = match fs::read(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        };
        serde_json::from_slice(&raw).with_context(|| format!("cannot parse {}", path.display()))
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        fsx::create_private_dir_all(&paths.state_dir)?;
        let mut body = serde_json::to_vec_pretty(self)?;
        body.push(b'\n');
        fsx::atomic_write(&paths.config_file(), &body, None, false)?;
        Ok(())
    }

    pub fn is_excluded(&self, acct: &str, org: &str) -> bool {
        let key = format!("{acct}/{org}");
        let named = |list: &[String]| list.iter().any(|e| e == acct || *e == key);
        named(&self.exclude) || (!self.only.is_empty() && !named(&self.only))
    }

    /// `exclude` and `only` entries that can never match a folder: anything but a full
    /// `<account>` or `<account>/<org>` id. A short id in `exclude` would silently leave that
    /// account syncing, so a sync refuses to run while there are any.
    pub fn invalid_entries(&self) -> Vec<String> {
        self.exclude.iter().chain(&self.only).filter(|e| !is_entry(e)).cloned().collect()
    }

    /// Fail when [`Config::invalid_entries`] finds any.
    pub fn check(&self) -> Result<()> {
        let bad = self.invalid_entries();
        anyhow::ensure!(
            bad.is_empty(),
            "config has id(s) that match no folder: {} (use full ids: `cc-same orgs` lists them)",
            bad.join(", ")
        );
        Ok(())
    }
}

/// A full `<account>` or `<account>/<org>` id.
pub fn is_entry(e: &str) -> bool {
    match e.split_once('/') {
        Some((acct, org)) => is_uuid(acct) && is_uuid(org),
        None => is_uuid(e),
    }
}

fn de_number<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    Ok(Value::deserialize(d)?.as_f64().unwrap_or(0.0))
}

fn de_count<'de, D: Deserializer<'de>>(d: D) -> Result<usize, D::Error> {
    Ok(Value::deserialize(d)?.as_f64().map(|n| n.max(0.0) as usize).unwrap_or(0))
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LastSync {
    pub at: f64,
    pub reason: String,
    pub applied: u64,
    pub errors: u64,
    pub deferred: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct State {
    pub version: u32,
    /// Snapshot taken before the very first change. Never pruned.
    pub baseline: Option<String>,
    /// surface -> partition keys that have been synced at least once.
    pub known: BTreeMap<String, Vec<String>>,
    /// surface -> collection file -> member key (or `__group__`) -> value at the last sync.
    pub bases: BTreeMap<String, BTreeMap<String, BTreeMap<String, Value>>>,
    pub last_snapshot_at: f64,
    pub last_sync: Option<LastSync>,
    /// Partitions we already told the user to restart Claude for.
    pub notified: Vec<String>,
    /// Sessions were copied into the index Claude has open; they show after it reloads.
    pub pending_restart: Option<PendingRestart>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PendingRestart {
    /// Partition keys that received sessions while loaded.
    pub keys: Vec<String>,
    pub sessions: u64,
    /// Desktop's `init_marker` at the time; a different marker means it reloaded.
    pub init_marker: Option<String>,
    pub at: f64,
}

impl State {
    pub fn load(paths: &Paths) -> State {
        let mut state: State = match fs::read(paths.state_file()) {
            Ok(raw) => serde_json::from_slice(&raw).unwrap_or_default(),
            Err(_) => State::default(),
        };
        // Up to 0.1.9, local Cowork sessions could be synced too.
        state.known.retain(|surface, _| Surface::parse(surface).is_some());
        state.bases.retain(|surface, _| Surface::parse(surface).is_some());
        state
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        fsx::create_private_dir_all(&paths.state_dir)?;
        let mut state = self.clone();
        state.version = 1;
        let mut body = serde_json::to_vec_pretty(&state)?;
        body.push(b'\n');
        fsx::atomic_write(&paths.state_file(), &body, None, false)?;
        Ok(())
    }

    pub fn known(&self, surface: Surface) -> Vec<String> {
        self.known.get(surface.as_str()).cloned().unwrap_or_default()
    }
}

/// Turn what someone typed for `exclude`/`only` into a full id: a full `<account>` or
/// `<account>/<org>` id as it is, or a prefix of one (such as the 8 characters `doctor` shows)
/// that names exactly one of the `known` account/org pairs.
pub fn resolve_entry(input: &str, known: &[(String, String)]) -> Result<String> {
    let input = input.trim().to_ascii_lowercase();
    if is_entry(&input) {
        return Ok(input);
    }
    let (acct, org) = match input.split_once('/') {
        Some((a, o)) => (a, Some(o)),
        None => (input.as_str(), None),
    };
    let prefix_ok = |s: &str| s.len() >= 4 && s.bytes().all(|b| b == b'-' || b.is_ascii_hexdigit());
    anyhow::ensure!(
        prefix_ok(acct) && org.is_none_or(prefix_ok),
        "`{input}` is not an id: use `<account>` or `<account>/<org>` (`cc-same orgs` lists them)"
    );
    let mut found: Vec<String> = known
        .iter()
        .filter(|(a, o)| a.starts_with(acct) && org.is_none_or(|p| o.starts_with(p)))
        .map(|(a, o)| if org.is_some() { format!("{a}/{o}") } else { a.clone() })
        .collect();
    found.sort();
    found.dedup();
    match found.len() {
        1 => Ok(found.remove(0)),
        0 => anyhow::bail!("`{input}` matches no account or org on this computer (`cc-same orgs` lists them)"),
        _ => anyhow::bail!("`{input}` matches more than one: {}", found.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "11111111-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const B: &str = "11111111-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    const O1: &str = "22222222-1111-4111-8111-111111111111";
    const O2: &str = "33333333-2222-4222-8222-222222222222";

    fn known() -> Vec<(String, String)> {
        vec![(A.into(), O1.into()), (A.into(), O2.into()), (B.into(), O1.into())]
    }

    #[test]
    fn a_short_exclude_stops_the_sync_instead_of_matching_nothing() {
        let cfg = Config { exclude: vec!["11111111".into(), format!("{A}/{O1}")], ..Config::default() };
        assert_eq!(cfg.invalid_entries(), vec!["11111111".to_string()]);
        assert!(cfg.check().is_err());
        let cfg = Config { exclude: vec![A.into(), format!("{B}/{O1}")], ..Config::default() };
        assert!(cfg.check().is_ok());
    }

    #[test]
    fn only_keeps_everything_else_out() {
        let cfg = Config { only: vec![format!("{A}/{O1}"), B.into()], ..Config::default() };
        assert!(!cfg.is_excluded(A, O1));
        assert!(cfg.is_excluded(A, O2));
        assert!(!cfg.is_excluded(B, O2));
        assert!(cfg.is_excluded("44444444-4444-4444-8444-444444444444", O1));
        let cfg = Config { only: vec![format!("{A}/{O1}")], exclude: vec![A.into()], ..Config::default() };
        assert!(cfg.is_excluded(A, O1), "exclude wins over only");
    }

    #[test]
    fn prefixes_resolve_only_when_they_name_one_folder() {
        let k = known();
        assert_eq!(resolve_entry(&A.to_uppercase(), &k).unwrap(), A);
        assert_eq!(resolve_entry("11111111-aaaa", &k).unwrap(), A);
        assert_eq!(resolve_entry("11111111-a/3333", &k).unwrap(), format!("{A}/{O2}"));
        assert!(resolve_entry("11111111", &k).is_err(), "two accounts share it");
        assert!(resolve_entry("11111111-aaaa/", &k).is_err());
        assert!(resolve_entry("9999", &k).is_err());
        assert!(resolve_entry("../x", &k).is_err());
        assert!(resolve_entry("abc", &k).is_err());
    }
}
