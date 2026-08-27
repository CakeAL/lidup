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
use std::sync::OnceLock;

pub type DisplayID = u32;

type CGError = i32;
type CGDisplayConfigRef = *mut c_void;

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
}

/// A snapshot of one display.
#[derive(Clone, Debug)]
pub struct DisplayInfo {
    pub id: DisplayID,
    pub builtin: bool,
    pub on: bool, // enabled / lit
    #[allow(dead_code)]
    pub vendor: u32,
    #[allow(dead_code)]
    pub model: u32,
    #[allow(dead_code)]
    pub serial: u32,
    pub width: u32,
    pub height: u32,
    pub name: String,
    /// Stable identity key used to bind the auto-off feature to a specific monitor.
    pub key: String,
}

static SKYLIGHT: OnceLock<Option<Library>> = OnceLock::new();

type SetEnabledFn = unsafe extern "C" fn(CGDisplayConfigRef, u32, bool) -> CGError;

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
        ids.into_iter().map(|id| describe(id)).collect()
    }
}

fn describe(id: DisplayID) -> DisplayInfo {
    unsafe {
        let builtin = CGDisplayIsBuiltin(id) != 0;
        let on = CGDisplayIsActive(id) != 0;
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
            vendor,
            model,
            serial,
            width,
            height,
            name,
            key,
        }
    }
}

/// Return the built-in display, if present.
pub fn builtin_display() -> Option<DisplayInfo> {
    online_displays().into_iter().find(|d| d.builtin)
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
pub fn set_enabled(id: DisplayID, enabled: bool) -> Result<(), String> {
    let set = set_enabled_fn().ok_or_else(|| "private display control unavailable".to_string())?;

    // Skip if already in the desired state.
    if is_on(id) == enabled {
        return Ok(());
    }

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
        // kCGConfigureForSession = 1: applies to this session, auto-reverts at
        // logout. Safer than kCGConfigurePermanently for a daemon-style app.
        let complete = CGCompleteDisplayConfiguration(config, 1);
        if complete != 0 {
            return Err(format!(
                "CGCompleteDisplayConfiguration failed ({complete})"
            ));
        }
        Ok(())
    }
}

/// If there is a built-in display and it differs from `on`, set it.
pub fn set_builtin(on: bool) -> Result<(), String> {
    if let Some(b) = builtin_display() {
        set_enabled(b.id, on)
    } else {
        Ok(())
    }
}
