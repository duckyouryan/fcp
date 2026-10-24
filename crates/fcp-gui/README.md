# fcp-gui

Slint desktop client for `fcp` and `fcptemp`. Uses the workspace's `fcp-client` crate, so socket framing, API error handling, and the hand-off from automatic to manual control are shared with `fcpctl`.

## Features

- Dark desktop interface with compact fan telemetry and manual PWM controls.
- Fan mode, throttle, RPM, low/high/percent control, and kickstart.
- Available and unavailable temperature sensors, including driver-reported reasons.
- Temperature rule listing and editing for linear, power, and points curves (up to eight points; milestones use `temperature:throttle`, e.g. `40:50, 50:75`).
- Separate global automation and per-rule enable/disable controls.
- Saving a rule always saves it disabled; enabling is a separate explicit action.
- System-tray icon with Open, Refresh, and Quit. Closing the main window hides it in the tray; Quit is in the tray menu.
- Live status refresh every two seconds, with API requests off the UI thread.

Temperature automation is not enabled automatically at startup. The user must explicitly enable it in the Temperature Rules page.

## Build and run

```sh
cargo build --release -p fcp-gui
./target/release/fcp-gui
```

Socket paths follow `fcp-client` behavior. They can be overridden with `FCP_SOCKET_PATH` and `FCPTEMP_SOCKET_PATH`.

The GUI uses Slint's `fluent-dark` widget style, the Winit backend (Wayland/X11 on Linux), and the software renderer. The tray uses Slint's built-in `SystemTrayIcon` and StatusNotifierItem/AppIndicator support; a working system tray host is required.

## Service dependency

For a system-wide installation, install and start `fcptemp` from the workspace root:

```sh
./scripts/install-fcptemp-service.sh
```

The unit exposes `/run/fcptemp/control.sock`. If the GUI was already running before the service started, restart the GUI so it selects the system socket. Temperature automation remains disabled until explicitly enabled through the GUI or CLI.

See [DESIGN.md](DESIGN.md) for the UI layout, spacing, palette and interaction guidelines.
