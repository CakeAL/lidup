//! lidup — a macOS menu-bar app that turns off the built-in display when a chosen
//! external display is connected, and provides per-display on/off toggles.
//!
//! The heavy lifting (enumeration + on/off via the private SkyLight API) lives in
//! `displays`. A background worker thread periodically re-checks state, applies the
//! auto-off rule and pushes a snapshot to the winit event loop, which renders the
//! menu-bar menu. Menu item ids are turned into commands on the worker thread.

use lidup::{auto, config, displays, launch, updates};

use config::Settings;
use displays::DisplayInfo;
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
            Self::Checking => "Checking for updates…".into(),
            Self::Current => format!("Up to date (v{})", env!("CARGO_PKG_VERSION")),
            Self::Available(version) => format!("Install {version}…"),
            Self::Installing(version) => format!("Installing {version}…"),
            Self::Installed(version) => format!("Restart to finish {version}"),
            Self::CheckFailed(error) => format!("Update check failed: {}", short_error(error)),
            Self::InstallFailed(error) => format!("Update failed: {}", short_error(error)),
        }
    }
}

fn short_error(error: &str) -> String {
    error
        .lines()
        .next()
        .unwrap_or("unknown error")
        .chars()
        .take(90)
        .collect()
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

fn snapshot(settings: &Settings) -> Box<Snapshot> {
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
) -> Option<Instant> {
    // auto = None means the user wants full manual control: do NOT touch the built-in
    // at all (no auto-off, no auto-restore). Otherwise the app would fight the user —
    // e.g. re-light the built-in after they closed the lid. Only in auto (bound)
    // mode do we manage the built-in.
    if settings.bound_key.is_none() {
        *missing_since = None;
        *bound_was_present = false;
        let _ = proxy.send_event(UserEvent::Snapshot(snapshot(settings)));
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
    let mut retry_at = None;
    if bound_present {
        *missing_since = None;
        *bound_was_present = true;
        // Do not run the auto rule against a transiently missing external during
        // wake: its restore path performs a permanent display configuration.
        auto::apply_auto_with_list(settings, &displays_now);
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
    let _ = proxy.send_event(UserEvent::Snapshot(snapshot(settings)));
    // A cable pull does not always emit a CoreGraphics callback. Check again
    // while auto mode is active so an unplug cannot leave the built-in dark.
    let watchdog = Instant::now() + Duration::from_secs(2);
    Some(retry_at.map_or(watchdog, |at| at.min(watchdog)))
}

#[cfg(test)]
mod worker_tests {
    use super::needs_builtin_restore;

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
        );
    }
}

// ---------------------------------------------------------------------------
// Menu construction
// ---------------------------------------------------------------------------

fn dims(d: &DisplayInfo) -> String {
    if d.width > 0 && d.height > 0 {
        format!(" [{}x{}]", d.width, d.height)
    } else {
        String::new()
    }
}

fn toggle_label(d: &DisplayInfo) -> String {
    format!(
        "{}{}  {}",
        d.name,
        dims(d),
        if d.on { "▶ on" } else { "■ off" }
    )
}

/// The set of menu items currently shown. **Structural identity** (which displays
/// are present, in order, plus the bound selection) is tracked via `signature`.
///
/// - While the signature is unchanged (a display on/off state or bound selection
///   changed) we mutate the existing items' checked/label **in place** — no rebuild,
///   so an open menu is never torn down and clicking stays safe (muda #173).
/// - When the signature changes (a display was plugged in or unplugged) we signal the
///   caller to **rebuild** the whole menu. This happens only on real hot-plug (rare,
///   and almost never while you're clicking it).
struct MenuState {
    menu: Menu,
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

        let bind = Submenu::new("Auto-off built-in when connected", true);
        let bind_none = CheckMenuItem::with_id(
            "bind:none",
            "None (manual only)",
            true,
            snap.bound.is_none(),
            None,
        );
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
        let _ = menu.append(&PredefinedMenuItem::separator());

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

        let _ = menu.append(&PredefinedMenuItem::separator());
        if !snap.control_ok {
            let note = MenuItem::with_id(
                "note",
                "Display control unavailable on this Mac",
                false,
                None,
            );
            let _ = menu.append(&note);
            let _ = menu.append(&PredefinedMenuItem::separator());
        }
        let start_login = CheckMenuItem::with_id(
            "start-login",
            "Start at Login",
            true,
            snap.launch_at_login,
            None,
        );
        let _ = menu.append(&start_login);
        let _ = menu.append(&PredefinedMenuItem::separator());
        let check_update = MenuItem::with_id(
            "check-update",
            "Check for Updates…",
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
        let quit = MenuItem::with_id("quit", "Quit lidup", true, None);
        let _ = menu.append(&quit);

        MenuState {
            menu,
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
            let snap = *snapshot(&settings);
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
            .with_tooltip("lidup — display control")
            .with_icon(icon)
            .with_icon_as_template(true)
            .build()
    }
}

/// A simple 24x24 black glyph (template so macOS auto-tints it for the menu bar).
fn tray_image() -> tray_icon::Icon {
    let (w, h) = (24u32, 24u32);
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    let cx = 12.0;
    let cy = 12.0;
    let r = 9.5;
    for y in 0..h {
        for x in 0..w {
            let dx = (x as f64) + 0.5 - cx;
            let dy = (y as f64) + 0.5 - cy;
            let dist = (dx * dx + dy * dy).sqrt();
            if dist <= r {
                let i = ((y * w + x) * 4) as usize;
                // filled disc with a hollow center ring for a "screen" look
                rgba[i + 0] = 0;
                rgba[i + 1] = 0;
                rgba[i + 2] = 0;
                rgba[i + 3] = if dist > r - 2.5 { 255 } else { 190 };
            }
        }
    }
    tray_icon::Icon::from_rgba(rgba, w, h).expect("valid rgba icon")
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

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
