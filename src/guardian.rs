//! Independent emergency guardian for `--exclusive` display sessions.
//!
//! The guardian is a separate vdisplay process. It captures the active Windows
//! display topology before the parent daemon creates or changes any display,
//! registers Ctrl+Alt+F11, and owns restoration until the original topology is
//! verified again.

use std::ffi::{c_int, c_void};
use std::io::{BufRead, BufReader, Write};
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::Devices::Display::SDC_ALLOW_CHANGES;
use windows::core::GUID;

use crate::display::Config;
use crate::driver::Driver;
use crate::logging;

const HOTKEY_ID: c_int = 0x5644;
const MOD_ALT_CONTROL_NOREPEAT: u32 = 0x4000 | 0x0001 | 0x0002;
const VK_F11: u32 = 0x7A;
const WM_HOTKEY: u32 = 0x0312;
const PM_REMOVE: u32 = 0x0001;
const EVENT_MODIFY_STATE: u32 = 0x0002;
const SYNCHRONIZE: u32 = 0x0010_0000;
const MUTEX_MODIFY_STATE: u32 = 0x0001;
const PROCESS_TERMINATE: u32 = 0x0001;
const WAIT_ABANDONED_0: u32 = 0x0000_0080;
const TRANSITION_WAIT_MS: u32 = 5_000;
const GUARDIAN_TERMINATE_EXIT_CODE: u32 = 0xE000_5644;
const WAIT_OBJECT_0: u32 = 0;
const WAIT_TIMEOUT: u32 = 0x0000_0102;
const WAIT_FAILED: u32 = 0xFFFF_FFFF;
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL_MS: u32 = 100;

#[repr(C)]
#[derive(Clone, Copy)]
struct Point {
    x: i32,
    y: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Msg {
    hwnd: *mut c_void,
    message: u32,
    w_param: usize,
    l_param: isize,
    time: u32,
    pt: Point,
    l_private: u32,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateEventW(
        event_attributes: *const c_void,
        manual_reset: i32,
        initial_state: i32,
        name: *const u16,
    ) -> *mut c_void;
    fn OpenEventW(access: u32, inherit_handle: i32, name: *const u16) -> *mut c_void;
    fn CreateMutexW(
        mutex_attributes: *const c_void,
        initial_owner: i32,
        name: *const u16,
    ) -> *mut c_void;
    fn OpenMutexW(access: u32, inherit_handle: i32, name: *const u16) -> *mut c_void;
    fn OpenProcess(access: u32, inherit_handle: i32, process_id: u32) -> *mut c_void;
    fn SetEvent(event: *mut c_void) -> i32;
    fn ReleaseMutex(mutex: *mut c_void) -> i32;
    fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
    fn TerminateProcess(process: *mut c_void, exit_code: u32) -> i32;
    fn WaitForMultipleObjects(
        count: u32,
        handles: *const *mut c_void,
        wait_all: i32,
        milliseconds: u32,
    ) -> u32;
    fn CloseHandle(handle: *mut c_void) -> i32;
}

#[link(name = "user32")]
unsafe extern "system" {
    fn RegisterHotKey(window: *mut c_void, id: c_int, modifiers: u32, virtual_key: u32) -> i32;
    fn UnregisterHotKey(window: *mut c_void, id: c_int) -> i32;
    fn PeekMessageW(
        message: *mut Msg,
        window: *mut c_void,
        min_filter: u32,
        max_filter: u32,
        remove_message: u32,
    ) -> i32;
}

struct WinHandle(*mut c_void);

impl WinHandle {
    fn new(raw: *mut c_void, what: &str) -> Result<Self, String> {
        if raw.is_null() {
            Err(format!(
                "{what} failed: {}",
                std::io::Error::last_os_error()
            ))
        } else {
            Ok(Self(raw))
        }
    }

    fn raw(&self) -> *mut c_void {
        self.0
    }
}

impl Drop for WinHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

pub struct TransitionGuard<'a> {
    mutex: &'a WinHandle,
}

impl Drop for TransitionGuard<'_> {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.mutex.raw());
        }
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn channel_names(session: &str) -> (String, String, String, String) {
    (
        format!("Local\\SunshineVDisplay.Guardian.{session}.Heartbeat"),
        format!("Local\\SunshineVDisplay.Guardian.{session}.Restore"),
        format!("Local\\SunshineVDisplay.Guardian.{session}.Transition"),
        format!("Local\\SunshineVDisplay.Guardian.{session}.Emergency"),
    )
}

/// Client held by the display daemon while an exclusive session is active.
#[allow(dead_code)] // Connected to run_daemon in the next integration step.
pub struct Guardian {
    child: Child,
    heartbeat: WinHandle,
    restore: WinHandle,
    transition: WinHandle,
    emergency: WinHandle,
    completed: bool,
}

#[allow(dead_code)] // Connected to run_daemon in the next integration step.
impl Guardian {
    /// Starts and handshakes with the independent guardian. No display has been
    /// created or changed when this function returns an error.
    pub fn start(monitor_guid: GUID) -> Result<Self, String> {
        let session_value = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| format!("system clock error: {e}"))?
            .as_nanos()
            ^ ((std::process::id() as u128) << 64);
        let session = format!("{session_value:032x}");
        let guid_text = format!("{:032x}", monitor_guid.to_u128());
        let parent_pid = std::process::id().to_string();
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;

        // Detached from the daemon's console/job where possible. The guardian
        // also watches the daemon process handle, so it does not depend solely
        // on an IPC heartbeat if the daemon terminates.
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        let flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
        let spawn = |creation_flags| {
            Command::new(&exe)
                .arg("guardian")
                .arg(&guid_text)
                .arg(&session)
                .arg(&parent_pid)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .creation_flags(creation_flags)
                .spawn()
        };
        let mut child = spawn(flags | CREATE_BREAKAWAY_FROM_JOB)
            .or_else(|_| spawn(flags))
            .map_err(|e| format!("cannot start emergency guardian: {e}"))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "guardian stdout pipe was not created".to_string())?;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut line = String::new();
            let _ = BufReader::new(stdout).read_line(&mut line);
            let _ = tx.send(line);
        });

        let line = match rx.recv_timeout(Duration::from_secs(15)) {
            Ok(line) => line,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("timed out waiting for the emergency guardian to arm".into());
            }
        };
        let line = line.trim();
        if line != "READY" {
            let _ = child.wait();
            let detail = line.strip_prefix("ERROR ").unwrap_or(line);
            return Err(format!("emergency guardian startup failed: {detail}"));
        }

        let (heartbeat_name, restore_name, transition_name, emergency_name) =
            channel_names(&session);
        let heartbeat_wide = wide(&heartbeat_name);
        let restore_wide = wide(&restore_name);
        let transition_wide = wide(&transition_name);
        let emergency_wide = wide(&emergency_name);
        let heartbeat = unsafe {
            WinHandle::new(
                OpenEventW(EVENT_MODIFY_STATE, 0, heartbeat_wide.as_ptr()),
                "OpenEventW(guardian heartbeat)",
            )
        };
        let restore = unsafe {
            WinHandle::new(
                OpenEventW(EVENT_MODIFY_STATE, 0, restore_wide.as_ptr()),
                "OpenEventW(guardian restore)",
            )
        };
        let transition = unsafe {
            WinHandle::new(
                OpenMutexW(
                    SYNCHRONIZE | MUTEX_MODIFY_STATE,
                    0,
                    transition_wide.as_ptr(),
                ),
                "OpenMutexW(guardian transition)",
            )
        };
        let emergency = unsafe {
            WinHandle::new(
                OpenEventW(SYNCHRONIZE, 0, emergency_wide.as_ptr()),
                "OpenEventW(guardian emergency)",
            )
        };

        match (heartbeat, restore, transition, emergency) {
            (Ok(heartbeat), Ok(restore), Ok(transition), Ok(emergency)) => Ok(Self {
                child,
                heartbeat,
                restore,
                transition,
                emergency,
                completed: false,
            }),
            (heartbeat, restore, transition, emergency) => {
                // This happens before the caller is allowed to change topology.
                let _ = child.kill();
                let _ = child.wait();
                Err(format!(
                    "guardian channel setup failed: heartbeat={}; restore={}; transition={}; emergency={}",
                    heartbeat.err().unwrap_or_else(|| "ok".into()),
                    restore.err().unwrap_or_else(|| "ok".into()),
                    transition.err().unwrap_or_else(|| "ok".into()),
                    emergency.err().unwrap_or_else(|| "ok".into())
                ))
            }
        }
    }

    /// Returns whether the independent guardian process is still alive.
    /// This does not reap the child; try_wait() remains responsible for that.
    pub fn is_running(&self) -> Result<bool, String> {
        let process = unsafe {
            WinHandle::new(
                OpenProcess(SYNCHRONIZE, 0, self.child.id()),
                "OpenProcess(guardian status)",
            )?
        };
        match unsafe { WaitForSingleObject(process.raw(), 0) } {
            WAIT_OBJECT_0 => Ok(false),
            WAIT_TIMEOUT => Ok(true),
            WAIT_FAILED => Err(format!(
                "cannot inspect guardian process: {}",
                std::io::Error::last_os_error()
            )),
            other => Err(format!(
                "unexpected guardian process wait result 0x{other:08X}"
            )),
        }
    }

    /// Acquires the shared topology-transition mutex without waiting. Hold the guard
    /// across every display API call that can alter the topology or virtual display.
    pub fn begin_transition(&self) -> Result<TransitionGuard<'_>, String> {
        if self.emergency_pending()? {
            return Err("emergency restoration is already pending".into());
        }
        match unsafe { WaitForSingleObject(self.transition.raw(), 0) } {
            WAIT_OBJECT_0 => {
                match self.emergency_pending() {
                    Ok(false) => Ok(TransitionGuard {
                        mutex: &self.transition,
                    }),
                    Ok(true) => {
                        unsafe {
                            let _ = ReleaseMutex(self.transition.raw());
                        }
                        Err("emergency restoration was requested during transition-lock acquisition".into())
                    }
                    Err(error) => {
                        unsafe {
                            let _ = ReleaseMutex(self.transition.raw());
                        }
                        Err(error)
                    }
                }
            }
            WAIT_ABANDONED_0 => {
                unsafe {
                    let _ = ReleaseMutex(self.transition.raw());
                }
                Err(
                    "guardian transition mutex was abandoned; refusing further display changes"
                        .into(),
                )
            }
            WAIT_TIMEOUT => {
                Err("guardian owns the transition mutex; restoration is in progress".into())
            }
            WAIT_FAILED => Err(format!(
                "cannot acquire guardian transition mutex: {}",
                std::io::Error::last_os_error()
            )),
            other => Err(format!("unexpected transition mutex result 0x{other:08X}")),
        }
    }

    /// Reports whether the guardian has requested emergency restoration.
    pub fn emergency_pending(&self) -> Result<bool, String> {
        match unsafe { WaitForSingleObject(self.emergency.raw(), 0) } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            WAIT_FAILED => Err(format!(
                "cannot inspect guardian emergency event: {}",
                std::io::Error::last_os_error()
            )),
            other => Err(format!("unexpected emergency-event result 0x{other:08X}")),
        }
    }

    /// Must be called by the daemon's main control loop, not by a detached
    /// heartbeat thread. A hung daemon therefore stops feeding the guardian.
    pub fn heartbeat(&self) -> Result<(), String> {
        let ok = unsafe { SetEvent(self.heartbeat.raw()) };
        if ok == 0 {
            Err(format!(
                "cannot signal guardian heartbeat: {}",
                std::io::Error::last_os_error()
            ))
        } else {
            Ok(())
        }
    }

    /// Returns Some(status) only after the guardian has completed restoration.
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>, String> {
        let status = self
            .child
            .try_wait()
            .map_err(|e| format!("cannot inspect guardian process: {e}"))?;
        if status.as_ref().is_some_and(|status| status.success()) {
            // A successful guardian exit occurs only after restoration was verified.
            self.completed = true;
        }
        Ok(status)
    }

    /// Requests restoration and does not return until the guardian reports that
    /// the original active topology was restored and verified.
    pub fn restore_and_wait(&mut self) -> Result<(), String> {
        if !self.completed {
            let ok = unsafe { SetEvent(self.restore.raw()) };
            if ok == 0 {
                return Err(format!(
                    "cannot request guardian restoration: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let status = self
                .child
                .wait()
                .map_err(|e| format!("cannot wait for guardian restoration: {e}"))?;
            if !status.success() {
                return Err(format!(
                    "guardian exited abnormally after restoration request: {status}"
                ));
            }
            self.completed = true;
        }
        Ok(())
    }
}

impl Drop for Guardian {
    fn drop(&mut self) {
        if !self.completed {
            // Best-effort fail-safe: if the daemon returns early or unwinds,
            // ask the still-independent guardian to restore. If the daemon is
            // then terminated, the guardian also observes its process handle.
            unsafe {
                let _ = SetEvent(self.restore.raw());
            }
        }
    }
}

/// Exercises the guardian/daemon transition mutex without changing any display.
/// The zero GUID makes the guardian verify the captured topology without removing
/// a device or applying a display configuration.
pub fn self_test() -> Result<(), String> {
    if crate::ipc::daemon_running() {
        return Err("a vdisplay daemon is already active; guardian self-test refused".into());
    }

    let before = Config::query()
        .map_err(|e| format!("cannot capture display topology before guardian self-test: {e}"))?;
    let mut guardian = Guardian::start(GUID::from_u128(0))?;
    println!("Guardian armed for transition-mutex test. No display will be created or changed.");

    // Simulate a daemon display transition in progress. The guardian is asked to
    // restore while this thread owns the mutex; it must signal Emergency and wait.
    let transition = guardian.begin_transition()?;
    if unsafe { SetEvent(guardian.restore.raw()) } == 0 {
        drop(transition);
        let cleanup = guardian.restore_and_wait();
        return Err(format!(
            "cannot request guardian self-test restoration: {}; cleanup={}",
            std::io::Error::last_os_error(),
            cleanup.err().unwrap_or_else(|| "ok".into())
        ));
    }

    let emergency_deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if guardian.emergency_pending()? {
            break;
        }
        if !guardian.is_running()? {
            drop(transition);
            return Err(
                "guardian exited before signaling emergency while the transition mutex was held"
                    .into(),
            );
        }
        if Instant::now() >= emergency_deadline {
            drop(transition);
            let cleanup = guardian.restore_and_wait();
            return Err(format!(
                "guardian did not signal the emergency event within 3 seconds; cleanup={}",
                cleanup.err().unwrap_or_else(|| "ok".into())
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }

    println!("Guardian requested restoration; verifying it waits for the transition mutex...");
    let hold_deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < hold_deadline {
        if !guardian.is_running()? {
            drop(transition);
            return Err("guardian exited before the transition mutex was released".into());
        }
        thread::sleep(Duration::from_millis(20));
    }

    // Release the mutex on the same thread that acquired it, then require a clean
    // guardian exit. No display operation is possible with the reserved zero GUID.
    drop(transition);
    guardian.restore_and_wait()?;

    let after = Config::query()
        .map_err(|e| format!("cannot verify display topology after guardian self-test: {e}"))?;
    if !before.same_active_layout(&after) {
        return Err(
            "guardian mutex self-test detected a display-topology difference; do not enable --exclusive"
                .into(),
        );
    }

    println!(
        "SUCCESS: guardian signaled emergency, waited for transition-mutex release, restored, and verified the original topology unchanged."
    );
    Ok(())
}
/// Entry point for the detached `vdisplay guardian` process.
pub fn cmd(args: &[String]) -> Result<(), String> {
    logging::set_echo(false);
    match run(args) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = writeln!(std::io::stdout(), "ERROR {error}");
            let _ = std::io::stdout().flush();
            Err(error)
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    if args.len() != 3 {
        return Err("guardian expects <monitor-guid-hex> <session-hex> <daemon-pid>".into());
    }
    let guid_value =
        u128::from_str_radix(&args[0], 16).map_err(|e| format!("invalid monitor GUID: {e}"))?;
    let guid = GUID::from_u128(guid_value);
    let session = &args[1];
    if session.len() != 32 || !session.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err("invalid guardian session identifier".into());
    }
    let daemon_pid: u32 = args[2]
        .parse()
        .map_err(|e| format!("invalid daemon PID: {e}"))?;

    // Capture the physical layout and prove that the recovery driver can be
    // opened before advertising READY to the daemon.
    let snapshot = Config::query()
        .map_err(|e| format!("cannot capture pre-exclusive display topology: {e}"))?;
    let driver =
        Driver::open().map_err(|e| format!("cannot open SudoVDA for emergency removal: {e}"))?;
    let parent = unsafe {
        WinHandle::new(
            OpenProcess(SYNCHRONIZE | PROCESS_TERMINATE, 0, daemon_pid),
            "OpenProcess(daemon)",
        )?
    };

    let (heartbeat_name, restore_name, transition_name, emergency_name) = channel_names(session);
    let heartbeat_wide = wide(&heartbeat_name);
    let restore_wide = wide(&restore_name);
    let transition_wide = wide(&transition_name);
    let emergency_wide = wide(&emergency_name);
    // Auto-reset heartbeat event: each signal is consumed by this process.
    let heartbeat = unsafe {
        WinHandle::new(
            CreateEventW(std::ptr::null(), 0, 0, heartbeat_wide.as_ptr()),
            "CreateEventW(guardian heartbeat)",
        )?
    };
    // Manual-reset restore request so a request cannot be missed.
    let restore = unsafe {
        WinHandle::new(
            CreateEventW(std::ptr::null(), 1, 0, restore_wide.as_ptr()),
            "CreateEventW(guardian restore)",
        )?
    };
    let transition = unsafe {
        WinHandle::new(
            CreateMutexW(std::ptr::null(), 0, transition_wide.as_ptr()),
            "CreateMutexW(guardian transition)",
        )?
    };
    let emergency = unsafe {
        WinHandle::new(
            CreateEventW(std::ptr::null(), 1, 0, emergency_wide.as_ptr()),
            "CreateEventW(guardian emergency)",
        )?
    };

    let registered = unsafe {
        RegisterHotKey(
            std::ptr::null_mut(),
            HOTKEY_ID,
            MOD_ALT_CONTROL_NOREPEAT,
            VK_F11,
        )
    };
    if registered == 0 {
        return Err(format!(
            "cannot register Ctrl+Alt+F11 emergency hotkey: {}",
            std::io::Error::last_os_error()
        ));
    }
    let hotkey = HotkeyGuard(true);

    writeln!(std::io::stdout(), "READY").map_err(|e| e.to_string())?;
    std::io::stdout().flush().map_err(|e| e.to_string())?;
    crate::log!(
        "guardian: armed; hotkey Ctrl+Alt+F11; heartbeat timeout {}s; daemon PID {}",
        HEARTBEAT_TIMEOUT.as_secs(),
        daemon_pid
    );

    let mut last_heartbeat = Instant::now();
    let handles = [parent.raw(), heartbeat.raw(), restore.raw()];
    let restore_reason = 'guardian: loop {
        // RegisterHotKey(NULL, ...) posts WM_HOTKEY to this thread's message
        // queue; pump it on the same thread that registered the hotkey.
        let mut message = Msg {
            hwnd: std::ptr::null_mut(),
            message: 0,
            w_param: 0,
            l_param: 0,
            time: 0,
            pt: Point { x: 0, y: 0 },
            l_private: 0,
        };
        while unsafe { PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
            if message.message == WM_HOTKEY && message.w_param == HOTKEY_ID as usize {
                break 'guardian "emergency hotkey pressed";
            }
        }

        let wait = unsafe {
            WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, POLL_INTERVAL_MS)
        };
        match wait {
            WAIT_OBJECT_0 => break "daemon process exited",
            x if x == WAIT_OBJECT_0 + 1 => last_heartbeat = Instant::now(),
            x if x == WAIT_OBJECT_0 + 2 => break "daemon requested restoration",
            WAIT_TIMEOUT => {}
            WAIT_FAILED => {
                crate::log!(
                    "guardian: WaitForMultipleObjects failed: {}",
                    std::io::Error::last_os_error()
                );
                break "guardian wait failure";
            }
            other => {
                crate::log!("guardian: unexpected wait result 0x{other:08X}");
                break "guardian wait failure";
            }
        }

        if last_heartbeat.elapsed() >= HEARTBEAT_TIMEOUT {
            break "daemon heartbeat timed out";
        }
    };

    crate::log!("guardian: restoration triggered: {restore_reason}");
    if unsafe { SetEvent(emergency.raw()) } == 0 {
        crate::log!(
            "guardian: failed to signal emergency event: {}",
            std::io::Error::last_os_error()
        );
    }
    let _transition = acquire_restore_transition(&transition, &parent);
    crate::log!("guardian: acquired topology-transition mutex; beginning restoration");
    restore_until_verified(&driver, guid, &snapshot)?;
    drop(hotkey);
    crate::log!("guardian: restoration verified; guardian disarmed");
    Ok(())
}

fn acquire_restore_transition<'a>(mutex: &'a WinHandle, parent: &WinHandle) -> TransitionGuard<'a> {
    loop {
        match unsafe { WaitForSingleObject(mutex.raw(), TRANSITION_WAIT_MS) } {
            WAIT_OBJECT_0 => return TransitionGuard { mutex },
            WAIT_ABANDONED_0 => {
                crate::log!(
                    "guardian: daemon abandoned the transition mutex; guardian acquired ownership"
                );
                return TransitionGuard { mutex };
            }
            WAIT_TIMEOUT => {
                if unsafe { WaitForSingleObject(parent.raw(), 0) } == WAIT_OBJECT_0 {
                    thread::sleep(Duration::from_millis(100));
                    continue;
                }
                crate::log!(
                    "guardian: daemon held transition mutex for 5s; terminating it to prevent concurrent display changes"
                );
                if unsafe { TerminateProcess(parent.raw(), GUARDIAN_TERMINATE_EXIT_CODE) } == 0 {
                    crate::log!(
                        "guardian: TerminateProcess failed: {}",
                        std::io::Error::last_os_error()
                    );
                    thread::sleep(Duration::from_secs(1));
                } else {
                    let waited = unsafe { WaitForSingleObject(parent.raw(), TRANSITION_WAIT_MS) };
                    if waited != WAIT_OBJECT_0 {
                        crate::log!(
                            "guardian: daemon termination is not yet confirmed (wait=0x{waited:08X})"
                        );
                        thread::sleep(Duration::from_secs(1));
                    }
                }
            }
            WAIT_FAILED => {
                crate::log!(
                    "guardian: transition mutex wait failed: {}",
                    std::io::Error::last_os_error()
                );
                thread::sleep(Duration::from_secs(1));
            }
            other => {
                crate::log!("guardian: unexpected transition mutex result 0x{other:08X}");
                thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

struct HotkeyGuard(bool);

impl Drop for HotkeyGuard {
    fn drop(&mut self) {
        if self.0 {
            unsafe {
                let _ = UnregisterHotKey(std::ptr::null_mut(), HOTKEY_ID);
            }
            self.0 = false;
        }
    }
}

/// Never disarms on a failed attempt. Retry until the pre-exclusive active
/// topology compares equal to the snapshot captured by this process.
fn restore_until_verified(driver: &Driver, guid: GUID, snapshot: &Config) -> Result<(), String> {
    let mut attempt: u64 = 0;
    loop {
        attempt += 1;
        if guid == GUID::from_u128(0) {
            return match Config::query() {
                Ok(current) if snapshot.same_active_layout(&current) => {
                    crate::log!(
                        "guardian: self-test topology verified unchanged; no display API changes made"
                    );
                    Ok(())
                }
                Ok(current) => Err(format!(
                    "guardian self-test found a topology change ({} active paths); no display changes were made",
                    current.paths.len()
                )),
                Err(error) => Err(format!(
                    "guardian self-test could not verify topology; no display changes were made: {error}"
                )),
            };
        }

        match driver.remove_display(guid) {
            Ok(()) => crate::log!("guardian: removed virtual display on restore attempt {attempt}"),
            Err(error) => crate::log!(
                "guardian: virtual display removal attempt {attempt} returned {error}; checking topology"
            ),
        }

        thread::sleep(Duration::from_millis(350));
        if let Ok(current) = Config::query() {
            if snapshot.same_active_layout(&current) {
                crate::log!(
                    "guardian: original display topology already verified (attempt {attempt})"
                );
                return Ok(());
            }
        }

        let mut candidate = snapshot.clone();
        match candidate.apply(SDC_ALLOW_CHANGES) {
            Ok(()) => crate::log!("guardian: applied saved topology (attempt {attempt})"),
            Err(error) => {
                crate::log!("guardian: topology restore attempt {attempt} failed: {error}")
            }
        }

        thread::sleep(Duration::from_millis(500));
        match Config::query() {
            Ok(current) if snapshot.same_active_layout(&current) => {
                crate::log!(
                    "guardian: original display topology restored and verified (attempt {attempt})"
                );
                return Ok(());
            }
            Ok(current) => crate::log!(
                "guardian: topology not yet restored after attempt {attempt}; {} active path(s)",
                current.paths.len()
            ),
            Err(error) => {
                crate::log!("guardian: cannot verify topology after attempt {attempt}: {error}")
            }
        }
        thread::sleep(Duration::from_secs(1));
    }
}
