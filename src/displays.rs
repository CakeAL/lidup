//! Display enumeration and on/off control.
//!
//! Enumeration, identification and geometry use the **public** CoreGraphics API.
//! Actually powering a display off/on uses the **private** SkyLight symbol
//! `CGSConfigureDisplayEnabled` (loaded at runtime via dlopen/dlsym so the app
//! degrades gracefully if the symbol ever moves). This is the same approach used
//! by `displayplacer` and references in the V2EX thread about closing the built-in
//! display when an external one is connected.

use libloading::{Library, Symbol};
use std::os::raw::c_void;
use std::sync::{Mutex, OnceLock};

pub type DisplayID = u32;

type CGError = i32;
type CGDisplayConfigRef = *mut c_void;

type ReconfigCallback = unsafe extern "C" fn(u32, u32, *mut c_void);

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

extern "C" {
    fn CGGetOnlineDisplayList(max_count: u32, ids: *mut u32, count: *mut u32) -> CGError;
    fn CGDisplayIsBuiltin(id: u32) -> i32;
    fn CGDisplayIsActive(id: u32) -> i32;
    fn CGDisplayVendorNumber(id: u32) -> u32;
    fn CGDisplayModelNumber(id: u32) -> u32;
    fn CGDisplaySerialNumber(id: u32) -> u32;
    fn CGDisplayBounds(id: u32) -> CGRect;
    fn CGBeginDisplayConfiguration(config: *mut CGDisplayConfigRef) -> CGError;
    fn CGCompleteDisplayConfiguration(config: CGDisplayConfigRef, option: u32) -> CGError;
    fn CGCancelDisplayConfiguration(config: CGDisplayConfigRef) -> CGError;
    fn CGDisplayRegisterReconfigurationCallback(
        callback: Option<ReconfigCallback>,
        user_info: *mut c_void,
    ) -> CGError;
    fn CGDisplayIsAsleep(id: u32) -> i32;
}

/// A snapshot of one display.
#[derive(Clone, Debug)]
pub struct DisplayInfo {
    pub id: DisplayID,
    pub builtin: bool,
    pub on: bool, // enabled / lit
    /// True when the display is asleep (e.g. the lid is closed). A sleeping built-in
    /// must NOT be force-lit — that's how the lid stays "off".
    pub asleep: bool,
    pub width: u32,
    pub height: u32,
    pub name: String,
    /// Stable identity key used to bind the auto-off feature to a specific monitor.
    pub key: String,
}

static SKYLIGHT: OnceLock<Option<Library>> = OnceLock::new();

type SetEnabledFn = unsafe extern "C" fn(CGDisplayConfigRef, u32, bool) -> CGError;
type GetDisplayListFn = unsafe extern "C" fn(u32, *mut DisplayID, *mut u32) -> CGError;

fn sky_library() -> Option<&'static Library> {
    SKYLIGHT
        .get_or_init(|| unsafe {
            Library::new("/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight").ok()
        })
        .as_ref()
}

fn set_enabled_fn() -> Option<SetEnabledFn> {
    let lib = sky_library()?;
    unsafe {
        let sym: Symbol<SetEnabledFn> = lib.get(b"CGSConfigureDisplayEnabled\0".as_slice()).ok()?;
        Some(*sym)
    }
}

/// Whether the private enable/disable symbol is available.
pub fn control_available() -> bool {
    set_enabled_fn().is_some()
}

/// Return all online (connected) displays.
pub fn online_displays() -> Vec<DisplayInfo> {
    unsafe {
        let mut count: u32 = 0;
        if CGGetOnlineDisplayList(0, std::ptr::null_mut(), &mut count) != 0 || count == 0 {
            return Vec::new();
        }
        let mut ids = vec![0u32; count as usize];
        if CGGetOnlineDisplayList(count, ids.as_mut_ptr(), &mut count) != 0 {
            return Vec::new();
        }
        ids.truncate(count as usize);
        ids.into_iter().map(describe).collect()
    }
}

/// WindowServer's known displays, including software-disabled displays that are
/// absent from the public online list. Use only for manual controls: an offline
/// entry is not evidence that a cable is connected or a screen is usable.
pub fn known_displays() -> Result<Vec<DisplayInfo>, String> {
    let lib = sky_library().ok_or("private display enumeration unavailable")?;
    let get = unsafe {
        *lib.get::<GetDisplayListFn>(b"CGSGetDisplayList\0")
            .map_err(|_| "private display enumeration unavailable")?
    };
    let ids = known_display_ids(|capacity, ids, count| unsafe { get(capacity, ids, count) })?;
    Ok(merge_displays(
        online_displays(),
        ids.into_iter().map(describe).collect(),
    ))
}

fn known_display_ids(
    mut get: impl FnMut(u32, *mut DisplayID, *mut u32) -> CGError,
) -> Result<Vec<DisplayID>, String> {
    let mut ids = vec![0; 16];
    loop {
        let mut count = 0;
        let error = get(ids.len() as u32, ids.as_mut_ptr(), &mut count);
        if error != 0 {
            return Err(format!("CGSGetDisplayList failed ({error})"));
        }
        if (count as usize) < ids.len() {
            ids.truncate(count as usize);
            ids.retain(|id| *id != 0);
            ids.sort_unstable();
            ids.dedup();
            return Ok(ids);
        }
        if ids.len() >= 1024 {
            return Err("CGSGetDisplayList returned too many displays".into());
        }
        ids.resize((ids.len() * 2).max(count as usize).min(1024), 0);
    }
}

fn merge_displays(mut online: Vec<DisplayInfo>, known: Vec<DisplayInfo>) -> Vec<DisplayInfo> {
    for display in known {
        if !online.iter().any(|current| current.id == display.id) {
            online.push(display);
        }
    }
    online
}

fn describe(id: DisplayID) -> DisplayInfo {
    unsafe {
        let builtin = CGDisplayIsBuiltin(id) != 0;
        let on = CGDisplayIsActive(id) != 0;
        let asleep = CGDisplayIsAsleep(id) != 0;
        let vendor = CGDisplayVendorNumber(id);
        let model = CGDisplayModelNumber(id);
        let serial = CGDisplaySerialNumber(id);
        let bounds = CGDisplayBounds(id);
        let width = bounds.size.width as u32;
        let height = bounds.size.height as u32;
        // The display itself doesn't expose a friendly name via public/private
        // CoreGraphics on Apple Silicon, so build a stable, distinguishable one.
        let name = if builtin {
            "Built-in Display".to_string()
        } else {
            format!("External Display {vendor:04x}-{model:04x}")
        };
        let key = format!("{vendor:04x}:{model:04x}:{serial}");
        DisplayInfo {
            id,
            builtin,
            on,
            asleep,
            width,
            height,
            name,
            key,
        }
    }
}

/// True if the display is currently on/enabled.
pub fn is_on(id: DisplayID) -> bool {
    unsafe { CGDisplayIsActive(id) != 0 }
}

/// True if `id` is the built-in display, even if it is currently powered off.
/// (A powered-off built-in drops out of the online list, but its id remains valid.)
pub fn builtin_probe(id: DisplayID) -> bool {
    unsafe { CGDisplayIsBuiltin(id) != 0 }
}

/// Power a display on (`true`) or off (`false`) via the private SkyLight API.
/// Skips the call when the display is already in the requested state.
pub fn set_enabled(id: DisplayID, enabled: bool) -> Result<(), String> {
    // Skip if already in the desired state.
    if is_on(id) == enabled {
        return Ok(());
    }
    configure_enabled(id, enabled)
}

/// Same as [`set_enabled`] but always issues the configuration call, even if the
/// display reports that it is already in the requested state. Used for recovery,
/// where `CGDisplayIsActive` can report the built-in as active while its panel is
/// actually dark — in which case `set_enabled` would silently skip the wake.
fn configure_enabled(id: DisplayID, enabled: bool) -> Result<(), String> {
    configure_enabled_with(id, enabled, false)
}

/// `configure_enabled` but `permanent = true` applies `kCGConfigurePermanently`,
/// which is more reliably accepted when re-enabling a display that was disabled and
/// then the hardware set changed (e.g. right after an external was unplugged).
fn configure_enabled_with(id: DisplayID, enabled: bool, permanent: bool) -> Result<(), String> {
    if !enabled {
        // Every disable path (manual and automatic) must pass this fresh check.
        // A sleeping or software-disabled screen cannot serve as the fallback.
        let list = online_displays();
        if !can_disable(id, &list) {
            return Err("必须保留至少一块已开启且未睡眠的显示器".into());
        }
    }
    let set = set_enabled_fn().ok_or_else(|| "private display control unavailable".to_string())?;
    crate::diagnostics::record(&format!(
        "display configure id={id} enabled={enabled} permanent={permanent}"
    ));
    unsafe {
        let mut config: CGDisplayConfigRef = std::ptr::null_mut();
        let begin = CGBeginDisplayConfiguration(&mut config);
        if begin != 0 {
            return Err(format!("CGBeginDisplayConfiguration failed ({begin})"));
        }
        let ret = set(config, id, enabled);
        if ret != 0 {
            CGCancelDisplayConfiguration(config);
            return Err(format!("CGSConfigureDisplayEnabled failed ({ret})"));
        }
        // 1 = kCGConfigureForSession (reverts at logout), 2 = kCGConfigurePermanently.
        let complete = CGCompleteDisplayConfiguration(config, if permanent { 2 } else { 1 });
        if complete != 0 {
            return Err(format!(
                "CGCompleteDisplayConfiguration failed ({complete})"
            ));
        }
        crate::diagnostics::record(&format!("display configured id={id} enabled={enabled}"));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Event-driven display-change notifications (replaces polling).
// ---------------------------------------------------------------------------

/// A completed CoreGraphics configuration change, including removal evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayChange {
    pub id: DisplayID,
    pub removed: bool,
}

type ReconfigHandler = Box<dyn Fn(DisplayChange) + Send + Sync>;
static RECONFIG: OnceLock<Mutex<Option<ReconfigHandler>>> = OnceLock::new();

// CoreGraphics calls the handler once *before* a configuration transaction and
// again after it. The online list is only authoritative in the latter callback.
const K_CG_DISPLAY_BEGIN_CONFIGURATION_FLAG: u32 = 1 << 0;

const K_CG_DISPLAY_REMOVE_FLAG: u32 = 1 << 5;

const fn completed_change(id: DisplayID, flags: u32) -> Option<DisplayChange> {
    if flags & K_CG_DISPLAY_BEGIN_CONFIGURATION_FLAG != 0 {
        return None;
    }
    Some(DisplayChange {
        id,
        removed: flags & K_CG_DISPLAY_REMOVE_FLAG != 0,
    })
}

extern "C" fn reconfig_trampoline(display: u32, flags: u32, _user: *mut c_void) {
    let Some(change) = completed_change(display, flags) else {
        return;
    };
    // CoreGraphics callbacks run on a non-main thread; forward to the handler.
    if let Some(lock) = RECONFIG.get() {
        if let Some(h) = lock.lock().unwrap().as_ref() {
            h(change);
        }
    }
}

/// Register a callback that fires whenever the display configuration changes.
/// Use this to wake a worker thread on insert/remove/timeout instead of polling.
pub fn register_reconfig_handler<F: Fn(DisplayChange) + Send + Sync + 'static>(
    handler: F,
) -> Result<(), String> {
    *RECONFIG.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some(Box::new(handler));
    let err = unsafe {
        CGDisplayRegisterReconfigurationCallback(Some(reconfig_trampoline), std::ptr::null_mut())
    };
    if err == 0 {
        Ok(())
    } else {
        Err(format!(
            "CGDisplayRegisterReconfigurationCallback failed ({err})"
        ))
    }
}

// ---------------------------------------------------------------------------
// Safety helpers: never leave every display dark.
// ---------------------------------------------------------------------------

/// Number of displays that are currently on (enabled/lit).
pub fn active_count() -> usize {
    online_displays()
        .iter()
        .filter(|d| d.on && !d.asleep)
        .count()
}

fn can_disable(id: DisplayID, list: &[DisplayInfo]) -> bool {
    list.iter().any(|d| d.id == id && d.on && !d.asleep)
        && list.iter().any(|d| d.id != id && d.on && !d.asleep)
}

/// Whether turning a display off would leave at least one other display on.
/// Returns `false` if this display is the only one currently lit.
pub fn is_last_active(id: DisplayID) -> bool {
    active_count() <= 1 && is_on(id)
}

/// Toggle a display on/off, but refuse to turn off the last lit display.
pub fn toggle_safe(id: DisplayID) -> Result<(), String> {
    // Re-enumerate immediately before a manual operation, including when turning
    // a display back on. A stale menu or a cached ID alone is not sufficient.
    let list = known_displays()?;
    let display = list
        .iter()
        .find(|display| display.id == id)
        .ok_or("显示器状态已变化，请重新打开菜单")?;
    if display.asleep {
        return Err("显示器正在睡眠，请唤醒后重试".into());
    }
    set_enabled(id, !display.on)
}

fn find_builtin_id(cached_id: Option<DisplayID>) -> Option<DisplayID> {
    online_displays()
        .iter()
        .find(|d| d.builtin)
        .map(|d| d.id)
        .or_else(|| cached_id.filter(|&id| builtin_probe(id)))
        .or_else(|| (1..=16u32).find(|&id| builtin_probe(id)))
}

/// Perform one forced recovery request. Scheduling and cancellation belong to
/// the worker; never block it with a retry loop.
pub fn enable_builtin_once(cached_id: Option<DisplayID>, permanent: bool) -> Result<(), String> {
    let id = find_builtin_id(cached_id).ok_or_else(|| "找不到内建显示器".to_string())?;
    configure_enabled_with(id, true, permanent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(id: DisplayID, on: bool, asleep: bool) -> DisplayInfo {
        DisplayInfo {
            id,
            builtin: id == 1,
            on,
            asleep,
            width: 1920,
            height: 1080,
            name: format!("display {id}"),
            key: format!("key:{id}"),
        }
    }

    #[test]
    fn every_disable_requires_another_awake_display() {
        let target = display(3, true, false);
        assert!(!can_disable(3, &[]));
        assert!(!can_disable(3, std::slice::from_ref(&target)));
        assert!(!can_disable(3, &[target.clone(), display(1, false, false)]));
        assert!(!can_disable(3, &[target.clone(), display(1, true, true)]));
        assert!(can_disable(3, &[target.clone(), display(1, true, false)]));
        assert!(!can_disable(99, &[target, display(1, true, false)]));
        // A closed/asleep laptop may never be disabled as part of a wake race.
        assert!(!can_disable(
            1,
            &[display(1, true, true), display(3, true, false)]
        ));
    }

    #[test]
    fn offline_external_remains_recoverable_after_public_list_loses_it() {
        let before = merge_displays(
            vec![display(1, true, false), display(3, true, false)],
            vec![display(1, true, false), display(3, true, false)],
        );
        // Reconstruct from WindowServer, without an in-memory/settings cache,
        // exactly as after restarting the app with the external disabled.
        let after = merge_displays(
            vec![display(1, true, false)],
            vec![display(1, true, false), display(3, false, false)],
        );
        assert_eq!(before.iter().map(|d| d.id).collect::<Vec<_>>(), vec![1, 3]);
        assert_eq!(after.iter().map(|d| d.id).collect::<Vec<_>>(), vec![1, 3]);
        assert!(!after[1].on);
        // Offline entries are for controls only, never for last-screen safety.
        assert!(!can_disable(1, &after));
    }

    #[test]
    fn online_observation_wins_over_stale_private_enumeration() {
        let list = merge_displays(
            vec![display(3, true, false)],
            vec![display(3, false, false), display(2, false, false)],
        );
        assert_eq!(list.len(), 2);
        assert!(list[0].on);
    }

    #[test]
    fn private_enumeration_grows_without_silently_losing_offline_ids() {
        let mut calls = 0;
        let ids = known_display_ids(|capacity, ids, count| {
            calls += 1;
            let len = capacity.min(19);
            for i in 0..len {
                unsafe { ids.add(i as usize).write(i + 1) };
            }
            unsafe { count.write(len) };
            0
        })
        .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(ids, (1..=19).collect::<Vec<_>>());
        assert!(known_display_ids(|_, _, _| 1001).is_err());
    }

    #[test]
    fn only_completed_callbacks_expose_removal_evidence() {
        assert_eq!(
            completed_change(
                3,
                K_CG_DISPLAY_BEGIN_CONFIGURATION_FLAG | K_CG_DISPLAY_REMOVE_FLAG
            ),
            None
        );
        assert_eq!(
            completed_change(3, K_CG_DISPLAY_REMOVE_FLAG),
            Some(DisplayChange {
                id: 3,
                removed: true
            })
        );
        assert_eq!(
            completed_change(3, 1 << 3),
            Some(DisplayChange {
                id: 3,
                removed: false
            })
        );
    }
}
