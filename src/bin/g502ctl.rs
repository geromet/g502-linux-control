//! g502ctl: inspect the G502 and change its shared DPI. Talks to ratbagd
//! directly, so it works with or without g502d running.

use anyhow::{Result, bail};
use g502_linux_control::{
    config::{Config, parse_color},
    dpi::{DpiBackend, Step, apply_batch},
    input,
    ratbag::{Controller, Ratbag, RawValue, Snapshot, lock_writes},
    restore as plan_mod,
};
use std::{
    io::{IsTerminal, Write},
    path::PathBuf,
    process::exit,
    time::{SystemTime, UNIX_EPOCH},
};

const USAGE: &str = "\
usage: g502ctl <command>

  status            show device, active profile and shared DPI
  dpi up|down       move one stage
  dpi set <DPI>     set the shared DPI (any value the device supports)
  check             read-only compatibility check and device report
  backup [FILE]     write the full current device configuration as TOML
                    (stdout if no FILE)
  restore FILE [--dry-run] [--yes]
                    write a backup back to the device. Shows a diff first and
                    asks to confirm (--yes skips the prompt, --dry-run only
                    shows the diff). Saves the pre-restore state as a backup.

Config: $G502_CONFIG or ~/.config/g502-linux-control/config.toml
";

fn main() {
    // Rust ignores SIGPIPE by default, which turns `g502ctl check | head`
    // into a panic. Restore the default so we just exit like other CLI tools.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args[..] {
        ["status"] => status(),
        ["dpi", "up"] => dpi_step(Step::Up),
        ["dpi", "down"] => dpi_step(Step::Down),
        ["dpi", "set", n] => dpi_set(n),
        ["check"] => check(),
        ["backup"] => backup(None),
        ["backup", file] => backup(Some(file)),
        ["restore", file, ref flags @ ..] if flags.iter().all(|f| ["--dry-run", "--yes"].contains(f)) => {
            restore(file, flags.contains(&"--dry-run"), flags.contains(&"--yes"))
        }
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

fn state_dir() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from(".local/state"));
    base.join("g502-linux-control/backups")
}

fn restore(file: &str, dry_run: bool, yes: bool) -> Result<()> {
    let cfg = load_config()?;
    let text = std::fs::read_to_string(file).map_err(|e| anyhow::anyhow!("reading {file}: {e}"))?;
    let target: Snapshot = toml::from_str(&text).map_err(|e| anyhow::anyhow!("{file} is not a g502ctl backup: {e}"))?;
    let rb = Ratbag::open(cfg.device.vendor, cfg.device.product)?;
    let plan = plan_mod::plan(&rb.snapshot()?, &target)?;

    if plan.is_empty() {
        println!("device already matches {file}; nothing to do");
        return Ok(());
    }
    println!("{} change(s) from {file}:", plan.len());
    for p in &plan {
        println!("  {}", p.summary);
    }
    if dry_run {
        println!("dry run: nothing was written");
        return Ok(());
    }
    if !yes {
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

    // Hold the lock so the daemon's DPI writes cannot interleave, and re-plan
    // under it in case the device changed while we were asking.
    let _guard = lock_writes()?;
    let before = rb.snapshot()?;
    let plan = plan_mod::plan(&before, &target)?;

    let dir = state_dir();
    std::fs::create_dir_all(&dir)?;
    let secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let saved = dir.join(format!("pre-restore-{secs}.toml"));
    std::fs::write(&saved, toml::to_string_pretty(&before)?)?;
    println!("saved current state to {}", saved.display());

    plan_mod::stage(&rb, &plan)?;
    rb.commit()?;

    let left = plan_mod::plan(&rb.snapshot()?, &target)?;
    if left.is_empty() {
        println!("restored {} change(s); device now matches {file}", plan.len());
        Ok(())
    } else {
        for p in &left {
            eprintln!("  still differs: {}", p.summary);
        }
        bail!("{} difference(s) remain after the write; undo with: g502ctl restore {}", left.len(), saved.display())
    }
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

    for (i, p) in &cfg.profiles {
        let want = parse_color(&p.color)?;
        let have = snap.profiles.get(*i as usize).and_then(|pr| pr.leds.first()).and_then(|l| parse_color(&l.color).ok());
        if have != Some(want) {
            r.warn(format!("profile {i} LED 0 is {} on the device, config says {} (not changed)", have.map_or("?".into(), |(r, g, b)| format!("{r:02x}{g:02x}{b:02x}")), p.color));
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
