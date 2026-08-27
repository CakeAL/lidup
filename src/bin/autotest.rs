//! End-to-end check of the auto-off rule. Always restores the built-in display.
use lidup::{auto, config, displays};
use std::time::Duration;

fn main() {
    let list = displays::online_displays();
    let ext = list.iter().find(|d| !d.builtin).cloned();
    let Some(ext) = ext else {
        println!("no external display connected; nothing to do");
        return;
    };
    let builtin = displays::builtin_display().expect("no built-in display");
    let builtin_id = builtin.id;
    let builtin_key = builtin.key.clone();
    println!("external: {} key={}", ext.name, ext.key);
    println!(
        "builtin:  name={} id={} key={} on={}",
        builtin.name, builtin_id, builtin_key, builtin.on
    );

    let mut settings = config::Settings {
        bound_key: Some(ext.key.clone()),
        restore_builtin: true,
        builtin_id: Some(builtin_id),
        builtin_key: Some(builtin.key.clone()),
        ..config::Settings::default()
    };

    // 1) External present -> built-in should go off.
    println!("external present, applying auto-off...");
    auto::apply_auto(&mut settings);
    std::thread::sleep(Duration::from_millis(900));
    let online = displays::online_displays();
    println!(
        "  built-in on={}  still-in-online-list={}",
        displays::is_on(builtin_id),
        online.iter().any(|d| d.id == builtin_id)
    );

    // 2) Simulate the external going away. We must re-enable the built-in by its
    //    *cached id* because a powered-off display leaves the online list.
    let off_builtin = displays::DisplayInfo {
        id: builtin_id,
        builtin: true,
        on: false,
        vendor: builtin.vendor,
        model: builtin.model,
        serial: builtin.serial,
        width: builtin.width,
        height: builtin.height,
        name: builtin.name.clone(),
        key: builtin_key,
    };
    let only_builtin = vec![off_builtin];
    println!("external absent (restore_builtin=true), applying auto-off...");
    auto::apply_auto_to(&settings, &only_builtin);
    std::thread::sleep(Duration::from_millis(900));
    println!(
        "  built-in on after external absent: {}",
        displays::is_on(builtin_id)
    );

    // 3) Explicit restore as a safety net so we never leave the screen dark.
    let _ = displays::set_enabled(builtin_id, true);
}
