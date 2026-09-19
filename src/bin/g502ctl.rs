//! g502ctl: inspect the G502 and change its shared DPI. Talks to ratbagd
//! directly, so it works with or without g502d running.

use anyhow::{Result, bail};
use g502_linux_control::{
    config::Config,
    dpi::{DpiBackend, Step, apply_batch},
    input,
    ratbag::{Controller, Ratbag, RawValue, Snapshot, describe, lock_writes, parse_action, parse_led_mode},
    apply as apply_mod,
    restore as plan_mod,
};
use std::{
    io::{IsTerminal, Write},
    process::exit,
};

const USAGE: &str = "\
usage: g502ctl <command>

  status            show device, active profile and shared DPI
  dpi up|down       move one stage
  dpi set <DPI>     set the shared DPI (any value the device supports)
  check             read-only compatibility check and device report
  backup [FILE]     write the full current device configuration as TOML
                    (stdout if no FILE)
  restore FILE      write a backup back to the device
  button set PROFILE BUTTON ACTION
                    ACTION: none | button N | special NAME | key KEY_X | macro KEY_X
                    (special names: see `g502ctl check`, e.g. resolution-alternate,
                    profile-cycle-up, wheel-left)
  apply             write the config file's [profiles.*] settings to the device
                    (buttons, LEDs, report rate, slots; only what the file names)
  config export [FILE]
                    print a config describing the device as it is now
                    (refuses to overwrite an existing FILE)
  identify          press physical buttons; shows which button index each one is
                    (Ctrl-C to stop)
  profile enable|disable PROFILE
  profile rate PROFILE 125|250|500|1000
  led set PROFILE LED [--mode off|on|cycle|breathing] [--color RRGGBB] [--brightness 0-255]

restore, apply, button set, led set and profile write to the mouse: they show a diff, ask to
confirm, save the previous state as a backup, commit once and verify.
  --dry-run   only show the diff
  --yes       do not ask (required when not on a terminal)

Config: $G502_CONFIG or ~/.config/g502-linux-control/config.toml
";

fn main() {
    // Rust ignores SIGPIPE by default, which turns `g502ctl check | head`
    // into a panic. Restore the default so we just exit like other CLI tools.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let yes = args.iter().any(|a| a == "--yes");
    let args: Vec<&str> = args.iter().map(String::as_str).filter(|a| *a != "--dry-run" && *a != "--yes").collect();
    let mode = Mode { dry_run, yes };
    // These flags only make sense for commands that ask before writing; never
    // let them silently ride along on e.g. `dpi up`.
    if (dry_run || yes) && !matches!(args.first(), Some(&("restore" | "apply" | "button" | "led" | "profile"))) {
        eprint!("{USAGE}");
        exit(2);
    }
    let result = match args[..] {
        ["status"] => status(),
        ["dpi", "up"] => dpi_step(Step::Up),
        ["dpi", "down"] => dpi_step(Step::Down),
        ["dpi", "set", n] => dpi_set(n),
        ["check"] => check(),
        ["identify"] => identify(),
        ["backup"] => backup(None),
        ["backup", file] => backup(Some(file)),
        ["restore", file] => restore(file, mode),
        ["button", "set", p, b, ref action @ ..] if !action.is_empty() => button_set(p, b, action, mode),
        ["apply"] => apply(mode),
        ["config", "export"] => config_export(None),
        ["config", "export", file] => config_export(Some(file)),
        ["profile", "enable", p] => profile_disabled(p, false, mode),
        ["profile", "disable", p] => profile_disabled(p, true, mode),
        ["profile", "rate", p, hz] => profile_rate(p, hz, mode),
        ["led", "set", p, l, ref opts @ ..] if !opts.is_empty() => led_set(p, l, opts, mode),
        _ => {
            eprint!("{USAGE}");
            exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("g502ctl: {e:#}");
        exit(1);
    }
}

fn load_config() -> Result<Config> {
    Ok(Config::load()?.0)
}

fn stage_label(stages: &[u32], dpi: u32) -> String {
    match stages.iter().position(|&s| s == dpi) {
        Some(i) => format!("stage {} of {}", i + 1, stages.len()),
        None => "off-stage".into(),
    }
}

fn status() -> Result<()> {
    let cfg = load_config()?;
    let rb = Ratbag::open(cfg.device.vendor, cfg.device.product)?;
    let snap = rb.snapshot()?;
    println!("{} ({})", snap.name, snap.model);
    let slot = cfg.dpi.shared_slot;
    for p in &snap.profiles {
        let dpi = rb.slot_dpi(p.index, slot).ok();
        let synced = cfg.dpi.profiles.contains(&p.index);
        println!(
            "profile {}: {}{}, {} Hz{}{}",
            p.index,
            if p.disabled { "disabled" } else { "enabled" },
            if p.is_active { ", active" } else { "" },
            p.report_rate,
            if synced { format!(", slot {slot} = ") } else { String::new() },
            match (synced, dpi) {
                (true, Some(d)) => format!("{d} DPI"),
                (true, None) => "?".into(),
                _ => String::new(),
            },
        );
    }
    let first = cfg.dpi.profiles[0];
    let dpi = rb.slot_dpi(first, slot)?;
    println!(
        "shared DPI: {dpi} ({}; stages {:?})",
        stage_label(&cfg.dpi.values, dpi),
        cfg.dpi.values
    );
    let others: Vec<_> = cfg.dpi.profiles.iter().filter(|&&p| rb.slot_dpi(p, slot).ok() != Some(dpi)).collect();
    if !others.is_empty() {
        println!("warning: profiles {others:?} do not match; next change will resync them");
    }
    Ok(())
}

fn dpi_step(step: Step) -> Result<()> {
    let cfg = load_config()?;
    let mut ctl = Controller::connect(&cfg)?;
    let _guard = lock_writes()?;
    match apply_batch(&mut ctl, &cfg.dpi.values, &[step])? {
        Some((from, to)) => println!("{from} -> {to} DPI"),
        None => println!("already at the {} stage", if step == Step::Up { "highest" } else { "lowest" }),
    }
    Ok(())
}

fn dpi_set(arg: &str) -> Result<()> {
    let dpi: u32 = arg.parse().map_err(|_| anyhow::anyhow!("not a DPI value: {arg:?}"))?;
    let cfg = load_config()?;
    let mut ctl = Controller::connect(&cfg)?;
    if !ctl.is_supported(dpi) {
        bail!("{dpi} DPI is not supported by the device (see `g502ctl check`)");
    }
    let _guard = lock_writes()?;
    let from = ctl.get_dpi()?;
    if from == dpi {
        println!("already {dpi} DPI");
    } else {
        ctl.set_dpi(dpi)?;
        println!("{from} -> {dpi} DPI");
    }
    Ok(())
}

fn backup(file: Option<&str>) -> Result<()> {
    let cfg = load_config()?;
    let snap = Ratbag::open(cfg.device.vendor, cfg.device.product)?.snapshot()?;
    let text = toml::to_string_pretty(&snap)?;
    match file {
        Some(f) => {
            std::fs::write(f, text)?;
            eprintln!("wrote {f}");
        }
        None => print!("{text}"),
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct Mode {
    dry_run: bool,
    yes: bool,
}

fn restore(file: &str, mode: Mode) -> Result<()> {
    let cfg = load_config()?;
    let text = std::fs::read_to_string(file).map_err(|e| anyhow::anyhow!("reading {file}: {e}"))?;
    let target: Snapshot = toml::from_str(&text).map_err(|e| anyhow::anyhow!("{file} is not a g502ctl backup: {e}"))?;
    let rb = Ratbag::open(cfg.device.vendor, cfg.device.product)?;
    write_snapshot(&rb, &target, file, mode)
}

/// The one write path for restore / button set / led set: plan the difference
/// to `target`, show it, confirm, back up, stage, commit once, verify.
fn write_snapshot(rb: &Ratbag, target: &Snapshot, label: &str, mode: Mode) -> Result<()> {
    let plan = plan_mod::plan(&rb.snapshot()?, target)?;
    if plan.is_empty() {
        println!("device already matches {label}; nothing to do");
        return Ok(());
    }
    println!("{} change(s) from {label}:", plan.len());
    for p in &plan {
        println!("  {}", p.summary);
    }
    if mode.dry_run {
        println!("dry run: nothing was written");
        return Ok(());
    }
    if !mode.yes {
        if !std::io::stdin().is_terminal() {
            bail!("not a terminal; pass --yes to write without asking");
        }
        print!("write these to the device? [y/N] ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            println!("aborted; nothing was written");
            return Ok(());
        }
    }

    let done = plan_mod::write(rb, target)?;
    if let Some(saved) = &done.backup {
        println!("saved previous state to {}", saved.display());
    }
    println!("done: {} change(s) written and verified", done.written);
    Ok(())
}

fn apply(mode: Mode) -> Result<()> {
    let (cfg, path) = Config::load()?;
    let Some(path) = path else {
        bail!("no config file at {} (or $G502_CONFIG); nothing to apply. `g502ctl config export` writes a starting point", Config::default_path().display());
    };
    if cfg.profiles.is_empty() {
        bail!("{} has no [profiles.*] sections; nothing to apply", path.display());
    }
    let rb = Ratbag::open(cfg.device.vendor, cfg.device.product)?;
    let target = apply_mod::overlay(&rb.snapshot()?, &cfg)?;
    write_snapshot(&rb, &target, &path.display().to_string(), mode)
}

fn config_export(file: Option<&str>) -> Result<()> {
    let cfg = load_config()?;
    let rb = Ratbag::open(cfg.device.vendor, cfg.device.product)?;
    let text = apply_mod::export(&rb.snapshot()?, &cfg)?;
    match file {
        Some(f) => {
            let mut out = std::fs::File::create_new(f).map_err(|e| anyhow::anyhow!("{f}: {e} (not overwriting)"))?;
            out.write_all(text.as_bytes())?;
            eprintln!("wrote {f}");
        }
        None => print!("{text}"),
    }
    Ok(())
}

fn index(what: &str, s: &str) -> Result<u32> {
    s.parse().map_err(|_| anyhow::anyhow!("{what} must be a number, got {s:?}"))
}

fn button_set(profile: &str, button: &str, action: &[&str], mode: Mode) -> Result<()> {
    let (p, b) = (index("PROFILE", profile)?, index("BUTTON", button)?);
    let (kind, value) = parse_action(action)?;
    let cfg = load_config()?;
    let rb = Ratbag::open(cfg.device.vendor, cfg.device.product)?;
    let mut target = rb.snapshot()?;
    let btn = target
        .profiles
        .get_mut(p as usize)
        .and_then(|pr| pr.buttons.get_mut(b as usize))
        .ok_or_else(|| anyhow::anyhow!("device has no profile {p} button {b}"))?;

    // The daemon needs F13/F14 macros on its buttons; say so before removing one.
    let f_key = |v: &Option<RawValue>| match v {
        Some(RawValue::MacroEvents(ev)) if btn.raw_type == 4 => ev.iter().any(|[_, k]| *k == 183 || *k == 184),
        _ => false,
    };
    if f_key(&btn.raw_value) && (kind, &value) != (btn.raw_type, &btn.raw_value) {
        eprintln!("warning: profile {p} button {b} is a KEY_F13/KEY_F14 macro; g502d's DPI buttons stop working if you change it");
    }

    btn.raw_type = kind;
    btn.action = describe(kind, &value);
    btn.raw_value = value;
    write_snapshot(&rb, &target, &format!("button set {p} {b}"), mode)
}

fn profile_disabled(profile: &str, disable: bool, mode: Mode) -> Result<()> {
    let p = index("PROFILE", profile)?;
    let cfg = load_config()?;
    let rb = Ratbag::open(cfg.device.vendor, cfg.device.product)?;
    let target = apply_mod::with_profile_disabled(&rb.snapshot()?, &cfg.dpi.profiles, p, disable)?;
    write_snapshot(&rb, &target, &format!("profile {} {p}", if disable { "disable" } else { "enable" }), mode)
}

fn profile_rate(profile: &str, hz: &str, mode: Mode) -> Result<()> {
    let p = index("PROFILE", profile)?;
    let hz = index("rate", hz)?;
    let cfg = load_config()?;
    let rb = Ratbag::open(cfg.device.vendor, cfg.device.product)?;
    let mut target = rb.snapshot()?;
    let pr = target.profiles.get_mut(p as usize).ok_or_else(|| anyhow::anyhow!("device has no profile {p}"))?;
    if !pr.report_rates.contains(&hz) {
        bail!("{hz} Hz is not supported; this profile supports {:?}", pr.report_rates);
    }
    pr.report_rate = hz;
    write_snapshot(&rb, &target, &format!("profile rate {p}"), mode)
}

fn led_set(profile: &str, led: &str, opts: &[&str], mode: Mode) -> Result<()> {
    let (p, l) = (index("PROFILE", profile)?, index("LED", led)?);
    let (mut new_mode, mut color, mut brightness) = (None, None, None);
    let mut it = opts.iter();
    while let Some(&flag) = it.next() {
        let val = *it.next().ok_or_else(|| anyhow::anyhow!("{flag} needs a value"))?;
        match flag {
            "--mode" => new_mode = Some(parse_led_mode(val)?),
            "--color" => color = Some(val.to_string()),
            "--brightness" => {
                let n: u32 = val.parse().ok().filter(|n| *n <= 255).ok_or_else(|| anyhow::anyhow!("brightness must be 0-255, got {val:?}"))?;
                brightness = Some(n);
            }
            other => bail!("unknown option {other:?} (use --mode, --color, --brightness)"),
        }
    }
    let cfg = load_config()?;
    let rb = Ratbag::open(cfg.device.vendor, cfg.device.product)?;
    let target = apply_mod::with_led(&rb.snapshot()?, p, l, new_mode, color.as_deref(), brightness)?;
    write_snapshot(&rb, &target, &format!("led set {p} {l}"), mode)
}

/// evdev code of "mouse button N" as the kernel's HID layer reports it:
/// button 1 is BTN_LEFT (0x110), button 2 BTN_RIGHT, ... N is BTN_MOUSE + N - 1.
fn mouse_button_code(n: u32) -> u16 {
    0x110 + (n as u16).saturating_sub(1)
}

/// Which (profile, button) pairs would produce this evdev key code?
fn buttons_for_code(snap: &Snapshot, code: u16) -> Vec<String> {
    let mut out = Vec::new();
    for p in snap.profiles.iter().filter(|p| !p.disabled) {
        for b in &p.buttons {
            let hit = match (&b.raw_type, &b.raw_value) {
                (1, Some(RawValue::Number(n))) => mouse_button_code(*n) == code,
                (4, Some(RawValue::MacroEvents(ev))) => ev.first().is_some_and(|e| e[0] == 1 && e[1] == u32::from(code)),
                _ => false,
            };
            if hit {
                out.push(format!("profile {} button {} ({})", p.index, b.index, b.action));
            }
        }
    }
    out
}

/// Press each physical button on the mouse and see which button index it is.
/// Read-only. Buttons whose action is a special function (sniper, profile
/// cycle, ...) send no event, so they stay silent; that is itself a clue.
fn identify() -> Result<()> {
    let cfg = load_config()?;
    let rb = Ratbag::open(cfg.device.vendor, cfg.device.product)?;
    let snap = rb.snapshot()?;
    let (tx, rx) = std::sync::mpsc::channel();
    let mut opened = 0;
    for (path, mut dev) in evdev::enumerate() {
        let id = dev.input_id();
        if id.vendor() != cfg.device.vendor || id.product() != cfg.device.product {
            continue;
        }
        opened += 1;
        eprintln!("listening on {} ({})", path.display(), dev.name().unwrap_or("?"));
        let tx = tx.clone();
        std::thread::spawn(move || {
            while let Ok(events) = dev.fetch_events() {
                for ev in events {
                    if tx.send((ev.event_type(), ev.code(), ev.value())).is_err() {
                        return;
                    }
                }
            }
        });
    }
    drop(tx);
    if opened == 0 {
        bail!("no readable input device for {:04x}:{:04x}", cfg.device.vendor, cfg.device.product);
    }
    eprintln!("Press one physical button at a time, then note what it says. Ctrl-C to stop.");
    eprintln!("(If g502d is running it holds the F13/F14 endpoint, so those presses will not show; use profile 0's mapping.)\n");
    for (ty, code, value) in rx {
        match (ty, value) {
            (evdev::EventType::KEY, 1) => {
                let name = format!("{:?}", evdev::KeyCode::new(code));
                let hits = buttons_for_code(&snap, code);
                if hits.is_empty() {
                    println!("{name} (code {code:#x}): not mapped by any enabled profile");
                } else {
                    println!("{name} (code {code:#x}): {}", hits.join("; "));
                }
            }
            (evdev::EventType::RELATIVE, v) if code == 8 || code == 6 => {
                let what = match (code, v.signum()) {
                    (8, 1) => "wheel scrolled up",
                    (8, _) => "wheel scrolled down",
                    (_, 1) => "wheel tilted right (HWHEEL)",
                    _ => "wheel tilted left (HWHEEL)",
                };
                println!("{what}");
            }
            _ => {}
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------- check

struct Report {
    failed: bool,
}

impl Report {
    fn ok(&self, m: impl std::fmt::Display) {
        println!("[ ok ] {m}");
    }
    fn warn(&self, m: impl std::fmt::Display) {
        println!("[warn] {m}");
    }
    fn fail(&mut self, m: impl std::fmt::Display) {
        println!("[FAIL] {m}");
        self.failed = true;
    }
}

/// Read-only. Never writes to the device.
fn check() -> Result<()> {
    let (cfg, path) = Config::load()?;
    let mut r = Report { failed: false };
    match path {
        Some(p) => r.ok(format!("config {}", p.display())),
        None => r.ok("no config file, using built-in defaults"),
    }

    let rb = match Ratbag::open(cfg.device.vendor, cfg.device.product) {
        Ok(rb) => rb,
        Err(e) => {
            r.fail(format!("{e:#}"));
            exit(1);
        }
    };
    r.ok(format!("ratbagd: {} ({})", rb.name, rb.model));
    if (cfg.device.vendor, cfg.device.product) != (0x046d, 0xc08b) || !rb.name.contains("G502 HERO") {
        r.warn("only the Logitech G502 HERO (046d:c08b) has been tested; this device may behave differently");
    }

    let snap = rb.snapshot()?;
    match Controller::connect(&cfg) {
        Ok(_) => r.ok(format!(
            "DPI control validated: profiles {:?}, slot {}, stages {:?} all supported",
            cfg.dpi.profiles, cfg.dpi.shared_slot, cfg.dpi.values
        )),
        Err(e) => r.fail(format!("DPI control not usable: {e:#}")),
    }

    // Shared slot state per synced profile.
    let mut dpis = Vec::new();
    for &pi in &cfg.dpi.profiles {
        let Some(slot) = snap.profiles.get(pi as usize).and_then(|p| p.resolutions.get(cfg.dpi.shared_slot as usize)) else {
            continue;
        };
        dpis.push(slot.dpi);
        if !(slot.is_active && slot.is_default) {
            r.warn(format!(
                "profile {pi} slot {} is not active+default; the mouse may not use the shared DPI",
                slot.index
            ));
        }
    }
    dpis.dedup();
    if dpis.len() > 1 {
        r.warn(format!("synced profiles currently disagree on DPI ({dpis:?}); the next change resyncs them"));
    }

    // Which buttons feed the daemon? KEY_F13 (183) = down, KEY_F14 (184) = up.
    for (code, name) in [(183, "F13 (DPI down)"), (184, "F14 (DPI up)")] {
        let want = Some(RawValue::MacroEvents(vec![[1, code], [2, code]]));
        let found: Vec<String> = snap
            .profiles
            .iter()
            .flat_map(|p| p.buttons.iter().filter(|b| b.raw_type == 4 && b.raw_value == want).map(move |b| format!("p{}b{}", p.index, b.index)))
            .collect();
        if found.is_empty() {
            r.warn(format!("no button is mapped to macro {name}; the daemon will never see it"));
        } else {
            r.ok(format!("macro {name} on {}", found.join(", ")));
        }
    }

    match input::find_endpoint(cfg.device.vendor, cfg.device.product) {
        Some((p, _)) => r.ok(format!("evdev endpoint {} readable (has KEY_F13/KEY_F14)", p.display())),
        None => r.warn("no readable evdev endpoint with KEY_F13/KEY_F14 (permissions? profile without F13/F14 macros?)"),
    }

    if !cfg.profiles.is_empty() {
        match apply_mod::overlay(&snap, &cfg).and_then(|t| plan_mod::plan(&snap, &t)) {
            Ok(p) if p.is_empty() => r.ok("device matches the [profiles.*] settings in the config"),
            Ok(p) => r.warn(format!("device differs from the config in {} place(s); see `g502ctl apply --dry-run`", p.len())),
            Err(e) => r.fail(format!("config cannot be applied to this device: {e:#}")),
        }
    }

    println!();
    print_report(&snap);
    println!("\nRead-only check: nothing was written to the device.");
    if r.failed {
        exit(1);
    }
    Ok(())
}

fn print_report(snap: &Snapshot) {
    for p in &snap.profiles {
        println!(
            "profile {} [{}{}] report rate {} Hz (supports {:?}), capabilities {:?}",
            p.index,
            if p.disabled { "disabled" } else { "enabled" },
            if p.is_active { ", active" } else { "" },
            p.report_rate, p.report_rates, p.capabilities,
        );
        if p.disabled {
            continue;
        }
        for res in &p.resolutions {
            let flags = [(res.is_active, "active"), (res.is_default, "default"), (res.is_disabled, "disabled")]
                .iter().filter(|f| f.0).map(|f| f.1).collect::<Vec<_>>().join(",");
            let range = match (res.supported_dpi.first(), res.supported_dpi.last()) {
                (Some(a), Some(b)) => format!("{a}..{b}"),
                _ => "?".into(),
            };
            println!("  resolution {}: {} DPI [{flags}] (accepts {range}, {} values)", res.index, res.dpi.map_or("?".into(), |d| d.to_string()), res.supported_dpi.len());
        }
        for b in &p.buttons {
            println!("  button {:>2}: {}", b.index, b.action);
        }
        for l in &p.leds {
            println!("  led {}: mode {} (modes {:?}), color {}, brightness {}, color depth {}", l.index, l.mode, l.modes, l.color, l.brightness, l.color_depth);
        }
    }
}
