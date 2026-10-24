# FCP-GUI

Slint GUI client for the `fcp` and `fcptemp` services.

The dashboard shows temperature sensors above expandable fan cards. Each card supports manual PWM and contains the temperature rule editor for that fan. Global temperature automation and the color scheme are controlled from the app bar. The UI polls both APIs once per second and has no manual refresh button.

Temperature readings use the locale's customary unit where detectable; rule thresholds remain explicitly Celsius.

Build from the workspace root:

```sh
cargo test
cargo build --release -p fcp-gui
./target/release/fcp-gui
```

See `DESIGN.md` for the grayscale palette, layout metrics, and interaction conventions.
