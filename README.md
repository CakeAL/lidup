# lidup

A tiny macOS **menu-bar** app written in Rust that turns off the MacBook's **built-in
display** when a chosen external display is connected, and turns it back on when the
trigger is unplugged. It also exposes a per-display **on/off toggle** for every display.

It answers the use case from
[V2EX t/1123499](https://www.v2ex.com/t/1123499): *"不合盖使用外接显示器如何彻底关闭内置显示器"*
(close the built-in screen without closing the lid).

---

## How it works

- **Enumerate / identify / geometry** — the *public* CoreGraphics API
  (`CGGetOnlineDisplayList`, `CGDisplayIsBuiltin`, `CGDisplayIsActive`,
  `CGDisplayVendorNumber`, `CGDisplayModelNumber`, `CGDisplaySerialNumber`,
  `CGDisplayBounds`).
- **Actually power a display off/on** — the *private* SkyLight symbol
  `CGSConfigureDisplayEnabled(config, displayID, enabled)` loaded at runtime via
  `dlopen`/`dlsym`, wrapped in a
  `CGBeginDisplayConfiguration` → `CGSConfigureDisplayEnabled` →
  `CGCompleteDisplayConfiguration(_, kCGConfigureForSession)` transaction. This is
  the same technique used by `displayplacer` and the script described in the V2EX
  thread. It is loaded dynamically so the app degrades gracefully if the symbol ever
  moves.

  > The public CoreGraphics header contains no `CGConfigureDisplayEnabled` — the
  > working symbol lives in the private `SkyLight` framework (verified on macOS 26/27
  > Apple Silicon here).

- **Config persistence** — JSON in `~/Library/Application Support/lidup/config.json`:
  the bound external display key (`vendor:model:serial`), poll interval, and whether
  to restore the built-in when the trigger is unplugged.

### A quirk you should know

When the built-in display is powered off it **drops out of the online display list**,
so lidup caches the built-in's display id in the settings to bring it back later. All
of this is handled automatically.

---

## Menu bar

When running, the menu bar icon (`lidup`) shows:

- **Auto-off built-in when connected** — a submenu that lists every *external*
  display. Mark the one that should close the built-in (or "None" for manual control).
  While that display is plugged in, the built-in is force-off; when it is unplugged,
  the built-in is restored.
- **Per-display toggles** — one checkable item per display (`▶ on` / `■ off`). Toggle
  any display on or off, including the built-in.
- **Start at Login** — a checkable item that installs/removes a per-user LaunchAgent
  so lidup launches automatically at login (pointed at the currently running binary).
- **Quit** — restores the built-in display, then exits.

To select which external display to bind, open the menu → **Auto-off built-in when
connected** → click the display.

---

## Build & run

Requires the **Xcode command line tools** and the **Rust** toolchain.

```sh
# build the menu-bar app bundle (no Dock icon)
./pack.sh

# run it
open target/release/lidup.app
```

The tray app can also be run directly (it will show a Dock icon unless bundled):

```sh
./target/release/lidup
```

### Command-line helpers

`lidup` also accepts a subcommand for scripting/diagnostics:

```sh
lidup list                     # enumerate displays + state + identity keys
lidup selftest                 # turn the built-in off/on (always restores)
lidup recover                  # force the built-in back on (if the screen is dark)
lidup autostart                # print whether launch-at-login is enabled
lidup autostart on|off         # enable / disable (installs a LaunchAgent)
```

---

## Launch at login

- **From the tray**: tick **Start at Login** (installs a LaunchAgent pointing at the
  currently running binary).
- **From the CLI**: `lidup autostart on` / `lidup autostart off`.

The manual script below is an optional fallback that installs a LaunchAgent for the
bundled `/Applications/lidup.app`:

```sh
./install-launchagent.sh        # copies the app to /Applications and loads the agent
```

---

## CI

`.github/workflows/build.yml` runs on every push/PR and on releases:

1. `cargo fmt --check`, `cargo build`, `cargo test`
2. release build + `./pack.sh` to produce `lidup.app`
3. uploads `lidup` and `lidup.app` as a workflow artifact (and as a release asset on
   tagged releases)

---

## Notes & limitations

- **Private API**: `CGSConfigureDisplayEnabled` is undocumented. It works on modern
  macOS (tested on macOS 27 / Apple Silicon), but a future macOS release could remove
  or rename it. In that case lidup simply shows *"Display control unavailable"* in the
  menu instead of crashing.
- **Auto-off is enforced** while the bound external is connected: if you manually
  turn the built-in back on in the menu, the auto rule switches it off again on the
  next cycle. This is the intended "close while external is connected" behaviour.
- Only **one** external display can be bound as the auto-off trigger.
- The menu labels external displays as `External Display <vendor-hex>-<model-hex>`
  because modern macOS exposes no friendly per-display name through public/private
  CoreGraphics on Apple Silicon (the vendor/model hex is stable and distinguishes
  different monitors).

---

## Project layout

```
src/
  lib.rs            # crate root (library: displays, config, auto, launch_agent)
  displays.rs       # CoreGraphics enumeration + SkyLight private on/off (dlsym)
  config.rs         # JSON settings
  auto.rs           # the auto-off rule (unit-testable)
  launch_agent.rs   # launch-at-login LaunchAgent (install/remove/uninstall)
  main.rs           # menu-bar app (winit event loop + tray-icon/muda menu + worker)
  bin/autotest.rs   # CLI end-to-end check of the auto-off rule
pack.sh             # build + bundle as lidup.app
install-launchagent.sh
LaunchAgent/com.lidup.app.plist
.github/workflows/build.yml   # CI build / test / artifact
```
