# fcptemp API quick reference

All requests are JSON Lines messages and responses use the envelope `{"id": ..., "ok": true, "data": ...}` or `{"id": ..., "ok": false, "error": {"code": ..., "message": ...}}`.

| Command | Required arguments | Effect |
|---|---|---|
| `status` | — | Global state and per-fan runtime status |
| `sensors.list` | — | Enumerate hwmon temperature inputs and availability |
| `rules.list` | — | Return configured rules and runtime status |
| `rule.set` | `rule` object | Create or replace the rule for its fan |
| `rule.delete` | `fan` | Remove the rule for one fan |
| `control.set` | `enabled`, optional `fan` | Change global automation, or one rule when `fan` is supplied |

A rule has `fan`, `sensor`, optional `enabled` (default `true`), and `curve`. Curve types are `linear`, `power` and `points`. See `../schemas/fcptemp-api.schema.json` for payload fields and `../schemas/fcptemp-config.schema.json` for service config.

When a client switches to manual control, it should first send `control.set` with the target fan and `enabled:false`, then call `fcp`'s `fan.set`. This prevents the periodic temperature loop from immediately overwriting the manual value.
