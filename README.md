# Fan Control

Rust workspace for custom PWM control and temperature-based automatic fan regulation on Linux.

- `fcp` directly controls configured `hwmon` PWM-enable files and reports RPM.
- `fcptemp` owns temperature rules and sends calculated throttle values to `fcp` over its Unix socket. It never writes hardware sysfs files itself.
- `fcp-client` contains shared JSON Lines socket-client code and the safe handoff from automatic to manual control.
- `fcpctl` is the CLI for both services; the planned Slint GUI will reuse `fcp-client`.

Manual control remains in `fcp`. For automatic mode, a client installs a rule in `fcptemp` and enables the rule plus global automation. Switching back to manual mode means disabling the corresponding `fcptemp` rule first, then setting the fan directly through `fcp`.

## Build

```sh
cargo build --release
```

The default workspace members build `fcp`, `fcptemp`, `fcpctl`, and the shared `fcp-client` library.

Binaries:

```text
target/release/fcp
target/release/fcptemp
target/release/fcpctl
```

## Workspace layout

```text
crates/
├── fcp/           # Existing low-level PWM service
├── fcp-client/    # Shared transport, API helpers and manual/automatic handoff
├── fcptemp/       # Temperature discovery, curves, API and service
└── fcpctl/        # CLI for both services
schemas/           # API and configuration JSON Schemas
deploy/systemd/    # Service units
scripts/           # Existing fanmix helper
docs/              # Architecture and API documentation
```

The planned Slint-based `fcp-gui` will use `fcp-client` rather than implementing the socket protocol independently. The CLI and GUI will share socket setup, error handling, and manual/automatic handoff behavior.

## `fcp` installation

Adjust the hardware profile before starting the service. For the currently used ThinkCentre M79 mapping:

```sh
if [ ! -e /etc/fcp/config.json ]; then sudo install -Dm644 crates/fcp/examples/thinkcentre-m79.json /etc/fcp/config.json; fi
sudo install -Dm755 target/release/fcp /usr/local/bin/fcp
sudo install -Dm644 deploy/systemd/fcp.service /etc/systemd/system/fcp.service
sudo systemctl daemon-reload
sudo systemctl enable --now fcp.service
```

The unit uses `/etc/fcp/config.json`, `/var/lib/fcp/state.json`, and `/run/fcp/control.sock`.

## `fcptemp` installation

```sh
sudo install -Dm755 target/release/fcptemp /usr/local/bin/fcptemp
if [ ! -e /etc/fcptemp/config.json ]; then sudo install -Dm644 crates/fcptemp/examples/fcptemp.json /etc/fcptemp/config.json; fi
sudo install -Dm644 deploy/systemd/fcptemp.service /etc/systemd/system/fcptemp.service
sudo systemctl daemon-reload
sudo systemctl enable --now fcptemp.service
```

By default, temperature automation is **disabled** and the state contains no rules. A client must explicitly configure rules and enable automation. The service's config has these fields:

- `fcp_socket`: path to the `fcp` socket.
- `hwmon_root`: root to scan for temperature sources; normally `/sys/class/hwmon`.
- `poll_interval_ms`: sensor/control polling interval from 100 to 60000 ms; default 1000 ms.

Environment overrides:

- `FCPTEMP_CONFIG_PATH` — service config.
- `FCPTEMP_STATE_PATH` — persistent rules and enabled state.
- `FCPTEMP_SOCKET_PATH` — API socket.

The default config path for a user-run instance is `~/.config/fcptemp/config.json`. State defaults to `~/.local/state/fcptemp/state.json`; the socket defaults to `$XDG_RUNTIME_DIR/fcptemp.sock` or `~/.local/state/fcptemp/fcptemp.sock`.

## `fcpctl` CLI

Build and run from the workspace root:

```sh
cargo build --release
./target/release/fcpctl status
./target/release/fcpctl fans
./target/release/fcpctl fan set CPU_FAN 35
./target/release/fcpctl fan mode CPU_FAN low
./target/release/fcpctl temp sensors
./target/release/fcpctl temp rules
```

`fcpctl status` queries both services and emits JSON containing their results. Other commands emit the corresponding API `data` object. See `docs/fcpctl.md` for all commands and examples.

Manual commands (`fan set` and `fan mode`) use shared `fcp-client` logic: if `fcptemp` has an enabled rule for that fan, the client disables that rule before changing `fcp`. If the `fcptemp` socket cannot be reached, the CLI warns and still applies the manual setting; check that automation is not active via a different socket. If a rule was found but could not be disabled, the manual change is cancelled.

`fcpctl` honors `FCP_SOCKET_PATH` and `FCPTEMP_SOCKET_PATH`. Command-line options `--fcp-socket PATH` and `--fcptemp-socket PATH` override paths for one invocation. Without overrides, the client prefers active system service sockets at `/run/fcp/control.sock` and `/run/fcptemp/control.sock`; otherwise it uses the user-instance defaults described above.

## `fcptemp` protocol

One JSON object per line over `AF_UNIX` / `SOCK_STREAM`. Each response echoes `id`, and uses the same `ok` / `error.code` / `error.message` envelope as `fcp`. The socket is local and currently has mode `0666`; access is controlled by parent-directory permissions.

List available temperatures, including disabled or invalid sources:

```sh
printf '%s\n' '{"id":1,"command":"sensors.list"}' | socat - UNIX-CONNECT:/run/fcptemp/control.sock
```

List rules and current runtime status:

```sh
printf '%s\n' '{"id":2,"command":"rules.list"}' | socat - UNIX-CONNECT:/run/fcptemp/control.sock
```

Install a power curve for a fan:

```sh
printf '%s\n' '{"id":3,"command":"rule.set","rule":{"fan":"CPU_FAN","sensor":"k10temp/temp1","enabled":true,"curve":{"type":"power","min_temp_c":35,"max_temp_c":75,"min_throttle":20,"max_throttle":100,"exponent":2}}}' | socat - UNIX-CONNECT:/run/fcptemp/control.sock
```

Enable global automation:

```sh
printf '%s\n' '{"id":4,"command":"control.set","enabled":true}' | socat - UNIX-CONNECT:/run/fcptemp/control.sock
```

Disable automation globally or for one fan:

```json
{"id":5,"command":"control.set","enabled":false}
{"id":6,"command":"control.set","fan":"CPU_FAN","enabled":false}
```

Delete one fan's temperature rule:

```json
{"id":7,"command":"rule.delete","fan":"CPU_FAN"}
```

Available commands are `status`, `sensors.list`, `rules.list`, `rule.set`, `rule.delete`, and `control.set`. See `schemas/fcptemp-api.schema.json` and `docs/fcptemp-api.md`.

### Curve semantics

Every curve specifies minimum/maximum temperature and minimum/maximum throttle. At or below the lower temperature, the service uses the lower throttle. At or above the upper temperature, it uses the upper throttle.

- `linear`: straight interpolation between the endpoints.
- `power`: `min_throttle + (max_throttle - min_throttle) * x^exponent`, where `x` is normalized to 0–1. An exponent of `2` is quadratic; values above `1` delay the ramp, and values below `1` accelerate it.
- `points`: linear interpolation through up to eight intermediate `{ "temp_c": ..., "throttle": ... }` milestones plus the configured endpoints.

Temperatures must increase strictly through the milestones; throttle may stay level or increase but may not decrease. The exponent range is `0.25..=8.0`. Rules are unique per fan.

If a sensor stops responding or returns an invalid value, `fcptemp` stops issuing automatic updates for that fan and reports `sensor_unavailable`. It does not force maximum throttle. The last mode/value remains in `fcp` until another client changes it or automation resumes. Stopping `fcptemp` likewise leaves `fcp` settings untouched.

## Schemas and tests

- `schemas/fcp-api.schema.json` — existing `fcp` API.
- `schemas/fcp-config.schema.json` — `fcp` hardware/profile config.
- `schemas/fcptemp-api.schema.json` — `fcptemp` requests and rule objects.
- `schemas/fcptemp-config.schema.json` — service config.
- `schemas/fcptemp-state.schema.json` — persistent automation state.

Run tests with:

```sh
cargo test
```
