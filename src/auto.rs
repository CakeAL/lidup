//! The auto-off rule: close the built-in display when the bound external display
//! is connected, and restore it when the trigger goes away.

use crate::config::Settings;
use crate::displays;

fn bound_present(list: &[displays::DisplayInfo], key: &str) -> bool {
    list.iter().any(|d| !d.builtin && d.key == key)
}

/// Pure decision: if a change is needed, return the built-in id to set and the
/// desired state. `None` means "leave the built-in alone".
pub fn decide(settings: &Settings, list: &[displays::DisplayInfo]) -> Option<(u32, bool)> {
    let bound = settings.bound_key.as_deref()?;
    let present = bound_present(list, bound);
    let builtin = list.iter().find(|d| d.builtin)?;

    // Sleep is not a request to change the display configuration. In particular,
    // disabling a still-active but sleeping panel can reconfigure the external
    // display during wake and discard its HDR mode.
    if builtin.asleep {
        return None;
    }

    if present {
        // The bound external is connected: keep the built-in off.
        if builtin.on {
            Some((builtin.id, false))
        } else {
            None
        }
    } else if settings.restore_builtin && !builtin.on {
        // The trigger went away: restore the built-in display.
        Some((builtin.id, true))
    } else {
        None
    }
}

/// Apply the auto-off rule described by `settings` to the live display state.
///
/// A powered-off built-in leaves the online display list, so its id is cached in
/// `settings` (refreshed whenever it is seen online) to allow restoring it.
pub fn apply_auto(settings: &mut Settings) {
    let list = displays::online_displays();
    apply_auto_with_list(settings, &list);
}

/// Apply the rule to a single settled snapshot. The worker must use the same
/// snapshot it checked for a bound external; re-enumerating could catch a
/// different, transient state halfway through wake.
pub fn apply_auto_with_list(settings: &mut Settings, list: &[displays::DisplayInfo]) {
    if let Some(b) = list.iter().find(|d| d.builtin) {
        settings.builtin_id = Some(b.id);
        settings.builtin_key = Some(b.key.clone());
        if b.asleep {
            return;
        }
    }
    let Some(bid) = settings.builtin_id else {
        return;
    };

    // Re-run the decision against the current snapshot, using the cached id for the
    // built-in in case it is currently powered off (and thus not in `list`).
    let bound = settings.bound_key.as_deref();
    let present = bound.map_or(false, |k| bound_present(&list, k));
    let builtin_on = list.iter().find(|d| d.builtin).map_or(false, |b| b.on);

    if present {
        if builtin_on {
            let _ = displays::set_enabled(bid, false);
        }
    } else if settings.restore_builtin && !builtin_on {
        // Restore via the windowed recovery (re-asserts until confirmed online),
        // rather than a one-shot enable that the unplug reconfig can roll back.
        displays::recover_builtin();
    }
}

/// Apply the decision against an explicit snapshot (used by tests to avoid driving
/// real display hardware).
pub fn apply_auto_to(settings: &Settings, list: &[displays::DisplayInfo]) {
    if let Some((id, on)) = decide(settings, list) {
        let _ = displays::set_enabled(id, on);
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
            vendor: 0,
            model: 0,
            serial: 0,
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
}
