//! Windows display configuration helpers, ported from Apollo's
//! `src/platform/windows/virtual_display.cpp`.

use std::mem::size_of;
use std::thread::sleep;
use std::time::{Duration, Instant};

use windows::Win32::Devices::Display::{
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_HEADER,
    DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE, DISPLAYCONFIG_MODE_INFO,
    DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_RATIONAL,
    DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
    DisplayConfigGetDeviceInfo, DisplayConfigSetDeviceInfo, GetDisplayConfigBufferSizes,
    QDC_ALL_PATHS, QDC_ONLY_ACTIVE_PATHS, QDC_VIRTUAL_MODE_AWARE, QUERY_DISPLAY_CONFIG_FLAGS,
    QueryDisplayConfig, SDC_ALLOW_CHANGES, SDC_APPLY, SDC_USE_SUPPLIED_DISPLAY_CONFIG,
    SDC_VIRTUAL_MODE_AWARE, SET_DISPLAY_CONFIG_FLAGS, SetDisplayConfig,
};
use windows::Win32::Foundation::{ERROR_SUCCESS, LUID};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
use windows::Win32::Graphics::Gdi::{
    CDS_TYPE, ChangeDisplaySettingsExW, DEVMODEW, DISP_CHANGE_SUCCESSFUL,
    DISPLAYCONFIG_PATH_ACTIVE, DISPLAYCONFIG_PATH_MODE_IDX_INVALID, DM_DISPLAYFREQUENCY,
    DM_PELSHEIGHT, DM_PELSWIDTH, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW,
};
use windows::core::PCWSTR;

use crate::log;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn from_wide(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

fn same_luid(a: LUID, b: LUID) -> bool {
    a.LowPart == b.LowPart && a.HighPart == b.HighPart
}

/// A snapshot of the active display topology.
#[derive(Clone)]
pub struct Config {
    pub paths: Vec<DISPLAYCONFIG_PATH_INFO>,
    pub modes: Vec<DISPLAYCONFIG_MODE_INFO>,
}

impl Config {
    pub fn query() -> Result<Self, String> {
        Self::query_with_flags(QDC_ONLY_ACTIVE_PATHS)
    }

    fn query_with_flags(flags: QUERY_DISPLAY_CONFIG_FLAGS) -> Result<Self, String> {
        // Interpret DISPLAYCONFIG_PATH_INFO unions using the virtual-mode-aware layout.
        let flags = flags | QDC_VIRTUAL_MODE_AWARE;
        // The topology can change between the two calls. Keep retrying and preserve
        // the actual Win32 status instead of collapsing every failure into one message.
        let mut last_query_error = None;
        for _ in 0..5 {
            let (mut path_count, mut mode_count) = (0u32, 0u32);
            let r = unsafe { GetDisplayConfigBufferSizes(flags, &mut path_count, &mut mode_count) };
            if r != ERROR_SUCCESS {
                return Err(format!(
                    "GetDisplayConfigBufferSizes failed: {} (0x{:08X})",
                    r.0, r.0
                ));
            }

            let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
            let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
            let r = unsafe {
                QueryDisplayConfig(
                    flags,
                    &mut path_count,
                    paths.as_mut_ptr(),
                    &mut mode_count,
                    modes.as_mut_ptr(),
                    None,
                )
            };
            if r == ERROR_SUCCESS {
                paths.truncate(path_count as usize);
                modes.truncate(mode_count as usize);
                return Ok(Config { paths, modes });
            }

            last_query_error = Some(r.0);
            sleep(Duration::from_millis(50));
        }

        let code = last_query_error.unwrap_or_default();
        Err(format!(
            "QueryDisplayConfig failed after 5 attempts: {} (0x{:08X})",
            code, code
        ))
    }

    pub fn apply(&mut self, extra: SET_DISPLAY_CONFIG_FLAGS) -> Result<(), String> {
        let r = unsafe {
            SetDisplayConfig(
                Some(&self.paths),
                Some(&self.modes),
                SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_VIRTUAL_MODE_AWARE | extra,
            )
        };
        if r == ERROR_SUCCESS.0 as i32 {
            Ok(())
        } else {
            Err(format!("SetDisplayConfig failed: {r} (0x{:08X})", r as u32))
        }
    }

    fn source_mode_index(&self, path: &DISPLAYCONFIG_PATH_INFO) -> Option<usize> {
        let src = &path.sourceInfo;
        self.modes.iter().position(|m| {
            m.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE
                && same_luid(m.adapterId, src.adapterId)
                && m.id == src.id
        })
    }

    fn path_index_by_target(&self, adapter: LUID, target_id: u32) -> Option<usize> {
        self.paths.iter().position(|p| {
            same_luid(p.targetInfo.adapterId, adapter) && p.targetInfo.id == target_id
        })
    }

    /// Checks that the same active source/target paths and desktop rectangles are back.
    pub fn same_active_layout(&self, other: &Self) -> bool {
        if self.paths.len() != other.paths.len() {
            return false;
        }

        self.paths.iter().all(|path| {
            let Some(other_path) = other.paths.iter().find(|candidate| {
                same_luid(candidate.sourceInfo.adapterId, path.sourceInfo.adapterId)
                    && candidate.sourceInfo.id == path.sourceInfo.id
                    && same_luid(candidate.targetInfo.adapterId, path.targetInfo.adapterId)
                    && candidate.targetInfo.id == path.targetInfo.id
            }) else {
                return false;
            };

            if path.targetInfo.refreshRate.Numerator != other_path.targetInfo.refreshRate.Numerator
                || path.targetInfo.refreshRate.Denominator
                    != other_path.targetInfo.refreshRate.Denominator
            {
                return false;
            }

            match (
                self.source_mode_index(path),
                other.source_mode_index(other_path),
            ) {
                (Some(left), Some(right)) => unsafe {
                    let a = &self.modes[left].Anonymous.sourceMode;
                    let b = &other.modes[right].Anonymous.sourceMode;
                    a.width == b.width
                        && a.height == b.height
                        && a.position.x == b.position.x
                        && a.position.y == b.position.y
                },
                (None, None) => true,
                _ => false,
            }
        })
    }
}

fn source_gdi_name(path: &DISPLAYCONFIG_PATH_INFO) -> Result<String, String> {
    let mut name = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
            size: size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
            adapterId: path.sourceInfo.adapterId,
            id: path.sourceInfo.id,
        },
        ..Default::default()
    };
    let r = unsafe { DisplayConfigGetDeviceInfo(&mut name.header) };
    if r == 0 {
        Ok(from_wide(&name.viewGdiDeviceName))
    } else {
        Err(format!(
            "DisplayConfigGetDeviceInfo(GET_SOURCE_NAME) failed for source {}:{}: {} (0x{:08X})",
            name.header.adapterId.HighPart as u32, name.header.adapterId.LowPart, r, r as u32
        ))
    }
}

fn summarize_path(path: &DISPLAYCONFIG_PATH_INFO) -> String {
    format!(
        "source={:08X}:{:08X}/{} target={:08X}:{:08X}/{} active={}",
        path.sourceInfo.adapterId.HighPart as u32,
        path.sourceInfo.adapterId.LowPart,
        path.sourceInfo.id,
        path.targetInfo.adapterId.HighPart as u32,
        path.targetInfo.adapterId.LowPart,
        path.targetInfo.id,
        path.flags & DISPLAYCONFIG_PATH_ACTIVE != 0
    )
}

fn summarize_paths(paths: &[DISPLAYCONFIG_PATH_INFO]) -> String {
    if paths.is_empty() {
        return "<none>".into();
    }
    paths
        .iter()
        .map(summarize_path)
        .collect::<Vec<_>>()
        .join("; ")
}

/// Waits for the target to become active and separately diagnoses whether Windows
/// knows it as an inactive path. QDC_ALL_PATHS is only queried after a timeout because
/// Microsoft documents it as an expensive query.
pub fn wait_for_display_name(
    adapter: LUID,
    target_id: u32,
    timeout: Duration,
) -> Result<String, String> {
    let deadline = Instant::now() + timeout;
    let mut last_query_error = None;
    let mut last_source_name_error = None;

    loop {
        match Config::query() {
            Ok(cfg) => {
                if let Some(i) = cfg.path_index_by_target(adapter, target_id) {
                    match source_gdi_name(&cfg.paths[i]) {
                        Ok(name) => return Ok(name),
                        Err(error) => last_source_name_error = Some(error),
                    }
                }
            }
            Err(error) => last_query_error = Some(error),
        }

        if Instant::now() >= deadline {
            break;
        }
        sleep(Duration::from_millis(100));
    }

    let active_details = match Config::query() {
        Ok(cfg) => format!(
            "{} active path(s): {}",
            cfg.paths.len(),
            summarize_paths(&cfg.paths)
        ),
        Err(error) => format!("active-path query failed: {error}"),
    };
    let all_path_details = match Config::query_with_flags(QDC_ALL_PATHS) {
        Ok(cfg) => match cfg.path_index_by_target(adapter, target_id) {
            Some(i) => {
                let path = &cfg.paths[i];
                if path.flags & DISPLAYCONFIG_PATH_ACTIVE != 0 {
                    format!(
                        "target is present in QDC_ALL_PATHS and flagged active: {}",
                        summarize_path(path)
                    )
                } else {
                    format!(
                        "target is known to Windows but inactive: {}",
                        summarize_path(path)
                    )
                }
            }
            None => "target is absent from QDC_ALL_PATHS".into(),
        },
        Err(error) => format!("QDC_ALL_PATHS diagnostic failed: {error}"),
    };

    Err(format!(
        "virtual display target {target_id} (adapter {:08X}:{:08X}) was not discoverable as an active display within {timeout:?}; {all_path_details}; {active_details}; last_query_error={}; last_source_name_error={}",
        adapter.HighPart as u32,
        adapter.LowPart,
        last_query_error.as_deref().unwrap_or("<none>"),
        last_source_name_error.as_deref().unwrap_or("<none>")
    ))
}

/// Waits until the given target is no longer an active path.
pub fn wait_for_display_gone(adapter: LUID, target_id: u32, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match Config::query() {
            Ok(cfg) if cfg.path_index_by_target(adapter, target_id).is_none() => return,
            _ => sleep(Duration::from_millis(100)),
        }
    }
}

/// Port of `VDISPLAY::changeDisplaySettings`: apply the integer refresh rate via the
/// legacy API first, then the exact (fractional) rate via `SetDisplayConfig`.
pub fn set_mode(device: &str, width: u32, height: u32, refresh_mhz: u32) -> Result<(), String> {
    let wname = wide(device);
    let mut dm = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    if unsafe { EnumDisplaySettingsW(PCWSTR(wname.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm) }
        .as_bool()
    {
        let mut target = refresh_mhz / 1000;
        let mut alt = target;
        let frac = refresh_mhz % 1000;
        if frac == 0 {
            alt = alt.saturating_sub(1);
        } else if frac >= 900 {
            target += 1;
        } else {
            alt += 1;
        }

        dm.dmPelsWidth = width;
        dm.dmPelsHeight = height;
        dm.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY;
        for hz in [target, alt] {
            dm.dmDisplayFrequency = hz;
            let r = unsafe {
                ChangeDisplaySettingsExW(PCWSTR(wname.as_ptr()), Some(&dm), None, CDS_TYPE(0), None)
            };
            if r == DISP_CHANGE_SUCCESSFUL {
                log!("baseline mode {width}x{height}@{hz} applied to {device}");
                break;
            }
            log!("baseline mode {width}x{height}@{hz} failed on {device}: {r:?}");
        }
    }

    let mut cfg = Config::query()?;
    let idx = cfg
        .paths
        .iter()
        .position(|p| source_gdi_name(p).ok().as_deref() == Some(device))
        .ok_or_else(|| format!("display {device} not found"))?;
    let mode_idx = cfg
        .source_mode_index(&cfg.paths[idx])
        .ok_or_else(|| format!("source mode for {device} not found"))?;
    unsafe {
        let src = &mut cfg.modes[mode_idx].Anonymous.sourceMode;
        src.width = width;
        src.height = height;
    }
    cfg.paths[idx].targetInfo.refreshRate = DISPLAYCONFIG_RATIONAL {
        Numerator: refresh_mhz,
        Denominator: 1000,
    };
    cfg.apply(SET_DISPLAY_CONFIG_FLAGS(0))?;
    log!(
        "exact mode {width}x{height}@{:.3} applied to {device}",
        refresh_mhz as f64 / 1000.0
    );
    Ok(())
}

/// Makes the virtual display primary by shifting the desktop so its source sits at (0,0).
/// (Apollo's legacy `ChangeDisplaySettingsEx(CDS_SET_PRIMARY)` approach fails on IddCx displays here.)
pub fn set_primary(adapter: LUID, target_id: u32) -> Result<(), String> {
    let mut cfg = Config::query()?;
    let path = cfg
        .path_index_by_target(adapter, target_id)
        .ok_or("virtual display path not found")?;
    let mode = cfg
        .source_mode_index(&cfg.paths[path])
        .ok_or("virtual display source mode not found")?;
    let origin = unsafe { cfg.modes[mode].Anonymous.sourceMode.position };
    for m in cfg
        .modes
        .iter_mut()
        .filter(|m| m.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE)
    {
        unsafe {
            let pos = &mut m.Anonymous.sourceMode.position;
            pos.x -= origin.x;
            pos.y -= origin.y;
        }
    }
    cfg.apply(SDC_ALLOW_CHANGES)
}

/// Deactivates every display except the virtual one, then verifies the resulting topology.
pub fn make_exclusive(adapter: LUID, target_id: u32) -> Result<(), String> {
    let mut cfg = Config::query()?;
    let keep = cfg
        .path_index_by_target(adapter, target_id)
        .ok_or("virtual display path not found in the active topology")?;
    if let Some(m) = cfg.source_mode_index(&cfg.paths[keep]) {
        unsafe {
            let pos = &mut cfg.modes[m].Anonymous.sourceMode.position;
            pos.x = 0;
            pos.y = 0;
        }
    }
    for (i, path) in cfg.paths.iter_mut().enumerate() {
        if i == keep {
            continue;
        }
        path.flags &= !DISPLAYCONFIG_PATH_ACTIVE;
        path.sourceInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
        path.targetInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
    }

    cfg.apply(SDC_ALLOW_CHANGES)?;
    log!("exclusive: SetDisplayConfig accepted; verifying active display topology");

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut last_state = "no verification result".to_string();
    while Instant::now() < deadline {
        match Config::query() {
            Ok(current) => {
                let target_active = current.path_index_by_target(adapter, target_id).is_some();
                if target_active && current.paths.len() == 1 {
                    log!("exclusive: verified exactly one active display (target {target_id})");
                    return Ok(());
                }
                last_state = format!(
                    "{} active path(s), target_active={target_active}: {}",
                    current.paths.len(),
                    summarize_paths(&current.paths)
                );
            }
            Err(error) => last_state = error,
        }
        sleep(Duration::from_millis(100));
    }

    Err(format!(
        "exclusive layout was not verified within 3 seconds; expected only target {target_id}; last state: {last_state}"
    ))
}

pub fn set_hdr(adapter: LUID, target_id: u32, enable: bool) -> Result<(), String> {
    let mut state = DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE,
            size: size_of::<DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE>() as u32,
            adapterId: adapter,
            id: target_id,
        },
        ..Default::default()
    };
    state.Anonymous.value = enable as u32;
    let r = unsafe { DisplayConfigSetDeviceInfo(&state.header) };
    if r == 0 {
        Ok(())
    } else {
        Err(format!("DisplayConfigSetDeviceInfo failed: {r}"))
    }
}

/// Finds the LUID of a GPU by its DXGI description (e.g. "NVIDIA GeForce RTX 4090").
pub fn find_adapter_luid(name: &str) -> Result<LUID, String> {
    let factory: IDXGIFactory1 =
        unsafe { CreateDXGIFactory1() }.map_err(|e| format!("CreateDXGIFactory1: {e}"))?;
    let mut found = Vec::new();
    let mut i = 0;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(i) } {
        i += 1;
        if let Ok(desc) = unsafe { adapter.GetDesc1() } {
            let desc_name = from_wide(&desc.Description);
            if desc_name.eq_ignore_ascii_case(name) {
                return Ok(desc.AdapterLuid);
            }
            found.push(desc_name);
        }
    }
    Err(format!(
        "adapter \"{name}\" not found, available: {found:?}"
    ))
}
