//! lidup — a macOS menu-bar app that turns off the built-in display when a chosen
//! external display is connected, and provides per-display on/off toggles.
//!
//! The heavy lifting (enumeration + on/off via the private SkyLight API) lives in
//! `displays`. A background worker thread periodically re-checks state, applies the
//! auto-off rule and pushes a snapshot to the winit event loop, which renders the
//! menu-bar menu. Menu item ids are turned into commands on the worker thread.

use lidup::{auto, config, displays};

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
}

/// Commands sent from the menu (event loop) to the worker thread.
enum Command {
    Toggle(u32),
    SetBound(Option<String>),
    Refresh,
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
    })
}

fn worker(proxy: winit::event_loop::EventLoopProxy<UserEvent>, rx: mpsc::Receiver<Command>) {
    let mut settings = Settings::load();
    loop {
        // Wait for a command (or a poll timeout).
        let command = rx.recv_timeout(Duration::from_millis(settings.poll_ms.max(300)));
        match command {
            Ok(cmd) => match cmd {
                Command::Toggle(id) => {
                    let on = displays::is_on(id);
                    let _ = displays::set_enabled(id, !on);
                }
                Command::SetBound(key) => {
                    settings.bound_key = key;
                    let _ = settings.save();
                }
                Command::Refresh => {}
                Command::Quit => {
                    // Restore the built-in display so the user isn't left with a dark screen.
                    if let Some(bid) = settings.builtin_id {
                        let _ = displays::set_enabled(bid, true);
                    }
                    let _ = proxy.send_event(UserEvent::Exit);
                    return;
                }
            },
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }

        // Re-apply the auto-off rule (may refresh the cached built-in id/key),
        // persist if they changed, then publish the current state.
        let before_id = settings.builtin_id;
        let before_key = settings.builtin_key.clone();
        auto::apply_auto(&mut settings);
        if settings.builtin_id != before_id || settings.builtin_key != before_key {
            let _ = settings.save();
        }
        let _ = proxy.send_event(UserEvent::Snapshot(snapshot(&settings)));
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

/// A persistent set of menu items, built **once** and never replaced. Item state
/// (checked / label) is mutated in place on each snapshot. Rebuilding the tray
/// menu (`set_menu`) on macOS while the user interacts with it is what caused the
/// click-crash (muda #173), so we deliberately never replace it after startup.
/// Consequently newly-hot-plugged displays appear only after an app restart; the
/// auto-off rule and per-display toggles still work for the displays seen at start.
struct MenuState {
    #[allow(dead_code)]
    menu: Menu,
    bind_none: CheckMenuItem,
    bind_items: Vec<(String, CheckMenuItem)>,
    toggle_items: Vec<(u32, CheckMenuItem)>,
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
            let note = MenuItem::with_id("note", "Display control unavailable on this Mac", false, None);
            let _ = menu.append(&note);
            let _ = menu.append(&PredefinedMenuItem::separator());
        }
        let quit = MenuItem::with_id("quit", "Quit lidup", true, None);
        let _ = menu.append(&quit);

        MenuState {
            menu,
            bind_none,
            bind_items,
            toggle_items,
        }
    }

    /// Update item state against a fresh snapshot (in place — no menu rebuild).
    fn update(&mut self, snap: &Snapshot) {
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
    }
}

// ---------------------------------------------------------------------------
// winit ApplicationHandler
// ---------------------------------------------------------------------------

struct App {
    cmd_tx: mpsc::Sender<Command>,
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
                if let Some(ms) = self.menu.as_mut() {
                    // In-place update only; never call set_menu again after startup.
                    ms.update(&snap);
                } else {
                    let first = MenuState::build(&snap);
                    if let Some(tray) = &self.tray {
                        tray.set_menu(Some(Box::new(first.menu.clone())));
                    }
                    self.menu = Some(first);
                }
                self.snapshot = Some(snap);
            }
            UserEvent::Menu(ev) => {
                let id = ev.id();
                if id == "quit" {
                    let _ = self.cmd_tx.send(Command::Quit);
                } else if id == "refresh" {
                    let _ = self.cmd_tx.send(Command::Refresh);
                } else if let Some(key) = id.as_ref().strip_prefix("bind:") {
                    let key = if key == "none" {
                        None
                    } else {
                        Some(key.to_string())
                    };
                    let _ = self.cmd_tx.send(Command::SetBound(key));
                } else if let Some(sid) = id.as_ref().strip_prefix("toggle:") {
                    if let Ok(did) = sid.parse::<u32>() {
                        let _ = self.cmd_tx.send(Command::Toggle(did));
                    }
                }
            }
            UserEvent::Tray => {}
            UserEvent::Exit => {
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, _event_loop: &ActiveEventLoop, _id: WindowId, _e: winit::event::WindowEvent) {}
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
            println!("display control available: {}", displays::control_available());
            println!("bound external key: {:?}", settings.bound_key);
            for d in displays::online_displays() {
                let role = if d.builtin { "built-in" } else { "external" };
                println!(
                    "  id={:>3} {role:<8} on={} vendor={:04x} model={:04x} {}x{} key={}",
                    d.id, d.on, d.vendor, d.model, d.width, d.height, d.key
                );
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
            let id = displays::builtin_display().map(|d| d.id).or(settings.builtin_id);
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

    let (cmd_tx, cmd_rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("lidup-worker".into())
        .spawn(move || worker(proxy, cmd_rx))?;

    let mut app = App {
        cmd_tx,
        tray: None,
        menu: None,
        snapshot: None,
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}
