//! lidup — a macOS menu-bar app that turns off the built-in display when a chosen
//! external display is connected, and provides per-display on/off toggles.
//!
//! The heavy lifting (enumeration + on/off via the private SkyLight API) lives in
//! `displays`. A background worker thread periodically re-checks state, applies the
//! auto-off rule and pushes a snapshot to the winit event loop, which renders the
//! menu-bar menu. Menu item ids are turned into commands on the worker thread.

use lidup::{auto, config, displays, launch};

use config::Settings;
use displays::DisplayInfo;
use std::sync::mpsc;
use std::time::Duration;
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

/// Events that wake the worker thread. The worker is **event-driven**: it blocks
/// until either the display configuration changes (a display is plugged in or
/// unplugged) or the user performs a menu action. There is no periodic polling.
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
    Menu(tray_icon::menu::MenuEvent),
    Tray,
    Exit,
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

/// Re-evaluate the current display situation and publish a fresh snapshot.
///
/// Recovery of the built-in display is driven by detecting that the **number of
/// external displays decreased** (an external was unplugged) while nothing is lit —
/// NOT by trusting a system callback, which doesn't always fire on a physical cable
/// pull. This is the same idea BetterDisplay uses. Because a low-frequency watchdog
/// re-checks the count, it recovers even mid-transition and never leaves the built-in
/// stuck off. Crucially, a **manual toggle doesn't change the external count**, so it
/// is never fought by this — no flicker when you turn the built-in off yourself.
fn refresh(
    proxy: &winit::event_loop::EventLoopProxy<UserEvent>,
    settings: &mut Settings,
    last_external: &mut Option<usize>,
) {
    let before_id = settings.builtin_id;
    let before_key = settings.builtin_key.clone();

    // How many external displays are plugged in right now?
    let current_external = displays::online_displays()
        .iter()
        .filter(|d| !d.builtin)
        .count();

    // Was an external unplugged since the last check? On the first check we don't
    // know (None), so only recover on a real 1 -> 0 (or higher -> lower) transition.
    let external_removed = matches!(last_external, Some(prev) if *prev > current_external);
    if let Some(existing) = last_external.as_mut() {
        *existing = current_external;
    } else {
        *last_external = Some(current_external);
    }

    auto::apply_auto(settings);

    // Safety: an external was unplugged and everything went dark — wake the built-in.
    if external_removed && displays::active_count() == 0 {
        displays::recover_builtin();
    }

    if settings.builtin_id != before_id || settings.builtin_key != before_key {
        let _ = settings.save();
    }
    let _ = proxy.send_event(UserEvent::Snapshot(snapshot(settings)));
}

fn worker(proxy: winit::event_loop::EventLoopProxy<UserEvent>, rx: mpsc::Receiver<WorkerEvent>) {
    let mut settings = Settings::load();

    // Sync the launch-at-login flag from the OS once at startup (we don't poll it).
    let os_launch = launch::check_reg_status();
    if settings.launch_at_login != os_launch {
        settings.launch_at_login = os_launch;
        let _ = settings.save();
    }

    // Track how many external displays are plugged in, so we can detect a removal
    // even without a reliable system callback. `None` = first evaluation, no baseline.
    let mut last_external: Option<usize> = None;

    // Event-driven from display changes / user actions, PLUS a low-frequency
    // watchdog (every 2s) that re-checks the external count. Physical cable unplugs
    // don't always fire a CoreGraphics callback, so the watchdog is what guarantees
    // the external-removal is noticed and the built-in recovers.
    loop {
        match rx.recv_timeout(Duration::from_millis(2000)) {
            Ok(ev) => match ev {
                WorkerEvent::DisplayChanged => {
                    // A display was inserted/removed/powered; re-evaluate.
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
                    settings.bound_key = key;
                    let _ = settings.save();
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
                    displays::recover_builtin();
                    let _ = proxy.send_event(UserEvent::Exit);
                    return;
                }
            },
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Watchdog: re-check the external count and recover the built-in if
                // an external was unplugged and nothing is lit. Mostly a no-op.
                refresh(&proxy, &mut settings, &mut last_external);
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }

        refresh(&proxy, &mut settings, &mut last_external);
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
    signature: String,
}

impl MenuState {
    fn build(snap: &Snapshot) -> MenuState {
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
        let quit = MenuItem::with_id("quit", "Quit lidup", true, None);
        let _ = menu.append(&quit);

        MenuState {
            menu,
            bind_none,
            bind_items,
            toggle_items,
            start_login,
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
            let ms = MenuState::build(&snap);
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
                        let first = MenuState::build(&snap);
                        if let Some(tray) = &self.tray {
                            tray.set_menu(Some(Box::new(first.menu.clone())));
                        }
                        self.menu = Some(first);
                        false
                    }
                };
                if rebuild {
                    let fresh = MenuState::build(&snap);
                    if let Some(tray) = &self.tray {
                        tray.set_menu(Some(Box::new(fresh.menu.clone())));
                    }
                    self.menu = Some(fresh);
                }
                self.snapshot = Some(snap);
            }
            UserEvent::Menu(ev) => {
                let id = ev.id();
                if id == "quit" {
                    let _ = self.cmd_tx.send(WorkerEvent::Quit);
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

/// Look for a built-in display id in the online list, falling back to the cached
/// id, then to scanning the low display ids (only used for `recover`).
fn find_builtin_id(settings: &Settings) -> Option<u32> {
    if let Some(b) = displays::builtin_display() {
        return Some(b.id);
    }
    if let Some(id) = settings.builtin_id {
        return Some(id);
    }
    // Fallback scan for the internal panel id.
    for id in 1..=16u32 {
        if displays::is_on(id) || displays::builtin_probe(id) {
            return Some(id);
        }
    }
    None
}

fn cli() -> bool {
    let args: Vec<String> = std::env::args().collect();
    let Some(cmd) = args.get(1) else {
        return false; // no subcommand -> run the tray app
    };
    let settings = Settings::load();
    match cmd.as_str() {
        "list" => {
            println!(
                "display control available: {}",
                displays::control_available()
            );
            println!("bound external key: {:?}", settings.bound_key);
            println!("launch at login: {}", launch::check_reg_status());
            for d in displays::online_displays() {
                let role = if d.builtin { "built-in" } else { "external" };
                println!(
                    "  id={:>3} {role:<8} on={} vendor={:04x} model={:04x} {}x{} key={}",
                    d.id, d.on, d.vendor, d.model, d.width, d.height, d.key
                );
            }
            true
        }
        "autostart" => {
            // `autostart` shows status; `autostart on|off` enables/disables.
            match args.get(2).map(String::as_str) {
                Some("on") => match launch::register() {
                    Ok(()) => println!("launch-at-login enabled"),
                    Err(e) => println!("failed to enable launch-at-login: {e}"),
                },
                Some("off") => match launch::unregister() {
                    Ok(()) => println!("launch-at-login disabled"),
                    Err(e) => println!("failed to disable launch-at-login: {e}"),
                },
                _ => println!("launch-at-login: {}", launch::check_reg_status()),
            }
            true
        }
        "recover" => {
            // Bring the built-in back on, using the cached id when known.
            if let Some(bid) = find_builtin_id(&settings) {
                match displays::set_enabled(bid, true) {
                    Ok(()) => println!("re-enabled built-in display {bid}"),
                    Err(e) => println!("failed to re-enable {bid}: {e}"),
                }
                let _ = settings.save();
            } else {
                println!("could not find the built-in display");
            }
            true
        }
        "selftest" => {
            // Safety: always restore the built-in before returning.
            let id = displays::builtin_display()
                .map(|d| d.id)
                .or(settings.builtin_id);
            if let Some(bid) = id {
                println!("turning off built-in {bid}");
                let _ = displays::set_enabled(bid, false);
                std::thread::sleep(std::time::Duration::from_millis(700));
                println!("  on={}", displays::is_on(bid));
                println!("turning back on built-in {bid}");
                let _ = displays::set_enabled(bid, true);
                std::thread::sleep(std::time::Duration::from_millis(700));
                println!("  on={}", displays::is_on(bid));
            } else {
                println!("no built-in display found");
            }
            true
        }
        _ => false,
    }
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
    if cli() {
        return Ok(());
    }

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

    std::thread::Builder::new()
        .name("lidup-worker".into())
        .spawn(move || worker(proxy, cmd_rx))?;

    // Evaluate once on launch (applies the auto rule to the current state and
    // publishes the first snapshot) so the menu is populated immediately.
    let _ = cmd_tx.send(WorkerEvent::DisplayChanged);

    // If this process is terminated (e.g. the OS kills the login item when it is
    // unticked in System Settings), make sure the built-in display comes back on.
    install_exit_guard();

    let mut app = App {
        cmd_tx,
        tray: None,
        menu: None,
        snapshot: None,
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}
