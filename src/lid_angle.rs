//! Read the MacBook hinge angle from Apple's built-in HID sensor.
//!
//! The `las` HID device exposes a feature report with the angle as a little
//! endian integer in whole degrees. A missing sensor or failed read must never
//! be interpreted as a closed lid.

use std::ffi::{c_char, c_void};
use std::ptr;
use std::time::{Duration, Instant};

type CFRef = *const c_void;
type HIDRef = *mut c_void;

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFTypeDictionaryKeyCallBacks: u8;
    static kCFTypeDictionaryValueCallBacks: u8;
    fn CFDictionaryCreateMutable(
        allocator: CFRef,
        capacity: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> HIDRef;
    fn CFDictionarySetValue(dictionary: HIDRef, key: CFRef, value: CFRef);
    fn CFStringCreateWithCString(allocator: CFRef, string: *const c_char, encoding: u32) -> CFRef;
    fn CFNumberCreate(allocator: CFRef, number_type: isize, value: *const i32) -> CFRef;
    fn CFSetGetCount(set: CFRef) -> isize;
    fn CFSetGetValues(set: CFRef, values: *mut CFRef);
    fn CFRetain(value: CFRef) -> CFRef;
    fn CFRelease(value: CFRef);
}

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOHIDManagerCreate(allocator: CFRef, options: u32) -> HIDRef;
    fn IOHIDManagerOpen(manager: HIDRef, options: u32) -> i32;
    fn IOHIDManagerClose(manager: HIDRef, options: u32) -> i32;
    fn IOHIDManagerSetDeviceMatching(manager: HIDRef, matching: CFRef);
    fn IOHIDManagerCopyDevices(manager: HIDRef) -> CFRef;
    fn IOHIDDeviceOpen(device: HIDRef, options: u32) -> i32;
    fn IOHIDDeviceClose(device: HIDRef, options: u32) -> i32;
    fn IOHIDDeviceGetReport(
        device: HIDRef,
        report_type: i32,
        report_id: isize,
        report: *mut u8,
        length: *mut isize,
    ) -> i32;
}

const UTF8: u32 = 0x0800_0100;
const FEATURE_REPORT: i32 = 2;

struct Sensor {
    manager: HIDRef,
    device: HIDRef,
}

impl Sensor {
    fn open() -> Option<Self> {
        unsafe {
            let manager = IOHIDManagerCreate(ptr::null(), 0);
            if manager.is_null() {
                return None;
            }
            let matching = CFDictionaryCreateMutable(
                ptr::null(),
                4,
                &raw const kCFTypeDictionaryKeyCallBacks as *const c_void,
                &raw const kCFTypeDictionaryValueCallBacks as *const c_void,
            );
            if matching.is_null() {
                CFRelease(manager.cast());
                return None;
            }
            for (key, value) in [
                (b"VendorID\0".as_slice(), 0x05ac),
                (b"ProductID\0".as_slice(), 0x8104),
                (b"PrimaryUsagePage\0".as_slice(), 0x20),
                (b"PrimaryUsage\0".as_slice(), 0x8a),
            ] {
                let key = CFStringCreateWithCString(ptr::null(), key.as_ptr().cast(), UTF8);
                let number = CFNumberCreate(ptr::null(), 3, &value);
                if !key.is_null() && !number.is_null() {
                    CFDictionarySetValue(matching, key, number);
                }
                if !key.is_null() {
                    CFRelease(key);
                }
                if !number.is_null() {
                    CFRelease(number);
                }
            }
            IOHIDManagerSetDeviceMatching(manager, matching.cast());
            CFRelease(matching.cast());
            if IOHIDManagerOpen(manager, 0) != 0 {
                CFRelease(manager.cast());
                return None;
            }
            let devices = IOHIDManagerCopyDevices(manager);
            let mut chosen = ptr::null_mut();
            if !devices.is_null() {
                let count = CFSetGetCount(devices);
                if count > 0 {
                    let mut items = vec![ptr::null(); count as usize];
                    CFSetGetValues(devices, items.as_mut_ptr());
                    for item in items {
                        let device = item.cast_mut();
                        if IOHIDDeviceOpen(device, 0) != 0 {
                            continue;
                        }
                        let mut report = [0u8; 8];
                        let mut length = report.len() as isize;
                        let readable = IOHIDDeviceGetReport(
                            device,
                            FEATURE_REPORT,
                            1,
                            report.as_mut_ptr(),
                            &mut length,
                        ) == 0
                            && decode(&report, length).is_some();
                        if readable {
                            chosen = CFRetain(item).cast_mut();
                            break;
                        }
                        IOHIDDeviceClose(device, 0);
                    }
                }
                CFRelease(devices);
            }
            if chosen.is_null() {
                IOHIDManagerClose(manager, 0);
                CFRelease(manager.cast());
                return None;
            }
            Some(Self {
                manager,
                device: chosen,
            })
        }
    }

    fn read(&self) -> Option<u16> {
        let mut report = [0u8; 8];
        let mut length = report.len() as isize;
        if unsafe {
            IOHIDDeviceGetReport(
                self.device,
                FEATURE_REPORT,
                1,
                report.as_mut_ptr(),
                &mut length,
            )
        } != 0
        {
            return None;
        }
        decode(&report, length)
    }
}

impl Drop for Sensor {
    fn drop(&mut self) {
        unsafe {
            IOHIDDeviceClose(self.device, 0);
            CFRelease(self.device.cast());
            IOHIDManagerClose(self.manager, 0);
            CFRelease(self.manager.cast());
        }
    }
}

fn decode(report: &[u8; 8], length: isize) -> Option<u16> {
    if length < 3 || report[0] != 1 {
        return None;
    }
    let angle = u16::from_le_bytes([report[1], report[2]]);
    (angle <= 180).then_some(angle)
}

/// Owned by the display worker; retries opening after wake or a failed read.
#[derive(Default)]
pub struct Reader {
    sensor: Option<Sensor>,
    last_attempt: Option<Instant>,
}

impl Reader {
    pub fn available(&self) -> bool {
        self.sensor.is_some()
    }

    pub fn reset(&mut self) {
        self.sensor = None;
        self.last_attempt = None;
    }

    pub fn sample(&mut self, now: Instant) -> Option<u16> {
        if self.sensor.is_none()
            && self
                .last_attempt
                .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(5))
        {
            self.last_attempt = Some(now);
            self.sensor = Sensor::open();
        }
        let result = self.sensor.as_ref().and_then(Sensor::read);
        if self.sensor.is_some() && result.is_none() {
            self.sensor = None;
        }
        result
    }
}

/// Debounce the crossing and hold the existing side within 2° of the threshold.
#[derive(Default)]
pub struct Gate {
    threshold: Option<u16>,
    open: Option<bool>,
    candidate: Option<(bool, Instant)>,
}

impl Gate {
    pub fn reset(&mut self) {
        self.open = None;
        self.candidate = None;
    }

    pub fn configure(&mut self, threshold: Option<u16>) {
        if self.threshold != threshold {
            *self = Self {
                threshold,
                ..Self::default()
            };
        }
    }

    pub fn open(&self) -> Option<bool> {
        self.open
    }

    pub fn settling(&self) -> bool {
        self.candidate.is_some()
    }

    pub fn observe(&mut self, angle: Option<u16>, now: Instant) -> bool {
        let (Some(threshold), Some(angle)) = (self.threshold, angle) else {
            // After a read failure, require a fresh settled reading instead of
            // briefly acting on the last angle when the device returns.
            self.reset();
            return false;
        };
        let next = match self.open {
            Some(true) => angle > threshold.saturating_sub(2),
            Some(false) => angle > threshold.saturating_add(2),
            None => angle > threshold,
        };
        if self.open == Some(next) {
            self.candidate = None;
            return false;
        }
        match self.candidate {
            Some((side, since))
                if side == next && now.duration_since(since) >= Duration::from_millis(500) =>
            {
                self.open = Some(next);
                self.candidate = None;
                true
            }
            Some((side, _)) if side == next => false,
            _ => {
                self.candidate = Some((next, now));
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoding_uses_whole_degrees_and_rejects_invalid_reports() {
        assert_eq!(decode(&[1, 90, 0, 0, 0, 0, 0, 0], 3), Some(90));
        assert_eq!(decode(&[2, 90, 0, 0, 0, 0, 0, 0], 3), None);
        assert_eq!(decode(&[1, 90, 0, 0, 0, 0, 0, 0], 2), None);
    }

    #[test]
    fn gate_requires_stable_crossing_and_avoids_thrashing() {
        let now = Instant::now();
        let mut gate = Gate::default();
        gate.configure(Some(90));
        assert!(!gate.observe(Some(91), now));
        assert!(gate.observe(Some(91), now + Duration::from_millis(600)));
        assert_eq!(gate.open(), Some(true));
        assert!(!gate.observe(Some(89), now + Duration::from_secs(1)));
        assert_eq!(gate.open(), Some(true));
        assert!(!gate.observe(Some(87), now + Duration::from_secs(2)));
        assert!(gate.observe(Some(87), now + Duration::from_millis(2600)));
        assert_eq!(gate.open(), Some(false));
        assert!(!gate.observe(None, now + Duration::from_secs(3)));
        assert_eq!(gate.open(), None);
        assert!(!gate.observe(Some(91), now + Duration::from_secs(4)));
        assert!(gate.observe(Some(91), now + Duration::from_millis(4600)));
    }

    #[test]
    #[ignore = "reads the current MacBook HID sensor"]
    fn reads_real_sensor() {
        let mut reader = Reader::default();
        assert!(reader.sample(Instant::now()).is_some());
    }
}
