//! lidup — a macOS menu-bar app that turns off the built-in display when a chosen
//! external display is connected, and provides per-display on/off toggles.
//!
//! The heavy lifting (enumeration + on/off via the private SkyLight API) lives in
//! `displays`. A background worker thread periodically re-checks state, applies the
//! auto-off rule and pushes a snapshot to the winit event loop, which renders the
//! menu-bar menu. Menu item ids are turned into commands on the worker thread.

use lidup::{auto, config, displays, launch, updates};

use block2::RcBlock;
use config::Settings;
use displays::DisplayInfo;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{
    NSWorkspace, NSWorkspaceDidWakeNotification, NSWorkspaceScreensDidSleepNotification,
    NSWorkspaceScreensDidWakeNotification, NSWorkspaceWillSleepNotification,
};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSObjectProtocol};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::WindowId;

use tray_icon::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::TrayIcon;

/// The window/frame state pushed from the worker thread to the event loop.
struct Snapshot {
    displays: Vec<DisplayInfo>,
    bound: Option<String>,
    auto_off_suppressed: bool,
    control_ok: bool,
    /// Whether launching at login is currently enabled.
    launch_at_login: bool,
}

/// Events that wake the worker thread. Display callbacks are coalesced before
/// evaluation; a missing bound monitor gets one delayed confirmation.
enum WorkerEvent {
    /// A display was inserted, removed, re-arranged or powered on/off.
    DisplayChanged,
    /// User toggled a display on/off from the menu.
    Toggle(u32),
    /// User selected the external monitor to auto-off (or None).
    SetBound(Option<String>),
    /// Enable (`true`) / disable (`false`) launch-at-login. `None` = no change.
    SetLaunchAtLogin(Option<bool>),
    /// NSWorkspace reports that the Mac or its displays are sleeping/waking.
    PowerSleep,
    PowerWake,
    Quit,
}

enum UserEvent {
    Snapshot(Box<Snapshot>),
    UpdateChecking,
    UpdateChecked(Result<Option<String>, String>),
    UpdateInstalling(String),
    UpdateInstalled(Result<String, String>),
    Menu(tray_icon::menu::MenuEvent),
    Tray,
    Exit,
}

#[derive(Clone)]
enum UpdateStatus {
    Checking,
    Current,
    Available(String),
    Installing(String),
    Installed(String),
    CheckFailed(String),
    InstallFailed(String),
}

impl UpdateStatus {
    fn label(&self) -> String {
        match self {
            Self::Checking => "正在检查更新…".into(),
            Self::Current => format!("已是最新版本 · v{}", env!("CARGO_PKG_VERSION")),
            Self::Available(version) => format!("下载并安装 {version}…"),
            Self::Installing(version) => format!("正在安装 {version}…"),
            Self::Installed(version) => format!("重启以完成 {version} 更新"),
            Self::CheckFailed(error) => format!("检查更新失败：{}", short_error(error)),
            Self::InstallFailed(error) => format!("安装更新失败：{}", short_error(error)),
        }
    }
}

fn short_error(error: &str) -> String {
    let line = error.lines().next().unwrap_or("未知错误").trim();
    let mut chars = line.chars();
    let label: String = chars.by_ref().take(54).collect();
    if chars.next().is_some() {
        format!("{label}…")
    } else {
        label
    }
}

enum UpdateCommand {
    Check,
    Install(String),
}

/// Network access runs separately from the display worker and the menu loop.
/// Check at launch, then daily or when the user requests it.
fn update_worker(
    proxy: winit::event_loop::EventLoopProxy<UserEvent>,
    rx: mpsc::Receiver<UpdateCommand>,
) {
    let mut command = UpdateCommand::Check;
    loop {
        match command {
            UpdateCommand::Check => {
                let _ = proxy.send_event(UserEvent::UpdateChecking);
                let result = updates::check_latest();
                if let Err(error) = &result {
                    eprintln!("update check failed: {error}");
                }
                let _ = proxy.send_event(UserEvent::UpdateChecked(result));
            }
            UpdateCommand::Install(tag) => {
                let _ = proxy.send_event(UserEvent::UpdateInstalling(tag.clone()));
                let result = updates::install(&tag);
                if let Err(error) = &result {
                    eprintln!("update install failed: {error}");
                }
                let _ = proxy.send_event(UserEvent::UpdateInstalled(result));
            }
        }
        command = match rx.recv_timeout(Duration::from_secs(24 * 60 * 60)) {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Timeout) => UpdateCommand::Check,
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
    }
}

// ---------------------------------------------------------------------------
// Worker thread (owns the settings + the actual display control)
// ---------------------------------------------------------------------------

fn snapshot(settings: &Settings, auto_off_suppressed: bool) -> Box<Snapshot> {
    let mut list = displays::online_displays();
    // A powered-off built-in leaves the online list; re-add it so the user can
    // always toggle it back on from the menu. It must use the built-in's *stable*
    // key (not a synthetic one) so the menu's structural signature doesn't change
    // when the built-in powers on/off (which would rebuild — and crash — the menu).
    if let Some(bid) = settings.builtin_id {
        if !list.iter().any(|d| d.id == bid) {
            let key = settings
                .builtin_key
                .clone()
                .unwrap_or_else(|| format!("builtin:{bid}"));
            list.insert(
                0,
                DisplayInfo {
                    id: bid,
                    builtin: true,
                    on: false,
                    asleep: false,
                    vendor: 0,
                    model: 0,
                    serial: 0,
                    width: 0,
                    height: 0,
                    name: "Built-in Display".to_string(),
                    key,
                },
            );
        }
    }
    list.sort_by_key(|d| (if d.builtin { 0 } else { 1 }, d.id));
    Box::new(Snapshot {
        displays: list,
        bound: settings.bound_key.clone(),
        auto_off_suppressed,
        control_ok: displays::control_available(),
        // Use the cached flag (queried once at startup and refreshed only when the
        // user toggles it) — we never call into SMAppService on every snapshot.
        launch_at_login: settings.launch_at_login,
    })
}

fn needs_builtin_restore(
    builtin_on: bool,
    another_external_on: bool,
    restore_builtin: bool,
    bound_was_present: bool,
) -> bool {
    (!builtin_on && (restore_builtin || !another_external_on))
        || (bound_was_present && !another_external_on)
}

/// Prevent a wake-time CoreGraphics state transition from causing another
/// private display reconfiguration. A cable pull still restores the built-in
/// through the normal five-second absence path.
#[derive(Default)]
struct WakeGuard {
    sleeping: bool,
    suppress_auto_off: bool,
    grace_until: Option<Instant>,
    absent_since: Option<Instant>,
}

impl WakeGuard {
    fn sleep(&mut self) {
        self.sleeping = true;
        self.suppress_auto_off = true;
        self.grace_until = None;
        self.absent_since = None;
    }

    fn wake(&mut self, now: Instant) {
        self.sleeping = false;
        self.suppress_auto_off = true;
        self.grace_until = Some(now + Duration::from_secs(60));
        self.absent_since = None;
    }

    fn reset(&mut self) {
        *self = Self::default();
    }

    fn observe_bound(&mut self, present: bool, now: Instant) {
        if present {
            self.absent_since = None;
            return;
        }
        let absent_since = *self.absent_since.get_or_insert(now);
        if now.duration_since(absent_since) >= Duration::from_secs(5)
            && self.grace_until.is_none_or(|deadline| now >= deadline)
        {
            // The bound monitor stayed away beyond the wake grace period: a
            // subsequent connection is a new plug, so the auto rule may run.
            self.suppress_auto_off = false;
            self.grace_until = None;
        }
    }
}

/// Re-evaluate the current display situation and publish a fresh snapshot.
///
/// A bound monitor must remain absent for five seconds before the built-in is
/// restored. Wake can temporarily remove it from the online list; restoring it
/// during that interval changes the external monitor's display configuration.
fn refresh(
    proxy: &winit::event_loop::EventLoopProxy<UserEvent>,
    settings: &mut Settings,
    missing_since: &mut Option<Instant>,
    bound_was_present: &mut bool,
    wake_guard: &mut WakeGuard,
) -> Option<Instant> {
    // auto = None means the user wants full manual control: do NOT touch the built-in
    // at all (no auto-off, no auto-restore). Otherwise the app would fight the user —
    // e.g. re-light the built-in after they closed the lid. Only in auto (bound)
    // mode do we manage the built-in.
    if settings.bound_key.is_none() {
        *missing_since = None;
        *bound_was_present = false;
        wake_guard.reset();
        let _ = proxy.send_event(UserEvent::Snapshot(snapshot(settings, false)));
        return None;
    }

    if wake_guard.sleeping {
        return None;
    }

    let before_id = settings.builtin_id;
    let before_key = settings.builtin_key.clone();

    let displays_now: Vec<DisplayInfo> = displays::online_displays();
    if let Some(b) = displays_now.iter().find(|d| d.builtin) {
        settings.builtin_id = Some(b.id);
        settings.builtin_key = Some(b.key.clone());
    }
    let bound_present = displays_now
        .iter()
        .any(|d| !d.builtin && Some(d.key.as_str()) == settings.bound_key.as_deref());
    wake_guard.observe_bound(bound_present, Instant::now());
    let mut retry_at = None;
    if bound_present {
        *missing_since = None;
        *bound_was_present = true;
        // Do not run the auto rule against a transiently missing external during
        // wake: its restore path performs a permanent display configuration.
        if !wake_guard.suppress_auto_off {
            auto::apply_auto_with_list(settings, &displays_now);
        }
    } else {
        // Only an online built-in has a trustworthy sleep state. A display
        // disabled by this app is absent from the online list and querying its
        // cached id can report "asleep" even after a real cable unplug.
        let builtin_asleep = displays_now.iter().any(|d| d.builtin && d.asleep);
        let builtin_on = displays_now.iter().any(|d| d.builtin && d.on);
        let another_external_on = displays_now.iter().any(|d| !d.builtin && d.on);
        // CoreGraphics can report the built-in as active while its panel is
        // still dark after unplug. Keep the old transition-based forced restore.
        let needs_restore = needs_builtin_restore(
            builtin_on,
            another_external_on,
            settings.restore_builtin,
            *bound_was_present,
        );
        if builtin_asleep || !needs_restore {
            *missing_since = None;
            if builtin_on {
                *bound_was_present = false;
            }
        } else {
            // A monitor can briefly disappear from CGGetOnlineDisplayList during
            // wake. Only restore the built-in if the bound monitor stays absent.
            let since = *missing_since.get_or_insert_with(Instant::now);
            let deadline = since + Duration::from_secs(5);
            if Instant::now() >= deadline {
                displays::recover_builtin_with_id(settings.builtin_id);
                *missing_since = None;
                *bound_was_present = false;
            } else {
                retry_at = Some(deadline);
            }
        }
    }

    if settings.builtin_id != before_id || settings.builtin_key != before_key {
        let _ = settings.save();
    }
    let _ = proxy.send_event(UserEvent::Snapshot(snapshot(
        settings,
        wake_guard.suppress_auto_off,
    )));
    // A cable pull does not always emit a CoreGraphics callback. Check again
    // while auto mode is active so an unplug cannot leave the built-in dark.
    let watchdog = Instant::now() + Duration::from_secs(2);
    Some(retry_at.map_or(watchdog, |at| at.min(watchdog)))
}

#[cfg(test)]
mod worker_tests {
    use super::{needs_builtin_restore, observe_power_events, WakeGuard, WorkerEvent};
    use objc2_app_kit::{
        NSWorkspace, NSWorkspaceDidWakeNotification, NSWorkspaceWillSleepNotification,
    };
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    #[test]
    fn unplug_restores_even_if_coregraphics_still_reports_builtin_active() {
        assert!(needs_builtin_restore(true, false, true, true));
    }

    #[test]
    fn missing_bound_monitor_restores_disabled_builtin() {
        assert!(needs_builtin_restore(false, false, true, false));
    }

    #[test]
    fn another_external_keeps_manual_restore_setting() {
        assert!(!needs_builtin_restore(false, true, false, true));
    }

    #[test]
    fn wake_does_not_reapply_auto_off_when_monitor_returns() {
        let now = Instant::now();
        let mut guard = WakeGuard::default();
        guard.sleep();
        guard.wake(now);
        guard.observe_bound(false, now + Duration::from_secs(2));
        guard.observe_bound(true, now + Duration::from_secs(12));
        guard.observe_bound(true, now + Duration::from_secs(70));
        assert!(guard.suppress_auto_off);
    }

    #[test]
    fn confirmed_unplug_rearms_auto_off_after_wake() {
        let now = Instant::now();
        let mut guard = WakeGuard::default();
        guard.wake(now);
        guard.observe_bound(false, now + Duration::from_secs(2));
        guard.observe_bound(false, now + Duration::from_secs(7));
        assert!(guard.suppress_auto_off);
        guard.observe_bound(false, now + Duration::from_secs(61));
        assert!(!guard.suppress_auto_off);
    }

    #[test]
    fn brief_unplug_after_wake_does_not_rearm_auto_off() {
        let now = Instant::now();
        let mut guard = WakeGuard::default();
        guard.wake(now);
        guard.observe_bound(true, now + Duration::from_secs(61));
        guard.observe_bound(false, now + Duration::from_secs(62));
        guard.observe_bound(true, now + Duration::from_secs(64));
        assert!(guard.suppress_auto_off);
    }

    #[test]
    fn workspace_power_notifications_reach_worker() {
        let (tx, rx) = mpsc::channel();
        let _observers = observe_power_events(tx);
        let center = NSWorkspace::sharedWorkspace().notificationCenter();
        unsafe {
            center.postNotificationName_object(NSWorkspaceWillSleepNotification, None);
        }
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            WorkerEvent::PowerSleep
        ));
        unsafe {
            center.postNotificationName_object(NSWorkspaceDidWakeNotification, None);
        }
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            WorkerEvent::PowerWake
        ));
    }
}

fn worker(proxy: winit::event_loop::EventLoopProxy<UserEvent>, rx: mpsc::Receiver<WorkerEvent>) {
    let mut settings = Settings::load();

    // Sync the launch-at-login flag from the OS once at startup (we don't poll it).
    let os_launch = launch::check_reg_status();
    if settings.launch_at_login != os_launch {
        settings.launch_at_login = os_launch;
        let _ = settings.save();
    }

    let mut missing_since = None;
    let mut bound_was_present = false;
    let mut wake_guard = WakeGuard::default();
    let mut pending_refresh: Option<Instant> = None;

    // Coalesce the per-display callbacks into one settled snapshot. A single
    // reconfiguration sends multiple callbacks, particularly during sleep/wake.
    loop {
        let event = if let Some(deadline) = pending_refresh {
            rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        } else {
            rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected)
        };
        match event {
            Ok(ev) => match ev {
                WorkerEvent::DisplayChanged => {
                    // Start the absence confirmation after the latest completed
                    // reconfiguration, including one delivered on system wake.
                    missing_since = None;
                    pending_refresh = Some(Instant::now() + Duration::from_millis(700));
                    continue;
                }
                WorkerEvent::PowerSleep => {
                    wake_guard.sleep();
                    pending_refresh = None;
                    continue;
                }
                WorkerEvent::PowerWake => {
                    wake_guard.wake(Instant::now());
                    missing_since = None;
                    pending_refresh = Some(Instant::now() + Duration::from_millis(700));
                    continue;
                }
                WorkerEvent::Toggle(id) => {
                    // A manual toggle is an explicit user override: disable the
                    // auto-off rule so it doesn't immediately fight the change.
                    if settings.bound_key.is_some() {
                        settings.bound_key = None;
                        let _ = settings.save();
                    }
                    let _ = displays::toggle_safe(id);
                }
                WorkerEvent::SetBound(key) => {
                    wake_guard.reset();
                    let going_to_manual = key.is_none();
                    settings.bound_key = key;
                    let _ = settings.save();
                    // Switching to manual (None) ends the auto-off: if the built-in
                    // was disabled by the auto rule, bring it back on now. Otherwise
                    // it would stay off (a fully-disabled built-in doesn't even
                    // recover on lid open, since it's software-disabled, not asleep).
                    if going_to_manual {
                        displays::recover_builtin_with_id(settings.builtin_id);
                    }
                }
                WorkerEvent::SetLaunchAtLogin(enabled) => {
                    if let Some(enabled) = enabled {
                        let result = if enabled {
                            launch::register()
                        } else {
                            launch::unregister()
                        };
                        if result.is_ok() {
                            settings.launch_at_login = enabled;
                            let _ = settings.save();
                        }
                    }
                }
                WorkerEvent::Quit => {
                    // Restore the built-in display so the user isn't left dark.
                    displays::recover_builtin_with_id(settings.builtin_id);
                    let _ = proxy.send_event(UserEvent::Exit);
                    return;
                }
            },
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }

        pending_refresh = refresh(
            &proxy,
            &mut settings,
            &mut missing_since,
            &mut bound_was_present,
            &mut wake_guard,
        );
    }
}

// ---------------------------------------------------------------------------
// Menu construction
// ---------------------------------------------------------------------------

fn dims(d: &DisplayInfo) -> String {
    if d.width > 0 && d.height > 0 {
        format!(" · {} × {}", d.width, d.height)
    } else {
        String::new()
    }
}

fn toggle_label(d: &DisplayInfo) -> String {
    format!(
        "{}{}  ·  {}",
        d.name,
        dims(d),
        if d.on { "已开启" } else { "已关闭" }
    )
}

fn auto_summary(snap: &Snapshot) -> String {
    if snap.auto_off_suppressed && snap.displays.iter().any(|d| d.builtin && d.on) {
        return "自动关闭内建屏：唤醒后暂停（保护 HDR）".into();
    }
    match snap.bound.as_deref() {
        None => "自动关闭内建屏：未启用".into(),
        Some(key) => match snap.displays.iter().find(|d| d.key == key) {
            Some(display) => format!("自动关闭内建屏：{}", display.name),
            None => "自动关闭内建屏：等待已绑定显示器".into(),
        },
    }
}

/// The set of menu items currently shown. **Structural identity** (which displays
/// are present, in order) is tracked via `signature`.
///
/// - While the signature is unchanged (a display on/off state or bound selection
///   changed) we mutate the existing items' checked/label **in place** — no rebuild,
///   so an open menu is never torn down and clicking stays safe (muda #173).
/// - When the signature changes (a display was plugged in or unplugged) we signal the
///   caller to **rebuild** the whole menu. This happens only on real hot-plug (rare,
///   and almost never while you're clicking it).
struct MenuState {
    menu: Menu,
    auto_summary: MenuItem,
    bind_none: CheckMenuItem,
    bind_items: Vec<(String, CheckMenuItem)>,
    toggle_items: Vec<(u32, CheckMenuItem)>,
    start_login: CheckMenuItem,
    check_update: MenuItem,
    update_status: MenuItem,
    signature: String,
}

impl MenuState {
    fn build(snap: &Snapshot, update: &UpdateStatus) -> MenuState {
        let menu = Menu::new();

        let title = MenuItem::with_id(
            "app-title",
            format!("lidup  ·  v{}", env!("CARGO_PKG_VERSION")),
            false,
            None,
        );
        let _ = menu.append(&title);
        let auto_summary = MenuItem::with_id("auto-summary", auto_summary(snap), false, None);
        let _ = menu.append(&auto_summary);
        let _ = menu.append(&PredefinedMenuItem::separator());

        let displays_heading = MenuItem::with_id("displays-heading", "显示器", false, None);
        let _ = menu.append(&displays_heading);
        let mut toggle_items = Vec::new();
        for d in &snap.displays {
            let item = CheckMenuItem::with_id(
                "toggle:".to_string() + &d.id.to_string(),
                toggle_label(d),
                true,
                d.on,
                None,
            );
            let _ = menu.append(&item);
            toggle_items.push((d.id, item));
        }
        if !snap.control_ok {
            let note = MenuItem::with_id("note", "当前无法控制显示器", false, None);
            let _ = menu.append(&note);
        }
        let _ = menu.append(&PredefinedMenuItem::separator());

        let bind = Submenu::new("自动关闭内建屏", true);
        let bind_none =
            CheckMenuItem::with_id("bind:none", "不自动关闭", true, snap.bound.is_none(), None);
        let _ = bind.append(&bind_none);
        let mut bind_items = Vec::new();
        for d in &snap.displays {
            if d.builtin {
                continue;
            }
            let item = CheckMenuItem::with_id(
                "bind:".to_string() + &d.key,
                format!("{}{}", d.name, dims(d)),
                true,
                snap.bound.as_deref() == Some(d.key.as_str()),
                None,
            );
            let _ = bind.append(&item);
            bind_items.push((d.key.clone(), item));
        }
        let _ = menu.append(&bind);
        let start_login = CheckMenuItem::with_id(
            "start-login",
            "登录时启动",
            true,
            snap.launch_at_login,
            None,
        );
        let _ = menu.append(&start_login);
        let _ = menu.append(&PredefinedMenuItem::separator());
        let check_update = MenuItem::with_id(
            "check-update",
            "检查更新…",
            !matches!(
                update,
                UpdateStatus::Checking | UpdateStatus::Installing(_) | UpdateStatus::Installed(_)
            ),
            None,
        );
        let _ = menu.append(&check_update);
        let update_status = MenuItem::with_id(
            "update-action",
            update.label(),
            matches!(
                update,
                UpdateStatus::Available(_) | UpdateStatus::Installed(_)
            ),
            None,
        );
        let _ = menu.append(&update_status);
        let _ = menu.append(&PredefinedMenuItem::separator());
        let quit = MenuItem::with_id("quit", "退出 lidup", true, None);
        let _ = menu.append(&quit);

        MenuState {
            menu,
            auto_summary,
            bind_none,
            bind_items,
            toggle_items,
            start_login,
            check_update,
            update_status,
            signature: structural_signature(snap),
        }
    }

    /// Update item state against a fresh snapshot. Returns `true` only when the
    /// structural identity changed (a display was plugged in/unplugged), meaning the
    /// caller must rebuild the menu. Otherwise updates items in place.
    fn update(&mut self, snap: &Snapshot) -> bool {
        let sig = structural_signature(snap);
        if sig != self.signature {
            return true;
        }
        self.signature = sig;

        self.auto_summary.set_text(auto_summary(snap));
        self.bind_none.set_checked(snap.bound.is_none());
        for (key, item) in &self.bind_items {
            item.set_checked(snap.bound.as_deref() == Some(key.as_str()));
        }
        for (id, item) in &self.toggle_items {
            if let Some(d) = snap.displays.iter().find(|d| d.id == *id) {
                if item.is_checked() != d.on || item.text() != toggle_label(d) {
                    item.set_checked(d.on);
                    item.set_text(toggle_label(d));
                }
            }
        }
        self.start_login.set_checked(snap.launch_at_login);
        false
    }

    fn set_update_status(&self, status: &UpdateStatus) {
        self.check_update.set_enabled(!matches!(
            status,
            UpdateStatus::Checking | UpdateStatus::Installing(_) | UpdateStatus::Installed(_)
        ));
        self.update_status.set_text(status.label());
        self.update_status.set_enabled(matches!(
            status,
            UpdateStatus::Available(_) | UpdateStatus::Installed(_)
        ));
    }
}

/// Stable identity of the display list: the ordered set of display ids + keys.
/// When this changes, a display was truly plugged in / unplugged. The bound
/// selection is *not* part of it (rebinding only flips a checkmark, handled in place).
fn structural_signature(snap: &Snapshot) -> String {
    let mut s = String::new();
    for d in &snap.displays {
        s.push_str(&format!("{},{},{};", d.id, d.builtin as u8, d.key));
    }
    s
}

// ---------------------------------------------------------------------------
// winit ApplicationHandler
// ---------------------------------------------------------------------------

struct App {
    cmd_tx: mpsc::Sender<WorkerEvent>,
    update_tx: mpsc::Sender<UpdateCommand>,
    update_status: UpdateStatus,
    tray: Option<TrayIcon>,
    menu: Option<MenuState>,
    snapshot: Option<Snapshot>,
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {
        if self.tray.is_none() {
            // Build the initial menu from the same snapshot logic the worker uses,
            // so the first worker-sent snapshot matches and we never rebuild the
            // tray menu right after startup.
            let settings = Settings::load();
            let snap = *snapshot(&settings, false);
            let ms = MenuState::build(&snap, &self.update_status);
            let icon = tray_image();
            match TrayIconBuilderCompat::build(icon, ms.menu.clone()) {
                Ok(t) => self.tray = Some(t),
                Err(e) => eprintln!("failed to create tray icon: {e}"),
            }
            self.menu = Some(ms);
            self.snapshot = Some(snap);
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Snapshot(snap) => {
                let snap = *snap;
                let rebuild = match self.menu.as_mut() {
                    // In-place update; returns true only when the display list really
                    // changed (hot-plug), which is when we rebuild the menu.
                    Some(ms) => ms.update(&snap),
                    None => {
                        let first = MenuState::build(&snap, &self.update_status);
                        if let Some(tray) = &self.tray {
                            tray.set_menu(Some(Box::new(first.menu.clone())));
                        }
                        self.menu = Some(first);
                        false
                    }
                };
                if rebuild {
                    let fresh = MenuState::build(&snap, &self.update_status);
                    if let Some(tray) = &self.tray {
                        tray.set_menu(Some(Box::new(fresh.menu.clone())));
                    }
                    self.menu = Some(fresh);
                }
                self.snapshot = Some(snap);
            }
            UserEvent::UpdateChecking => {
                self.update_status = UpdateStatus::Checking;
                if let Some(menu) = &self.menu {
                    menu.set_update_status(&self.update_status);
                }
            }
            UserEvent::UpdateChecked(result) => {
                self.update_status = match result {
                    Ok(Some(version)) => UpdateStatus::Available(version),
                    Ok(None) => UpdateStatus::Current,
                    Err(error) => UpdateStatus::CheckFailed(error),
                };
                if let Some(menu) = &self.menu {
                    menu.set_update_status(&self.update_status);
                }
            }
            UserEvent::UpdateInstalling(tag) => {
                self.update_status = UpdateStatus::Installing(tag);
                if let Some(menu) = &self.menu {
                    menu.set_update_status(&self.update_status);
                }
            }
            UserEvent::UpdateInstalled(result) => {
                self.update_status = match result {
                    Ok(tag) => UpdateStatus::Installed(tag),
                    Err(error) => UpdateStatus::InstallFailed(error),
                };
                if let Some(menu) = &self.menu {
                    menu.set_update_status(&self.update_status);
                }
            }
            UserEvent::Menu(ev) => {
                let id = ev.id();
                if id == "quit" {
                    let _ = self.cmd_tx.send(WorkerEvent::Quit);
                } else if id == "check-update" {
                    if !matches!(
                        self.update_status,
                        UpdateStatus::Checking
                            | UpdateStatus::Installing(_)
                            | UpdateStatus::Installed(_)
                    ) {
                        self.update_status = UpdateStatus::Checking;
                        if let Some(menu) = &self.menu {
                            menu.set_update_status(&self.update_status);
                        }
                        let _ = self.update_tx.send(UpdateCommand::Check);
                    }
                } else if id == "update-action" {
                    match &self.update_status {
                        UpdateStatus::Available(tag) => {
                            let tag = tag.clone();
                            self.update_status = UpdateStatus::Installing(tag.clone());
                            if let Some(menu) = &self.menu {
                                menu.set_update_status(&self.update_status);
                            }
                            let _ = self.update_tx.send(UpdateCommand::Install(tag));
                        }
                        UpdateStatus::Installed(_) => {
                            let Err(error) = self_update::restart::restart();
                            eprintln!("could not restart updated app: {error}");
                            self.update_status = UpdateStatus::InstallFailed(error.to_string());
                            if let Some(menu) = &self.menu {
                                menu.set_update_status(&self.update_status);
                            }
                        }
                        _ => {}
                    }
                } else if id == "start-login" {
                    // Toggle based on the state shown in the menu (cached), so the
                    // action matches the checkmark. No SMAppService query per click.
                    let enabled = self
                        .snapshot
                        .as_ref()
                        .map(|s| s.launch_at_login)
                        .unwrap_or(false);
                    let _ = self
                        .cmd_tx
                        .send(WorkerEvent::SetLaunchAtLogin(Some(!enabled)));
                } else if let Some(key) = id.as_ref().strip_prefix("bind:") {
                    let key = if key == "none" {
                        None
                    } else {
                        Some(key.to_string())
                    };
                    let _ = self.cmd_tx.send(WorkerEvent::SetBound(key));
                } else if let Some(sid) = id.as_ref().strip_prefix("toggle:") {
                    if let Ok(did) = sid.parse::<u32>() {
                        let _ = self.cmd_tx.send(WorkerEvent::Toggle(did));
                    }
                }
            }
            UserEvent::Tray => {}
            UserEvent::Exit => {
                event_loop.exit();
            }
        }
    }

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _id: WindowId,
        _e: winit::event::WindowEvent,
    ) {
    }
}

// ---------------------------------------------------------------------------
// Tray icon helpers
// ---------------------------------------------------------------------------

struct TrayIconBuilderCompat;
impl TrayIconBuilderCompat {
    fn build(icon: tray_icon::Icon, menu: Menu) -> Result<TrayIcon, tray_icon::Error> {
        tray_icon::TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("lidup · 显示器管理")
            .with_icon(icon)
            .with_icon_as_template(true)
            .build()
    }
}

/// A crisp monitor outline with a stand. macOS tints the template for light/dark menus.
fn tray_image() -> tray_icon::Icon {
    let (w, h) = (24u32, 24u32);
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let mut covered = 0u8;
            for sub_y in 0..4 {
                for sub_x in 0..4 {
                    let px = x as f64 + (sub_x as f64 + 0.5) / 4.0;
                    let py = y as f64 + (sub_y as f64 + 0.5) / 4.0;
                    let outer = rounded_rect_contains(px, py, 12.0, 9.5, 10.0, 7.0, 2.0);
                    let inner = rounded_rect_contains(px, py, 12.0, 9.2, 8.1, 4.9, 0.7);
                    let stem = (11.0..=13.0).contains(&px) && (16.4..=20.2).contains(&py);
                    let foot = rounded_rect_contains(px, py, 12.0, 20.8, 4.7, 0.8, 0.8);
                    covered += u8::from((outer && !inner) || stem || foot);
                }
            }
            rgba[((y * w + x) * 4 + 3) as usize] = ((covered as u16 * 255) / 16) as u8;
        }
    }
    tray_icon::Icon::from_rgba(rgba, w, h).expect("valid rgba icon")
}

fn rounded_rect_contains(x: f64, y: f64, cx: f64, cy: f64, hx: f64, hy: f64, r: f64) -> bool {
    let qx = (x - cx).abs() - (hx - r);
    let qy = (y - cy).abs() - (hy - r);
    qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) <= r
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

struct PowerObservers {
    center: Retained<NSNotificationCenter>,
    tokens: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
}

impl Drop for PowerObservers {
    fn drop(&mut self) {
        for token in &self.tokens {
            let protocol: &ProtocolObject<dyn NSObjectProtocol> = token;
            let observer: &objc2::runtime::AnyObject = protocol.as_ref();
            unsafe { self.center.removeObserver(observer) };
        }
    }
}

/// NSWorkspace notifications identify sleep/wake explicitly. Display-change
/// callbacks alone cannot distinguish a waking monitor from a cable pull.
fn observe_power_events(tx: mpsc::Sender<WorkerEvent>) -> PowerObservers {
    let center = NSWorkspace::sharedWorkspace().notificationCenter();
    let notifications = unsafe {
        [
            (NSWorkspaceWillSleepNotification, true),
            (NSWorkspaceScreensDidSleepNotification, true),
            (NSWorkspaceDidWakeNotification, false),
            (NSWorkspaceScreensDidWakeNotification, false),
        ]
    };
    let tokens = notifications
        .into_iter()
        .map(|(name, sleeping)| {
            let tx = tx.clone();
            let block = RcBlock::new(move |_notification: std::ptr::NonNull<NSNotification>| {
                let event = if sleeping {
                    WorkerEvent::PowerSleep
                } else {
                    WorkerEvent::PowerWake
                };
                let _ = tx.send(event);
            });
            unsafe {
                center.addObserverForName_object_queue_usingBlock(Some(name), None, None, &block)
            }
        })
        .collect();
    PowerObservers { center, tokens }
}

/// If lidup is terminated (SIGTERM/SIGINT/SIGHUP — e.g. the OS kills the login
/// item when it is unticked in System Settings, or the app is quit externally),
/// restore the built-in display so the user is never left with a dark screen.
/// The handle runs on its own thread (event-driven via a self-pipe, no polling).
fn install_exit_guard() {
    use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;
    std::thread::spawn(move || {
        if let Ok(mut signals) = Signals::new([SIGHUP, SIGINT, SIGTERM]) {
            if signals.forever().next().is_some() {
                displays::recover_builtin();
                std::process::exit(0);
            }
        }
    });
}

/// Hide the Dock icon so lidup runs purely in the menu bar. Must run on the main
/// thread before the app's event loop starts.
fn set_menu_bar_only() {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    if let Some(mtm) = MainThreadMarker::new() {
        let app = NSApplication::sharedApplication(mtm);
        let _ = app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    set_menu_bar_only();

    // Set up the tray/menu event handlers once, forwarding into the winit loop so
    // the loop wakes up on every menu/tray interaction.
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();

    let tray_proxy = proxy.clone();
    tray_icon::TrayIconEvent::set_event_handler(Some(move |_event| {
        let _ = tray_proxy.send_event(UserEvent::Tray);
    }));
    let menu_proxy = proxy.clone();
    tray_icon::menu::MenuEvent::set_event_handler(Some(move |event| {
        let _ = menu_proxy.send_event(UserEvent::Menu(event));
    }));

    // Event-driven worker: wake it on display changes and on manual actions.
    let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerEvent>();
    let _power_observers = observe_power_events(cmd_tx.clone());

    // Fire whenever a display is inserted/removed/powered — forwards to the worker.
    let reconfig_tx = cmd_tx.clone();
    let _ = displays::register_reconfig_handler(move || {
        let _ = reconfig_tx.send(WorkerEvent::DisplayChanged);
    });

    let update_proxy = proxy.clone();
    std::thread::Builder::new()
        .name("lidup-worker".into())
        .spawn(move || worker(proxy, cmd_rx))?;

    let (update_tx, update_rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("lidup-updates".into())
        .spawn(move || update_worker(update_proxy, update_rx))?;

    // Evaluate once on launch (applies the auto rule to the current state and
    // publishes the first snapshot) so the menu is populated immediately.
    let _ = cmd_tx.send(WorkerEvent::DisplayChanged);

    // If this process is terminated (e.g. the OS kills the login item when it is
    // unticked in System Settings), make sure the built-in display comes back on.
    install_exit_guard();

    let mut app = App {
        cmd_tx,
        update_tx,
        update_status: UpdateStatus::Checking,
        tray: None,
        menu: None,
        snapshot: None,
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}
