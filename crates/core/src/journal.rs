//! Undoing a sync. Before a sync changes a file, a copy-on-write clone of the old one is kept,
//! and every change of the run is written down: which file, the clone of what it was before (or
//! none: the sync created it), and what it looked like after. `undo` walks a run backwards and
//! puts each file back, skipping any that changed again since, so nothing done after the sync
//! is lost. What `undo` takes away goes to the trash.
//!
//! `<state>/undo/<run>/journal.json` and `<state>/undo/<run>/files/<n>`.

use crate::apply::Lock;
use crate::config::State;
use crate::ctx::Ctx;
use crate::desktop;
use crate::fsx;
use crate::snapshot;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

/// Runs kept for undo; older ones also go once they are older than `trashDays`.
pub const KEEP_RUNS: usize = 50;

/// What a file looked like: modification time (ns) and size; `None` when it did not exist.
pub type Look = Option<(i128, u64)>;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    /// Relative to Desktop's data folder.
    pub path: String,
    /// The clone of the file as it was, under `files/`; `None` when the sync created it.
    pub before: Option<String>,
    /// The file right after the sync changed it.
    pub after: Look,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Journal {
    pub id: String,
    pub reason: String,
    /// Seconds since the Unix epoch.
    pub created: f64,
    pub entries: Vec<Entry>,
    #[serde(default)]
    pub undone_at: Option<f64>,
}

fn root(ctx: &Ctx) -> PathBuf {
    ctx.paths.state_dir.join("undo")
}

pub fn look(path: &Path) -> Look {
    fs::symlink_metadata(path).ok().filter(|m| m.is_file()).map(|m| (fsx::mtime_ns(&m), m.len()))
}

/// Writes down the changes of one sync run.
pub struct Recorder {
    journal: Journal,
    dir: PathBuf,
    user_data: PathBuf,
    /// Clones kept so far (their names).
    kept: usize,
}

impl Recorder {
    pub fn new(ctx: &Ctx, reason: &str) -> Recorder {
        let base = format!("{}-{reason}", snapshot::stamp());
        let dir = fsx::unique_path(root(ctx).join(&base));
        let id = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(base);
        Recorder {
            journal: Journal {
                id,
                reason: reason.into(),
                created: fsx::now_secs(),
                entries: Vec::new(),
                undone_at: None,
            },
            dir,
            user_data: ctx.paths.user_data.clone(),
            kept: 0,
        }
    }

    /// Keep a clone of `target` as it is now, before a change. Returns what to pass to
    /// [`Recorder::changed`] or [`Recorder::unchanged`].
    pub fn before(&mut self, target: &Path) -> Result<Option<String>> {
        if look(target).is_none() {
            return Ok(None);
        }
        let files = self.dir.join("files");
        fsx::create_private_dir_all(&files)?;
        self.kept += 1;
        let name = self.kept.to_string();
        fsx::clone_file(target, &files.join(&name))
            .with_context(|| format!("keeping {} for undo", target.display()))?;
        Ok(Some(name))
    }

    /// The change was made.
    pub fn changed(&mut self, target: &Path, before: Option<String>) {
        let path = target.strip_prefix(&self.user_data).unwrap_or(target).to_string_lossy().into_owned();
        self.journal.entries.push(Entry { path, before, after: look(target) });
    }

    /// The change was not made: drop the clone.
    pub fn unchanged(&mut self, before: Option<String>) {
        if let Some(name) = before {
            let _ = fs::remove_file(self.dir.join("files").join(name));
        }
    }

    /// Save the journal if the run changed anything.
    pub fn finish(self, ctx: &Ctx) -> Result<Option<String>> {
        if self.journal.entries.is_empty() {
            let _ = fs::remove_dir_all(&self.dir);
            return Ok(None);
        }
        fsx::create_private_dir_all(&self.dir)?;
        let body = serde_json::to_vec_pretty(&self.journal)?;
        fsx::atomic_write(&self.dir.join("journal.json"), &body, None, false)?;
        prune(ctx);
        Ok(Some(self.journal.id))
    }
}

/// Every run that can be undone or was, oldest first.
pub fn list(ctx: &Ctx) -> Vec<Journal> {
    let Ok(rd) = fs::read_dir(root(ctx)) else { return Vec::new() };
    let mut out: Vec<Journal> = rd
        .flatten()
        .filter_map(|e| fsx::read_json(&e.path().join("journal.json"), 64 * 1024 * 1024).ok())
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect();
    out.sort_by(|a: &Journal, b: &Journal| a.created.total_cmp(&b.created).then(a.id.cmp(&b.id)));
    out
}

/// Keep the newest [`KEEP_RUNS`] runs, and none older than `trashDays`.
pub fn prune(ctx: &Ctx) {
    let runs = list(ctx);
    let max_age = ctx.config().trash_days.max(0.0) * 86400.0;
    let excess = runs.len().saturating_sub(KEEP_RUNS);
    for (i, j) in runs.iter().enumerate() {
        if i < excess || fsx::now_secs() - j.created > max_age {
            let _ = fs::remove_dir_all(root(ctx).join(&j.id));
        }
    }
}

/// What an undo did, or would do.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UndoReport {
    pub id: String,
    /// Files put back as they were before the sync.
    pub restored: Vec<String>,
    /// Files the sync had created, moved to the trash.
    pub removed: Vec<String>,
    /// Files that changed again after the sync: left as they are.
    pub skipped: Vec<String>,
}

/// A relative path inside Desktop's data folder, or an error.
fn inside(user_data: &Path, rel: &str) -> Result<PathBuf> {
    let rel = Path::new(rel);
    anyhow::ensure!(
        !rel.as_os_str().is_empty() && rel.components().all(|c| matches!(c, Component::Normal(_))),
        "odd path in the undo journal: {}",
        rel.display()
    );
    Ok(user_data.join(rel))
}

/// Undo run `id` (default: the newest one not undone yet). With `dry_run`, only report.
/// Claude must be quit. Afterwards syncing is paused: the other orgs still have what the run
/// copied, so the next sync would do it all again.
pub fn undo(ctx: &Ctx, id: Option<&str>, dry_run: bool) -> Result<UndoReport> {
    let runs = list(ctx);
    let journal = match id {
        Some(id) => runs.into_iter().find(|j| j.id == id).with_context(|| format!("no such sync to undo: {id}"))?,
        None => runs.into_iter().rev().find(|j| j.undone_at.is_none()).context("no sync to undo")?,
    };
    if journal.undone_at.is_some() {
        bail!("sync {} was undone already", journal.id);
    }
    if !dry_run && desktop::is_running(ctx) {
        bail!("quit Claude first, then try again");
    }
    let _lock = if dry_run { None } else { Some(Lock::acquire(ctx, Duration::from_secs(30))?) };
    let dir = root(ctx).join(&journal.id);
    let stamp = snapshot::stamp();
    let mut report = UndoReport { id: journal.id.clone(), ..UndoReport::default() };
    for e in journal.entries.iter().rev() {
        let path = inside(&ctx.paths.user_data, &e.path)?;
        if look(&path) != e.after {
            report.skipped.push(e.path.clone());
            continue;
        }
        match &e.before {
            Some(name) => {
                anyhow::ensure!(
                    !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit()),
                    "odd file in the undo journal: {name}"
                );
                if !dry_run {
                    put_back(ctx, &path, &dir.join("files").join(name), &e.path, &stamp)?;
                }
                report.restored.push(e.path.clone());
            }
            None => {
                if !dry_run {
                    fsx::move_path(&path, &trash(ctx, &e.path, &stamp))?;
                }
                report.removed.push(e.path.clone());
            }
        }
    }
    if dry_run {
        return Ok(report);
    }
    let mut done = journal;
    done.undone_at = Some(fsx::now_secs());
    fsx::atomic_write(&dir.join("journal.json"), &serde_json::to_vec_pretty(&done)?, None, false)?;
    let mut cfg = ctx.config();
    cfg.paused = true;
    cfg.save(&ctx.paths)?;
    ctx.set_config(cfg);
    let mut state = State::load(&ctx.paths);
    state.bases.clear(); // re-learn task-list bases from the files as they are now
    state.save(&ctx.paths)?;
    Ok(report)
}

fn trash(ctx: &Ctx, rel: &str, stamp: &str) -> PathBuf {
    fsx::unique_path(ctx.paths.trash_dir().join(stamp).join("undone").join(rel))
}

/// Replace `path` with a clone of `backup` (its old content and time); the current file goes to
/// the trash.
fn put_back(ctx: &Ctx, path: &Path, backup: &Path, rel: &str, stamp: &str) -> Result<()> {
    let parent = path.parent().context("no parent folder")?;
    fsx::ensure_real_dir(parent, &ctx.paths.user_data)?;
    let tmp = fsx::unique_path(parent.join(format!("{}undo", fsx::TMP_PREFIX)));
    fsx::clone_file(backup, &tmp).with_context(|| format!("restoring {rel}"))?;
    if look(path).is_some() {
        if let Err(e) = fsx::move_path(path, &trash(ctx, rel, stamp)) {
            let _ = fs::remove_file(&tmp);
            return Err(e.into());
        }
    }
    fs::rename(&tmp, path).with_context(|| format!("restoring {rel}"))?;
    Ok(())
}
