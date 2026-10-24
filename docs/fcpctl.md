# fcpctl

`fcpctl` is a small CLI for both services. It emits JSON so its output can be inspected by scripts and piped to tools such as `jq`.

Build:

```sh
cargo build --release
./target/release/fcpctl help
```

## Socket selection

- `FCP_SOCKET_PATH` overrides the `fcp` socket.
- `FCPTEMP_SOCKET_PATH` overrides the `fcptemp` socket.
- `--fcp-socket PATH` and `--fcptemp-socket PATH` override the respective paths for one invocation. Put these options before the command.
- When no override is set, an existing `/run/fcp/control.sock` or `/run/fcptemp/control.sock` is preferred. Otherwise, `fcp.sock` / `fcptemp.sock` under `XDG_RUNTIME_DIR` is used; without it, the service's home-directory default is used.

## Status and manual control

```sh
fcpctl status
fcpctl fans
fcpctl fans CPU_FAN
fcpctl fan set CPU_FAN 35
fcpctl fan mode CPU_FAN low
fcpctl fan mode CPU_FAN percent 65
fcpctl fan kickstart CPU_FAN
```

`fan set` accepts an integer throttle from 0 to 100. Values 0 and 100 select the corresponding boundary mode according to the `fcp` API. `fan mode` accepts `low`, `high`, or `percent`; the optional throttle is saved for later percentage mode use.

Before a manual `fan set` or `fan mode`, `fcp-client` checks `fcptemp`. If an enabled temperature rule exists for the fan, the client disables that rule first. If the rule cannot be disabled, the manual change is cancelled. If `fcptemp` cannot be reached at all, a warning is printed and the operation proceeds through `fcp`, since an unavailable service cannot actively overwrite the setting.

## Temperature automation

```sh
fcpctl temp status
fcpctl temp sensors
fcpctl temp rules
fcpctl temp enable
fcpctl temp disable
fcpctl temp rule enable CPU_FAN
fcpctl temp rule disable CPU_FAN
fcpctl temp rule delete CPU_FAN
```

`temp enable` and `temp disable` toggle automation globally. `temp rule enable FAN` and `temp rule disable FAN` only toggle that fan's rule; global automation must also be enabled for temperature updates to be applied.

To create or replace a rule, give `temp rule set` a JSON file (or `-` to read JSON from stdin). The file contains the `Rule` object itself, not an API envelope. For example, `cpu-rule.json`:

```json
{
  "fan": "CPU_FAN",
  "sensor": "k10temp/temp1",
  "enabled": true,
  "curve": {
    "type": "power",
    "min_temp_c": 35,
    "max_temp_c": 75,
    "min_throttle": 20,
    "max_throttle": 100,
    "exponent": 2
  }
}
```

Then:

```sh
fcpctl temp rule set cpu-rule.json
fcpctl temp rule enable CPU_FAN
fcpctl temp enable
```

The first command saves the rule. The second ensures that rule is enabled. The last command activates automation globally. The rule's sensor ID must match an available ID returned by `fcpctl temp sensors`.

## Shared code

Transport, endpoint selection, response validation and the manual-control handoff live in `crates/fcp-client`. The CLI and future GUI should call these helpers instead of implementing the socket protocol or handoff independently.
