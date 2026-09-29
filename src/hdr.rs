//! Preserve an external display's user-selected HDR setting across sleep.
//!
//! WindowServer can recreate an external display in SDR mode during wake even
//! when lidup makes no display configuration call. MonitorPanel is the private
//! framework used by macOS Displays settings for the HDR preference. All calls
//! here are optional: if its interface changes, display on/off still works.

use libloading::Library;
use std::ffi::{c_char, c_void};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

type Obj = *mut c_void;
type Sel = *mut c_void;

#[link(name = "objc")]
extern "C" {
    fn objc_getClass(name: *const c_char) -> Obj;
    fn object_getClass(object: Obj) -> Obj;
    fn class_respondsToSelector(class: Obj, selector: Sel) -> i8;
    fn sel_registerName(name: *const c_char) -> Sel;
    fn objc_msgSend();
    fn objc_autoreleasePoolPush() -> Obj;
    fn objc_autoreleasePoolPop(pool: Obj);
}

static MONITOR_PANEL: OnceLock<Option<Library>> = OnceLock::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HdrState {
    pub supported: bool,
    pub enabled: bool,
}

struct AutoreleasePool(Obj);

impl AutoreleasePool {
    unsafe fn new() -> Self {
        Self(objc_autoreleasePoolPush())
    }
}

impl Drop for AutoreleasePool {
    fn drop(&mut self) {
        unsafe { objc_autoreleasePoolPop(self.0) }
    }
}

/// Read HDR state for a CoreGraphics display ID. `None` means MonitorPanel did
/// not expose that display yet (common while a monitor is reconnecting).
pub fn state(id: u32) -> Option<HdrState> {
    with_display(id, |display| unsafe {
        let supports = send_bool(display, b"hasHDRModes\0") != 0;
        let enabled = send_bool(display, b"preferHDRModes\0") != 0;
        HdrState {
            supported: supports,
            enabled,
        }
    })
}

/// Re-enable HDR only if the user had it on before sleeping. The caller checks
/// `state` first, so this never changes a display already in the desired mode.
pub fn enable(id: u32) -> bool {
    let changed = with_display(id, |display| unsafe {
        if send_bool(display, b"hasHDRModes\0") == 0
            || !responds_to(display, b"setPreferHDRModes:\0")
        {
            return false;
        }
        let set: unsafe extern "C" fn(Obj, Sel, i8) =
            std::mem::transmute(objc_msgSend as *const ());
        set(
            display,
            sel_registerName(b"setPreferHDRModes:\0".as_ptr().cast()),
            1,
        );
        true
    })
    .unwrap_or(false);
    crate::diagnostics::record(&format!("HDR restore id={id} requested={changed}"));
    changed
}

fn with_display<T>(id: u32, f: impl FnOnce(Obj) -> T) -> Option<T> {
    MONITOR_PANEL
        .get_or_init(|| unsafe {
            Library::new("/System/Library/PrivateFrameworks/MonitorPanel.framework/MonitorPanel")
                .ok()
        })
        .as_ref()?;

    unsafe {
        let _pool = AutoreleasePool::new();
        let manager_class = objc_getClass(b"MPDisplayMgr\0".as_ptr().cast());
        if manager_class.is_null() {
            return None;
        }
        let manager = send_obj(manager_class, b"new\0");
        if manager.is_null() {
            return None;
        }
        let displays = if responds_to(manager, b"displays\0") {
            send_obj(manager, b"displays\0")
        } else {
            std::ptr::null_mut()
        };
        let mut result = None;
        if !displays.is_null() {
            let count: unsafe extern "C" fn(Obj, Sel) -> usize =
                std::mem::transmute(objc_msgSend as *const ());
            let at: unsafe extern "C" fn(Obj, Sel, usize) -> Obj =
                std::mem::transmute(objc_msgSend as *const ());
            let n = count(displays, sel_registerName(b"count\0".as_ptr().cast()));
            for index in 0..n {
                let display = at(
                    displays,
                    sel_registerName(b"objectAtIndex:\0".as_ptr().cast()),
                    index,
                );
                if !display.is_null()
                    && responds_to(display, b"displayID\0")
                    && responds_to(display, b"hasHDRModes\0")
                    && responds_to(display, b"preferHDRModes\0")
                    && send_u32(display, b"displayID\0") == id
                {
                    result = Some(f(display));
                    break;
                }
            }
        }
        let release: unsafe extern "C" fn(Obj, Sel) =
            std::mem::transmute(objc_msgSend as *const ());
        release(manager, sel_registerName(b"release\0".as_ptr().cast()));
        result
    }
}

unsafe fn responds_to(object: Obj, selector: &[u8]) -> bool {
    let class = object_getClass(object);
    !class.is_null()
        && class_respondsToSelector(class, sel_registerName(selector.as_ptr().cast())) != 0
}

unsafe fn send_obj(object: Obj, selector: &[u8]) -> Obj {
    let send: unsafe extern "C" fn(Obj, Sel) -> Obj =
        std::mem::transmute(objc_msgSend as *const ());
    send(object, sel_registerName(selector.as_ptr().cast()))
}

unsafe fn send_bool(object: Obj, selector: &[u8]) -> i8 {
    let send: unsafe extern "C" fn(Obj, Sel) -> i8 = std::mem::transmute(objc_msgSend as *const ());
    send(object, sel_registerName(selector.as_ptr().cast()))
}

unsafe fn send_u32(object: Obj, selector: &[u8]) -> u32 {
    let send: unsafe extern "C" fn(Obj, Sel) -> u32 =
        std::mem::transmute(objc_msgSend as *const ());
    send(object, sel_registerName(selector.as_ptr().cast()))
}

/// Keeps a stable observation from before sleep; wake-time SDR observations must
/// not overwrite it before the monitor has had a chance to finish reconnecting.
#[derive(Default)]
pub struct HdrRecovery {
    bound_key: Option<String>,
    last_enabled: bool,
    off_since: Option<Instant>,
    observe_requested: bool,
    pending: bool,
    ready_at: Option<Instant>,
    expires_at: Option<Instant>,
    attempts: u8,
}

impl HdrRecovery {
    pub fn restore_pending(&self) -> bool {
        self.pending
    }

    pub fn bind(&mut self, key: Option<&str>) {
        if self.bound_key.as_deref() != key {
            *self = Self {
                bound_key: key.map(str::to_owned),
                observe_requested: key.is_some(),
                ..Self::default()
            };
        }
    }

    pub fn wake(&mut self, now: Instant) {
        if self.last_enabled && !self.pending {
            self.pending = true;
            self.ready_at = Some(now + Duration::from_secs(3));
            self.expires_at = Some(now + Duration::from_secs(45));
            self.attempts = 0;
        }
    }

    /// Screen-wake notification arrives after the first WindowServer mode
    /// negotiation. Check shortly afterward, while the monitor may still be
    /// blank, instead of waiting for the generic three-second fallback.
    pub fn screens_woke(&mut self, now: Instant) {
        if self.pending && self.attempts == 0 {
            self.ready_at = Some(now + Duration::from_millis(200));
        }
    }

    /// Called only for an active, awake bound monitor. Stable state needs no
    /// background reads; events and a pending recovery request the next read.
    pub fn observe(&mut self, id: u32, now: Instant) -> Option<Instant> {
        self.observe_requested_with(id, now, state, enable)
    }

    fn observe_requested_with(
        &mut self,
        id: u32,
        now: Instant,
        read: impl Fn(u32) -> Option<HdrState>,
        restore: impl Fn(u32) -> bool,
    ) -> Option<Instant> {
        if !self.pending && !self.observe_requested {
            return None;
        }
        if !self.pending {
            if let Some(deadline) = self.off_since.map(|since| since + Duration::from_secs(5)) {
                if now < deadline {
                    return Some(deadline);
                }
            }
        }
        let retry = self.observe_with(id, now, read, restore);
        if !self.pending {
            self.observe_requested = self.off_since.is_some();
            return self
                .off_since
                .map(|since| since + Duration::from_secs(5))
                .or(retry);
        }
        retry
    }

    pub fn request_observation(&mut self) {
        self.observe_requested = true;
    }

    /// A live reading before sleep or a display change takes precedence over an
    /// older sample. The caller must confirm the monitor is active and awake.
    pub fn capture_current_preference(&mut self, id: u32) {
        if self.pending {
            // SDR can be transient while an earlier HDR recovery is in flight.
            return;
        }
        if let Some(current) = state(id).filter(|state| state.supported) {
            self.last_enabled = current.enabled;
            self.off_since = None;
            self.observe_requested = false;
            crate::diagnostics::record(&format!(
                "HDR preference captured id={id} enabled={}",
                current.enabled
            ));
        }
    }

    fn observe_with(
        &mut self,
        id: u32,
        now: Instant,
        read: impl Fn(u32) -> Option<HdrState>,
        restore: impl Fn(u32) -> bool,
    ) -> Option<Instant> {
        if !self.pending {
            if let Some(current) = read(id) {
                if current.supported {
                    if current.enabled {
                        if !self.last_enabled {
                            crate::diagnostics::record(&format!("HDR observed enabled id={id}"));
                        }
                        self.last_enabled = true;
                        self.off_since = None;
                    } else {
                        // A display may briefly report SDR as it goes to sleep.
                        // Treat a sustained change as the user's HDR preference.
                        if !self.last_enabled {
                            self.off_since = None;
                        } else if self.off_since.is_some_and(|since| {
                            now.duration_since(since) >= Duration::from_secs(5)
                        }) {
                            crate::diagnostics::record(&format!("HDR observed disabled id={id}"));
                            self.last_enabled = false;
                            self.off_since = None;
                        } else {
                            self.off_since.get_or_insert(now);
                        }
                    }
                }
            }
            return None;
        }
        if self.expires_at.is_some_and(|expires| now >= expires) || self.attempts >= 3 {
            self.pending = false;
            return None;
        }
        let ready = self.ready_at.unwrap_or(now);
        if now < ready {
            return Some(ready);
        }
        match read(id) {
            Some(HdrState {
                supported: true,
                enabled: true,
            }) => {
                crate::diagnostics::record(&format!("HDR confirmed enabled id={id}"));
                self.pending = false;
                self.last_enabled = true;
                self.off_since = None;
                None
            }
            Some(HdrState {
                supported: true,
                enabled: false,
            }) => {
                crate::diagnostics::record(&format!("HDR wake observed SDR id={id}"));
                let _ = restore(id);
                self.attempts += 1;
                let next = now + Duration::from_secs(2);
                self.ready_at = Some(next);
                Some(next)
            }
            _ => {
                let next = now + Duration::from_secs(2);
                self.ready_at = Some(next);
                Some(next)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    const HDR_ON: HdrState = HdrState {
        supported: true,
        enabled: true,
    };
    const HDR_OFF: HdrState = HdrState {
        supported: true,
        enabled: false,
    };

    #[test]
    fn wake_recovery_only_arms_for_the_same_hdr_enabled_monitor() {
        let now = Instant::now();
        let mut recovery = HdrRecovery::default();
        recovery.bind(Some("monitor-a"));
        recovery.wake(now);
        assert!(!recovery.pending);

        recovery.last_enabled = true;
        recovery.wake(now);
        assert!(recovery.pending);
        assert_eq!(recovery.ready_at, Some(now + Duration::from_secs(3)));

        recovery.bind(Some("monitor-b"));
        assert!(!recovery.pending);
        assert!(!recovery.last_enabled);
    }

    #[test]
    fn wake_restores_only_after_monitor_settles() {
        let now = Instant::now();
        let calls = Cell::new(0);
        let mut recovery = HdrRecovery::default();
        recovery.bind(Some("monitor-a"));
        recovery.observe_with(2, now, |_| Some(HDR_ON), |_| unreachable!());
        recovery.wake(now);

        let restore = |_| {
            calls.set(calls.get() + 1);
            true
        };
        assert_eq!(
            recovery.observe_with(2, now + Duration::from_secs(1), |_| Some(HDR_OFF), restore),
            Some(now + Duration::from_secs(3))
        );
        assert_eq!(calls.get(), 0);
        recovery.observe_with(2, now + Duration::from_secs(3), |_| Some(HDR_OFF), restore);
        assert_eq!(calls.get(), 1);
        recovery.observe_with(2, now + Duration::from_secs(5), |_| Some(HDR_ON), restore);
        assert_eq!(calls.get(), 1);
        assert!(!recovery.restore_pending());
    }

    #[test]
    fn screen_wake_advances_recovery_without_rearming_on_duplicate_wake() {
        let now = Instant::now();
        let mut recovery = HdrRecovery::default();
        recovery.bind(Some("monitor-a"));
        recovery.last_enabled = true;
        recovery.wake(now);
        recovery.screens_woke(now + Duration::from_secs(1));
        let ready = now + Duration::from_millis(1200);
        assert_eq!(recovery.ready_at, Some(ready));
        recovery.wake(now + Duration::from_secs(2));
        assert_eq!(recovery.ready_at, Some(ready));
    }

    #[test]
    fn sustained_manual_hdr_off_is_respected_on_next_wake() {
        let now = Instant::now();
        let mut recovery = HdrRecovery::default();
        recovery.bind(Some("monitor-a"));
        recovery.observe_with(2, now, |_| Some(HDR_ON), |_| unreachable!());
        recovery.observe_with(
            2,
            now + Duration::from_secs(2),
            |_| Some(HDR_OFF),
            |_| unreachable!(),
        );
        recovery.observe_with(
            2,
            now + Duration::from_secs(8),
            |_| Some(HDR_OFF),
            |_| unreachable!(),
        );
        recovery.wake(now + Duration::from_secs(10));
        assert!(!recovery.restore_pending());
    }

    #[test]
    fn stable_hdr_is_read_only_on_events_and_off_is_confirmed_once() {
        let now = Instant::now();
        let mut recovery = HdrRecovery::default();
        recovery.bind(Some("monitor-a"));
        let reads = Cell::new(0);
        let on = |_| {
            reads.set(reads.get() + 1);
            Some(HDR_ON)
        };
        assert_eq!(
            recovery.observe_requested_with(2, now, on, |_| unreachable!()),
            None
        );
        assert_eq!(reads.get(), 1);
        assert_eq!(
            recovery.observe_requested_with(
                2,
                now + Duration::from_secs(30),
                on,
                |_| unreachable!()
            ),
            None
        );
        assert_eq!(reads.get(), 1);

        recovery.request_observation();
        let off = |_| {
            reads.set(reads.get() + 1);
            Some(HDR_OFF)
        };
        assert_eq!(
            recovery.observe_requested_with(
                2,
                now + Duration::from_secs(31),
                off,
                |_| unreachable!()
            ),
            Some(now + Duration::from_secs(36))
        );
        assert_eq!(reads.get(), 2);
        assert_eq!(
            recovery.observe_requested_with(
                2,
                now + Duration::from_secs(33),
                off,
                |_| unreachable!()
            ),
            Some(now + Duration::from_secs(36))
        );
        assert_eq!(reads.get(), 2);
        assert_eq!(
            recovery.observe_requested_with(
                2,
                now + Duration::from_secs(36),
                off,
                |_| unreachable!()
            ),
            None
        );
        assert_eq!(reads.get(), 3);
        recovery.wake(now + Duration::from_secs(37));
        assert!(!recovery.restore_pending());
    }

    #[test]
    #[ignore = "reads the current macOS display session"]
    fn monitor_panel_reads_connected_display() {
        let external = crate::displays::online_displays()
            .into_iter()
            .find(|d| !d.builtin && d.on)
            .expect("connect an external display before running this test");
        assert!(state(external.id).is_some());
    }
}
