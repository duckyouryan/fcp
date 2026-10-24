# Architecture

```text
CLI / fcpctl ──┐
               ├── fcp-client ── JSON Lines ── fcp
GUI / Slint ───┘                      ▲
                                      │ fan.set(mode=percent, throttle=N)
                                fcptemp service
                                      ▲
                                      │ read-only
                                  hwmon sensors
```

`fcp` owns low-level PWM and RPM. `fcptemp` owns automatic temperature rules and only communicates with `fcp` through its API. Clients call both services directly: `fcp` for manual modes and status, `fcptemp` for automatic rules and global/per-fan auto enablement.

`fcp-client` is the common blocking Unix-socket transport. The UI should call it from a worker thread so socket timeouts do not block rendering. CLI and GUI should not duplicate JSON Lines framing, size limits, socket timeouts, or API error-envelope parsing.

## Automatic state

The persistent state stores whether automation is globally enabled and at most one rule per fan. Each rule links a fan label to a discovered sensor ID and a curve. Runtime sensor values and status remain transient; the rules themselves persist across restarts.

`fcptemp` writes through `fcp` only while global automation and the individual rule are enabled. If a sensor becomes unavailable, updates stop for that fan. Neither a sensor fault nor a normal `fcptemp` shutdown rewrites the fan to an assumed safety value.


## Client applications

`fcpctl` and the future Slint GUI are clients, not services. Both use `fcp-client` for socket discovery, JSON Lines transport, API error handling, and safe manual-control handoff. They talk directly to both service APIs. `fcp` remains the sole PWM controller; `fcptemp` owns temperature rules and sends automatic throttle updates to `fcp`. Switching a fan to manual control disables its enabled temperature rule before applying the requested setting. If the temperature service cannot be reached, the client reports this and proceeds with manual control; if a discovered rule cannot be disabled, the manual change is cancelled.
