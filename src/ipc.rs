//! Named kernel objects shared between `start`, `stop` and the daemon.
//!
//! * `Alive` mutex: owned by the running daemon; waiting on it returns once the daemon exits.
//! * `Stop` event: signalled by `stop` to ask the daemon to tear down.
//!
//! Objects get a permissive DACL with a low integrity label, so elevated and
//! non-elevated invocations (Sunshine's per-command "elevated" flag) can talk to each other.

use std::ffi::c_void;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HLOCAL, LocalFree, WAIT_ABANDONED,
    WAIT_OBJECT_0,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, OpenEventW, OpenMutexW, ReleaseMutex,
    ResetEvent, SYNCHRONIZATION_SYNCHRONIZE, SetEvent, WaitForSingleObject,
};
use windows::core::{PCWSTR, w};

const ALIVE_NAME: PCWSTR = w!("Local\\SunshineVDisplay.Alive");
const STOP_NAME: PCWSTR = w!("Local\\SunshineVDisplay.Stop");
const GUARDIAN_EXCLUSIVE_STARTED_NAME: PCWSTR =
    w!("Local\\SunshineVDisplay.Guardian.ExclusiveStarted");
const GUARDIAN_EXCLUSIVE_CONFIRM_NAME: PCWSTR =
    w!("Local\\SunshineVDisplay.Guardian.ExclusiveConfirm");
const GUARDIAN_EXCLUSIVE_ACK_NAME: PCWSTR =
    w!("Local\\SunshineVDisplay.Guardian.ExclusiveConfirmAck");

pub struct Handle(pub HANDLE);

impl Handle {
    pub fn raw(&self) -> *mut c_void {
        self.0.0
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

struct Sa {
    sd: PSECURITY_DESCRIPTOR,
    attrs: SECURITY_ATTRIBUTES,
}

impl Sa {
    fn everyone() -> Option<Self> {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                w!("D:(A;;GA;;;WD)(A;;GA;;;SY)S:(ML;;NW;;;LW)"),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )
            .ok()?;
        }
        Some(Sa {
            sd,
            attrs: SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: sd.0,
                bInheritHandle: false.into(),
            },
        })
    }
}

impl Drop for Sa {
    fn drop(&mut self) {
        unsafe {
            LocalFree(Some(HLOCAL(self.sd.0)));
        }
    }
}

/// Daemon side: claims the alive mutex and creates a fresh stop event.
pub fn daemon_objects() -> Result<(Handle, Handle), String> {
    let sa = Sa::everyone();
    let attrs = sa.as_ref().map(|s| &s.attrs as *const SECURITY_ATTRIBUTES);
    unsafe {
        let alive =
            CreateMutexW(attrs, true, ALIVE_NAME).map_err(|e| format!("CreateMutex: {e}"))?;
        let alive = Handle(alive);
        if GetLastError() == ERROR_ALREADY_EXISTS {
            // Another daemon still holds it: wait for it to release (it may be tearing down).
            let r = WaitForSingleObject(alive.0, 10_000);
            if r != WAIT_OBJECT_0 && r != WAIT_ABANDONED {
                return Err("another vdisplay daemon is already running".into());
            }
        }
        let stop = Handle(
            CreateEventW(attrs, true, false, STOP_NAME).map_err(|e| format!("CreateEvent: {e}"))?,
        );
        let _ = ResetEvent(stop.0);
        Ok((alive, stop))
    }
}

/// Returns true if a daemon is currently running.
pub fn daemon_running() -> bool {
    unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, ALIVE_NAME) }
        .map(Handle)
        .is_ok()
}

fn create_unique_named_event(
    name: PCWSTR,
    manual_reset: bool,
    label: &str,
) -> Result<Handle, String> {
    let security =
        Sa::everyone().ok_or_else(|| format!("cannot create security descriptor for {label}"))?;
    let attrs = Some(&security.attrs as *const SECURITY_ATTRIBUTES);
    unsafe {
        let handle = CreateEventW(attrs, manual_reset, false, name)
            .map_err(|e| format!("CreateEventW({label}): {e}"))?;
        let handle = Handle(handle);
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return Err(format!(
                "{label} already exists; another guardian may still be active"
            ));
        }
        Ok(handle)
    }
}

/// Creates the one-shot event the daemon signals after make_exclusive succeeds.
/// A duplicate name is rejected so an orphaned guardian cannot receive a new session's signal.
pub fn create_guardian_exclusive_started_event() -> Result<Handle, String> {
    create_unique_named_event(
        GUARDIAN_EXCLUSIVE_STARTED_NAME,
        false,
        "guardian exclusive-start event",
    )
}

/// Creates the confirmation event and acknowledgement event when the 30-second
/// safety window actually begins. The acknowledgement prevents a late command
/// from claiming success after the guardian has already started restoring displays.
pub fn create_guardian_exclusive_confirmation_events() -> Result<(Handle, Handle), String> {
    let acknowledgement = create_unique_named_event(
        GUARDIAN_EXCLUSIVE_ACK_NAME,
        true,
        "guardian exclusive-confirmation acknowledgement event",
    )?;
    let confirmation = create_unique_named_event(
        GUARDIAN_EXCLUSIVE_CONFIRM_NAME,
        false,
        "guardian exclusive-confirmation event",
    )?;
    Ok((confirmation, acknowledgement))
}

/// Starts the guardian's confirmation countdown. Called only after the exclusive
/// topology transition has succeeded and all requested display configuration is complete.
pub fn signal_guardian_exclusive_started() -> Result<(), String> {
    unsafe {
        let event = OpenEventW(EVENT_MODIFY_STATE, false, GUARDIAN_EXCLUSIVE_STARTED_NAME)
            .map_err(|e| format!("cannot open guardian exclusive-start event: {e}"))?;
        let event = Handle(event);
        SetEvent(event.0).map_err(|e| format!("cannot start guardian safety window: {e}"))
    }
}

/// Confirms the active exclusive test window and waits for explicit guardian acknowledgement.
pub fn confirm_exclusive() -> Result<(), String> {
    if !daemon_running() {
        return Err(
            "no vdisplay daemon is running; there is no exclusive session to confirm".into(),
        );
    }
    signal_and_wait_exclusive_confirmation()
}

pub(crate) fn confirm_exclusive_self_test() -> Result<(), String> {
    signal_and_wait_exclusive_confirmation()
}

fn signal_and_wait_exclusive_confirmation() -> Result<(), String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
    let (confirmation, acknowledgement) = loop {
        let confirmation_result =
            unsafe { OpenEventW(EVENT_MODIFY_STATE, false, GUARDIAN_EXCLUSIVE_CONFIRM_NAME) };
        match confirmation_result {
            Ok(confirmation) => {
                let acknowledgement_result = unsafe {
                    OpenEventW(
                        SYNCHRONIZATION_SYNCHRONIZE,
                        false,
                        GUARDIAN_EXCLUSIVE_ACK_NAME,
                    )
                };
                match acknowledgement_result {
                    Ok(acknowledgement) => {
                        break (Handle(confirmation), Handle(acknowledgement));
                    }
                    Err(error) if std::time::Instant::now() < deadline => {
                        let _ = error;
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    Err(error) => {
                        return Err(format!(
                            "guardian confirmation acknowledgement is unavailable; the safety window may have expired: {error}"
                        ));
                    }
                }
            }
            Err(error) if std::time::Instant::now() < deadline => {
                let _ = error;
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(error) => {
                return Err(format!(
                    "no exclusive confirmation window is active; run this command within 30 seconds of exclusive startup: {error}"
                ));
            }
        }
    };

    unsafe {
        SetEvent(confirmation.0)
            .map_err(|e| format!("cannot signal exclusive confirmation: {e}"))?;
        match WaitForSingleObject(acknowledgement.0, 5_000) {
            WAIT_OBJECT_0 => Ok(()),
            _ => Err(
                "guardian did not acknowledge confirmation within 5 seconds; the safety deadline may have expired and display restoration may be underway".into(),
            ),
        }
    }
}
/// Signals the daemon to stop and waits up to `timeout_ms` for it to exit.
/// Returns Ok(false) if no daemon was running.
pub fn stop_daemon(timeout_ms: u32) -> Result<bool, String> {
    unsafe {
        let Ok(alive) = OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, ALIVE_NAME) else {
            return Ok(false);
        };
        let alive = Handle(alive);
        let stop = OpenEventW(EVENT_MODIFY_STATE, false, STOP_NAME)
            .map_err(|e| format!("OpenEvent: {e}"))?;
        let stop = Handle(stop);
        SetEvent(stop.0).map_err(|e| format!("SetEvent: {e}"))?;
        match WaitForSingleObject(alive.0, timeout_ms) {
            WAIT_OBJECT_0 | WAIT_ABANDONED => {
                let _ = ReleaseMutex(alive.0);
                Ok(true)
            }
            _ => Err("timed out waiting for the vdisplay daemon to exit".into()),
        }
    }
}
