//! Config <-> device. `overlay` turns a config into the snapshot the device
//! should end up with (touching only what the config names); `export` writes
//! a config describing the device as it is. Both are pure.

use crate::config::{Config, parse_color};
use crate::ratbag::{Snapshot, action_string, describe, led_mode_name, parse_action, parse_led_mode};
use crate::restore::check_can_disable;
use anyhow::{Context, Result, bail};
use std::fmt::Write;

fn normalized(color: &str) -> Result<String> {
    let (r, g, b) = parse_color(color)?;
    Ok(format!("{r:02x}{g:02x}{b:02x}"))
}

/// `current` with everything named in `cfg.profiles` applied. Feed the result to
/// `restore::plan` to get the diff. Errors if the config refers to something the
/// device does not have or breaks a safety rule.
pub fn overlay(current: &Snapshot, cfg: &Config) -> Result<Snapshot> {
    let mut t = current.clone();
    for (&pi, pc) in &cfg.profiles {
        let at = |what: &str| format!("profiles.{pi}.{what}");
        let cur = current.profiles.get(pi as usize).with_context(|| format!("config names profile {pi}, but the device has {}", current.profiles.len()))?;
        let p = &mut t.profiles[pi as usize];

        if let Some(enabled) = pc.enabled {
            if !enabled && !cur.disabled {
                check_can_disable(current, &cfg.dpi.profiles, pi).with_context(|| at("enabled"))?;
            }
            p.disabled = !enabled;
        }
        if let Some(hz) = pc.report_rate {
            p.report_rate = hz;
        }
        if let Some(c) = &pc.color {
            let c = normalized(c)?;
            p.leds.iter_mut().for_each(|l| l.color = c.clone());
        }
        for (&li, lc) in &pc.leds {
            let led = p.leds.get_mut(li as usize).with_context(|| format!("{}: profile {pi} has no LED {li}", at("leds")))?;
            if let Some(m) = &lc.mode {
                led.mode = parse_led_mode(m)?;
            }
            if let Some(c) = &lc.color {
                led.color = normalized(c)?;
            }
            if let Some(b) = lc.brightness {
                led.brightness = b;
            }
        }
        for (&bi, bc) in &pc.buttons {
            let btn = p.buttons.get_mut(bi as usize).with_context(|| format!("{}: profile {pi} has no button {bi}", at("buttons")))?;
            let words: Vec<&str> = bc.onboard.split_whitespace().collect();
            let (kind, value) = parse_action(&words)?;
            btn.action = describe(kind, &value);
            btn.raw_type = kind;
            btn.raw_value = value;
        }
        for (&si, rc) in &pc.resolutions {
            let res = p.resolutions.get_mut(si as usize).with_context(|| format!("{}: profile {pi} has no resolution slot {si}", at("resolutions")))?;
            if let Some(dpi) = rc.dpi {
                res.dpi = Some(dpi);
            }
            if let Some(enabled) = rc.enabled {
                res.is_disabled = !enabled;
            }
        }
    }
    Ok(t)
}

/// `current` with one button set to `action` (the `button set` grammar, e.g.
/// `["key", "KEY_A"]`). Feed the result to `restore::plan`.
pub fn with_button(current: &Snapshot, profile: u32, button: u32, action: &[&str]) -> Result<Snapshot> {
    let (kind, value) = parse_action(action)?;
    let mut t = current.clone();
    let btn = t
        .profiles
        .get_mut(profile as usize)
        .and_then(|p| p.buttons.get_mut(button as usize))
        .with_context(|| format!("device has no profile {profile} button {button}"))?;
    btn.action = describe(kind, &value);
    btn.raw_type = kind;
    btn.raw_value = value;
    Ok(t)
}

/// `current` with one LED changed. Only the given fields change; colour is
/// `RRGGBB` (a leading `#` is fine), brightness 0-255.
pub fn with_led(current: &Snapshot, profile: u32, led: u32, mode: Option<u32>, color: Option<&str>, brightness: Option<u32>) -> Result<Snapshot> {
    let mut t = current.clone();
    let l = t
        .profiles
        .get_mut(profile as usize)
        .and_then(|p| p.leds.get_mut(led as usize))
        .with_context(|| format!("device has no profile {profile} LED {led}"))?;
    if let Some(m) = mode {
        l.mode = m;
    }
    if let Some(c) = color {
        l.color = normalized(c)?;
    }
    if let Some(b) = brightness {
        if b > 255 {
            bail!("brightness must be 0-255, got {b}");
        }
        l.brightness = b;
    }
    Ok(t)
}

/// `current` with one profile's report rate changed. Unsupported rates are
/// caught by `restore::plan`.
pub fn with_report_rate(current: &Snapshot, profile: u32, hz: u32) -> Result<Snapshot> {
    let mut t = current.clone();
    t.profiles.get_mut(profile as usize).with_context(|| format!("device has no profile {profile}"))?.report_rate = hz;
    Ok(t)
}

/// `current` with a profile enabled or disabled. Disabling follows the safety
/// rules of `check_can_disable` (not a profile g502d syncs, not the active or last
/// enabled profile); an already-disabled profile may be stated as disabled again.
pub fn with_profile_disabled(current: &Snapshot, dpi_profiles: &[u32], profile: u32, disabled: bool) -> Result<Snapshot> {
    let cur = current.profiles.get(profile as usize).with_context(|| format!("device has no profile {profile}"))?;
    if disabled && !cur.disabled {
        check_can_disable(current, dpi_profiles, profile)?;
    }
    let mut t = current.clone();
    t.profiles[profile as usize].disabled = disabled;
    Ok(t)
}

/// `current` with one resolution slot changed. The shared DPI slot of a profile
/// that g502d syncs is refused: the daemon owns it.
pub fn with_slot(current: &Snapshot, cfg: &Config, profile: u32, slot: u32, dpi: Option<u32>, enabled: Option<bool>) -> Result<Snapshot> {
    if slot == cfg.dpi.shared_slot && cfg.dpi.profiles.contains(&profile) {
        bail!("slot {slot} of profile {profile} holds the shared DPI managed by g502d; change dpi.values in the config or use `g502ctl dpi`");
    }
    let mut t = current.clone();
    let r = t
        .profiles
        .get_mut(profile as usize)
        .and_then(|p| p.resolutions.get_mut(slot as usize))
        .with_context(|| format!("device has no profile {profile} resolution slot {slot}"))?;
    if let Some(d) = dpi {
        r.dpi = Some(d);
    }
    if let Some(e) = enabled {
        r.is_disabled = !e;
    }
    Ok(t)
}

/// A complete config for the device as it is now. Applying it again is an empty
/// diff. The daemon-managed shared DPI slot is left out (that is `[dpi]`'s job),
/// and disabled profiles only record that they are disabled. Things that have no
/// config spelling (multi-event macros, unknown keys) become comments.
pub fn export(snap: &Snapshot, cfg: &Config) -> Result<String> {
    let mut o = String::new();
    let w = &mut o;
    writeln!(w, "# g502-linux-control config, exported from: {} ({})", snap.name, snap.model)?;
    writeln!(w, "# `g502ctl apply` writes what is named here and leaves everything else alone.\n")?;
    writeln!(w, "[device]\nvendor = {:#06x}\nproduct = {:#06x}\n", cfg.device.vendor, cfg.device.product)?;
    let values: Vec<String> = cfg.dpi.values.iter().map(u32::to_string).collect();
    writeln!(w, "[dpi]\nvalues = [{}]\nshared_slot = {}\nprofiles = {:?}\n", values.join(", "), cfg.dpi.shared_slot, cfg.dpi.profiles)?;

    for p in &snap.profiles {
        let i = p.index;
        writeln!(w, "[profiles.{i}]\nenabled = {}\nreport_rate = {}", !p.disabled, p.report_rate)?;
        if p.disabled {
            writeln!(w)?;
            continue;
        }
        writeln!(w)?;
        for l in &p.leds {
            writeln!(w, "[profiles.{i}.leds.{}]", l.index)?;
            match led_mode_name(l.mode) {
                Some(m) => writeln!(w, "mode = \"{m}\"")?,
                None => writeln!(w, "# mode {} has no name here", l.mode)?,
            }
            writeln!(w, "color = \"{}\"\nbrightness = {}\n", l.color, l.brightness)?;
        }
        for b in &p.buttons {
            match action_string(b.raw_type, &b.raw_value) {
                Some(a) => writeln!(w, "[profiles.{i}.buttons.{}]\nonboard = \"{a}\"\n", b.index)?,
                None => writeln!(w, "# profiles.{i}.buttons.{}: {} (cannot be written in a config)\n", b.index, b.action)?,
            }
        }
        for r in &p.resolutions {
            let shared = r.index == cfg.dpi.shared_slot && cfg.dpi.profiles.contains(&i);
            if shared {
                continue;
            }
            writeln!(w, "[profiles.{i}.resolutions.{}]", r.index)?;
            if let Some(d) = r.dpi {
                writeln!(w, "dpi = {d}")?;
            }
            writeln!(w, "enabled = {}\n", !r.is_disabled)?;
        }
    }
    if o.trim().is_empty() {
        bail!("nothing to export");
    }
    Ok(o)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ratbag::{ButtonInfo, LedInfo, ProfileInfo, RawValue, ResolutionInfo};
    use crate::restore::plan;

    fn button(index: u32, kind: u32, value: Option<RawValue>) -> ButtonInfo {
        ButtonInfo { index, action_types: vec![0, 1, 2, 3, 4], action: describe(kind, &value), raw_type: kind, raw_value: value }
    }

    fn profile(index: u32, disabled: bool, active: bool) -> ProfileInfo {
        let res = |i, dpi| ResolutionInfo {
            index: i, dpi: Some(dpi), is_active: i == 1, is_default: i == 1, is_disabled: false,
            capabilities: vec![2], supported_dpi: vec![1000, 1800, 2000, 2400],
        };
        ProfileInfo {
            index, disabled, is_active: active, report_rate: 1000, report_rates: vec![125, 250, 500, 1000],
            capabilities: vec![102],
            buttons: vec![
                button(0, 1, Some(RawValue::Number(1))),
                button(1, 2, Some(RawValue::Number((1 << 30) + 11))),
                button(2, 4, Some(RawValue::MacroEvents(vec![[1, 183], [2, 183]]))),
                button(3, 3, Some(RawValue::Number(30))),
                button(4, 0, None),
                button(5, 4, Some(RawValue::MacroEvents(vec![[1, 29], [1, 46], [2, 46], [2, 29]]))),
            ],
            resolutions: vec![res(0, 1000), res(1, 1800), res(2, 2400)],
            leds: vec![
                LedInfo { index: 0, mode: 1, modes: vec![0, 1, 2, 3], color: "0000ff".into(), brightness: 255, color_depth: 1 },
                LedInfo { index: 1, mode: 1, modes: vec![0, 1, 2, 3], color: "0000ff".into(), brightness: 255, color_depth: 1 },
            ],
        }
    }

    fn device() -> Snapshot {
        Snapshot {
            name: "Test Mouse".into(),
            model: "usb:046d:c08b:0".into(),
            profiles: vec![profile(0, false, true), profile(1, false, false), profile(2, true, false)],
        }
    }

    fn cfg(text: &str) -> Config {
        Config::parse(text).unwrap()
    }

    #[test]
    fn exported_config_applies_as_an_empty_diff() {
        let dev = device();
        let text = export(&dev, &Config::default()).unwrap();
        let parsed = Config::parse(&text).unwrap_or_else(|e| panic!("{e:#}\n{text}"));
        let target = overlay(&dev, &parsed).unwrap();
        assert!(plan(&dev, &target).unwrap().is_empty());
    }

    #[test]
    fn export_skips_the_daemons_slot_and_comments_what_it_cannot_spell() {
        let text = export(&device(), &Config::default()).unwrap();
        assert!(!text.contains("[profiles.0.resolutions.1]"), "{text}");
        assert!(text.contains("[profiles.0.resolutions.2]"));
        assert!(!text.contains("[profiles.1.resolutions.1]"));
        assert!(text.contains("onboard = \"special resolution-alternate\""));
        assert!(text.contains("onboard = \"macro KEY_F13\""));
        assert!(text.contains("# profiles.0.buttons.5: macro ["), "multi-event macro becomes a comment");
        assert!(text.contains("[profiles.2]\nenabled = false"));
        assert!(!text.contains("[profiles.2.buttons"), "disabled profiles only record that they are disabled");
    }

    #[test]
    fn only_named_things_change() {
        let dev = device();
        let c = cfg("[profiles.1]\ncolor = \"ff0000\"\n[profiles.1.buttons.0]\nonboard = \"button 2\"");
        let p = plan(&dev, &overlay(&dev, &c).unwrap()).unwrap();
        let lines: Vec<_> = p.iter().map(|p| p.summary.as_str()).collect();
        assert_eq!(lines.len(), 3, "{lines:?}"); // 2 LEDs + 1 button, nothing else
        assert!(lines.iter().all(|l| l.starts_with("profile 1 ")));
        assert!(plan(&dev, &overlay(&dev, &cfg("")).unwrap()).unwrap().is_empty());
    }

    #[test]
    fn with_button_changes_only_that_button() {
        let dev = device();
        let t = with_button(&dev, 1, 3, &["special", "wheel-left"]).unwrap();
        let p = plan(&dev, &t).unwrap();
        assert_eq!(p.len(), 1);
        assert!(p[0].summary.contains("profile 1 button 3: key KEY_A -> special wheel-left"), "{}", p[0].summary);
        assert!(plan(&dev, &with_button(&dev, 1, 3, &["key", "a"]).unwrap()).unwrap().is_empty());
        assert!(with_button(&dev, 1, 99, &["none"]).is_err());
        assert!(with_button(&dev, 9, 0, &["none"]).is_err());
        assert!(with_button(&dev, 0, 0, &["special", "nope"]).is_err());
    }

    #[test]
    fn with_led_changes_only_what_is_given() {
        let dev = device();
        let t = with_led(&dev, 0, 1, Some(3), Some("#00FF00"), None).unwrap();
        let p = plan(&dev, &t).unwrap();
        assert_eq!(p.len(), 1);
        assert!(p[0].summary.contains("LED 1: mode 1 -> 3, color 0000ff -> 00ff00"), "{}", p[0].summary);
        assert!(!p[0].summary.contains("brightness"));
        assert!(plan(&dev, &with_led(&dev, 0, 0, None, None, None).unwrap()).unwrap().is_empty());
        assert!(with_led(&dev, 0, 0, None, Some("red"), None).is_err());
        assert!(with_led(&dev, 0, 0, None, None, Some(256)).is_err());
        assert!(with_led(&dev, 0, 9, None, None, None).is_err());
        // an unsupported mode is caught by the plan
        assert!(plan(&dev, &with_led(&dev, 0, 0, Some(9), None, None).unwrap()).is_err());
    }

    #[test]
    fn with_report_rate_is_validated_by_the_plan() {
        let dev = device();
        let ok = plan(&dev, &with_report_rate(&dev, 0, 500).unwrap()).unwrap();
        assert_eq!(ok.len(), 1);
        assert!(plan(&dev, &with_report_rate(&dev, 0, 333).unwrap()).is_err());
        assert!(with_report_rate(&dev, 9, 500).is_err());
    }

    #[test]
    fn with_profile_disabled_follows_the_safety_rules() {
        let dev = device();
        assert!(with_profile_disabled(&dev, &[0, 1], 1, true).is_err(), "daemon-synced");
        assert!(with_profile_disabled(&dev, &[], 0, true).is_err(), "active");
        assert!(with_profile_disabled(&dev, &[0, 1], 2, true).is_ok(), "already disabled");
        let p = plan(&dev, &with_profile_disabled(&dev, &[0, 1], 2, false).unwrap()).unwrap();
        assert_eq!(p.len(), 1);
        assert!(p[0].summary.contains("profile 2: disabled -> enabled"), "{}", p[0].summary);
        assert!(with_profile_disabled(&dev, &[], 9, true).is_err());
    }

    #[test]
    fn with_slot_protects_the_daemons_slot() {
        let dev = device();
        let cfg = Config::default(); // shared slot 1, profiles [0, 1]
        assert!(with_slot(&dev, &cfg, 0, 1, Some(2000), None).is_err());
        assert!(with_slot(&dev, &cfg, 2, 1, Some(2000), None).is_ok(), "profile 2 is not synced");
        let p = plan(&dev, &with_slot(&dev, &cfg, 0, 2, Some(2000), Some(false)).unwrap()).unwrap();
        assert_eq!(p.len(), 2, "{:?}", p.iter().map(|p| &p.summary).collect::<Vec<_>>());
        assert!(with_slot(&dev, &cfg, 0, 9, Some(2000), None).is_err());
        assert!(plan(&dev, &with_slot(&dev, &cfg, 0, 2, Some(1234), None).unwrap()).is_err(), "unsupported DPI");
    }

    #[test]
    fn per_led_setting_beats_the_profile_shorthand() {
        let dev = device();
        let c = cfg("[profiles.0]\ncolor = \"ff0000\"\n[profiles.0.leds.1]\ncolor = \"00ff00\"\nmode = \"breathing\"");
        let t = overlay(&dev, &c).unwrap();
        assert_eq!(t.profiles[0].leds[0].color, "ff0000");
        assert_eq!(t.profiles[0].leds[1].color, "00ff00");
        assert_eq!(t.profiles[0].leds[1].mode, 3);
        assert_eq!(t.profiles[0].leds[0].mode, 1);
    }

    #[test]
    fn colour_spelling_is_normalised() {
        let dev = device();
        let same = cfg("[profiles.0]\ncolor = \"#0000FF\"");
        assert!(plan(&dev, &overlay(&dev, &same).unwrap()).unwrap().is_empty());
    }

    #[test]
    fn button_and_slot_edits() {
        let dev = device();
        let c = cfg(
            "[profiles.0.buttons.3]\nonboard = \"key KEY_B\"\n[profiles.0.resolutions.2]\ndpi = 2000\nenabled = false\n[profiles.0]\nreport_rate = 250",
        );
        let p = plan(&dev, &overlay(&dev, &c).unwrap()).unwrap();
        let text: Vec<_> = p.iter().map(|p| p.summary.clone()).collect();
        assert_eq!(text.len(), 4, "{text:?}");
        assert!(text.iter().any(|t| t.contains("key KEY_A -> key KEY_B")));
        assert!(text.iter().any(|t| t.contains("2400 -> 2000 DPI")));
        assert!(text.iter().any(|t| t.contains("report rate 1000 -> 250")));
    }

    #[test]
    fn refuses_things_the_device_does_not_have() {
        let dev = device();
        for bad in [
            "[profiles.9]\nenabled = true",
            "[profiles.0.buttons.99]\nonboard = \"none\"",
            "[profiles.0.leds.5]\nmode = \"on\"",
            "[profiles.0.resolutions.9]\ndpi = 1000",
        ] {
            assert!(overlay(&dev, &cfg(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn safety_rules_apply_to_enabled_false() {
        let dev = device();
        // profiles 0 and 1 are the daemon's
        assert!(overlay(&dev, &cfg("[profiles.1]\nenabled = false")).is_err());
        // an already-disabled profile may be stated as disabled (even if ratbagd calls it active)
        assert!(plan(&dev, &overlay(&dev, &cfg("[profiles.2]\nenabled = false")).unwrap()).unwrap().is_empty());
        // and enabling it is a normal change
        let p = plan(&dev, &overlay(&dev, &cfg("[profiles.2]\nenabled = true")).unwrap()).unwrap();
        assert_eq!(p.len(), 1);
    }

    #[test]
    fn unsupported_values_are_caught_by_the_plan() {
        let dev = device();
        let t = overlay(&dev, &cfg("[profiles.0.resolutions.2]\ndpi = 3300")).unwrap();
        assert!(plan(&dev, &t).is_err());
    }
}
