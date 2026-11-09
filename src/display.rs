//! Windows display configuration helpers, ported from Apollo's
//! `src/platform/windows/virtual_display.cpp`.

use std::mem::size_of;
use std::thread::sleep;
use std::time::{Duration, Instant};

use windows::Win32::Devices::Display::{
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_HEADER,
    DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE, DISPLAYCONFIG_MODE_INFO,
    DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_RATIONAL,
    DISPLAYCONFIG_SCANLINE_ORDERING_UNSPECIFIED, DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE,
    DISPLAYCONFIG_SOURCE_DEVICE_NAME, DisplayConfigGetDeviceInfo, DisplayConfigSetDeviceInfo,
    GetDisplayConfigBufferSizes, QDC_ALL_PATHS, QDC_ONLY_ACTIVE_PATHS, QDC_VIRTUAL_MODE_AWARE,
    QUERY_DISPLAY_CONFIG_FLAGS, QueryDisplayConfig, SDC_ALLOW_CHANGES,
    SDC_ALLOW_PATH_ORDER_CHANGES, SDC_APPLY, SDC_SAVE_TO_DATABASE, SDC_TOPOLOGY_SUPPLIED,
    SDC_USE_SUPPLIED_DISPLAY_CONFIG, SDC_VIRTUAL_MODE_AWARE, SET_DISPLAY_CONFIG_FLAGS,
    SetDisplayConfig,
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
    /// Logs every mismatch so restoration failures can be diagnosed without changing behavior.
    pub fn same_active_layout(&self, other: &Self) -> bool {
        let mut matches = true;

        if self.paths.len() != other.paths.len() {
            log!(
                "topology compare: active path count differs; snapshot={}, current={}",
                self.paths.len(),
                other.paths.len()
            );
            matches = false;
        }

        for path in &self.paths {
            let Some(other_path) = other.paths.iter().find(|candidate| {
                same_luid(candidate.sourceInfo.adapterId, path.sourceInfo.adapterId)
                    && candidate.sourceInfo.id == path.sourceInfo.id
                    && same_luid(candidate.targetInfo.adapterId, path.targetInfo.adapterId)
                    && candidate.targetInfo.id == path.targetInfo.id
            }) else {
                log!(
                    "topology compare: snapshot path missing from current topology: {}",
                    summarize_path(path)
                );
                matches = false;
                continue;
            };

            let left_refresh = (
                path.targetInfo.refreshRate.Numerator,
                path.targetInfo.refreshRate.Denominator,
            );
            let right_refresh = (
                other_path.targetInfo.refreshRate.Numerator,
                other_path.targetInfo.refreshRate.Denominator,
            );
            if left_refresh != right_refresh {
                log!(
                    "topology compare: refresh differs for {}; snapshot={}/{}, current={}/{}",
                    summarize_path(path),
                    left_refresh.0,
                    left_refresh.1,
                    right_refresh.0,
                    right_refresh.1
                );
                matches = false;
            }

            match (
                self.source_mode_index(path),
                other.source_mode_index(other_path),
            ) {
                (Some(left), Some(right)) => unsafe {
                    let a = &self.modes[left].Anonymous.sourceMode;
                    let b = &other.modes[right].Anonymous.sourceMode;
                    if a.width != b.width
                        || a.height != b.height
                        || a.position.x != b.position.x
                        || a.position.y != b.position.y
                    {
                        log!(
                            "topology compare: source mode differs for {}; snapshot={}x{} at ({}, {}), current={}x{} at ({}, {})",
                            summarize_path(path),
                            a.width,
                            a.height,
                            a.position.x,
                            a.position.y,
                            b.width,
                            b.height,
                            b.position.x,
                            b.position.y
                        );
                        matches = false;
                    }
                },
                (None, None) => {}
                (left, right) => {
                    log!(
                        "topology compare: source mode availability differs for {}; snapshot_index_present={}, current_index_present={}",
                        summarize_path(path),
                        left.is_some(),
                        right.is_some()
                    );
                    matches = false;
                }
            }
        }

        for path in &other.paths {
            let exists_in_snapshot = self.paths.iter().any(|candidate| {
                same_luid(candidate.sourceInfo.adapterId, path.sourceInfo.adapterId)
                    && candidate.sourceInfo.id == path.sourceInfo.id
                    && same_luid(candidate.targetInfo.adapterId, path.targetInfo.adapterId)
                    && candidate.targetInfo.id == path.targetInfo.id
            });
            if !exists_in_snapshot {
                log!(
                    "topology compare: current path absent from snapshot: {}",
                    summarize_path(path)
                );
                matches = false;
            }
        }

        matches
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

/// Activates a connected but inactive virtual target using the current active
/// paths as supplied data, so existing physical-display modes are preserved.
fn allocate_source_id(counters: &mut Vec<(LUID, u32)>, adapter: LUID) -> Result<u32, String> {
    let next = if let Some(index) = counters
        .iter()
        .position(|(luid, _)| same_luid(*luid, adapter))
    {
        let value = counters[index].1;
        counters[index].1 = value.checked_add(1).ok_or("source ID space exhausted")?;
        value
    } else {
        counters.push((adapter, 1));
        0
    };
    Ok(next)
}

fn allocate_clone_group(next: &mut u32) -> Result<u16, String> {
    let value = *next;
    if value >= u16::MAX as u32 {
        return Err("clone-group ID space exhausted".into());
    }
    *next = value
        .checked_add(1)
        .ok_or("clone-group ID space exhausted")?;
    Ok(value as u16)
}

fn available_path_indices(config: &Config) -> Vec<usize> {
    let mut indices = Vec::<usize>::new();
    for (index, path) in config.paths.iter().enumerate() {
        if !path.targetInfo.targetAvailable.as_bool() {
            continue;
        }
        if !indices.iter().any(|known_index| {
            let known = &config.paths[*known_index];
            same_luid(known.targetInfo.adapterId, path.targetInfo.adapterId)
                && known.targetInfo.id == path.targetInfo.id
        }) {
            indices.push(index);
        }
    }
    indices
}

fn set_source_clone_group(path: &mut DISPLAYCONFIG_PATH_INFO, clone_group: u16) {
    if (path.flags & 0x0000_0008) != 0 {
        // Virtual-mode-aware union: sourceModeInfoIdx in the high word,
        // cloneGroupId in the low word.
        path.sourceInfo.Anonymous.modeInfoIdx = ((u16::MAX as u32) << 16) | u32::from(clone_group);
    } else {
        path.sourceInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
    }
}

pub fn deactivate_target_for_test(adapter: LUID, target_id: u32) -> Result<(), String> {
    let mut config = Config::query()?;
    let index = config
        .path_index_by_target(adapter, target_id)
        .ok_or_else(|| format!("temporary target {target_id} is not active before deactivation"))?;

    config.paths[index].flags &= !DISPLAYCONFIG_PATH_ACTIVE;
    config.paths[index].sourceInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
    config.paths[index].targetInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
    config.apply(SDC_ALLOW_CHANGES)?;

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if Config::query()?
            .path_index_by_target(adapter, target_id)
            .is_none()
        {
            log!("activation test: temporary target {target_id} is inactive");
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "activation test could not deactivate temporary target {target_id} within 5s"
            ));
        }
        sleep(Duration::from_millis(100));
    }
}
pub fn activate_target(adapter: LUID, target_id: u32) -> Result<(), String> {
    if Config::query()?
        .path_index_by_target(adapter, target_id)
        .is_some()
    {
        return Ok(());
    }

    let config = Config::query_with_flags(QDC_ALL_PATHS)?;
    let target_index = config
        .path_index_by_target(adapter, target_id)
        .ok_or_else(|| format!("target {target_id} is absent from QDC_ALL_PATHS"))?;
    if config.paths[target_index].flags & DISPLAYCONFIG_PATH_ACTIVE != 0 {
        return Ok(());
    }
    if !config.paths[target_index]
        .targetInfo
        .targetAvailable
        .as_bool()
    {
        return Err(format!(
            "target {target_id} is present in QDC_ALL_PATHS but Windows reports targetAvailable=false"
        ));
    }

    let available_indices = available_path_indices(&config);
    if !available_indices.contains(&target_index) {
        return Err(format!(
            "target {target_id} is missing from the available QDC_ALL_PATHS paths"
        ));
    }

    // Keep the complete QDC_ALL_PATHS array. Only available path indexes are
    // reconstructed below; SetDisplayConfig receives the original full array.
    let mut paths = config.paths.clone();

    // Reconstruct active source clone groups from their existing desktop positions,
    // then add the inactive VDisplay as a separate active source. This follows the
    // topology-supplied activation strategy used by the DisplayConfig utility.
    let mut source_ids = Vec::<(LUID, u32)>::new();
    let mut next_clone_group = 0u32;
    let mut clone_groups = Vec::<(i32, i32, u16)>::new();

    for (path_index, path) in paths.iter_mut().enumerate() {
        if !available_indices.contains(&path_index) {
            if path.flags & DISPLAYCONFIG_PATH_ACTIVE != 0 {
                return Err(format!(
                    "QDC_ALL_PATHS path {path_index} is active but targetAvailable=false: {}",
                    summarize_path(path)
                ));
            }
            set_source_clone_group(path, u16::MAX);
            path.targetInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
            continue;
        }

        // QueryDisplayConfig output-only status fields must not be replayed as input.
        let is_target =
            same_luid(path.targetInfo.adapterId, adapter) && path.targetInfo.id == target_id;
        let was_active = path.flags & DISPLAYCONFIG_PATH_ACTIVE != 0;
        path.targetInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;

        // Windows requires UNSPECIFIED scan-line ordering whenever refreshRate is 0/0.
        // The inactive VDD target reports no preferred refresh rate in QDC_ALL_PATHS.
        if path.targetInfo.refreshRate.Numerator == 0
            && path.targetInfo.refreshRate.Denominator == 0
        {
            log!(
                "activation: target {} has refreshRate 0/0; scanLineOrdering {:?} -> UNSPECIFIED",
                path.targetInfo.id,
                path.targetInfo.scanLineOrdering
            );
            path.targetInfo.scanLineOrdering = DISPLAYCONFIG_SCANLINE_ORDERING_UNSPECIFIED;
        }

        if !was_active && !is_target {
            // SDC_TOPOLOGY_SUPPLIED requires source mode information to be absent.
            // For virtual-mode-aware paths, the clone group must also be invalid.
            set_source_clone_group(path, u16::MAX);
            continue;
        }

        let source_adapter = path.sourceInfo.adapterId;
        let clone_group = if was_active {
            let original_path = config
                .paths
                .iter()
                .find(|original| {
                    same_luid(original.targetInfo.adapterId, path.targetInfo.adapterId)
                        && original.targetInfo.id == path.targetInfo.id
                })
                .ok_or_else(|| {
                    "an active display path disappeared during activation setup".to_string()
                })?;
            let mode_index = config.source_mode_index(original_path).ok_or_else(|| {
                format!(
                    "source mode is missing for active target {}",
                    path.targetInfo.id
                )
            })?;
            let position = unsafe { config.modes[mode_index].Anonymous.sourceMode.position };

            if let Some((_, _, group)) = clone_groups
                .iter()
                .find(|(x, y, _)| *x == position.x && *y == position.y)
            {
                *group
            } else {
                let group = allocate_clone_group(&mut next_clone_group)?;
                clone_groups.push((position.x, position.y, group));
                group
            }
        } else {
            allocate_clone_group(&mut next_clone_group)?
        };

        set_source_clone_group(path, clone_group);
        path.sourceInfo.id = allocate_source_id(&mut source_ids, source_adapter)?;
        path.flags |= DISPLAYCONFIG_PATH_ACTIVE;
    }

    // First use SDC_TOPOLOGY_SUPPLIED: only path topology is provided, while
    // Windows resolves source/target modes from its display configuration DB.
    log!(
        "activation payload: paths={}, queried_modes={}, mode_array=NULL; flags=SDC_TOPOLOGY_SUPPLIED|SDC_APPLY|SDC_ALLOW_PATH_ORDER_CHANGES|SDC_VIRTUAL_MODE_AWARE",
        paths.len(),
        config.modes.len()
    );
    for (index, path) in paths.iter().enumerate() {
        let source_raw = unsafe { path.sourceInfo.Anonymous.modeInfoIdx };
        let target_raw = unsafe { path.targetInfo.Anonymous.modeInfoIdx };
        log!(
            "activation payload path[{index}]: flags=0x{:08X} virtual={} source={:08X}:{:08X}/{} source_union=0x{:08X} sourceModeInfoIdx=0x{:04X} cloneGroupId=0x{:04X} source_status=0x{:08X} target={:08X}:{:08X}/{} target_union=0x{:08X} desktopModeInfoIdx=0x{:04X} targetModeInfoIdx=0x{:04X} available={} target_status=0x{:08X} refresh={}/{} outputTechnology={:?} rotation={:?} scaling={:?} scanLineOrdering={:?}",
            path.flags,
            (path.flags & 0x0000_0008) != 0,
            path.sourceInfo.adapterId.HighPart as u32,
            path.sourceInfo.adapterId.LowPart,
            path.sourceInfo.id,
            source_raw,
            (source_raw >> 16) as u16,
            source_raw as u16,
            path.sourceInfo.statusFlags,
            path.targetInfo.adapterId.HighPart as u32,
            path.targetInfo.adapterId.LowPart,
            path.targetInfo.id,
            target_raw,
            target_raw as u16,
            (target_raw >> 16) as u16,
            path.targetInfo.targetAvailable.as_bool(),
            path.targetInfo.statusFlags,
            path.targetInfo.refreshRate.Numerator,
            path.targetInfo.refreshRate.Denominator,
            path.targetInfo.outputTechnology,
            path.targetInfo.rotation,
            path.targetInfo.scaling,
            path.targetInfo.scanLineOrdering
        );
    }
    let topology_flags =
        SDC_TOPOLOGY_SUPPLIED | SDC_APPLY | SDC_ALLOW_PATH_ORDER_CHANGES | SDC_VIRTUAL_MODE_AWARE;
    let topology_result = unsafe { SetDisplayConfig(Some(&paths), None, topology_flags) };

    if topology_result != ERROR_SUCCESS.0 as i32 {
        if topology_result != 31 && topology_result != 87 {
            return Err(format!(
                "SetDisplayConfig(SDC_TOPOLOGY_SUPPLIED) failed: {topology_result} (0x{:08X})",
                topology_result as u32
            ));
        }

        // ERROR_GEN_FAILURE means there is no complete persisted topology for
        // this newly-created target. Retry using the current supplied paths/modes,
        // as the DisplayConfig utility does for this exact CCD failure.
        log!(
            "activation: SDC_TOPOLOGY_SUPPLIED returned {topology_result}; retrying with SDC_USE_SUPPLIED_DISPLAY_CONFIG and the queried mode array"
        );
        let mut fallback_paths = config.paths.clone();
        for path in &mut fallback_paths {
            path.targetInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
        }
        let fallback_target_index = fallback_paths
            .iter()
            .position(|path| {
                same_luid(path.targetInfo.adapterId, adapter) && path.targetInfo.id == target_id
            })
            .ok_or_else(|| {
                format!("target {target_id} disappeared before supplied-configuration fallback")
            })?;

        let source_adapter = fallback_paths[fallback_target_index].sourceInfo.adapterId;
        let mut source_id = 0u32;
        while fallback_paths.iter().any(|path| {
            path.flags & DISPLAYCONFIG_PATH_ACTIVE != 0
                && same_luid(path.sourceInfo.adapterId, source_adapter)
                && path.sourceInfo.id == source_id
        }) {
            source_id = source_id
                .checked_add(1)
                .ok_or("source ID space exhausted")?;
        }
        let mut clone_group = 0u16;
        while fallback_paths.iter().any(|path| {
            path.flags & DISPLAYCONFIG_PATH_ACTIVE != 0
                && (path.flags & 0x0000_0008) != 0
                && unsafe { (path.sourceInfo.Anonymous.modeInfoIdx as u16) == clone_group }
        }) {
            clone_group = clone_group
                .checked_add(1)
                .ok_or("clone-group ID space exhausted")?;
        }
        if clone_group == u16::MAX {
            return Err("clone-group ID space exhausted".into());
        }
        let fallback_target = &mut fallback_paths[fallback_target_index];
        fallback_target.sourceInfo.id = source_id;
        set_source_clone_group(fallback_target, clone_group);
        fallback_target.flags |= DISPLAYCONFIG_PATH_ACTIVE;

        let supplied_flags = SDC_USE_SUPPLIED_DISPLAY_CONFIG
            | SDC_APPLY
            | SDC_SAVE_TO_DATABASE
            | SDC_VIRTUAL_MODE_AWARE;
        let supplied_result =
            unsafe { SetDisplayConfig(Some(&fallback_paths), Some(&config.modes), supplied_flags) };
        if supplied_result != ERROR_SUCCESS.0 as i32 {
            return Err(format!(
                "SetDisplayConfig topology activation failed ({topology_result}); supplied-configuration fallback failed: {supplied_result} (0x{:08X})",
                supplied_result as u32
            ));
        }
    }

    if wait_for_target_active(adapter, target_id, Duration::from_secs(5))? {
        log!("activation: virtual display target {target_id} is now active");
        Ok(())
    } else {
        let current = Config::query()?;
        Err(format!(
            "SetDisplayConfig accepted activation, but target {target_id} is still inactive; current active paths: {}",
            summarize_paths(&current.paths)
        ))
    }
}

fn wait_for_target_active(
    adapter: LUID,
    target_id: u32,
    timeout: Duration,
) -> Result<bool, String> {
    let deadline = Instant::now() + timeout;
    loop {
        if Config::query()?
            .path_index_by_target(adapter, target_id)
            .is_some()
        {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        sleep(Duration::from_millis(100));
    }
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
