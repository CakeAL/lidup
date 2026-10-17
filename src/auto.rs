//! The auto-off rule: close the built-in display when the bound external display
//! is connected, and restore it when the trigger goes away.

use crate::config::Settings;
use crate::displays;

fn bound_present(list: &[displays::DisplayInfo], key: &str) -> bool {
    list.iter().any(|d| !d.builtin && d.key == key)
}

fn bound_ready(list: &[displays::DisplayInfo], key: &str) -> bool {
    list.iter()
        .any(|d| !d.builtin && d.key == key && d.on && !d.asleep)
}

/// Pure decision: if a change is needed, return the built-in id to set and the
/// desired state. `None` means "leave the built-in alone".
pub fn decide(settings: &Settings, list: &[displays::DisplayInfo]) -> Option<(u32, bool)> {
    let bound = settings.bound_key.as_deref()?;
    let present = bound_present(list, bound);
    let builtin = list.iter().find(|d| d.builtin);
    let id = builtin.map(|d| d.id).or(settings.builtin_id)?;
    let builtin_on = builtin.is_some_and(|d| d.on);

    // Sleep is not a request to change the display configuration. In particular,
    // disabling a still-active but sleeping panel can reconfigure the external
    // display during wake and discard its HDR mode.
    if builtin.is_some_and(|d| d.asleep) {
        return None;
    }

    if present {
        // A waking external display can be online before it is active. Turning
        // off the built-in then reconfigures the display set during HDR setup.
        if !bound_ready(list, bound) {
            return None;
        }
        // The bound external is connected: keep the built-in off.
        if builtin_on {
            Some((id, false))
        } else {
            None
        }
    } else if settings.restore_builtin && !builtin_on {
        // The trigger went away: restore the built-in display.
        Some((id, true))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::displays::DisplayInfo;

    fn disp(id: u32, builtin: bool, on: bool, key: &str) -> DisplayInfo {
        DisplayInfo {
            id,
            builtin,
            on,
            asleep: false,
            width: 1920,
            height: 1080,
            name: format!("d{id}"),
            key: key.to_string(),
        }
    }

    fn settings(bound: Option<&str>, restore: bool) -> Settings {
        Settings {
            bound_key: bound.map(|s| s.to_string()),
            restore_builtin: restore,
            ..Settings::default()
        }
    }

    #[test]
    fn bound_present_off_when_builtin_on() {
        let s = settings(Some("ext:1"), true);
        let list = vec![
            disp(1, true, true, "builtin"),
            disp(2, false, true, "ext:1"),
        ];
        assert_eq!(decide(&s, &list), Some((1, false)));
    }

    #[test]
    fn bound_present_leaves_off_builtin_alone() {
        let s = settings(Some("ext:1"), true);
        let list = vec![
            disp(1, true, false, "builtin"),
            disp(2, false, true, "ext:1"),
        ];
        assert_eq!(decide(&s, &list), None);
    }

    #[test]
    fn bound_absent_restores() {
        let s = settings(Some("ext:9"), true);
        let list = vec![disp(1, true, false, "builtin")];
        assert_eq!(decide(&s, &list), Some((1, true)));
    }

    #[test]
    fn bound_absent_restores_builtin_missing_from_online_list() {
        let mut s = settings(Some("ext:9"), true);
        s.builtin_id = Some(42);
        assert_eq!(decide(&s, &[]), Some((42, true)));
        let list = vec![disp(2, false, true, "ext:9")];
        assert_eq!(decide(&s, &list), None);
    }

    #[test]
    fn bound_absent_no_restore_when_disabled() {
        let s = settings(Some("ext:9"), false);
        let list = vec![disp(1, true, false, "builtin")];
        assert_eq!(decide(&s, &list), None);
    }

    #[test]
    fn no_bound_is_noop() {
        let s = settings(None, true);
        let list = vec![
            disp(1, true, true, "builtin"),
            disp(2, false, true, "ext:1"),
        ];
        assert_eq!(decide(&s, &list), None);
    }

    #[test]
    fn missing_builtin_is_noop() {
        let s = settings(Some("ext:1"), true);
        let list = vec![disp(2, false, true, "ext:1")];
        assert_eq!(decide(&s, &list), None);
    }

    #[test]
    fn sleeping_builtin_is_never_reconfigured() {
        let s = settings(Some("ext:1"), true);
        let mut builtin = disp(1, true, true, "builtin");
        builtin.asleep = true;
        assert_eq!(
            decide(&s, &[builtin.clone(), disp(2, false, true, "ext:1")]),
            None
        );
        builtin.on = false;
        assert_eq!(decide(&s, &[builtin]), None);
    }

    #[test]
    fn waking_external_does_not_turn_off_builtin() {
        let s = settings(Some("ext:1"), true);
        let builtin = disp(1, true, true, "builtin");
        let mut external = disp(2, false, true, "ext:1");
        external.asleep = true;
        assert_eq!(decide(&s, &[builtin.clone(), external.clone()]), None);
        external.asleep = false;
        external.on = false;
        assert_eq!(decide(&s, &[builtin, external]), None);
    }
}
