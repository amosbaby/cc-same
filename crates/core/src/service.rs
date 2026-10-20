//! Running the sync in the background at login.
//!
//! * macOS: a LaunchAgent (`~/Library/LaunchAgents/<label>.plist`).
//! * Windows: a `Run` value under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.
//! * Linux: a systemd user service, or an XDG autostart entry without systemd.
//!
//! The agent is the installing binary started with `watch`. The command-line tool, and the app on
//! Windows and Linux, run a copy of themselves in `<state>/bin`, so it keeps working when the
//! original is moved or updated. The Mac app runs its own executable in place
//! ([`install_in_place`]): the app's signature covers that file only inside the app, and macOS
//! refuses to run a copy of it anywhere else. The agent writes a heartbeat file that front-ends
//! read to tell whether it is alive.

use crate::ctx::Ctx;
use crate::fsx;
use crate::paths::Paths;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const LABEL: &str = "io.github.songkeys.cc-same";
/// Agents from before the rename (the Python prototype, then "Uni Claude"), replaced on install.
pub const LEGACY_LABELS: &[&str] = &["local.uni-claude", "io.github.songkeys.uni-claude"];
pub const DISPLAY_NAME: &str = "CC Same";
/// The app's bundle identifier. System Settings lists a background item under the app it names;
/// without one, a standalone agent is listed under the name on its signing certificate.
#[cfg(target_os = "macos")]
const APP_BUNDLE_ID: &str = "io.github.songkeys.cc-same";
/// The display name before the rename (the Windows `Run` value).
#[cfg(windows)]
const LEGACY_DISPLAY_NAME: &str = "Uni Claude";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServiceStatus {
    pub installed: bool,
    pub running: Option<bool>,
    /// Whether the service manager has the agent loaded, where that is known (macOS). An agent
    /// that is installed but not loaded never starts.
    pub loaded: Option<bool>,
    pub detail: String,
    /// The registered command line, when readable.
    pub program: Vec<String>,
    /// The original Python agent is still registered.
    pub legacy: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Heartbeat {
    pub pid: u32,
    pub version: String,
    pub exe: String,
    pub started_at: f64,
    pub heartbeat_at: f64,
}

impl Heartbeat {
    pub fn read(paths: &Paths) -> Option<Heartbeat> {
        serde_json::from_slice(&fsx::read_limited(&paths.heartbeat_file(), 64 * 1024).ok()?).ok()
    }

    /// The agent wrote its heartbeat within the last minute.
    pub fn is_fresh(&self) -> bool {
        fsx::now_secs() - self.heartbeat_at < 60.0
    }

    pub fn write(&self, paths: &Paths) {
        if let Ok(body) = serde_json::to_vec(self) {
            let _ = fsx::create_private_dir_all(&paths.state_dir);
            let _ = fsx::atomic_write(&paths.heartbeat_file(), &body, None, false);
        }
    }
}

/// Copy `exe` into `<state>/bin` and register it to run `watch` at login, starting it now.
pub fn install(ctx: &Ctx, exe: &Path, name: &str) -> Result<PathBuf> {
    let bin = ctx.paths.bin_dir();
    fsx::create_private_dir_all(&bin)?;
    let installed = bin.join(name);
    // The new agent replaces the old one anyway, and Windows cannot replace the file of a
    // program that is still running.
    stop_heartbeat_agent(ctx);
    if fs::canonicalize(exe).ok() != fs::canonicalize(&installed).ok() {
        let tmp = bin.join(format!("{}{name}", fsx::TMP_PREFIX));
        let _ = fs::remove_file(&tmp);
        fs::copy(exe, &tmp).with_context(|| format!("copying {}", exe.display()))?;
        clear_download_mark(&tmp);
        replace_file(&tmp, &installed)
            .with_context(|| format!("replacing {} (is an older agent still running?)", installed.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&installed, fs::Permissions::from_mode(0o755))?;
    }
    platform::register(ctx, &agent_program(ctx, &installed))?;
    Ok(installed)
}

/// Register `exe` itself, where it is, to run `watch` at login, starting it now. For a program
/// that only runs where it is: the Mac app's executable, signed as part of its bundle.
pub fn install_in_place(ctx: &Ctx, exe: &Path) -> Result<()> {
    stop_heartbeat_agent(ctx);
    platform::register(ctx, &agent_program(ctx, exe))
}

/// The agent's command line: `exe`, the locations when they are not the default ones, `watch`.
fn agent_program(ctx: &Ctx, exe: &Path) -> Vec<String> {
    let mut program = vec![exe.to_string_lossy().into_owned()];
    if ctx.paths.user_data != crate::paths::default_user_data() {
        program.push("--user-data".into());
        program.push(ctx.paths.user_data.to_string_lossy().into_owned());
    }
    if ctx.paths.state_dir != crate::paths::default_state_dir() {
        program.push("--state-dir".into());
        program.push(ctx.paths.state_dir.to_string_lossy().into_owned());
    }
    program.push("watch".into());
    program
}

/// Rename `from` over `to`. On Windows a virus scanner, or a program that is just exiting, can
/// hold `to` open for a moment, so keep trying briefly.
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match fs::rename(from, to) {
            Err(_) if cfg!(windows) && Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            result => return result,
        }
    }
}

/// A copy inherits the "downloaded from the internet" mark of the file it came from, and the
/// system would then hold the agent at login for a confirmation nobody sees. The user already
/// chose to run this program, so its private copy starts without the mark.
#[cfg(target_os = "macos")]
fn clear_download_mark(path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    if let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) {
        unsafe { libc::removexattr(path.as_ptr(), c"com.apple.quarantine".as_ptr(), libc::XATTR_NOFOLLOW) };
    }
}

#[cfg(windows)]
fn clear_download_mark(path: &Path) {
    let mut stream = path.as_os_str().to_owned();
    stream.push(":Zone.Identifier");
    let _ = fs::remove_file(stream);
}

#[cfg(not(any(target_os = "macos", windows)))]
fn clear_download_mark(_path: &Path) {}

pub fn uninstall(ctx: &Ctx) -> Result<()> {
    platform::unregister(ctx)?;
    stop_heartbeat_agent(ctx);
    Ok(())
}

pub fn status(ctx: &Ctx) -> ServiceStatus {
    let mut st = platform::status(ctx);
    if st.running.is_none() {
        st.running = Some(Heartbeat::read(&ctx.paths).is_some_and(|h| h.is_fresh()));
    }
    st
}

/// Stop an agent that is still running from an earlier install (Windows and the XDG
/// fallback have no service manager to do it for us). Only a process that is running the
/// program named in the heartbeat is stopped, so a reused process id is left alone.
fn stop_heartbeat_agent(ctx: &Ctx) {
    let Some(hb) = Heartbeat::read(&ctx.paths) else { return };
    if !hb.is_fresh() || hb.pid == std::process::id() {
        return;
    }
    #[cfg(any(windows, target_os = "linux"))]
    terminate(hb.pid, &hb.exe);
    let _ = fs::remove_file(ctx.paths.heartbeat_file());
}

#[cfg(target_os = "linux")]
fn terminate(pid: u32, exe: &str) {
    let Ok(running) = fs::read_link(format!("/proc/{pid}/exe")) else { return };
    if running.to_string_lossy().trim_end_matches(" (deleted)") != exe {
        return;
    }
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    let deadline = Instant::now() + Duration::from_secs(3);
    while Path::new(&format!("/proc/{pid}")).exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(windows)]
fn terminate(pid: u32, exe: &str) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    };
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, 0, pid);
        if handle.is_null() {
            return;
        }
        let mut buf = [0u16; 1024];
        let mut size = buf.len() as u32;
        if QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut size) != 0
            && String::from_utf16_lossy(&buf[..size as usize]).eq_ignore_ascii_case(exe)
        {
            TerminateProcess(handle, 1);
            WaitForSingleObject(handle, 3000);
        }
        CloseHandle(handle);
    }
}

fn run_quiet(cmd: &mut Command) -> std::io::Result<std::process::Output> {
    cmd.stdin(Stdio::null()).output()
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use crate::paths::home;
    use anyhow::bail;

    fn uid() -> u32 {
        unsafe { libc::getuid() }
    }

    fn plist_path(label: &str) -> PathBuf {
        home().join("Library").join("LaunchAgents").join(format!("{label}.plist"))
    }

    fn xml(s: &str) -> String {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
    }

    fn service(label: &str) -> String {
        format!("gui/{}/{label}", uid())
    }

    /// Whether launchd has the job loaded.
    fn loaded(label: &str) -> bool {
        run_quiet(Command::new("/bin/launchctl").args(["print", &service(label)])).is_ok_and(|o| o.status.success())
    }

    /// Unload a job, and wait for launchd to let go of it: `bootout` can return while the job is
    /// still on its way out, and loading it again then fails ("Input/output error").
    fn bootout(label: &str) {
        if !loaded(label) {
            return;
        }
        let _ = run_quiet(Command::new("/bin/launchctl").args(["bootout", &service(label)]));
        let deadline = Instant::now() + Duration::from_secs(10);
        while loaded(label) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// The LaunchAgent: `program` at login and whenever it stops, logging to `log`.
    pub(super) fn plist(program: &[String], log: &Path) -> String {
        let log = xml(&log.to_string_lossy());
        let args: String = program.iter().map(|a| format!("\n        <string>{}</string>", xml(a))).collect();
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>{args}
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ProcessType</key>
    <string>Background</string>
    <key>LowPriorityIO</key>
    <true/>
    <key>Nice</key>
    <integer>10</integer>
    <key>ThrottleInterval</key>
    <integer>30</integer>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
    <key>AssociatedBundleIdentifiers</key>
    <array>
        <string>{APP_BUNDLE_ID}</string>
    </array>
</dict>
</plist>
"#
        )
    }

    pub fn register(ctx: &Ctx, program: &[String]) -> Result<()> {
        for legacy in LEGACY_LABELS {
            bootout(legacy);
            let _ = fs::remove_file(plist_path(legacy));
        }
        let path = plist_path(LABEL);
        let body = plist(program, &ctx.paths.log_file.with_extension("agent.log"));
        // The same job, loaded already (after an update, a new program at the same path): restart
        // it in place, without unloading it.
        if fs::read_to_string(&path).is_ok_and(|current| current == body) && loaded(LABEL) {
            let out = run_quiet(Command::new("/bin/launchctl").args(["kickstart", "-k", &service(LABEL)]))?;
            if out.status.success() {
                return Ok(());
            }
        }
        bootout(LABEL);
        fs::create_dir_all(path.parent().unwrap())?;
        fsx::atomic_write(&path, body.as_bytes(), None, false)?;
        // launchd creates the agent's log with the default 0644; create it private first.
        let _ = fsx::open_private_append(&ctx.paths.log_file.with_extension("agent.log"));
        // launchd may still be letting go of the old job for a moment.
        let mut error = String::new();
        for attempt in 1..=5 {
            let out =
                run_quiet(Command::new("/bin/launchctl").args(["bootstrap", &format!("gui/{}", uid())]).arg(&path))?;
            if out.status.success() {
                return Ok(());
            }
            error = String::from_utf8_lossy(&out.stderr).trim().to_string();
            std::thread::sleep(Duration::from_millis(400 * attempt));
        }
        bail!("launchctl bootstrap failed: {error}")
    }

    pub fn unregister(_ctx: &Ctx) -> Result<()> {
        for label in std::iter::once(&LABEL).chain(LEGACY_LABELS) {
            bootout(label);
            let _ = fs::remove_file(plist_path(label));
        }
        Ok(())
    }

    /// The `ProgramArguments` of a plist written by `register`.
    pub(super) fn program_arguments(plist: &str) -> Vec<String> {
        let Some(rest) = plist.split("<key>ProgramArguments</key>").nth(1) else { return Vec::new() };
        let array = rest.split("</array>").next().unwrap_or_default();
        array
            .split("<string>")
            .skip(1)
            .filter_map(|s| s.split("</string>").next())
            .map(|s| s.replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&"))
            .collect()
    }

    pub fn status(_ctx: &Ctx) -> ServiceStatus {
        let legacy = LEGACY_LABELS.iter().any(|l| plist_path(l).exists());
        let path = plist_path(LABEL);
        if !path.exists() {
            return ServiceStatus {
                legacy,
                detail: "not installed".into(),
                running: Some(false),
                ..Default::default()
            };
        }
        let program = fs::read_to_string(&path).ok().map(|p| program_arguments(&p)).unwrap_or_default();
        let out = run_quiet(Command::new("/bin/launchctl").args(["print", &service(LABEL)]));
        let running = out.ok().filter(|o| o.status.success()).map(|o| {
            let text = String::from_utf8_lossy(&o.stdout).into_owned();
            text.lines().any(|l| l.trim() == "state = running")
        });
        let detail = match running {
            Some(true) => "running",
            Some(false) => "loaded, not running",
            None => "installed, not loaded",
        };
        let loaded = Some(running.is_some());
        ServiceStatus { installed: true, running, loaded, detail: detail.into(), program, legacy }
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use anyhow::bail;
    use std::os::windows::process::CommandExt;

    const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const DETACHED_PROCESS: u32 = 0x0000_0008;

    fn quote(a: &str) -> String {
        if a.contains(' ') || a.is_empty() {
            format!("\"{a}\"")
        } else {
            a.to_string()
        }
    }

    fn delete_value(name: &str) {
        let _ =
            run_quiet(Command::new("reg").args(["delete", RUN_KEY, "/v", name, "/f"]).creation_flags(CREATE_NO_WINDOW));
    }

    pub fn register(_ctx: &Ctx, program: &[String]) -> Result<()> {
        delete_value(LEGACY_DISPLAY_NAME);
        let line = program.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ");
        let out = run_quiet(
            Command::new("reg")
                .args(["add", RUN_KEY, "/v", DISPLAY_NAME, "/t", "REG_SZ", "/d", &line, "/f"])
                .creation_flags(CREATE_NO_WINDOW),
        )?;
        if !out.status.success() {
            bail!("reg add failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        // Start it now as well; the Run value covers the next login.
        Command::new(&program[0])
            .args(&program[1..])
            .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("starting the agent")?;
        Ok(())
    }

    pub fn unregister(_ctx: &Ctx) -> Result<()> {
        delete_value(DISPLAY_NAME);
        delete_value(LEGACY_DISPLAY_NAME);
        Ok(())
    }

    pub fn status(_ctx: &Ctx) -> ServiceStatus {
        let out = run_quiet(
            Command::new("reg").args(["query", RUN_KEY, "/v", DISPLAY_NAME]).creation_flags(CREATE_NO_WINDOW),
        );
        match out {
            Ok(o) if o.status.success() => {
                let text = String::from_utf8_lossy(&o.stdout).into_owned();
                let line = text
                    .lines()
                    .find(|l| l.contains("REG_SZ"))
                    .and_then(|l| l.split("REG_SZ").nth(1))
                    .unwrap_or("")
                    .trim()
                    .to_string();
                ServiceStatus {
                    installed: true,
                    running: None,
                    loaded: None,
                    detail: "registered to start at login".into(),
                    program: vec![line],
                    legacy: false,
                }
            }
            _ => ServiceStatus { detail: "not installed".into(), running: Some(false), ..Default::default() },
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod platform {
    use super::*;
    use crate::paths::home;

    const UNIT: &str = "cc-same.service";
    /// Before the rename.
    const LEGACY_UNIT: &str = "uni-claude.service";

    fn config_home() -> PathBuf {
        dirs::config_dir().unwrap_or_else(|| home().join(".config"))
    }

    fn unit_path() -> PathBuf {
        config_home().join("systemd").join("user").join(UNIT)
    }

    fn autostart_path() -> PathBuf {
        config_home().join("autostart").join("cc-same.desktop")
    }

    /// Take down the agent registered under the old name, if any.
    fn remove_legacy() {
        let unit = config_home().join("systemd").join("user").join(LEGACY_UNIT);
        if unit.exists() {
            let _ = systemctl(&["disable", "--now", LEGACY_UNIT]);
            let _ = fs::remove_file(&unit);
            let _ = systemctl(&["daemon-reload"]);
        }
        let _ = fs::remove_file(config_home().join("autostart").join("uni-claude.desktop"));
    }

    fn systemctl(args: &[&str]) -> bool {
        run_quiet(Command::new("systemctl").arg("--user").args(args)).map(|o| o.status.success()).unwrap_or(false)
    }

    fn quote(a: &str) -> String {
        format!("\"{}\"", a.replace('\\', "\\\\").replace('"', "\\\""))
    }

    pub fn register(_ctx: &Ctx, program: &[String]) -> Result<()> {
        remove_legacy();
        let exec = program.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ");
        let has_systemd =
            run_quiet(Command::new("systemctl").arg("--version")).map(|o| o.status.success()).unwrap_or(false);
        if has_systemd {
            let unit = format!(
                "[Unit]\nDescription=CC Same: keep Claude Desktop sessions in sync across accounts\n\n\
                 [Service]\nExecStart={exec}\nRestart=on-failure\nRestartSec=30\nNice=10\n\n\
                 [Install]\nWantedBy=default.target\n"
            );
            let path = unit_path();
            fs::create_dir_all(path.parent().unwrap())?;
            fsx::atomic_write(&path, unit.as_bytes(), None, false)?;
            if systemctl(&["daemon-reload"]) && systemctl(&["enable", UNIT]) && systemctl(&["restart", UNIT]) {
                let _ = fs::remove_file(autostart_path());
                return Ok(());
            }
            let _ = fs::remove_file(&path);
        }
        // No systemd user session: XDG autostart for the next login, and start it now.
        let path = autostart_path();
        fs::create_dir_all(path.parent().unwrap())?;
        fsx::atomic_write(
            &path,
            format!("[Desktop Entry]\nType=Application\nName={DISPLAY_NAME}\nExec={exec}\nNoDisplay=true\nX-GNOME-Autostart-enabled=true\n").as_bytes(),
            None,
            false,
        )?;
        Command::new(&program[0])
            .args(&program[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("starting the agent")?;
        Ok(())
    }

    pub fn unregister(_ctx: &Ctx) -> Result<()> {
        if unit_path().exists() {
            let _ = systemctl(&["disable", "--now", UNIT]);
            let _ = fs::remove_file(unit_path());
            let _ = systemctl(&["daemon-reload"]);
        }
        let _ = fs::remove_file(autostart_path());
        remove_legacy();
        Ok(())
    }

    pub fn status(_ctx: &Ctx) -> ServiceStatus {
        if unit_path().exists() {
            let active = run_quiet(Command::new("systemctl").args(["--user", "is-active", UNIT]))
                .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "active")
                .ok();
            let detail = if active == Some(true) {
                "running (systemd user service)"
            } else {
                "installed (systemd user service)"
            };
            return ServiceStatus { installed: true, running: active, detail: detail.into(), ..Default::default() };
        }
        if autostart_path().exists() {
            return ServiceStatus {
                installed: true,
                running: None,
                detail: "starts at login (autostart)".into(),
                ..Default::default()
            };
        }
        ServiceStatus { detail: "not installed".into(), running: Some(false), ..Default::default() }
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use super::*;
    use anyhow::bail;
    pub fn register(_ctx: &Ctx, _program: &[String]) -> Result<()> {
        bail!("background sync is not supported on this platform")
    }
    pub fn unregister(_ctx: &Ctx) -> Result<()> {
        Ok(())
    }
    pub fn status(_ctx: &Ctx) -> ServiceStatus {
        ServiceStatus { detail: "unsupported".into(), running: Some(false), ..Default::default() }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "macos")]
    #[test]
    fn agent_copy_starts_without_the_download_mark() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("agent");
        std::fs::write(&file, "copy").unwrap();
        let path = std::ffi::CString::new(file.as_os_str().as_bytes()).unwrap();
        let name = c"com.apple.quarantine";
        let marked = || unsafe { libc::getxattr(path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0, 0, 0) } >= 0;
        let value = b"0081;66f9a2c1;Safari;";
        assert_eq!(
            unsafe { libc::setxattr(path.as_ptr(), name.as_ptr(), value.as_ptr().cast(), value.len(), 0, 0) },
            0
        );
        assert!(marked());
        super::clear_download_mark(&file);
        assert!(!marked());
    }

    #[test]
    fn the_agent_runs_watch_with_the_locations_that_differ() {
        use crate::{Config, Ctx, FakeDesktop, LogSink, Paths};
        use std::path::Path;
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().join("claude"), dir.path().join("state"));
        let ctx = Ctx::new(paths.clone(), Config::default(), FakeDesktop::default(), LogSink::Silent);
        let program = super::agent_program(&ctx, Path::new("/Applications/CC Same.app/Contents/MacOS/CC Same"));
        assert_eq!(program.first().map(String::as_str), Some("/Applications/CC Same.app/Contents/MacOS/CC Same"));
        assert_eq!(program.last().map(String::as_str), Some("watch"));
        assert!(program.windows(2).any(|w| w[0] == "--state-dir" && Path::new(&w[1]) == paths.state_dir));
        assert!(program.windows(2).any(|w| w[0] == "--user-data" && Path::new(&w[1]) == paths.user_data));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn launch_agent_names_the_app_and_reads_back() {
        let program: Vec<String> =
            ["/Users/a & b/cc-same-agent", "--state-dir", "/tmp/<state>", "watch"].map(String::from).to_vec();
        let plist = super::platform::plist(&program, std::path::Path::new("/tmp/agent.log"));
        assert!(plist.contains("<key>AssociatedBundleIdentifiers</key>"));
        assert!(plist.contains("<string>io.github.songkeys.cc-same</string>"));
        assert_eq!(super::platform::program_arguments(&plist), program);
    }
}
