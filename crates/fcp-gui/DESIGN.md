# FCP-GUI interface rules

## Structure

- A single dashboard replaces separate fan and temperature pages.
- The app bar contains the product identity, service connectivity, the global temperature-automation switch, and the light/dark switch.
- A compact live-sensor table stays above the independently scrollable fan list.
- Each fan is an accordion. Its collapsed row contains the name, RPM, rule status, manual mode, a staged PWM slider, a save icon, and the disclosure control.
- The expanded area contains only that fan's temperature rule editor.
- The app polls the service APIs every second. There is no manual refresh control.

## Palette

Only grayscale surfaces are used. No blue, green, orange, or red status accents.

| Token | Dark | Light |
|---|---|---|
| Window | `#141414` | `#F2F2F2` |
| Main surface | `#1F1F1F` | `#FFFFFF` |
| Raised control | `#292929` | `#E8E8E8` |
| Row surface | `#262626` | `#EEEEEE` |
| Selected surface | `#383838` | `#DDDDDD` |
| Main text | `#F0F0F0` | `#202020` |
| Secondary text | `#B8B8B8` | `#5A5A5A` |

## Metrics

- Cards and accordion rows: 12 px radius.
- Inputs, dropdowns, and buttons: 8–9 px radius.
- Switch track: fully pill-shaped.
- Main horizontal inset: 20 px.
- Repeated gaps use 8, 9, 12, or 14 px; avoid one-off spacing values when adding controls.
- Cards do not draw borders or separator frames. Adjacent surfaces and spacing establish hierarchy.

## Interaction

- Moving a PWM slider only edits a local draft. The checkmark applies it to `fcp`.
- Choosing Low, High, or Percent applies the selected mode. Percent reuses the fan's stored throttle.
- Expanding a fan selects it as the editor target. Saving a rule preserves the service's existing behavior: the saved rule is disabled and must be explicitly enabled.
- The global automation toggle is unavailable while `fcptemp` is disconnected. Rule controls are also disabled until the service is available.
- Sensor readings follow the process locale: locales from the US, Bahamas, Belize, Cayman Islands, Liberia, and Palau display Fahrenheit; otherwise Celsius is used. Temperature-rule configuration remains explicitly in Celsius because the service stores and computes curves in Celsius.
