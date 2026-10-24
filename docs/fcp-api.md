# fancontrol-custom-pwm

A small Linux utility for custom PWM fan control using the `hwmon` sysfs interface. The binary is named `fcp` and runs in the foreground; systemd can supervise it as a normal service.

The control algorithm alternates between configured `high` and `low` values. `frequency_hz` is the target number of complete cycles per second; `throttle` is the percentage of each cycle spent at `high`. Real timing depends on Linux scheduling and is approximate, especially at high frequencies.

## Build

```sh
cargo build --release
sudo install -Dm755 target/release/fcp /usr/local/bin/fcp
```

Dependencies are deliberately minimal: `serde`, `serde_json`, and `libc`.

## Configuration and state

The default config is created on the first launch at:

```text
~/.local/share/fancontrol-custom-pwm/config.json
```

The state file is stored at:

```text
~/.local/share/fancontrol-custom-pwm/state.json
```

`state.json` keeps each fan's `mode` and `throttle` across restarts. If a state file does not exist, every fan starts with `mode: "low"` and `throttle: 50`.

Set any of these environment variables to override paths independently:

- `CONFIG_PATH` — JSON hardware/profile config.
- `STATE_PATH` — JSON runtime settings.
- `SOCKET_PATH` — Unix-domain control socket.

A system service should set these explicitly; see `deploy/systemd/fcp.service`.

### Config fields

- `hwmon_name`: exact content of a controller's `hwmon*/name` file.
- `hwmon_root`: directory to search, normally `/sys/class/hwmon`.
- `frequency_hz`: complete PWM cycles per second, from `0.1` through `1000`.
- `fans[].label`: unique API name.
- `fans[].pwm_path`: control file relative to the discovered controller, or an absolute path. `null` leaves control unconfigured.
- `fans[].rpm_path`: RPM input file relative to the discovered controller, or an absolute path. `null` disables RPM reporting.
- `fans[].low` / `fans[].high`: exact values written to `pwm_path`.

For the M79 example, `CPU_FAN` → `pwm3_enable` and `SYS_FAN` → `pwm2_enable` match the existing `fanmix` script. The PWM mapping for `AUX1_FAN` and `AUX2_FAN` has not yet been verified, so both are left without a control path in the generated profile; `AUX1_FAN` still reads `fan1_input`. After verifying a physical connector, set its `pwm_path` (for example, `pwm1_enable`) in the config and restart. Missing or inaccessible paths do not stop the process: the affected fan's API `mode` or `rpm` field is `null`, and the reason is logged.

Config changes require a process restart; there is no config hot reload.

## Client protocol schema

`../schemas/fcp-api.schema.json` describes the JSON messages used by clients. Its root schema validates requests; response messages can be validated with the `$defs/Response` definition. It documents `status`, `fans.list`, `fan.set`, and `fan.kickstart`, including nullable `mode`/`rpm` fields and the `throttle` range.

The schema validates one JSON object, not the newline-delimited stream framing. Send one request per line and read one response line per request.

## Control socket

The default socket is `$XDG_RUNTIME_DIR/fcp.sock` when `XDG_RUNTIME_DIR` exists, otherwise `~/.local/state/fancontrol-custom-pwm/fcp.sock`. The socket is created with mode `0666`; parent directory permissions still apply.

Protocol: one JSON object per line (JSON Lines) over `AF_UNIX` / `SOCK_STREAM`. Each reply repeats the request's `id`. Local clients can connect without authentication.

Example using `socat`:

```sh
printf '%s\n' '{"id":1,"command":"fans.list"}' | socat - UNIX-CONNECT:/run/fcp/control.sock
```

### Commands

List all fans, including RPM readings:

```json
{"id":1,"command":"fans.list"}
```

List one fan only:

```json
{"id":2,"command":"fans.list","label":"CPU_FAN"}
```

Compact status without RPM reads:

```json
{"id":3,"command":"status"}
```

Set a throttle; this chooses `low`, `high`, or `percent` automatically for boundary values:

```json
{"id":4,"command":"fan.set","label":"CPU_FAN","throttle":35}
```

Set mode only, preserving the stored throttle:

```json
{"id":5,"command":"fan.set","label":"CPU_FAN","mode":"high"}
```

Set mode and stored throttle together. An explicit `mode` always wins:

```json
{"id":6,"command":"fan.set","label":"CPU_FAN","mode":"low","throttle":35}
```

Retry control for a fan that became inactive:

```json
{"id":7,"command":"fan.kickstart","label":"CPU_FAN"}
```

Modes:

- `low`: write the configured `low` value and stop PWM cycling.
- `high`: write the configured `high` value and stop PWM cycling.
- `percent`: cycle between `high` and `low` according to the saved `throttle`.

For `fan.set`, at least one of `mode` and `throttle` is required. If `mode` is omitted, `throttle=0` chooses `low`, `throttle=100` chooses `high`, and `1..99` chooses `percent`. If `mode` is supplied, that mode is used regardless of the supplied throttle; the throttle is still stored for later use of `percent` mode.

`mode: null` means the program currently cannot control that fan. `rpm: null` means its RPM reading is unavailable. Logs are written to stdout, including file problems, mode changes, kickstart attempts, and shutdown.

## systemd

```sh
sudo install -Dm644 crates/fcp/examples/thinkcentre-m79.json /etc/fcp/config.json
sudo install -Dm644 deploy/systemd/fcp.service /etc/systemd/system/fcp.service
sudo systemctl daemon-reload
sudo systemctl enable --now fcp.service
```

The service runs as root because writing most `hwmon` control files requires elevated permissions. Adjust the profile and physical channel mappings before enabling it.
