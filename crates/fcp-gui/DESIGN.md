# FCP Control UI design

## Direction

A compact dark desktop utility for monitoring and changing fan behavior. It should feel like a precise system-control panel, not a web dashboard. Information density is intentional, but controls remain grouped by task.

## Palette

- Window background: `#101722`
- Header: `#17263A`
- Panels: `#192638`
- Interactive/data rows: `#202D3E`
- Borders and dividers: `#30445B` / `#2B3D52`
- Primary text: `#EDF3FB`
- Secondary text: `#94A9C2`
- Selection/action accent: Fluent Dark primary blue
- Healthy state: muted green
- Warning/unavailable state: amber

Green and amber indicate state only; they are not decorative accents.

## Layout

- Fixed header: application identity, compact service status, automation state, refresh.
- Two native buttons navigate between the fan and temperature tasks.
- Fans page: live fan table first, manual controls second.
- Temperature page: automation master control, sensors and rules side by side, rule editor below.
- Panels keep natural height. Only the page body scrolls; cards must not stretch to fill unused vertical space.

## Rhythm and typography

- Outer page padding: 18 px.
- Panel padding: 15–16 px.
- Section spacing: 13–14 px.
- Labels: 11 px; body/data: 12–13 px; section headings: 16–17 px.
- Use the platform UI font through Slint rather than bundling another font dependency.

## Interaction rules

- Keep one clear primary action for the current task.
- Use standard Slint buttons and inputs so keyboard focus and native widget behavior remain available.
- Hide irrelevant curve controls: exponent appears only for the power curve; milestones appear only for the points curve.
- Do not repeat "reading normally" for every sensor. Healthy readings display as values; unavailable readings display the driver's reason.
- Saving a rule keeps it disabled. Automation never enables itself on startup.
- Status messages persist across periodic refreshes; a refresh must not erase the last action result.
