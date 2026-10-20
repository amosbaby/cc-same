//! The background agent: notice changes, wait for Desktop to finish writing, sync.

use crate::apply;
use crate::config::State;
use crate::ctx::Ctx;
use crate::desktop;
use crate::fsx;
use crate::model::Surface;
use crate::notify::notify;
use crate::scan;
use crate::service::Heartbeat;
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct WatchOptions {
    pub interval: Duration,
    /// Stop after this many passes (tests).
    pub iterations: Option<u64>,
}

impl Default for WatchOptions {
    fn default() -> WatchOptions {
        WatchOptions { interval: Duration::from_secs(2), iterations: None }
    }
}

/// Cheap change detector: index folder listings with mtimes and sizes, Desktop's
/// `config.json`, and whether Desktop runs. Desktop's log is left out: it grows constantly.
pub fn fingerprint(ctx: &Ctx, running: bool) -> u64 {
    let mut h = DefaultHasher::new();
    running.hash(&mut h);
    if let Ok(md) = fs::metadata(ctx.paths.desktop_config()) {
        fsx::mtime_ns(&md).hash(&mut h);
    }
    for surface in Surface::ALL {
        for p in scan::discover(ctx, surface) {
            p.path.hash(&mut h);
            p.is_link.hash(&mut h);
            let Ok(rd) = fs::read_dir(&p.path) else { continue };
            let mut entries: Vec<(String, i128, u64)> = rd
                .flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if name.starts_with(fsx::TMP_PREFIX) {
                        return None;
                    }
                    let md = fs::symlink_metadata(e.path()).ok()?;
                    Some((name, fsx::mtime_ns(&md), md.len()))
                })
                .collect();
            entries.sort();
            entries.hash(&mut h);
            if let Ok(md) = fs::symlink_metadata(p.path.join("backlog").join("tasks.json")) {
                (fsx::mtime_ns(&md), md.len()).hash(&mut h);
            }
        }
    }
    h.finish()
}

/// A brand-new account has no index folder until its first session is saved. Desktop's log
/// already names its account and org, so create the folder and let first-join seeding fill it.
pub fn ensure_active_partition(ctx: &Ctx, app: &crate::model::AppState, state: &State) {
    let cfg = ctx.config();
    if !app.running || !cfg.auto_join_new || state.known(Surface::Code).is_empty() {
        return;
    }
    for (acct, org) in &app.pairs {
        if cfg.is_excluded(acct, org) {
            continue;
        }
        let path = ctx.paths.surface_root(Surface::Code).join(acct).join(org);
        if fs::symlink_metadata(&path).is_ok() {
            continue;
        }
        match fsx::ensure_real_dir(&path, &ctx.paths.user_data) {
            Ok(()) => ctx.log(format!("created index folder for new account {}/{}", &acct[..8], &org[..8])),
            Err(e) => ctx.log(format!("cannot create index folder {}: {e}", path.display())),
        }
    }
}

/// Entry point for background-agent binaries: `[--user-data <dir>] [--state-dir <dir>] [watch]`.
/// Logs to the log file and runs until the process is stopped.
pub fn run_agent(args: impl IntoIterator<Item = std::ffi::OsString>) {
    use crate::{Config, Ctx, FakeDesktop, LogSink, Paths};
    use std::path::PathBuf;
    let mut user_data: Option<PathBuf> = None;
    let mut state_dir: Option<PathBuf> = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--user-data") => user_data = args.next().map(PathBuf::from),
            Some("--state-dir") => state_dir = args.next().map(PathBuf::from),
            _ => {}
        }
    }
    let detected = Paths::detect();
    let paths = Paths::new(user_data.unwrap_or(detected.user_data), state_dir.unwrap_or(detected.state_dir));
    // Unreadable settings must not mean default settings (no `only` list): nothing syncs until
    // they can be read again.
    let (config, problem) = match Config::load(&paths) {
        Ok(c) => (c, None),
        Err(e) => (Config { paused: true, ..Config::default() }, Some(format!("{e:#}"))),
    };
    let log = LogSink::File(paths.log_file.clone());
    let ctx = Ctx::new(paths, config, FakeDesktop::from_env(), log);
    *ctx.config_problem.lock().unwrap() = problem;
    run(&ctx, &WatchOptions::default(), &AtomicBool::new(false));
}

const ALERT_CONFIG: &str = "config";
const ALERT_SYNC: &str = "sync";
const ALERT_UNREADABLE: &str = "unreadable";
const ALERT_PAUSED: &str = "paused";
/// When the agent first saw syncing paused (not itself notified).
const ALERT_PAUSED_SINCE: &str = "paused-since";
/// A pause longer than this is pointed out once.
const LONG_PAUSE: f64 = 24.0 * 3600.0;

/// Remember the notification language while the settings can be read.
fn remember_language(ctx: &Ctx) {
    let chinese = ctx.config().chinese();
    let mut state = State::load(&ctx.paths);
    if state.chinese != Some(chinese) {
        state.chinese = Some(chinese);
        let _ = state.save(&ctx.paths);
    }
}

/// Notify about a problem once, until [`clear_alert`] says it is gone.
fn alert(ctx: &Ctx, key: &str, en: &str, zh: &str) {
    let mut state = State::load(&ctx.paths);
    if state.alerts.contains_key(key) {
        return;
    }
    let chinese = if ctx.config_problem.lock().unwrap().is_some() {
        state.chinese.unwrap_or_else(|| ctx.config().chinese())
    } else {
        ctx.config().chinese()
    };
    notify(ctx, if chinese { zh } else { en });
    state.alerts.insert(key.into(), fsx::now_secs());
    let _ = state.save(&ctx.paths);
}

fn clear_alert(ctx: &Ctx, key: &str) {
    let mut state = State::load(&ctx.paths);
    if state.alerts.remove(key).is_some() {
        let _ = state.save(&ctx.paths);
    }
}

fn sync_failed(ctx: &Ctx, error: &str) {
    let short: String = error.chars().take(160).collect();
    alert(
        ctx,
        ALERT_SYNC,
        &format!("CC Same could not sync: {short}. Run `cc-same doctor` to see more."),
        &format!("CC Same 同步失败：{short}。运行 `cc-same doctor` 查看详情。"),
    );
}

/// Session files that cannot be read usually mean Claude Desktop changed its format.
fn check_unreadable(ctx: &Ctx, plan: &crate::model::Plan) {
    if plan.notes.iter().any(|n| n.contains("unreadable") || n.contains("skipped (")) {
        alert(
            ctx,
            ALERT_UNREADABLE,
            "Some Claude session files cannot be read and are left alone; Claude Desktop may have changed their format. Run `cc-same doctor`.",
            "有些 Claude 会话文件读不了，已跳过不处理；可能是 Claude Desktop 升级后改了格式。运行 `cc-same doctor` 查看。",
        );
    } else {
        clear_alert(ctx, ALERT_UNREADABLE);
    }
}

/// Point out once that syncing has been paused for a day.
fn check_long_pause(ctx: &Ctx) {
    let mut state = State::load(&ctx.paths);
    let since = *state.alerts.entry(ALERT_PAUSED_SINCE.into()).or_insert_with(fsx::now_secs);
    let _ = state.save(&ctx.paths);
    if fsx::now_secs() - since > LONG_PAUSE {
        alert(
            ctx,
            ALERT_PAUSED,
            "CC Same has been paused for a day; your orgs are not kept in sync. `cc-same resume` turns it back on.",
            "CC Same 已暂停超过一天，组织之间没有在同步。用 `cc-same resume` 恢复。",
        );
    }
}

fn sleep_while(stop: &AtomicBool, total: Duration) {
    let deadline = Instant::now() + total;
    while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100).min(deadline.saturating_duration_since(Instant::now())));
    }
}

/// Run until `stop` is set (or forever). Errors are logged and the loop keeps going.
pub fn run(ctx: &Ctx, opts: &WatchOptions, stop: &AtomicBool) {
    ctx.log(format!("watch started (pid {}, v{})", std::process::id(), crate::VERSION));
    let full_every = Duration::from_secs(60); // also picks up accounts leaving the grace period
    let started = fsx::now_secs();
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let mut last_fp: Option<u64> = None;
    let mut last_full: Option<Instant> = None;
    let mut last_running: Option<bool> = None;
    let mut passes = 0u64;
    let mut paused_logged = false;
    let mut last_config_error: Option<String> = None;
    let mut was_blocked = false;
    // The heartbeat has a thread of its own, from the start: a long first sync must not make a
    // working agent look dead to the app.
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            while !stop.load(Ordering::Relaxed) && !done.load(Ordering::Relaxed) {
                Heartbeat {
                    pid: std::process::id(),
                    version: crate::VERSION.into(),
                    exe: exe.clone(),
                    started_at: started,
                    heartbeat_at: fsx::now_secs(),
                }
                .write(&ctx.paths);
                let next = Instant::now() + Duration::from_secs(15);
                while Instant::now() < next && !stop.load(Ordering::Relaxed) && !done.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        });
        while !stop.load(Ordering::Relaxed) {
            passes += 1;
            match ctx.reload_config() {
                Ok(()) => {
                    last_config_error = None;
                    clear_alert(ctx, ALERT_CONFIG);
                    remember_language(ctx);
                }
                Err(e) => {
                    let msg = format!("config: {e:#}");
                    if last_config_error.as_deref() != Some(msg.as_str()) {
                        ctx.log(&msg);
                        last_config_error = Some(msg);
                    }
                    alert(
                        ctx,
                        ALERT_CONFIG,
                        "CC Same cannot read its settings, so it stopped syncing. Run `cc-same doctor` to see why.",
                        "CC Same 读不了自己的配置文件，已停止同步。运行 `cc-same doctor` 查看原因。",
                    );
                }
            }
            let config_ok = ctx.config_problem.lock().unwrap().is_none();
            // Back from a pause or from unreadable settings: sync now, not at the next change.
            let blocked = !config_ok || ctx.config().paused;
            if was_blocked && !blocked {
                last_fp = None;
            }
            was_blocked = blocked;
            let running = desktop::is_running(ctx);
            let mut fp = fingerprint(ctx, running);
            let quit_edge = last_running == Some(true) && !running;
            if last_fp != Some(fp) || last_full.is_none_or(|t| t.elapsed() > full_every) {
                if !quit_edge && last_fp.is_some() {
                    // Let Desktop finish a burst of writes before copying anything.
                    let deadline = Instant::now() + Duration::from_secs(6);
                    while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
                        sleep_while(stop, Duration::from_millis(800));
                        let next = fingerprint(ctx, running);
                        if next == fp {
                            break;
                        }
                        fp = next;
                    }
                }
                if !config_ok || ctx.config().paused {
                    if config_ok {
                        check_long_pause(ctx);
                    }
                    if !paused_logged {
                        ctx.log(if config_ok {
                            "syncing is paused; `cc-same resume` turns it back on"
                        } else {
                            "not syncing: the settings cannot be read"
                        });
                        paused_logged = true;
                    }
                    last_fp = Some(fp);
                    last_full = Some(Instant::now());
                    last_running = Some(running);
                    if opts.iterations.is_some_and(|max| passes >= max) {
                        break;
                    }
                    sleep_while(stop, opts.interval);
                    continue;
                }
                paused_logged = false;
                clear_alert(ctx, ALERT_PAUSED_SINCE);
                clear_alert(ctx, ALERT_PAUSED);
                let state = State::load(&ctx.paths);
                let app = desktop::detect(ctx);
                ensure_active_partition(ctx, &app, &state);
                // An account Claude signs in to joins the list even with no window open.
                crate::accounts::observe(ctx);
                match apply::run_sync(ctx, "watch") {
                    Ok((plan, out)) => {
                        if out.errors.is_empty() {
                            clear_alert(ctx, ALERT_SYNC);
                        } else {
                            sync_failed(ctx, &out.errors[0]);
                        }
                        check_unreadable(ctx, &plan);
                        if out.applied_total() > 0 || !out.errors.is_empty() {
                            let kinds: Vec<String> = out.applied.iter().map(|(k, n)| format!("{k:?} {n}")).collect();
                            ctx.log(format!(
                                "sync: {}{}",
                                kinds.join(", "),
                                if out.errors.is_empty() {
                                    String::new()
                                } else {
                                    format!(", {} error(s)", out.errors.len())
                                }
                            ));
                        }
                        for (key, n) in &out.seeded_while_loaded {
                            let mut st = State::load(&ctx.paths);
                            if *n > 0 && !st.notified.contains(key) {
                                let text = if ctx.config().chinese() {
                                    format!("已把其他组织的 {n} 个会话复制进 Claude 当前打开的组织。退出并重新打开 Claude 就能看到。")
                                } else {
                                    format!("Copied {n} session(s) from your other accounts into the one Claude has open. Quit and reopen Claude to see them.")
                                };
                                notify(ctx, &text);
                                st.notified.push(key.clone());
                                let _ = st.save(&ctx.paths);
                            }
                        }
                    }
                    Err(e) => {
                        ctx.log(format!("watch: {e:#}"));
                        sync_failed(ctx, &format!("{e:#}"));
                    }
                }
                // Remember what this pass started from, not what it left: a change made while it
                // ran must trigger the next pass. Our own writes do too, and that pass finds nothing.
                last_fp = Some(fp);
                last_full = Some(Instant::now());
            }
            if last_running.is_some_and(|r| r != running) {
                ctx.log(if running { "Claude started" } else { "Claude quit" });
            }
            last_running = Some(running);
            if opts.iterations.is_some_and(|max| passes >= max) {
                break;
            }
            sleep_while(stop, opts.interval);
        }
        done.store(true, Ordering::Relaxed);
    });
}
