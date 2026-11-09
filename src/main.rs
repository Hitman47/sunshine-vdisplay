//! `vdisplay`: create an Apollo-style SudoVDA virtual display from Sunshine prep commands.
//!
//! ```text
//! do:   vdisplay.exe start --primary
//! undo: vdisplay.exe stop
//! ```
//!
//! The SudoVDA driver removes all virtual displays once nobody has talked to it for
//! its watchdog timeout (3s by default), so `start` spawns a detached daemon that owns
//! the display and keeps pinging the driver until `stop` is called.

#![windows_subsystem = "windows"]

mod display;
mod driver;
mod guardian;
mod install;
mod ipc;
mod logging;

use std::io::{BufRead, BufReader, Write};
use std::os::windows::process::CommandExt;
use std::process::{Command, ExitCode, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Console::{
    ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_OUTPUT_HANDLE,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS, OpenProcess,
    PROCESS_SYNCHRONIZE, WaitForMultipleObjects,
};
use windows::core::GUID;

use crate::driver::Driver;
use crate::guardian::Guardian;
use crate::ipc::Handle;

/// Version shown at runtime. Release builds get it from `VDISPLAY_VERSION` (set by CI
/// from the release tag); local builds fall back to the Cargo.toml version.
const VERSION: &str = match option_env!("VDISPLAY_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

const USAGE: &str = "\
Usage:
  vdisplay start [options]   Create the virtual display (use as Sunshine \"do\" command)
  vdisplay stop              Remove it again (use as Sunshine \"undo\" command)
  vdisplay status            Show whether a virtual display is active
  vdisplay install-driver    (Re)install the bundled SudoVDA driver (asks for admin rights)
  vdisplay uninstall-driver  Remove the SudoVDA device, driver package and certificate
                             (asks for admin rights)
  vdisplay version           Print the version

Options for start (defaults come from Sunshine's SUNSHINE_CLIENT_* variables):
  --width <px>          Display width          [SUNSHINE_CLIENT_WIDTH, else 1920]
  --height <px>         Display height         [SUNSHINE_CLIENT_HEIGHT, else 1080]
  --fps <hz>            Refresh rate, e.g. 60 or 59.94 [SUNSHINE_CLIENT_FPS, else 60]
  --hdr [true|false]    Force HDR on/off (also --no-hdr) [SUNSHINE_CLIENT_HDR, else untouched]
  --primary             Make the virtual display the primary display
  --exclusive           Disable all other displays while streaming (implies --primary)
  --name <text>         Monitor name / identity [SUNSHINE_CLIENT_NAME]
  --id <text>           Stable identity used to derive the monitor GUID [--name]
  --adapter <gpu>       Render GPU by name, e.g. \"NVIDIA GeForce RTX 4090\"
  --no-watch            Don't tear down automatically when sunshine.exe exits
";

#[derive(Debug, Clone)]
struct Options {
    width: u32,
    height: u32,
    refresh_mhz: u32,
    hdr: Option<bool>,
    primary: bool,
    exclusive: bool,
    name: String,
    id: String,
    adapter: Option<String>,
    watch: bool,
    watch_pid: Option<u32>,
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn parse_fps(s: &str) -> Result<u32, String> {
    let fps: f64 = s.trim().parse().map_err(|_| format!("invalid fps: {s}"))?;
    if fps <= 0.0 {
        return Err(format!("invalid fps: {s}"));
    }
    // Accept both Hz (60, 59.94) and millihertz (59940) like Apollo does.
    Ok(if fps >= 1000.0 {
        fps.round() as u32
    } else {
        (fps * 1000.0).round() as u32
    })
}

fn parse_options(args: &[String]) -> Result<Options, String> {
    let num = |v: Option<String>, def: u32| -> Result<u32, String> {
        match v {
            Some(v) => v.trim().parse().map_err(|_| format!("invalid number: {v}")),
            None => Ok(def),
        }
    };
    let mut width = num(env("SUNSHINE_CLIENT_WIDTH"), 1920)?;
    let mut height = num(env("SUNSHINE_CLIENT_HEIGHT"), 1080)?;
    let mut refresh_mhz = match env("SUNSHINE_CLIENT_FPS") {
        Some(v) => parse_fps(&v)?,
        None => 60_000,
    };
    let mut hdr = env("SUNSHINE_CLIENT_HDR").map(|v| v.eq_ignore_ascii_case("true"));
    let mut name = env("SUNSHINE_CLIENT_NAME");
    let mut id = None;
    let (mut primary, mut exclusive, mut watch, mut adapter, mut watch_pid) =
        (false, false, true, None, None);

    let mut it = args.iter().peekable();
    while let Some(arg) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{arg} needs a value"))
        };
        match arg.as_str() {
            "--width" => width = num(Some(value()?), 0)?,
            "--height" => height = num(Some(value()?), 0)?,
            "--fps" => refresh_mhz = parse_fps(&value()?)?,
            "--hdr" => {
                // Optional value so `--hdr %SUNSHINE_CLIENT_HDR%` works.
                let v = it
                    .next_if(|v| v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("false"));
                hdr = Some(v.is_none_or(|v| v.eq_ignore_ascii_case("true")));
            }
            "--no-hdr" => hdr = Some(false),
            "--primary" => primary = true,
            "--exclusive" => exclusive = true,
            "--name" => name = Some(value()?),
            "--id" => id = Some(value()?),
            "--adapter" => adapter = Some(value()?),
            "--no-watch" => watch = false,
            "--watch-pid" => watch_pid = Some(num(Some(value()?), 0)?),
            other => return Err(format!("unknown option: {other}")),
        }
    }
    if width == 0 || height == 0 {
        return Err("width and height must be non-zero".into());
    }
    let name = name.unwrap_or_else(|| "Sunshine".into());
    let id = id.unwrap_or_else(|| name.clone());
    Ok(Options {
        width,
        height,
        refresh_mhz,
        hdr,
        primary,
        exclusive,
        name,
        id,
        adapter,
        watch,
        watch_pid,
    })
}

impl Options {
    /// Arguments forwarded from `start` to the daemon, fully resolved.
    fn to_daemon_args(&self) -> Vec<String> {
        let mut a = vec![
            "daemon".into(),
            "--width".into(),
            self.width.to_string(),
            "--height".into(),
            self.height.to_string(),
            "--fps".into(),
            self.refresh_mhz.to_string(),
            "--name".into(),
            self.name.clone(),
            "--id".into(),
            self.id.clone(),
        ];
        match self.hdr {
            Some(true) => a.push("--hdr".into()),
            Some(false) => a.push("--no-hdr".into()),
            None => {}
        }
        if self.primary {
            a.push("--primary".into());
        }
        if self.exclusive {
            a.push("--exclusive".into());
        }
        if let Some(adapter) = &self.adapter {
            a.extend(["--adapter".into(), adapter.clone()]);
        }
        if let Some(pid) = self.watch_pid {
            a.extend(["--watch-pid".into(), pid.to_string()]);
        }
        a
    }
}

fn fnv1a(seed: u64, data: &[u8]) -> u64 {
    data.iter()
        .fold(seed, |h, &b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

/// Stable per-client monitor GUID, so Windows remembers scaling/arrangement per client
/// (Apollo uses the client UUID; Sunshine only exposes the client name).
fn monitor_guid(id: &str) -> GUID {
    let data = format!("sunshine-vdisplay:{id}");
    let hi = fnv1a(0xcbf29ce484222325, data.as_bytes());
    let lo = fnv1a(0x84222325cbf29ce4, data.as_bytes());
    let mut v = ((hi as u128) << 64) | lo as u128;
    // Mark as RFC 4122 variant / version 8 (custom).
    v = (v & !(0xF << 76)) | (0x8 << 76);
    v = (v & !(0x3 << 62)) | (0x2 << 62);
    GUID::from_u128(v)
}

/// Finds the closest `sunshine.exe` ancestor of this process.
fn find_sunshine_ancestor() -> Option<u32> {
    let mut procs = std::collections::HashMap::new();
    unsafe {
        let snap = Handle(CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?);
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut ok = Process32FirstW(snap.0, &mut entry).is_ok();
        while ok {
            let len = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            let exe = String::from_utf16_lossy(&entry.szExeFile[..len]);
            procs.insert(entry.th32ProcessID, (entry.th32ParentProcessID, exe));
            ok = Process32NextW(snap.0, &mut entry).is_ok();
        }
    }
    let mut pid = std::process::id();
    for _ in 0..8 {
        let (parent, _) = procs.get(&pid)?;
        let (_, exe) = procs.get(parent)?;
        if exe.eq_ignore_ascii_case("sunshine.exe") {
            return Some(*parent);
        }
        pid = *parent;
    }
    None
}

fn cmd_start(args: &[String]) -> Result<(), String> {
    let mut opts = parse_options(args)?;
    if opts.watch && opts.watch_pid.is_none() {
        opts.watch_pid = find_sunshine_ancestor();
    }
    log!("start: {opts:?}");

    if ipc::stop_daemon(15_000)? {
        log!("stopped previous virtual display");
    }
    if !install::driver_present() {
        return Err(
            "SudoVDA driver not found; run `vdisplay install-driver` once to install it".into(),
        );
    }
    remove_stale_display();

    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let spawn = |flags| {
        Command::new(&exe)
            .args(opts.to_daemon_args())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(flags)
            .spawn()
    };
    let base = (DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP).0;
    // Break away from any job object so the caller can't take the daemon down with it.
    let mut child = spawn(base | CREATE_BREAKAWAY_FROM_JOB.0)
        .or_else(|_| spawn(base))
        .map_err(|e| format!("failed to spawn daemon: {e}"))?;

    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(stdout).read_line(&mut line);
        let _ = tx.send(line);
    });
    let line = rx
        .recv_timeout(Duration::from_secs(30))
        .map_err(|_| "timed out waiting for the virtual display".to_string())?;
    let line = line.trim();
    if let Some(name) = line.strip_prefix("READY ") {
        log!("virtual display ready: {name}");
        println!("{name}");
        Ok(())
    } else if let Some(err) = line.strip_prefix("ERROR ") {
        Err(err.to_string())
    } else {
        Err(format!(
            "daemon exited unexpectedly (see {})",
            logging::path().display()
        ))
    }
}

fn cmd_stop() -> Result<(), String> {
    if ipc::stop_daemon(15_000)? {
        log!("virtual display removed");
    } else if !remove_stale_display() {
        log!("stop: no virtual display was active");
    }
    Ok(())
}

/// Records the GUID of the display owned by the daemon.
fn state_path() -> std::path::PathBuf {
    logging::path().with_file_name("vdisplay.state")
}

/// Removes a display left behind by a daemon that died without cleaning up. The driver
/// watchdog normally does this, but not while another client (e.g. Apollo) keeps pinging it.
fn remove_stale_display() -> bool {
    let Ok(state) = std::fs::read_to_string(state_path()) else {
        return false;
    };
    let Ok(guid) = u128::from_str_radix(state.trim(), 16).map(GUID::from_u128) else {
        let _ = std::fs::remove_file(state_path());
        return false;
    };
    let removed = match Driver::open() {
        Ok(driver) => match driver.remove_display(guid) {
            Ok(()) => {
                log!("removed stale virtual display {guid:?}");
                true
            }
            // Already gone (e.g. removed by the watchdog).
            Err(_) => false,
        },
        Err(e) => {
            log!("cannot clean up stale virtual display: {e}");
            return false;
        }
    };
    let _ = std::fs::remove_file(state_path());
    removed
}

fn cmd_daemon(args: &[String]) -> Result<(), String> {
    logging::set_echo(false);
    let opts = parse_options(args)?;
    run_daemon(&opts).inspect_err(|e| {
        // Before READY, `start` is reading our stdout; afterwards nobody is.
        let _ = writeln!(std::io::stdout(), "ERROR {e}");
    })
}

fn with_guardian_transition<T>(
    guardian: Option<&Guardian>,
    operation: &str,
    action: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let Some(active_guardian) = guardian else {
        return action();
    };

    if !active_guardian.is_running()? {
        return Err(format!("emergency guardian exited before {operation}"));
    }

    let transition = active_guardian
        .begin_transition()
        .map_err(|error| format!("cannot begin {operation}: {error}"))?;

    if !active_guardian.is_running()? {
        drop(transition);
        return Err(format!(
            "emergency guardian exited while preparing {operation}"
        ));
    }
    if active_guardian.emergency_pending()? {
        drop(transition);
        return Err(format!(
            "emergency restoration was requested before {operation}"
        ));
    }

    let result = action();
    drop(transition);

    // Check after releasing the mutex: the guardian signals this manual-reset
    // event before waiting for the mutex, so a request raised during the API call
    // remains observable here even if the guardian acquires the mutex immediately.
    if !active_guardian.is_running()? {
        return Err(format!("emergency guardian exited during {operation}"));
    }
    if active_guardian.emergency_pending()? {
        return Err(format!(
            "emergency restoration was requested during {operation}"
        ));
    }

    result
}

fn guardian_fallback_is_safe(active_guardian: &mut Guardian, context: &str) -> bool {
    match active_guardian.try_wait() {
        Ok(Some(status)) if status.success() => {
            log!(
                "daemon: guardian restoration was verified after {context}; local fallback is unnecessary"
            );
            false
        }
        Ok(Some(status)) => {
            log!(
                "daemon: guardian exited abnormally ({status}) after {context}; local fallback may proceed"
            );
            true
        }
        Ok(None) => {
            log!(
                "CRITICAL: guardian is still running after {context}; skipping local fallback to avoid concurrent display changes"
            );
            false
        }
        Err(error) => {
            log!(
                "CRITICAL: cannot establish whether guardian exited after {context} ({error}); skipping local fallback"
            );
            false
        }
    }
}

fn teardown_session(
    driver: &Driver,
    guid: GUID,
    target: Option<(windows::Win32::Foundation::LUID, u32)>,
    snapshot: Option<&display::Config>,
    guardian: &mut Option<Guardian>,
) {
    if let Some(active_guardian) = guardian.as_mut() {
        match active_guardian.try_wait() {
            Ok(Some(status)) if status.success() => {
                log!("daemon: independent guardian restored and verified the original topology");
                return;
            }
            Ok(Some(status)) => {
                log!(
                    "guardian already exited abnormally ({status}); local topology reconciliation may proceed"
                );
            }
            Ok(None) => match active_guardian.restore_and_wait() {
                Ok(()) => {
                    log!(
                        "daemon: independent guardian restored and verified the original topology"
                    );
                    return;
                }
                Err(error) => {
                    log!("ERROR: guardian restoration request failed: {error}");
                    if !guardian_fallback_is_safe(active_guardian, "restoration request failure") {
                        return;
                    }
                }
            },
            Err(error) => match active_guardian.restore_and_wait() {
                Ok(()) => {
                    log!("daemon: guardian restored topology after process-status error: {error}");
                    return;
                }
                Err(restore_error) => {
                    log!(
                        "ERROR: guardian status failed ({error}) and restoration failed ({restore_error})"
                    );
                    if !guardian_fallback_is_safe(
                        active_guardian,
                        "process-status/restoration failure",
                    ) {
                        return;
                    }
                }
            },
        }
    }

    match driver.remove_display(guid) {
        Ok(()) => {
            log!("daemon: virtual display removed by local fallback");
            let _ = std::fs::remove_file(state_path());
        }
        Err(error) => log!("daemon: local fallback could not remove virtual display: {error}"),
    }
    let _ = std::fs::remove_file(state_path());

    if let Some((adapter_luid, target_id)) = target {
        display::wait_for_display_gone(adapter_luid, target_id, Duration::from_secs(3));
    }
    std::thread::sleep(Duration::from_millis(300));

    if let Some(snapshot) = snapshot {
        let mut restored = false;
        for attempt in 1..=3 {
            let mut candidate = snapshot.clone();
            match candidate.apply(windows::Win32::Devices::Display::SET_DISPLAY_CONFIG_FLAGS(0)) {
                Ok(()) => {
                    std::thread::sleep(Duration::from_millis(350));
                    match display::Config::query() {
                        Ok(current) if snapshot.same_active_layout(&current) => {
                            log!(
                                "daemon: fallback topology restoration verified (attempt {attempt}/3)"
                            );
                            restored = true;
                            break;
                        }
                        Ok(current) => log!(
                            "daemon: fallback restore attempt {attempt}/3 did not verify; {} active path(s)",
                            current.paths.len()
                        ),
                        Err(error) => log!(
                            "daemon: fallback restore verification attempt {attempt}/3 failed: {error}"
                        ),
                    }
                }
                Err(error) => log!("daemon: fallback restore attempt {attempt}/3 failed: {error}"),
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        if !restored {
            log!(
                "CRITICAL: neither guardian nor daemon fallback could verify the original display topology"
            );
        }
    }
}
fn run_daemon(opts: &Options) -> Result<(), String> {
    log!("daemon: vdisplay {VERSION}");
    let (_alive, stop) = ipc::daemon_objects()?;
    let driver = Arc::new(Driver::open()?);

    // Keep the driver watchdog fed for as long as we own the display.
    let watchdog = driver
        .watchdog()
        .map_err(|e| format!("cannot query watchdog: {e}"))?;
    log!("daemon: watchdog timeout {}s", watchdog.timeout);
    let pinging = Arc::new(AtomicBool::new(true));
    let ping_failed = Arc::new(AtomicBool::new(false));
    let pinger = (watchdog.timeout > 0).then(|| {
        let (driver, pinging, ping_failed) = (driver.clone(), pinging.clone(), ping_failed.clone());
        let interval = Duration::from_millis((watchdog.timeout as u64 * 1000 / 3).max(200));
        std::thread::spawn(move || {
            let mut failures = 0;
            while pinging.load(Ordering::Relaxed) {
                if driver.ping().is_err() {
                    failures += 1;
                    if failures > 3 {
                        ping_failed.store(true, Ordering::Relaxed);
                        return;
                    }
                } else {
                    failures = 0;
                }
                std::thread::sleep(interval);
            }
        })
    });

    if let Some(adapter) = &opts.adapter {
        match display::find_adapter_luid(adapter)
            .and_then(|luid| driver.set_render_adapter(luid).map_err(|e| e.to_string()))
        {
            Ok(()) => log!("render adapter set to {adapter}"),
            Err(e) => log!("cannot set render adapter: {e}"),
        }
    }

    let changes_topology = opts.primary || opts.exclusive;
    let snapshot = if changes_topology {
        Some(display::Config::query().map_err(|e| {
            format!("cannot snapshot the current display layout before changing topology: {e}")
        })?)
    } else {
        None
    };

    let guid = monitor_guid(&opts.id);
    let mut guardian = if opts.exclusive {
        log!("exclusive: starting independent emergency guardian before display creation");
        Some(Guardian::start(guid)?)
    } else {
        None
    };
    let serial = format!("{:012X}", guid.data1 as u64 | ((guid.data2 as u64) << 32));
    // Recorded before adding so a killed daemon's display can still be cleaned up.
    let _ = std::fs::write(state_path(), format!("{:032x}", guid.to_u128()));
    let added_result =
        with_guardian_transition(guardian.as_ref(), "virtual display creation", || {
            driver
                .add_display(
                    opts.width,
                    opts.height,
                    opts.refresh_mhz,
                    guid,
                    &opts.name,
                    &serial,
                )
                .map_err(|error| error.to_string())
        });
    let added = match added_result {
        Ok(added) => added,
        Err(error) => {
            teardown_session(&driver, guid, None, snapshot.as_ref(), &mut guardian);
            return Err(format!("failed to add virtual display: {error}"));
        }
    };
    log!(
        "daemon: added display {guid:?} (target {})",
        added.target_id
    );

    let teardown = |driver: &Driver, guardian: &mut Option<Guardian>| {
        teardown_session(
            driver,
            guid,
            Some((added.adapter_luid, added.target_id)),
            snapshot.as_ref(),
            guardian,
        );
    };
    let device = match display::wait_for_display_name(
        added.adapter_luid,
        added.target_id,
        Duration::from_secs(10),
    ) {
        Ok(device) => device,
        Err(error) => {
            if opts.exclusive {
                log!(
                    "exclusive startup aborted before topology switching: the virtual display never became active, so a --primary fallback cannot be applied safely"
                );
            }
            teardown(&driver, &mut guardian);
            return Err(error);
        }
    };
    log!("daemon: virtual display is {device}");

    let mode_result =
        with_guardian_transition(guardian.as_ref(), "virtual display mode setup", || {
            display::set_mode(&device, opts.width, opts.height, opts.refresh_mhz)
                .map_err(|e| e.to_string())
        });
    if let Err(error) = mode_result {
        if opts.exclusive {
            log!("ERROR: exclusive startup aborted during display-mode setup: {error}");
            teardown(&driver, &mut guardian);
            return Err(format!(
                "exclusive startup aborted during display-mode setup: {error}"
            ));
        }
        log!("daemon: cannot set display mode: {error}");
    }
    if opts.exclusive {
        if guardian.is_none() {
            teardown(&driver, &mut guardian);
            return Err("exclusive mode refused: emergency guardian is not running".into());
        }

        let guardian_status = guardian
            .as_mut()
            .expect("guardian checked above")
            .try_wait();
        match guardian_status {
            Ok(None) => {}
            Ok(Some(status)) => {
                teardown(&driver, &mut guardian);
                return Err(format!(
                    "emergency guardian exited before exclusive activation: {status}"
                ));
            }
            Err(error) => {
                teardown(&driver, &mut guardian);
                return Err(format!(
                    "cannot verify emergency guardian before exclusive activation: {error}"
                ));
            }
        }

        let heartbeat_result = guardian
            .as_ref()
            .expect("guardian checked above")
            .heartbeat();
        if let Err(error) = heartbeat_result {
            teardown(&driver, &mut guardian);
            return Err(format!(
                "cannot heartbeat emergency guardian before exclusive activation: {error}"
            ));
        }

        log!("exclusive: independent guardian armed; attempting topology switch");
        match with_guardian_transition(guardian.as_ref(), "exclusive topology switch", || {
            display::make_exclusive(added.adapter_luid, added.target_id).map_err(|e| e.to_string())
        }) {
            Ok(()) => log!("exclusive: active; physical displays disabled"),
            Err(error) => {
                log!("ERROR: exclusive activation failed: {error}");
                teardown(&driver, &mut guardian);
                return Err(format!(
                    "exclusive activation failed; session aborted and restoration requested: {error}"
                ));
            }
        }
    } else if opts.primary {
        match display::set_primary(added.adapter_luid, added.target_id) {
            Ok(()) => log!("daemon: {device} is now primary"),
            Err(e) => log!("daemon: cannot set primary display: {e}"),
        }
    }
    if let Some(hdr) = opts.hdr {
        match with_guardian_transition(guardian.as_ref(), "HDR configuration", || {
            display::set_hdr(added.adapter_luid, added.target_id, hdr).map_err(|e| e.to_string())
        }) {
            Ok(()) => log!("daemon: HDR {}", if hdr { "enabled" } else { "disabled" }),
            Err(error) if opts.exclusive => {
                log!("ERROR: exclusive startup aborted during HDR configuration: {error}");
                teardown(&driver, &mut guardian);
                return Err(format!(
                    "exclusive startup aborted during HDR configuration: {error}"
                ));
            }
            Err(error) => log!("daemon: cannot set HDR: {error}"),
        }
    }

    {
        let mut out = std::io::stdout();
        let _ = writeln!(out, "READY {device}");
        let _ = out.flush();
    }

    // Wait for `stop`, for Sunshine to go away, or for the driver to stop answering.
    let watched = opts.watch_pid.and_then(|pid| {
        match unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
            Ok(h) => {
                log!("daemon: watching sunshine.exe (pid {pid})");
                Some(Handle(h))
            }
            Err(e) => {
                log!("daemon: cannot watch pid {pid}: {e}");
                None
            }
        }
    });
    let mut handles: Vec<HANDLE> = vec![stop.0];
    handles.extend(watched.as_ref().map(|h| h.0));
    loop {
        let r = unsafe { WaitForMultipleObjects(&handles, false, 1000) };
        if r == WAIT_OBJECT_0 {
            log!("daemon: stop requested");
            break;
        } else if r.0 == WAIT_OBJECT_0.0 + 1 {
            log!("daemon: sunshine.exe exited");
            break;
        } else if r == WAIT_TIMEOUT {
            if ping_failed.load(Ordering::Relaxed) {
                log!("daemon: driver stopped answering pings");
                break;
            }
            if let Some(active_guardian) = guardian.as_mut() {
                match active_guardian.try_wait() {
                    Ok(Some(status)) => {
                        if status.success() {
                            log!(
                                "guardian: emergency restoration completed; ending exclusive session"
                            );
                        } else {
                            log!(
                                "guardian: exited abnormally ({status}); local restoration will be attempted"
                            );
                        }
                        break;
                    }
                    Ok(None) => {
                        match active_guardian.emergency_pending() {
                            Ok(true) => {
                                log!(
                                    "guardian: emergency restoration requested; ending exclusive session"
                                );
                                break;
                            }
                            Ok(false) => {}
                            Err(error) => {
                                log!(
                                    "guardian: cannot inspect emergency state: {error}; ending exclusive session"
                                );
                                break;
                            }
                        }
                        if let Err(error) = active_guardian.heartbeat() {
                            log!("guardian: heartbeat failed: {error}; ending exclusive session");
                            break;
                        }
                    }
                    Err(error) => {
                        log!(
                            "guardian: process status check failed: {error}; ending exclusive session"
                        );
                        break;
                    }
                }
            }
        } else {
            log!("daemon: wait failed ({r:?})");
            break;
        }
    }

    teardown(&driver, &mut guardian);
    pinging.store(false, Ordering::Relaxed);
    if let Some(p) = pinger {
        let _ = p.join();
    }
    Ok(())
}

fn attach_console() {
    unsafe {
        // GUI-subsystem binary: when run by hand from a console, print there.
        if GetStdHandle(STD_OUTPUT_HANDLE).map_or(true, |h| h.is_invalid() || h.0.is_null()) {
            let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

fn cmd_guardian_recovery_test(args: &[String]) -> Result<(), String> {
    if crate::ipc::daemon_running() {
        return Err("a vdisplay daemon is already active; guardian recovery test refused".into());
    }

    println!(
        "WARNING: this test creates a real temporary 1920x1080 virtual monitor, then asks the independent guardian to remove it. It does not call --exclusive or --primary, but Windows may briefly reflow the desktop."
    );
    let [confirmation] = args else {
        return Err("usage: vdisplay guardian-recovery-test RESTORE-TEST".into());
    };
    if confirmation != "RESTORE-TEST" {
        return Err("guardian recovery test cancelled; no display was created".into());
    }

    let before = display::Config::query().map_err(|e| {
        format!("cannot capture display topology before guardian recovery test: {e}")
    })?;
    let driver = std::sync::Arc::new(
        crate::driver::Driver::open()
            .map_err(|e| format!("cannot open SudoVDA for guardian recovery test: {e}"))?,
    );

    // Keep the SudoVDA watchdog alive for the entire test, matching run_daemon().
    let watchdog = driver
        .watchdog()
        .map_err(|e| format!("cannot query driver watchdog: {e}"))?;
    let pinging = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let ping_failed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let pinger = (watchdog.timeout > 0).then(|| {
        let driver = std::sync::Arc::clone(&driver);
        let pinging = std::sync::Arc::clone(&pinging);
        let ping_failed = std::sync::Arc::clone(&ping_failed);
        let interval =
            std::time::Duration::from_millis((watchdog.timeout as u64 * 1000 / 3).max(200));
        std::thread::spawn(move || {
            let mut failures = 0;
            while pinging.load(std::sync::atomic::Ordering::Relaxed) {
                if driver.ping().is_err() {
                    failures += 1;
                    if failures > 3 {
                        ping_failed.store(true, std::sync::atomic::Ordering::Relaxed);
                        return;
                    }
                } else {
                    failures = 0;
                }
                std::thread::sleep(interval);
            }
        })
    });

    // Keep the Guardian outside the test closure so every error path can request
    // and await restoration before stopping the driver watchdog.
    let mut guardian: Option<guardian::Guardian> = None;
    let test_result = (|| -> Result<(), String> {
        let unique_id = format!(
            "GuardianRecoveryTest-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| format!("system clock is before UNIX epoch: {e}"))?
                .as_nanos()
        );
        let guid = monitor_guid(&unique_id);
        let serial = format!("{:012X}", guid.data1 as u64 | ((guid.data2 as u64) << 32));

        // Guardian captures the original layout before the temporary monitor is added.
        guardian = Some(guardian::Guardian::start(guid)?);
        println!(
            "Guardian armed. Creating a temporary 1920x1080 virtual display; --exclusive and --primary are not used."
        );
        println!(
            "Windows may briefly update the desktop layout while the temporary monitor is present."
        );

        let active_guardian = guardian
            .as_ref()
            .ok_or_else(|| "guardian was not initialized".to_string())?;
        let added = with_guardian_transition(
            Some(active_guardian),
            "guardian recovery test display creation",
            || {
                driver
                    .add_display(1920, 1080, 60_000, guid, "GdnRecovery", &serial)
                    .map_err(|error| error.to_string())
            },
        )?;

        let device = display::wait_for_display_name(
            added.adapter_luid,
            added.target_id,
            std::time::Duration::from_secs(10),
        )?;
        if ping_failed.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("SudoVDA watchdog ping failed during temporary monitor creation".into());
        }

        guardian
            .as_ref()
            .ok_or_else(|| "guardian disappeared before heartbeat".to_string())?
            .heartbeat()?;
        guardian
            .as_mut()
            .ok_or_else(|| "guardian disappeared before restoration request".to_string())?
            .restore_and_wait()
            .map_err(|e| format!("guardian did not confirm restoration: {e}"))?;

        // wait_for_display_gone polls active paths; the exact snapshot comparison
        // below then verifies that the temporary target is no longer active and
        // every original active path has returned.
        display::wait_for_display_gone(
            added.adapter_luid,
            added.target_id,
            std::time::Duration::from_secs(10),
        );
        let after = display::Config::query().map_err(|e| {
            format!("guardian exited, but final display-topology query failed: {e}")
        })?;
        if !before.same_active_layout(&after) {
            return Err(
                "CRITICAL: guardian completed but the original active display topology was not restored; do not start another vdisplay session".into(),
            );
        }
        if ping_failed.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("SudoVDA watchdog ping failed during guardian restoration".into());
        }

        println!(
            "SUCCESS: temporary monitor {device} was created, removed by the independent guardian, and the original active display topology was verified unchanged."
        );
        Ok(())
    })();

    // Always request/await guardian restoration before stopping the watchdog.
    let cleanup_result = match guardian.as_mut() {
        Some(active_guardian) => active_guardian.restore_and_wait(),
        None => Ok(()),
    };
    pinging.store(false, std::sync::atomic::Ordering::Relaxed);
    let pinger_result = match pinger {
        Some(handle) => handle
            .join()
            .map_err(|_| "SudoVDA watchdog thread panicked".to_string()),
        None => Ok(()),
    };

    let mut errors = Vec::new();
    if let Err(error) = test_result {
        errors.push(error);
    }
    if let Err(error) = cleanup_result {
        errors.push(format!(
            "CRITICAL: guardian cleanup/restoration failed: {error}"
        ));
    }
    if let Err(error) = pinger_result {
        errors.push(error);
    }
    if ping_failed.load(std::sync::atomic::Ordering::Relaxed) {
        errors.push("SudoVDA watchdog reported more than three consecutive ping failures".into());
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn main() -> ExitCode {
    attach_console();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, rest) = args
        .split_first()
        .map_or(("", &[][..]), |(c, r)| (c.as_str(), r));
    let result = match cmd {
        "start" => cmd_start(rest),
        "stop" => cmd_stop(),
        "daemon" => cmd_daemon(rest),
        "guardian" => guardian::cmd(rest),
        "guardian-test" => guardian::self_test(),
        "guardian-recovery-test" => cmd_guardian_recovery_test(rest),
        "install-driver" | "--install-driver" => install::install(),
        "uninstall-driver" | "--uninstall-driver" => install::uninstall(),
        "version" | "--version" | "-V" => {
            println!("vdisplay {VERSION}");
            Ok(())
        }
        "status" => {
            println!("version: {VERSION}");
            println!(
                "{}",
                if ipc::daemon_running() {
                    "active"
                } else {
                    "inactive"
                }
            );
            println!(
                "driver: {}",
                if install::driver_present() {
                    "installed"
                } else {
                    "not installed"
                }
            );
            println!("log: {}", logging::path().display());
            Ok(())
        }
        _ => {
            print!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
