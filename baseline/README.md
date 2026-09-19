# Baseline: the working shell/Python prototype

These are verbatim copies of the scripts this project replaces, kept as the
reference for "known good" behaviour. They are not used by the Rust code.

| File | Role |
|---|---|
| `g502-dpid` | python3-evdev daemon: grabs the G502 keyboard endpoint, F13 → `g502-dpi down`, F14 → `g502-dpi up` |
| `g502-dpi` | bash: steps an index over 1000/1800/2400/3200/4000, writes slot 1 of profiles 0 and 1 with two `ratbagctl` calls (first `--nocommit`), serialized with `flock` |
| `g502-dpid.service` | systemd user unit for the daemon |

The prototype hardcodes the ratbag nickname (`warbling-mara`), which differs per
system; the Rust version matches on USB id instead.

## Device programming the prototype relies on

Only profiles 0 and 1 are enabled; 2–4 are disabled. Profile naming is not
supported on this mouse through libratbag.

| Button | Profile 0 (BLUE, solid `0000ff`) | Profile 1 (RED, solid `ff0000`) |
|---|---|---|
| 0–4 | mouse buttons 1–5 | mouse buttons 1–5 |
| 5 | special: resolution-alternate (sniper) | same |
| 6 | mouse button 6 | macro `KEY_F13` (DPI down) |
| 7 | mouse button 7 | macro `KEY_F14` (DPI up) |
| 8 | special: profile-cycle-up | same |
| 9, 10 | mouse buttons 8, 9 | same |

Both profiles: resolution slot 1 is active+default and holds the shared DPI.

LED syntax in current ratbagctl: `ratbagctl DEV profile P led N set mode on color RRGGBB`.

## Verification

On 2026-09-19 (Fedora 44, KDE Plasma/Wayland, ratbagd API 2) `g502ctl check`
reported exactly the button table, LED colours and slot-1 state above.
