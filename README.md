# g502-linux-control

A small Rust replacement for the useful remapping/profile parts of Logitech G HUB
on Linux, for the **Logitech G502 HERO**. No Electron, no Node. It sits on top of
[libratbag](https://github.com/libratbag/libratbag)/`ratbagd` and talks to it over D-Bus.

Status: first slice. It keeps a **shared DPI** across profiles and can inspect the mouse.
Button remapping, LEDs and a GUI are not implemented yet.

## Tested hardware

| | |
|---|---|
| Mouse | Logitech G502 HERO Gaming Mouse, USB `046d:c08b`, 1000 Hz |
| OS | Fedora 44, KDE Plasma / Wayland |
| Stack | libratbag / ratbagd (D-Bus API version 2) |

Nothing else is claimed to work. G502 X, Lightspeed, Plus and other variants are
untested; `g502ctl check` warns for anything that is not `046d:c08b`.

## The problem, and why this exists

The G502 has five onboard profiles. I use two: profile 0 (blue) with normal buttons,
and profile 1 (red) where the two side buttons become DPI down/up. I want DPI to
survive switching profiles.

The obvious design — map those buttons to `resolution-up`/`resolution-down` and let
the firmware cycle its DPI slots — cannot be observed from Linux:

```
press hardware DPI+/-   ->  the pointer speed clearly changes
ratbagctl ... resolution active get   ->  keeps returning 1
```

`ratbagd` does not see the firmware-side active-resolution change, so nothing on the
host can know the current DPI, mirror it into the other profile, or show it.

So DPI is done host-side instead:

1. On profile 1, those two buttons send `KEY_F13` (down) and `KEY_F14` (up) as macros.
2. `g502d` reads the mouse's keyboard-like evdev endpoint, grabs it exclusively
   (F13/F14 never reach KDE or apps, so no desktop shortcut setup is needed), and
3. rewrites the DPI **value** of resolution slot 1 in both profiles through ratbagd,
   committed in one hardware write. Slot 1 is active+default in both profiles, so
   switching profiles keeps the DPI.

Stages: 1000, 1800, 2400, 3200, 4000 (configurable). The exact prototype behaviour
and button layout is recorded in [`baseline/`](baseline/README.md).

## Install

Needs Rust, a running `ratbagd` (`ratbagd.service`), and the mouse programmed as in the baseline.

```sh
# 1. If you use the old g502-dpid prototype, stop it: only one process can grab the endpoint.
systemctl --user disable --now g502-dpid

# 2. Permissions for the mouse's input node (see below).
sudo cp packaging/71-g502.rules /etc/udev/rules.d/
sudo udevadm control --reload && sudo udevadm trigger --subsystem-match=input --action=change

# 3. Build, check, run.
cargo install --path .                      # g502d, g502ctl -> ~/.cargo/bin
g502ctl check                               # read-only; look for [ok] everywhere
mkdir -p ~/.config/systemd/user
cp packaging/g502d.service ~/.config/systemd/user/
systemctl --user enable --now g502d
journalctl --user -u g502d -f
```

`g502d` logs one error if the grab fails (usually the old daemon still running).
Config is optional: `~/.config/g502-linux-control/config.toml` (or `$G502_CONFIG`);
see [`packaging/config.example.toml`](packaging/config.example.toml). Missing file = the tested defaults.

### GUI (optional)

```sh
cargo install --path . --features gui     # adds g502-gui next to g502d and g502ctl
g502-gui
```

A native Wayland window (iced, software renderer, no Electron/Node) that shows what
`g502ctl check` reports: profile tabs, the shared DPI and its stages, every button with
its action, resolution slots, report rate and LED colours. It marks each action as
*onboard* (stored in the mouse) and flags the F13/F14 macros that `g502d` reads, because
those two kinds of action behave differently. It also says whether the device matches
your config.

**Editing:** every button has an *Edit* control: pick mouse button / special action /
keyboard key / key macro / none, and the GUI shows the exact diff that would be written
(computed by the same code that writes it, so invalid input shows an error and Apply
stays disabled). *Apply* goes through the same path as `g502ctl` (write lock, backup of
the previous state, one commit, verify by re-reading) and the window then re-reads the
device. Nothing is written before Apply. It warns if you are about to change one of the
F13/F14 buttons `g502d` depends on.

Not in the GUI yet: editing LEDs, DPI stages, report rate or profiles (use `g502ctl`),
and an "active profile" indicator (ratbagd's value for it is unreliable on this mouse,
see below). Button labels are `Button N` because the mapping from index to physical
button has not been verified on the hardware. The GUI is behind a cargo feature so a plain
`cargo install --path .` (daemon and CLI) stays small.

For development, `G502_GUI_SCREENSHOT=out.png` makes the window save a picture of its own
contents once loaded and exit; `G502_GUI_PROFILE=N`, `G502_GUI_EDIT=P:B:KIND:VALUE` and
`G502_GUI_APPLY=1` (which *does* write to the mouse) drive it without a mouse or keyboard.

### Permissions

`/dev/input/event*` is `root:input 0660` on Fedora, and there is no reason to put
yourself in the broad `input` group for one mouse. [`packaging/71-g502.rules`](packaging/71-g502.rules)
gives the logged-in seat user access to this device's nodes only (`uaccess`).

On the machine this was developed on the rule was **not needed and was not tested**:
the OpenRGB package's `60-openrgb.rules` already tags `046d:c08b` with `uaccess`, which
is where the working ACL on the endpoint came from. Talking to ratbagd needed no extra
setup for the logged-in user.

## Commands

```
g502ctl status            device, profiles, current shared DPI
g502ctl dpi up|down       one stage
g502ctl dpi set 2400      any DPI the device accepts
g502ctl check             read-only compatibility check + full device report
g502ctl backup [FILE]     dump the current device configuration as TOML
g502ctl restore FILE      write a backup back
g502ctl button set P B ACTION   ACTION: none | button N | special NAME | key KEY_X | macro KEY_X
g502ctl apply             write the config file's [profiles.*] settings to the device
g502ctl config export [FILE]   print a config describing the device as it is now
g502ctl profile enable|disable P
g502ctl profile rate P 125|250|500|1000
g502ctl led set P L [--mode off|on|cycle|breathing] [--color RRGGBB] [--brightness 0-255]
```

`restore`, `apply`, `button set`, `led set` and `profile` share one write path: show a diff, ask (or `--yes`;
`--dry-run` only shows the diff), save the previous state as a backup, commit once, then
re-read the device and fail loudly if it does not match. The flags are rejected on every
other command, so `g502ctl dpi up --yes` cannot do anything unexpected. Special action names:
`doubleclick, wheel-left, wheel-right, wheel-up, wheel-down, ratchet-mode-switch,
resolution-cycle-up, resolution-cycle-down, resolution-up, resolution-down,
resolution-alternate, resolution-default, profile-cycle-up, profile-cycle-down, profile-up,
profile-down, second-mode, battery-level` (ids from libratbag; which of them the G502 HERO
honours is up to the firmware, and the post-write verification catches a refusal).
`button set` warns before changing a KEY_F13/KEY_F14 macro, since `g502d` depends on those.

`check` and `backup` never write to the mouse. Any command that writes DPI first
verifies that the device is the configured one (matched by USB id from
`Device.Model`, never by ratbag's per-system nickname), that the synced profiles are
enabled, and that every configured stage is a DPI the hardware accepts.

## Architecture

One crate, no async runtime in the daemon or CLI. Binaries: `g502d`, `g502ctl` and the
optional `g502-gui`.

```
src/dpi.rs      pure stepping logic + DpiBackend trait + burst batching   (unit-tested)
src/config.rs   TOML config, defaults, validation                         (unit-tested)
src/restore.rs  plan/diff between two snapshots, staging, and the shared `write` path (plan is pure, unit-tested)
src/apply.rs    config <-> snapshot overlay and export                    (pure, unit-tested)
src/ratbag.rs   blocking zbus client for org.freedesktop.ratbag1, snapshot, Controller
src/input.rs    find/grab the F13/F14 evdev endpoint, reconnect on unplug
src/bin/g502d.rs    input thread --channel--> loop: batch -> lock -> read DPI -> write
src/bin/g502ctl.rs  status / dpi / check / backup / restore / apply / button / led / profile
src/bin/g502-gui.rs iced GUI: view + button editing (cargo feature "gui")
```

- **The mouse is the state.** Every update reads the current DPI from ratbagd, steps it,
  and writes it back. There is no state file to drift out of sync, and `g502ctl`,
  `ratbagctl` or the daemon can all change DPI without confusing each other.
- **Bursts.** Key presses go through a channel. While a hardware write is in flight,
  further presses queue up; the next iteration takes all of them, folds them
  step-by-step (each step clamps on its own) and writes once. There is no debounce
  timer, so a lone press is written immediately. `UP UP UP` from 1000 ends at 3200,
  usually in one or two commits.
- **One hardware transaction.** ratbagd property writes only stage changes; a single
  `Device.Commit()` writes profile 0 and 1 together (what `--nocommit` did in the shell version).
- **Serialization.** A `flock` on `$XDG_RUNTIME_DIR/g502-linux-control.lock` keeps the
  daemon and `g502ctl` from interleaving read-modify-write cycles.
- **The daemon only ever writes DPI values.** It never touches button mappings or LEDs.
- **Logging** goes to stderr (journald picks up priorities); no notifications.

### Config-driven setup (`apply`)

The config can describe the device programming, not just DPI: per profile `enabled`,
`report_rate`, a `color` shorthand, per-LED `mode`/`color`/`brightness`, `buttons.N.onboard`
actions and non-shared `resolutions.N`. `g502ctl apply` overlays exactly what the file names
onto the device's current state (everything else is left alone), then goes through the usual
diff, confirm, backup, commit, verify path. `g502ctl config export` writes a complete config
for the current device, and applying that is an empty diff.
[`packaging/config.baseline.toml`](packaging/config.baseline.toml) is the layout from
[`baseline/`](baseline/README.md); [`packaging/config.example.toml`](packaging/config.example.toml)
documents every key.

- Button actions are `onboard = "..."` strings using the same words as `button set`. The key is
  called `onboard` because actions stored in the mouse are different from host-side actions
  run by a daemon; the latter will get their own key when they exist (unknown keys are
  rejected today).
- `apply` needs a config file. With none present it refuses (the built-in defaults contain no
  profiles), so it can never write anything you did not ask for.
- The shared DPI slot of a profile in `dpi.profiles` cannot be set from `[profiles.*]`, and
  `enabled = false` follows the same guards as `profile disable`.
- Checked by `check`: it reports whether the device matches the config.

### Restore

`restore` compares the file with the device and lists exactly what would change
(profile enabled/disabled, report rate, every resolution slot's DPI and disabled flag,
button mappings, LED mode/colour/brightness). Before writing it checks that the file is
for the same device model and shape, and that every value is one the device says it
supports (DPI list, report rates, LED modes, button action types). It asks for
confirmation (`--yes` to skip; refuses without a terminal otherwise), saves the current
state to `~/.local/state/g502-linux-control/backups/pre-restore-<unix-time>.toml` so the
restore can itself be undone, commits once, and then re-reads the device to confirm it
matches the file. Restoring also restores the DPI values, so a backup taken at a
different DPI will put that DPI back.

Not restored: which profile/resolution is currently active or default (ratbagd applies
those immediately rather than staging them) and LED effect duration.

### ratbagd D-Bus notes

- `Profile.IsActive` is not trustworthy on the tested mouse. After a few commits ratbagd
  reported a *disabled* profile (2) as the active one, and profile 0 as inactive, while the
  mouse was in use. It is the same class of problem as the stale active-DPI slot described
  above. `profile disable` refuses to disable "the active profile" using this value, so
  that guard is best-effort. `Profile.SetActive` returns 0 and does switch the mouse,
  but `g502ctl` has no command for it because its result cannot be verified through ratbagd.

- `Resolution` is typed `v` and arrives double-wrapped (`Value(U32(..))` inside the
  property variant), so zbus' typed `TryFrom` fails on read. Writing needs an explicit
  `Value::Value(Box::new(Value::U32(dpi)))`; a plain `u32` is rejected with
  `expected 'v', got 'u'`.
- `Button.Mapping` is `(uv)` and takes a single variant layer (unlike `Resolution`).
  ratbagd marks the profile dirty even when the same value is written back.
- Property writes only stage (`Profile.IsDirty`); `Device.Commit()` returns `0` on
  success and takes ~270 ms on the G502 HERO.
- Devices are found by `Device.Model` (`usb:046d:c08b:0`); the nickname `ratbagctl`
  shows is not on D-Bus.

### Action model (planned, not implemented)

The GUI must keep *onboard* actions (stored in the mouse: mouse button, special,
profile cycle, …) distinct from *host-side* actions (run by the daemon: key, key
combo, command, later macro), because they behave differently. Sketch:

```toml
action = { type = "command", command = "playerctl next" }
action = { type = "macro", steps = [ {key_down="LEFTCTRL"}, {key="C"}, {key_up="LEFTCTRL"} ] }
```

Only one process may `EVIOCGRAB` an endpoint. If xremap or input-remapper support is
added, either this daemon executes the mappings itself or it is the sole grabber and
they consume something it re-emits; never two grabbers on the same node.

## Limitations

- Only the G502 HERO `046d:c08b` is a target. The DPI stages must be values ratbagd lists for the slot.
- The daemon grabs the whole keyboard-like endpoint. Every key that endpoint emits
  is swallowed, not just F13/F14. On the tested setup only the F13/F14 macros are mapped to keys.
- `Resolution` is handled as a single value (`u`); devices with separate X/Y DPI are refused.
- Profile names are not supported on this mouse by libratbag.
- A commit takes roughly 270 ms on the tested mouse (measured via `g502ctl dpi set`),
  so the pointer speed changes that long after a press. Presses during a commit are
  coalesced into the next one, not lost.
- The GUI edits button actions only. Host-side actions (key combos, commands, multi-step macros) are not implemented;
  multi-event macros stored in the mouse show up in `config export` as comments.
- LED brightness is unreliable on the tested mouse: ratbagd reported 255 at first, then 0 for every
  LED after commits, while the LEDs kept their colour. `led set --brightness` writes and reads
  back the value, but its physical effect was not verified.
- Verified by hand on the tested mouse (2026-09-19): single presses, boundaries, bursts
  (29 presses folded into one commit), unplug/replug reconnect, pointer speed, and no
  F13/F14 leaking to the desktop while the grab is held. Not automated: the real
  press path has no CI test.

## Development

```sh
cargo test        # pure logic: stepping, boundaries, bursts, config, restore planning
cargo build
```
