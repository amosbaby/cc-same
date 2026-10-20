//! `cc-same`: keep Claude Desktop's local sessions identical across all your accounts.

use anyhow::{bail, Result};
use cc_same_core::accounts::{self, Roster};
#[cfg(feature = "account-switching")]
use cc_same_core::accounts::{NotFound, Slot, Strategy};
use cc_same_core::desktop;
#[cfg(feature = "account-switching")]
use cc_same_core::desktop::Quitting;
use cc_same_core::logins;
use cc_same_core::report::{self, Overview, Warning};
use cc_same_core::retention::{self, Kept, Limit};
use cc_same_core::service::{self, Heartbeat};
use cc_same_core::usage::Usage;
use cc_same_core::watch::{self, WatchOptions};
use cc_same_core::{apply, fsx, plan, scan, short, snapshot, Config, Ctx, FakeDesktop, LogSink, Paths, State, Surface};
use clap::{Parser, Subcommand, ValueEnum};
use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "cc-same",
    version,
    about = "Keep Claude Desktop's local sessions identical across all your accounts."
)]
struct Cli {
    /// Claude Desktop's data folder (default: this platform's location)
    #[arg(long, global = true, env = "CC_SAME_USER_DATA", value_name = "DIR")]
    user_data: Option<PathBuf>,
    /// Where cc-same keeps its config, snapshots and trash
    #[arg(long, global = true, env = "CC_SAME_STATE_DIR", value_name = "DIR")]
    state_dir: Option<PathBuf>,
    #[arg(short, long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Clone, Copy, ValueEnum)]
enum OnOff {
    On,
    Off,
}

#[derive(Subcommand)]
enum Cmd {
    /// Read-only check of every account's session index
    Doctor,
    /// Show what a sync would change (dry run)
    Plan,
    /// Sync once
    Sync {
        #[arg(short, long)]
        yes: bool,
    },
    /// Keep syncing (the background agent runs this)
    Watch {
        /// Seconds between checks
        #[arg(long, default_value_t = 2.0)]
        interval: f64,
        #[arg(long, hide = true)]
        iterations: Option<u64>,
        /// Log to the terminal instead of the log file
        #[arg(long)]
        foreground: bool,
    },
    /// Sync now, then keep syncing in the background from login
    Install {
        #[arg(short, long)]
        yes: bool,
    },
    /// Stop background syncing (every account keeps its full copy)
    Uninstall,
    /// Background sync and last sync
    Status,
    /// List snapshots
    Snapshots,
    /// Put every session index back as it was in a snapshot (quit Claude first)
    Restore {
        snapshot: String,
        #[arg(short, long)]
        yes: bool,
    },
    /// Turn symlinked index folders left by other tools into real folders (quit Claude first)
    FixSymlinks {
        #[arg(short, long)]
        yes: bool,
    },
    /// Show how long Claude Code keeps transcripts; keep them, undo that, or set cleanupPeriodDays
    Retention {
        /// Set cleanupPeriodDays (terminal sessions and other data)
        days: Option<u32>,
        /// Keep the transcripts of Desktop sessions (only needed when Claude Code would delete them)
        #[arg(long, conflicts_with_all = ["days", "undo"])]
        keep: bool,
        /// Put back the setting CC Same changed
        #[arg(long, conflicts_with = "days")]
        undo: bool,
    },
    /// Your accounts, numbered: which one Claude is signed in to, and what each has used
    #[command(visible_alias = "list")]
    Accounts {
        #[arg(long)]
        json: bool,
    },
    #[cfg(feature = "account-switching")]
    /// Switch Claude to another account, or the next one (Claude restarts if it is open)
    Switch {
        /// Its number, alias or email (default: the next account in the list)
        account: Option<String>,
        /// Without an account: the next one, the next one with plan left, or the one with the most left
        #[arg(long, value_enum, conflicts_with = "account")]
        strategy: Option<StrategyArg>,
        #[arg(long)]
        json: bool,
    },
    #[cfg(feature = "account-switching")]
    /// Restart Claude signed out, to add an account; the current one is kept
    #[command(alias = "sign-in")]
    Add,
    #[cfg(feature = "account-switching")]
    /// Take an account off the list and forget the sign-in kept for it (its sessions stay)
    #[command(alias = "forget")]
    Remove {
        /// Its number, alias or email
        account: String,
        #[arg(short, long)]
        yes: bool,
    },
    #[cfg(feature = "account-switching")]
    /// Give an account a short name, or list them
    Alias {
        /// Its number, alias or email
        account: Option<String>,
        /// The new alias
        name: Option<String>,
        /// Take the alias away
        #[arg(long, requires = "account", conflicts_with = "name")]
        unset: bool,
    },
    #[cfg(feature = "account-switching")]
    /// Leave an account out of `switch` without an account (it can still be named)
    Disable { account: String },
    #[cfg(feature = "account-switching")]
    /// Put an account back in the rotation
    Enable { account: String },
    #[cfg(feature = "account-switching")]
    /// Give an account another number (the account that has it takes the old one)
    Move { account: String, number: u32 },
    /// Every account/org folder on this computer, with full ids
    Orgs {
        #[arg(long, conflicts_with = "tsv")]
        json: bool,
        /// One tab-separated line per org: id, name, email, sessions, syncs, open, recent titles
        /// (`-` for an empty field)
        #[arg(long)]
        tsv: bool,
    },
    /// Show or change settings
    Config {
        /// Keep an account (or `<account>/<org>`) separate. A unique prefix is expanded to the full id
        #[arg(long, value_name = "ID")]
        exclude: Vec<String>,
        /// Undo an --exclude
        #[arg(long, value_name = "ID")]
        include: Vec<String>,
        /// Sync only these accounts (or `<account>/<org>`); everything else stays separate,
        /// including accounts that appear later. A unique prefix is expanded to the full id
        #[arg(long, value_name = "ID")]
        only: Vec<String>,
        /// Empty the --only list (every account not excluded takes part again)
        #[arg(long)]
        clear_only: bool,
        /// Name an org so lists show it: `<account>/<org>=<name>` (a unique prefix is enough)
        #[arg(long, value_name = "ID=NAME")]
        name: Vec<String>,
        /// Take a name away
        #[arg(long, value_name = "ID")]
        unname: Vec<String>,
        /// Let accounts that appear later join automatically
        #[arg(long)]
        auto_join: Option<OnOff>,
        /// Desktop notifications from the background agent
        #[arg(long)]
        notify: Option<OnOff>,
    },
}

#[cfg(feature = "account-switching")]
#[derive(Clone, Copy, ValueEnum)]
enum StrategyArg {
    Next,
    NextAvailable,
    Best,
}

#[cfg(feature = "account-switching")]
impl From<StrategyArg> for Strategy {
    fn from(s: StrategyArg) -> Strategy {
        match s {
            StrategyArg::Next => Strategy::Next,
            StrategyArg::NextAvailable => Strategy::NextAvailable,
            StrategyArg::Best => Strategy::Best,
        }
    }
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("cc-same: {e:#}");
        std::process::exit(2);
    }
}

fn run(cli: Cli) -> Result<()> {
    let detected = Paths::detect();
    let paths = Paths::new(cli.user_data.unwrap_or(detected.user_data), cli.state_dir.unwrap_or(detected.state_dir));
    let config = Config::load(&paths)?;
    let mut ctx = Ctx::new(paths, config, FakeDesktop::from_env(), LogSink::Stderr);
    ctx.verbose = cli.verbose;
    let Some(cmd) = cli.cmd else {
        return doctor(&ctx);
    };
    match cmd {
        Cmd::Doctor => doctor(&ctx),
        Cmd::Plan => show_plan(&ctx, cli.verbose),
        Cmd::Sync { yes } => sync(&ctx, yes),
        Cmd::Watch { interval, iterations, foreground } => {
            if !foreground {
                ctx.logger.set_sink(LogSink::File(ctx.paths.log_file.clone()));
            }
            let opts = WatchOptions { interval: Duration::from_secs_f64(interval.max(0.2)), iterations };
            watch::run(&ctx, &opts, &AtomicBool::new(false));
            Ok(())
        }
        Cmd::Install { yes } => install(&ctx, yes),
        Cmd::Uninstall => {
            service::uninstall(&ctx)?;
            println!("Background sync removed. Every account keeps its full copy of your sessions.");
            println!("Snapshots and trash stay in {}", ctx.paths.state_dir.display());
            Ok(())
        }
        Cmd::Status => status(&ctx),
        Cmd::Snapshots => snapshots(&ctx),
        Cmd::Restore { snapshot, yes } => {
            let manifest = snapshot::list(&ctx).into_iter().find(|m| m.id == snapshot);
            let Some(m) = manifest else { bail!("no such snapshot: {snapshot} (see `cc-same snapshots`)") };
            println!(
                "Restore {} session folder(s) from {}. The current ones go to the trash first.",
                m.partitions.len(),
                m.id
            );
            if !yes && !confirm("Continue?") {
                return Ok(());
            }
            let pre = snapshot::restore(&ctx, &m.id)?;
            println!("Restored. The state before this restore is snapshot {pre}.");
            Ok(())
        }
        Cmd::FixSymlinks { yes } => {
            let todo = snapshot::symlinked_folders(&ctx);
            if todo.is_empty() {
                println!("No symlinked session folders.");
                return Ok(());
            }
            for t in &todo {
                let target = std::fs::read_link(t).map(|p| p.display().to_string()).unwrap_or_default();
                println!("  {} -> {target}", t.strip_prefix(&ctx.paths.user_data).unwrap_or(t).display());
            }
            println!("Each link becomes a real folder holding a copy of what it points to.");
            if !yes && !confirm("Continue?") {
                return Ok(());
            }
            let fixed = snapshot::fix_symlinks(&ctx)?;
            println!("Fixed {} folder(s). Run `cc-same sync` to merge them.", fixed.len());
            Ok(())
        }
        Cmd::Retention { days, keep, undo } => retention(&ctx, days, keep, undo),
        Cmd::Accounts { json } => accounts(&ctx, json),
        #[cfg(feature = "account-switching")]
        Cmd::Switch { account, strategy, json } => switch(&ctx, account.as_deref(), strategy.map(Into::into), json),
        #[cfg(feature = "account-switching")]
        Cmd::Add => add(&ctx),
        #[cfg(feature = "account-switching")]
        Cmd::Remove { account, yes } => remove(&ctx, &account, yes),
        #[cfg(feature = "account-switching")]
        Cmd::Alias { account, name, unset } => alias(&ctx, account.as_deref(), name.as_deref(), unset),
        #[cfg(feature = "account-switching")]
        Cmd::Disable { account } => sit_out(&ctx, &account, true),
        #[cfg(feature = "account-switching")]
        Cmd::Enable { account } => sit_out(&ctx, &account, false),
        #[cfg(feature = "account-switching")]
        Cmd::Move { account, number } => move_account(&ctx, &account, number),
        Cmd::Orgs { json, tsv } => orgs(&ctx, json, tsv),
        Cmd::Config { exclude, include, only, clear_only, name, unname, auto_join, notify } => {
            let mut cfg = ctx.config();
            let before = cfg.clone();
            let known = known_pairs(&ctx);
            for id in exclude {
                let id = resolve(&id, &known)?;
                if !cfg.exclude.contains(&id) {
                    cfg.exclude.push(id);
                }
            }
            let include: Vec<String> = include.iter().map(|i| i.trim().to_ascii_lowercase()).collect();
            cfg.exclude.retain(|e| !include.contains(e));
            if clear_only {
                cfg.only.clear();
            }
            for id in only {
                let id = resolve(&id, &known)?;
                if !cfg.only.contains(&id) {
                    cfg.only.push(id);
                }
            }
            for pair in name {
                let Some((id, label)) = pair.split_once('=') else { bail!("--name takes <account>/<org>=<name>") };
                let label = label.trim();
                anyhow::ensure!(!label.is_empty() && !label.contains(['\t', '\n', '\r']), "--name: empty or odd name");
                let id = resolve(id, &known)?;
                anyhow::ensure!(id.contains('/'), "--name takes <account>/<org>, not an account alone: {id}");
                cfg.names.insert(id, label.to_string());
            }
            for id in unname {
                let id = resolve(&id, &known)?;
                cfg.names.remove(&id);
            }
            if let Some(v) = auto_join {
                cfg.auto_join_new = matches!(v, OnOff::On);
            }
            if let Some(v) = notify {
                cfg.notify = matches!(v, OnOff::On);
            }
            if cfg != before {
                cfg.save(&ctx.paths)?;
            }
            println!("{}", serde_json::to_string_pretty(&cfg)?);
            for w in cfg.invalid_entries() {
                eprintln!("! `{w}` is not a full id and matches nothing; syncing stays stopped until it is removed");
            }
            Ok(())
        }
    }
}

/// Every account/org pair this computer knows of: the session folders, and the pairs in
/// Claude's log (an account that has no folder yet).
fn known_pairs(ctx: &Ctx) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = Surface::ALL
        .iter()
        .flat_map(|s| scan::discover(ctx, *s))
        .map(|p| (p.acct, p.org))
        .chain(desktop::detect(ctx).pairs)
        .chain(desktop::last_init_pair(ctx))
        .collect();
    pairs.sort();
    pairs.dedup();
    pairs
}

fn resolve(input: &str, known: &[(String, String)]) -> Result<String> {
    let id = cc_same_core::config::resolve_entry(input, known)?;
    if id != input.trim().to_ascii_lowercase() {
        eprintln!("{input} -> {id}");
    } else if !known.iter().any(|(a, o)| id == *a || id == format!("{a}/{o}")) {
        eprintln!("note: {id} is not on this computer yet; it applies once it appears");
    }
    Ok(id)
}

fn orgs(ctx: &Ctx, json: bool, tsv: bool) -> Result<()> {
    let cfg = ctx.config();
    let labels = scan::account_labels(ctx);
    let org_labels = scan::org_labels(ctx);
    let app = desktop::detect(ctx);
    let parts = scan::discover(ctx, Surface::Code);
    let mut rows = Vec::new();
    for (acct, org) in known_pairs(ctx) {
        let id = format!("{acct}/{org}");
        let scanned = parts.iter().find(|p| p.acct == acct && p.org == org).map(|p| scan::scan_partition(ctx, p));
        let open =
            app.open_account.as_deref() == Some(acct.as_str()) && app.open_org.as_deref().is_none_or(|o| o == org);
        rows.push(serde_json::json!({
            "id": id,
            "account": acct,
            "org": org,
            "name": cfg.names.get(&id).or(org_labels.get(&id)),
            "email": labels.get(&acct),
            "sessions": scanned.as_ref().map(|s| s.live_sessions()),
            "recent": scanned.as_ref().map(|s| scan::recent_titles(s, 3)).unwrap_or_default(),
            "open": open && app.running,
            "syncs": !cfg.is_excluded(&acct, &org),
        }));
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    let text = |v: &serde_json::Value| v.as_str().unwrap_or_default().replace(['\t', '\n', '\r'], " ");
    if tsv {
        // Empty fields read as `-`: a shell splitting on tabs would merge two in a row.
        let or_dash = |s: String| if s.trim().is_empty() { "-".to_string() } else { s };
        for r in &rows {
            let recent: Vec<String> = r["recent"].as_array().into_iter().flatten().map(text).collect();
            println!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                text(&r["id"]),
                or_dash(text(&r["name"])),
                or_dash(text(&r["email"])),
                or_dash(r["sessions"].as_u64().map(|n| n.to_string()).unwrap_or_default()),
                if r["syncs"] == true { "1" } else { "0" },
                if r["open"] == true { "1" } else { "0" },
                or_dash(recent.join(" | ")),
            );
        }
        return Ok(());
    }
    if rows.is_empty() {
        println!("No Claude account folders found in {}", ctx.paths.user_data.display());
    }
    for r in &rows {
        let sessions =
            r["sessions"].as_u64().map(|n| format!("{n} sessions")).unwrap_or_else(|| "no folder yet".into());
        println!(
            "{}  {:<16} {:<30} {:<14} {}{}",
            text(&r["id"]),
            r["name"].as_str().unwrap_or("-"),
            r["email"].as_str().unwrap_or("-"),
            sessions,
            if r["syncs"] == true { "syncs" } else { "separate" },
            if r["open"] == true { "  OPEN IN CLAUDE" } else { "" },
        );
        let recent: Vec<String> = r["recent"].as_array().into_iter().flatten().map(text).collect();
        if !recent.is_empty() {
            println!("      recent: {}", recent.join(" | "));
        }
    }
    Ok(())
}

fn confirm(prompt: &str) -> bool {
    if !std::io::stdin().is_terminal() {
        println!("No terminal to ask in. Review the plan above, then run again with --yes.");
        return false;
    }
    print!("{prompt} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).is_ok() && matches!(line.trim().to_lowercase().as_str(), "y" | "yes")
}

fn email_or_id(ov: &Overview, acct: &str) -> String {
    ov.labels.get(acct).cloned().unwrap_or_else(|| format!("account {}", short(acct)))
}

/// Account emails plus org names (`<account>/<org>` keys), so two orgs of one account can be
/// told apart.
fn with_names(ctx: &Ctx, mut labels: BTreeMap<String, String>) -> BTreeMap<String, String> {
    let names = ctx.config().names;
    for (id, name) in scan::org_labels(ctx).into_iter().chain(names) {
        let email = id.split('/').next().and_then(|a| labels.get(a)).cloned();
        labels.insert(id, email.map(|e| format!("{name} ({e})")).unwrap_or(name));
    }
    labels
}

fn describe_plan(ov_plan: &report::PlanView, labels: &std::collections::BTreeMap<String, String>) -> Vec<String> {
    let mut lines = Vec::new();
    if ov_plan.total == 0 {
        lines.push("Nothing to do: every linked account already has the same sessions.".to_string());
    }
    for (p, kinds) in &ov_plan.per_partition {
        let who = labels.get(&p.key()).or(labels.get(&p.acct)).cloned().unwrap_or_else(|| p.label());
        let what: Vec<String> = kinds.iter().map(|(k, n)| format!("{n} {k}")).collect();
        lines.push(format!("  {:<6} {:<32} {}", p.surface.as_str(), who, what.join(", ")));
    }
    if !ov_plan.deferred.is_empty() {
        lines.push("Waiting for Claude (it has these open; applied after you switch accounts or quit):".into());
        for (key, n) in &ov_plan.deferred {
            let acct = key.split('/').next().unwrap_or(key);
            let who = labels.get(key).or(labels.get(acct)).cloned().unwrap_or_else(|| short(acct).to_string());
            lines.push(format!("  {who}: {n} change(s)"));
        }
    }
    for n in &ov_plan.notes {
        lines.push(format!("note: {n}"));
    }
    lines
}

fn doctor(ctx: &Ctx) -> Result<()> {
    let ov = report::overview(ctx);
    println!("cc-same {}", cc_same_core::VERSION);
    let desktop = match (&ov.desktop_version, ov.app.running) {
        (Some(v), true) => format!("Claude {v}, running"),
        (Some(v), false) => format!("Claude {v}, not running"),
        (None, true) => "Claude is running".to_string(),
        (None, false) => "Claude is not running".to_string(),
    };
    let open = ov.app.open_account.as_deref().map(|a| format!("; open account: {}", email_or_id(&ov, a)));
    println!("{desktop}{}", open.unwrap_or_default());
    println!("Background sync: {}", service_line(&ov));
    let names = with_names(ctx, ov.labels.clone());
    for sv in &ov.surfaces {
        println!();
        println!("{} sessions ({})", sv.surface.title(), sv.surface.dir_name());
        for pv in &sv.partitions {
            let mut flags = Vec::new();
            if ov.app.showing(&pv.part) {
                flags.push("OPEN IN CLAUDE");
            }
            if pv.excluded {
                flags.push("excluded");
            }
            if pv.part.is_link {
                flags.push("SYMLINK");
            }
            let who = names.get(&pv.part.key()).or(pv.email.as_ref()).cloned().unwrap_or_default();
            if let Some(err) = &pv.error {
                println!("  {}  {:<30} {}  {err}", pv.part.label(), who, flags.join(" "));
                continue;
            }
            let mut facts = vec![format!("{} sessions", pv.sessions)];
            if pv.archived > 0 {
                facts.push(format!("{} archived", pv.archived));
            }
            if pv.missing > 0 && !pv.excluded {
                facts.push(format!("missing {}", pv.missing));
            }
            if pv.unreadable > 0 {
                facts.push(format!("{} unreadable", pv.unreadable));
            }
            println!("  {}  {:<30} {}  {}", pv.part.label(), who, facts.join(" · "), flags.join(" "));
            if ctx.verbose && !pv.unmanaged.is_empty() {
                println!("      left untouched: {}", pv.unmanaged.join(", "));
            }
        }
    }
    let retention = match ov.retention.desktop_days {
        Some(d) => format!("deleted after {d} days"),
        None => "kept at any age".into(),
    };
    println!();
    println!("Transcripts of Desktop sessions: {retention}");
    if let Some(ls) = &ov.last_sync {
        println!("Last sync: {} ({}), {} change(s), {} error(s)", report::ago(ls.at), ls.reason, ls.applied, ls.errors);
    }
    if let Some(hint) = ov.restart_hint() {
        println!("\nQuit and reopen Claude to see {} session(s) copied into the account it has open.", hint.sessions);
    }
    if !ov.warnings.is_empty() {
        println!();
        for w in &ov.warnings {
            // Several organizations is the normal case, so it reads as a tip, not a problem.
            let mark = if matches!(w, Warning::SpansOrgs { .. }) { "·" } else { "!" };
            println!("{mark} {}", warning_text(w));
        }
    }
    println!("\nNot synced by design: sidebar groups and order (a server setting of each account), pins, Cowork tasks, claude.ai chats, projects and memory, cloud sessions, connectors and Remote Control links.");
    println!("\nIf you sync now:");
    for l in describe_plan(&ov.plan, &with_names(ctx, ov.labels.clone())) {
        println!("{l}");
    }
    Ok(())
}

fn service_line(ov: &Overview) -> String {
    let beat = ov.heartbeat.as_ref().filter(|h| h.is_fresh());
    match (ov.service.installed, beat) {
        (true, Some(h)) => format!("on (agent v{}, last check {})", h.version, report::ago(h.heartbeat_at)),
        (true, None) => format!("on ({})", ov.service.detail),
        (false, _) if ov.service.legacy => {
            "the original Python agent is still installed; `cc-same install` replaces it".into()
        }
        (false, _) => "off (turn on with `cc-same install`)".into(),
    }
}

fn warning_text(w: &Warning) -> String {
    match w {
        Warning::SymlinkedFolders { surface, count } => format!(
            "{count} {surface} session folder(s) are symlinks. Claude silently stops saving sessions there. Quit Claude, then run: cc-same fix-symlinks"
        ),
        Warning::LeftoverFolders { dirs, .. } => format!("Leftover backup folders from other tools: {} (safe to archive by hand)", dirs.join(", ")),
        Warning::SpansOrgs { orgs, .. } => format!(
            "Your accounts span {orgs} organizations, and synced sessions appear in all of them. To keep one separate: cc-same config --exclude <account-id>"
        ),
        Warning::ShortRetention { days, limited_by, path } => {
            let why = match limited_by {
                Limit::DesktopSetting => format!("desktopSessionCleanupPeriodDays in {}", path.display()),
                Limit::Organization => "your organization's managed settings".into(),
                Limit::OlderClaudeCode => "a Claude Code before 2.1.248; updating Claude keeps them at any age".into(),
            };
            let fix = match limited_by {
                Limit::Organization => "",
                _ => " Keep them: cc-same retention --keep",
            };
            format!("Claude Code deletes the transcripts of Desktop sessions after {days} days ({why}), and a session opens empty without one.{fix}")
        }
        Warning::MissingTranscripts { missing, total } => {
            format!("{missing} of {total} sessions no longer have a transcript on disk; they open empty or not at all")
        }
        Warning::DesktopDataMissing { path } => format!("Claude Desktop's data folder was not found at {}", path.display()),
        Warning::InvalidConfigEntries { entries } => format!(
            "Syncing is stopped: these config ids are not full ids and match nothing: {}. Remove them (cc-same config --include <id> / --clear-only) and add the full ids from `cc-same orgs`",
            entries.join(", ")
        ),
    }
}

fn show_plan(ctx: &Ctx, verbose: bool) -> Result<()> {
    let (plan, app, _) = plan::build_plan(ctx, None, None);
    if app.running {
        println!("Claude is running; changes to the account it has open wait until you switch accounts or quit.");
    }
    let labels = with_names(ctx, cc_same_core::scan::account_labels(ctx));
    for l in describe_plan(&report::summarize(&plan), &labels) {
        println!("{l}");
    }
    if verbose {
        for a in &plan.actions {
            let from = a.src.as_ref().map(|s| format!("  <- {}", s.label())).unwrap_or_default();
            println!("  {:<16} {}/{}{from}", format!("{:?}", a.kind), a.part.label(), a.name);
        }
    }
    Ok(())
}

fn sync(ctx: &Ctx, yes: bool) -> Result<()> {
    let (plan, _, _) = plan::build_plan(ctx, None, None);
    let labels = with_names(ctx, cc_same_core::scan::account_labels(ctx));
    for l in describe_plan(&report::summarize(&plan), &labels) {
        println!("{l}");
    }
    if !plan.is_empty() && !yes && !confirm("Apply these changes?") {
        return Ok(());
    }
    // Run even without changes: it records which accounts are linked and what each task list
    // looked like, which later merges depend on.
    let (_, out) = apply::run_sync(ctx, "manual")?;
    if !plan.is_empty() {
        println!(
            "Done: {} change(s){}.",
            out.applied_total(),
            if out.errors.is_empty() { String::new() } else { format!(", {} error(s)", out.errors.len()) }
        );
    }
    if !out.seeded_while_loaded.is_empty() {
        println!("Quit and reopen Claude to see the sessions copied into the account it has open.");
    }
    if !out.errors.is_empty() {
        bail!("{} change(s) failed; see above", out.errors.len());
    }
    Ok(())
}

fn install(ctx: &Ctx, yes: bool) -> Result<()> {
    let (plan, _, _) = plan::build_plan(ctx, None, None);
    let labels = with_names(ctx, cc_same_core::scan::account_labels(ctx));
    println!("First sync:");
    for l in describe_plan(&report::summarize(&plan), &labels) {
        println!("{l}");
    }
    if !yes && !confirm("Sync now and keep syncing in the background?") {
        return Ok(());
    }
    if !ctx.paths.config_file().exists() {
        ctx.config().save(&ctx.paths)?;
    }
    let (_, out) = apply::run_sync(ctx, "install")?;
    let exe = std::env::current_exe()?;
    // On Windows the windowless agent build runs in the background.
    let agent = if cfg!(windows) { exe.with_file_name("cc-same-agent.exe") } else { exe.clone() };
    let (source, name) = if agent.exists() {
        (agent, if cfg!(windows) { "cc-same-agent.exe" } else { "cc-same" })
    } else {
        (exe, if cfg!(windows) { "cc-same.exe" } else { "cc-same" })
    };
    let installed = service::install(ctx, &source, name)?;
    println!("Background sync is on ({}).", installed.display());
    println!("Log: {}", ctx.paths.log_file.display());
    if !out.seeded_while_loaded.is_empty() {
        println!("Quit and reopen Claude once to see the sessions copied into the account it has open.");
    }
    Ok(())
}

/// Every account CC Same knows of, by ID, with its name: the list, then the ones only seen in
/// Claude's session folders.
fn known_accounts(ctx: &Ctx, roster: &Roster) -> BTreeMap<String, String> {
    let labels = scan::account_labels(ctx);
    let mut known: BTreeMap<String, String> = roster.slots.iter().map(|s| (s.account.clone(), s.label())).collect();
    for part in scan::discover(ctx, Surface::Code) {
        let name = labels.get(&part.acct).cloned().unwrap_or_else(|| format!("Account {}", short(&part.acct)));
        known.entry(part.acct).or_insert(name);
    }
    known
}

#[cfg(feature = "account-switching")]
/// The account `query` names in the list; an account only seen in Claude's session folders is
/// named in the error, with how to add it.
fn named(ctx: &Ctx, roster: &Roster, query: &str) -> Result<(String, String)> {
    match roster.find(query) {
        Ok(slot) => Ok((slot.account.clone(), slot.label())),
        Err(NotFound::Several(names)) => bail!("{query} could be {}", names.join(" or ")),
        Err(NotFound::None(_)) => {
            let query = query.trim();
            let others: Vec<(String, String)> = known_accounts(ctx, roster)
                .into_iter()
                .filter(|(id, name)| name.eq_ignore_ascii_case(query) || (query.len() >= 4 && id.starts_with(query)))
                .collect();
            match others.as_slice() {
                [(_, name)] => bail!("{name} is not in the list yet: sign in to it once in Claude (`cc-same add`)"),
                [] => bail!("no account called {query} (see `cc-same accounts`)"),
                _ => bail!(
                    "{query} could be {}",
                    others.iter().map(|(_, n)| n.as_str()).collect::<Vec<_>>().join(" or ")
                ),
            }
        }
    }
}

#[cfg(feature = "account-switching")]
fn name_in(roster: &Roster, account: &str) -> String {
    roster.slot(account).map(Slot::label).unwrap_or_else(|| format!("Account {}", short(account)))
}

/// "5h 12% · 7d 58%, 2 min ago", or nothing when Claude has no reading.
fn usage_text(usage: Option<&Usage>, now: f64) -> String {
    let Some(u) = usage else { return String::new() };
    let mut parts = Vec::new();
    if let Some(p) = u.five_hour_at(now) {
        parts.push(format!("5h {p:.0}%"));
    }
    if let Some(p) = u.weekly_at(now) {
        parts.push(format!("7d {p:.0}%"));
    }
    if parts.is_empty() {
        return String::new();
    }
    format!("{}, {}", parts.join(" · "), report::ago(u.at))
}

fn accounts(ctx: &Ctx, json: bool) -> Result<()> {
    let roster = accounts::observe(ctx);
    let found = logins::list(ctx);
    let usage = accounts::usage(ctx, &roster);
    let now = fsx::now_secs();
    let signed_in = found.signed_in.as_deref();
    if json {
        let rows: Vec<serde_json::Value> = roster
            .slots
            .iter()
            .map(|s| {
                let kept = found.saved(&s.account);
                let u = usage.get(&s.account);
                serde_json::json!({
                    "number": s.number,
                    "account": s.account,
                    "email": s.email,
                    "alias": s.alias,
                    "disabled": s.disabled,
                    "signedIn": signed_in == Some(s.account.as_str()),
                    "switchable": kept.is_some(),
                    "keptAt": kept.map(|k| k.set_aside_at),
                    "stale": kept.is_some_and(|k| k.stale()),
                    "usage": u.map(|u| serde_json::json!({
                        "at": u.at,
                        "fiveHour": u.five_hour_at(now),
                        "weekly": u.weekly_at(now),
                    })),
                })
            })
            .collect();
        let out = serde_json::json!({ "schemaVersion": 1, "signedIn": signed_in, "accounts": rows });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    if roster.slots.is_empty() {
        println!("No accounts yet: sign in to Claude.");
        return Ok(());
    }
    if !found.supported {
        println!("Switching accounts is not supported on this system yet.\n");
    }
    let name_width = roster.slots.iter().map(|s| s.label().chars().count()).max().unwrap_or(0);
    let alias_width =
        roster.slots.iter().filter_map(|s| s.alias.as_ref()).map(|a| a.chars().count()).max().unwrap_or(0);
    for s in &roster.slots {
        let state = if signed_in == Some(s.account.as_str()) {
            "signed in".to_string()
        } else if let Some(kept) = found.saved(&s.account) {
            let stale = if kept.stale() { " (may ask to sign in again)" } else { "" };
            format!("kept, used {}{stale}", report::ago(kept.set_aside_at))
        } else {
            "sign in once to switch here (`cc-same add`)".to_string()
        };
        let state = if s.disabled { format!("{state}, sits out") } else { state };
        let used = usage_text(usage.get(&s.account), now);
        let alias = s.alias.clone().unwrap_or_default();
        let line = format!("  {:>2}  {:name_width$}  {alias:alias_width$}  {state:<30}  {used}", s.number, s.label());
        println!("{}", line.trim_end());
    }
    let others: Vec<String> = known_accounts(ctx, &roster)
        .into_iter()
        .filter(|(id, _)| roster.slot(id).is_none())
        .map(|(_, name)| name)
        .collect();
    if !others.is_empty() {
        println!("\nAlso in your sessions: {} (sign in to them once in Claude to add them).", others.join(", "));
    }
    Ok(())
}

#[cfg(feature = "account-switching")]
fn switch(ctx: &Ctx, account: Option<&str>, strategy: Option<Strategy>, json: bool) -> Result<()> {
    let roster = accounts::observe(ctx);
    let found = logins::list(ctx);
    let target = match account {
        Some(query) => named(ctx, &roster, query)?.0,
        None => {
            let usage = accounts::usage(ctx, &roster);
            let strategy = strategy.unwrap_or_default();
            let can_switch = |s: &Slot| found.saved(&s.account).is_some();
            let current = found.signed_in.as_deref();
            match accounts::pick(&roster, current, strategy, can_switch, &usage, fsx::now_secs()) {
                Some(slot) => slot.account.clone(),
                None => return Err(accounts::Refused::NoOther.into()),
            }
        }
    };
    let name = name_in(&roster, &target);
    if found.signed_in.as_deref() == Some(target.as_str()) {
        if json {
            let out = serde_json::json!({ "schemaVersion": 1, "switched": false, "to": target, "reason": "already-signed-in" });
            println!("{}", serde_json::to_string_pretty(&out)?);
        } else {
            println!("Claude is already signed in to {name}.");
        }
        return Ok(());
    }
    let done = accounts::switch(ctx, &target, &quit_watch())?;
    if json {
        let out = serde_json::json!({
            "schemaVersion": 1, "switched": true, "from": done.from, "to": done.to, "launched": done.launched,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    let restarted = if done.launched { " (restarted)" } else { "" };
    println!("Claude is signed in to {name}{restarted}.");
    Ok(())
}

#[cfg(feature = "account-switching")]
/// Claude asks before it quits when it has work in progress: say so, and what Ctrl-C does then.
fn quit_watch() -> desktop::Watch<'static> {
    fn tell(quitting: Quitting) {
        match quitting {
            Quitting::Asked => {}
            Quitting::Confirming => eprintln!(
                "Claude is asking whether to stop its work in progress: answer it in Claude. (Ctrl-C stops waiting; nothing changes.)"
            ),
            Quitting::AfterWork => eprintln!(
                "Claude quits once its work in progress is done, and CC Same carries on then. (Ctrl-C stops waiting; Claude still quits then.)"
            ),
        }
    }
    desktop::Watch { progress: Some(&tell), stop: None }
}

#[cfg(feature = "account-switching")]
fn add(ctx: &Ctx) -> Result<()> {
    let switched = accounts::add(ctx, &quit_watch())?;
    let roster = Roster::load(&ctx.paths);
    if let Some(from) = &switched.from {
        let name = name_in(&roster, from);
        let number = roster.slot(from).map(|s| s.number.to_string()).unwrap_or_else(|| name.clone());
        println!("Kept {name}: `cc-same switch {number}` brings it back.");
    }
    println!("Claude is open on its sign-in page: sign in to the other account there.");
    println!("To switch later, use `cc-same switch`: Log out in Claude ends a sign-in for good.");
    Ok(())
}

#[cfg(feature = "account-switching")]
fn remove(ctx: &Ctx, query: &str, yes: bool) -> Result<()> {
    let roster = accounts::observe(ctx);
    let (id, name) = named(ctx, &roster, query)?;
    println!("Remove {name} from the list? The sign-in kept for it is forgotten; its sessions stay.");
    if !yes && !confirm("Remove it?") {
        return Ok(());
    }
    accounts::remove(ctx, &id)?;
    println!("Removed {name}. Sign in to it in Claude to add it again.");
    Ok(())
}

#[cfg(feature = "account-switching")]
fn alias(ctx: &Ctx, account: Option<&str>, name: Option<&str>, unset: bool) -> Result<()> {
    let roster = accounts::observe(ctx);
    let Some(query) = account else {
        let aliased: Vec<&Slot> = roster.slots.iter().filter(|s| s.alias.is_some()).collect();
        if aliased.is_empty() {
            println!("No aliases yet: `cc-same alias <account> <name>`.");
        }
        for s in aliased {
            println!("  {:<12}  {}  {}", s.alias.as_deref().unwrap_or_default(), s.number, s.label());
        }
        return Ok(());
    };
    let (id, label) = named(ctx, &roster, query)?;
    if unset {
        accounts::set_alias(ctx, &id, None)?;
        println!("{label} has no alias now.");
    } else if let Some(name) = name {
        let roster = accounts::set_alias(ctx, &id, Some(name))?;
        let alias = roster.slot(&id).and_then(|s| s.alias.clone()).unwrap_or_default();
        println!("{label} is also {alias}: `cc-same switch {alias}`.");
    } else {
        match roster.slot(&id).and_then(|s| s.alias.as_deref()) {
            Some(alias) => println!("{alias}"),
            None => println!("{label} has no alias."),
        }
    }
    Ok(())
}

#[cfg(feature = "account-switching")]
fn sit_out(ctx: &Ctx, query: &str, disabled: bool) -> Result<()> {
    let roster = accounts::observe(ctx);
    let (id, name) = named(ctx, &roster, query)?;
    accounts::set_disabled(ctx, &id, disabled)?;
    if disabled {
        println!("{name} sits out of the rotation; `cc-same switch {query}` still switches to it.");
    } else {
        println!("{name} is back in the rotation.");
    }
    Ok(())
}

#[cfg(feature = "account-switching")]
fn move_account(ctx: &Ctx, query: &str, number: u32) -> Result<()> {
    let roster = accounts::observe(ctx);
    let (id, name) = named(ctx, &roster, query)?;
    let old = roster.slot(&id).map(|s| s.number);
    let swapped = accounts::move_to(ctx, &id, number)?;
    println!("{name} is number {number} now.");
    if let (Some(other), Some(old)) = (swapped, old) {
        println!("{} took number {old}.", name_in(&roster, &other));
    }
    Ok(())
}

fn status(ctx: &Ctx) -> Result<()> {
    let st = service::status(ctx);
    println!("Background sync: {}{}", if st.installed { "on, " } else { "off, " }, st.detail);
    if let Some(h) = Heartbeat::read(&ctx.paths) {
        println!("Agent: v{} (pid {}), last check {}", h.version, h.pid, report::ago(h.heartbeat_at));
    }
    let state = State::load(&ctx.paths);
    match &state.last_sync {
        Some(ls) => println!(
            "Last sync: {} ({}): {} change(s), {} error(s), {} waiting for Claude",
            report::ago(ls.at),
            ls.reason,
            ls.applied,
            ls.errors,
            ls.deferred
        ),
        None => println!("Never synced."),
    }
    println!("Baseline snapshot: {}", state.baseline.as_deref().unwrap_or("none"));
    for (surface, keys) in &state.known {
        let list: Vec<String> = keys.iter().map(|k| k.split('/').map(short).collect::<Vec<_>>().join("/")).collect();
        println!("Linked {surface}: {}", list.join(", "));
    }
    println!("Log: {}", ctx.paths.log_file.display());
    Ok(())
}

fn snapshots(ctx: &Ctx) -> Result<()> {
    let state = State::load(&ctx.paths);
    let list = snapshot::list(ctx);
    if list.is_empty() {
        println!("No snapshots yet. The first sync takes one.");
    }
    for m in &list {
        let baseline = if Some(&m.id) == state.baseline.as_ref() { "  (before the first sync)" } else { "" };
        println!("{}  {} folder(s){baseline}", m.id, m.partitions.len());
    }
    println!("Location: {}", ctx.paths.snapshots_dir().display());
    Ok(())
}

fn retention(ctx: &Ctx, days: Option<u32>, keep: bool, undo: bool) -> Result<()> {
    if let Some(days) = days {
        let backup = retention::set_cleanup_days(&ctx.paths, days)?;
        println!("cleanupPeriodDays = {days} in {}", ctx.paths.claude_settings.display());
        if let Some(b) = backup {
            println!("Previous file saved as {}", b.display());
        }
        return Ok(());
    }
    if keep {
        match retention::keep(&ctx.paths)? {
            Kept::Already => println!("Claude Code already keeps the transcripts of Desktop sessions at any age."),
            Kept::AnyAge => {
                println!("Removed desktopSessionCleanupPeriodDays: Desktop sessions keep their transcripts at any age.")
            }
            Kept::TenYears => {
                println!("cleanupPeriodDays = 3650. Updating Claude keeps Desktop transcripts at any age.")
            }
        }
        println!("Undo with: cc-same retention --undo");
        return Ok(());
    }
    if undo {
        match retention::undo(&ctx.paths)? {
            true => println!("Put back the setting CC Same changed in {}.", ctx.paths.claude_settings.display()),
            false => println!("Nothing to undo."),
        }
        return Ok(());
    }
    let r = retention::read(&ctx.paths);
    let version = r.claude_code.as_deref().map(|v| format!("Claude Code {v}")).unwrap_or_else(|| "Claude Code".into());
    match (r.desktop_days, r.limited_by) {
        (Some(d), Some(Limit::DesktopSetting)) => {
            println!("Desktop sessions: transcripts deleted after {d} days (desktopSessionCleanupPeriodDays).")
        }
        (Some(d), Some(Limit::Organization)) => {
            println!("Desktop sessions: transcripts deleted after {d} days (set by your organization).")
        }
        (Some(d), _) => println!("Desktop sessions: transcripts deleted after {d} days ({version} predates 2.1.248)."),
        (None, _) => println!("Desktop sessions: transcripts kept at any age ({version})."),
    }
    println!("Terminal sessions and other data: deleted after {} days (cleanupPeriodDays).", r.cleanup_days);
    if r.is_short() && r.limited_by != Some(Limit::Organization) {
        println!("Keep them: cc-same retention --keep");
    }
    if r.undo.is_some() {
        println!("Put back what CC Same changed: cc-same retention --undo");
    }
    Ok(())
}
