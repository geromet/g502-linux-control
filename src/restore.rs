//! Restore a `g502ctl backup` snapshot: compare it with the device, validate
//! every difference against what the device says it supports, and stage it.
//! `plan` is pure (no I/O) so it is unit-tested without hardware.
//!
//! Restored: profile enabled/disabled, report rate, every resolution slot's DPI
//! and disabled flag, button mappings, LED mode/colour/brightness.
//! Not restored: which profile / resolution is active or default (those are
//! immediate actions in ratbagd, not staged properties), and LED effect duration.

use crate::config::parse_color;
use crate::ratbag::{Ratbag, RawValue, Snapshot};
use anyhow::{Result, bail, ensure};

#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    ProfileDisabled { profile: u32, to: bool },
    ReportRate { profile: u32, to: u32 },
    Dpi { profile: u32, slot: u32, to: u32 },
    SlotDisabled { profile: u32, slot: u32, to: bool },
    Button { profile: u32, button: u32, kind: u32, value: Option<RawValue> },
    Led { profile: u32, led: u32, mode: Option<u32>, color: Option<String>, brightness: Option<u32> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Planned {
    pub change: Change,
    /// One human-readable line: what changes, from -> to.
    pub summary: String,
}

/// Everything that would have to change to turn `current` into `target`.
/// Errors if the target is for a different device/shape or asks for something
/// the device does not support; nothing is planned in that case.
pub fn plan(current: &Snapshot, target: &Snapshot) -> Result<Vec<Planned>> {
    ensure!(
        current.model == target.model,
        "snapshot is for {} ({}), connected device is {} ({})",
        target.name, target.model, current.name, current.model
    );
    ensure!(
        current.profiles.len() == target.profiles.len(),
        "snapshot has {} profiles, device has {}",
        target.profiles.len(), current.profiles.len()
    );

    let mut out = Vec::new();
    let mut push = |change, summary: String| out.push(Planned { change, summary });

    for (c, t) in current.profiles.iter().zip(&target.profiles) {
        let p = c.index;
        ensure!(t.index == p, "snapshot profile order differs at profile {p}");
        ensure!(
            c.buttons.len() == t.buttons.len() && c.resolutions.len() == t.resolutions.len() && c.leds.len() == t.leds.len(),
            "profile {p}: snapshot has a different number of buttons/resolutions/LEDs than the device"
        );

        if c.disabled != t.disabled {
            let word = |d| if d { "disabled" } else { "enabled" };
            push(
                Change::ProfileDisabled { profile: p, to: t.disabled },
                format!("profile {p}: {} -> {}", word(c.disabled), word(t.disabled)),
            );
        }
        if c.report_rate != t.report_rate {
            ensure!(c.report_rates.contains(&t.report_rate), "profile {p}: {} Hz is not a supported report rate", t.report_rate);
            push(
                Change::ReportRate { profile: p, to: t.report_rate },
                format!("profile {p}: report rate {} -> {} Hz", c.report_rate, t.report_rate),
            );
        }

        for (cr, tr) in c.resolutions.iter().zip(&t.resolutions) {
            let slot = cr.index;
            if let Some(dpi) = tr.dpi
                && cr.dpi != Some(dpi)
            {
                ensure!(cr.supported_dpi.contains(&dpi), "profile {p} slot {slot}: {dpi} DPI is not supported");
                let from = cr.dpi.map_or("?".into(), |d| d.to_string());
                push(Change::Dpi { profile: p, slot, to: dpi }, format!("profile {p} resolution {slot}: {from} -> {dpi} DPI"));
            }
            if cr.is_disabled != tr.is_disabled {
                push(
                    Change::SlotDisabled { profile: p, slot, to: tr.is_disabled },
                    format!("profile {p} resolution {slot}: disabled {} -> {}", cr.is_disabled, tr.is_disabled),
                );
            }
        }

        for (cb, tb) in c.buttons.iter().zip(&t.buttons) {
            if cb.raw_type == tb.raw_type && (tb.raw_type == 0 || cb.raw_value == tb.raw_value) {
                continue;
            }
            ensure!(
                cb.action_types.contains(&tb.raw_type),
                "profile {p} button {}: action type {} is not supported (supported: {:?})",
                cb.index, tb.raw_type, cb.action_types
            );
            match (tb.raw_type, &tb.raw_value) {
                (0, _) | (1..=3, Some(RawValue::Number(_))) | (4, Some(RawValue::MacroEvents(_))) => {}
                (t, v) => bail!("profile {p} button {}: malformed value {v:?} for action type {t}", cb.index),
            }
            push(
                Change::Button { profile: p, button: cb.index, kind: tb.raw_type, value: tb.raw_value.clone() },
                format!("profile {p} button {}: {} -> {}", cb.index, cb.action, tb.action),
            );
        }

        for (cl, tl) in c.leds.iter().zip(&t.leds) {
            let mode = (cl.mode != tl.mode).then_some(tl.mode);
            let color = (cl.color != tl.color).then(|| tl.color.clone());
            let brightness = (cl.brightness != tl.brightness).then_some(tl.brightness);
            if mode.is_none() && color.is_none() && brightness.is_none() {
                continue;
            }
            if let Some(m) = mode {
                ensure!(cl.modes.contains(&m), "profile {p} LED {}: mode {m} is not supported (supported: {:?})", cl.index, cl.modes);
            }
            if let Some(col) = &color {
                parse_color(col)?;
            }
            let mut parts = Vec::new();
            if mode.is_some() {
                parts.push(format!("mode {} -> {}", cl.mode, tl.mode));
            }
            if color.is_some() {
                parts.push(format!("color {} -> {}", cl.color, tl.color));
            }
            if brightness.is_some() {
                parts.push(format!("brightness {} -> {}", cl.brightness, tl.brightness));
            }
            push(
                Change::Led { profile: p, led: cl.index, mode, color, brightness },
                format!("profile {p} LED {}: {}", cl.index, parts.join(", ")),
            );
        }
    }
    Ok(out)
}

/// Rules for disabling a profile. The first failing rule is the one reported.
pub fn check_can_disable(snap: &Snapshot, dpi_profiles: &[u32], profile: u32) -> Result<()> {
    let p = snap
        .profiles
        .get(profile as usize)
        .ok_or_else(|| anyhow::anyhow!("device has no profile {profile}"))?;
    ensure!(
        !dpi_profiles.contains(&profile),
        "profile {profile} is in dpi.profiles, so g502d keeps its DPI in sync; remove it from the config first"
    );
    ensure!(!p.is_active, "profile {profile} is the active profile; switch to another profile first");
    let enabled = snap.profiles.iter().filter(|q| !q.disabled).count();
    ensure!(p.disabled || enabled > 1, "refusing to disable the only enabled profile");
    Ok(())
}

/// Stage every planned change. Does not commit. Enabling a profile is staged
/// before edits to it, disabling one after, so a profile is never edited while
/// it would be disabled by this same restore.
pub fn stage(rb: &Ratbag, plan: &[Planned]) -> Result<()> {
    let enabling = |c: &Change| matches!(c, Change::ProfileDisabled { to: false, .. });
    let disabling = |c: &Change| matches!(c, Change::ProfileDisabled { to: true, .. });
    let ordered = plan
        .iter()
        .filter(|p| enabling(&p.change))
        .chain(plan.iter().filter(|p| !enabling(&p.change) && !disabling(&p.change)))
        .chain(plan.iter().filter(|p| disabling(&p.change)));
    for p in ordered {
        match &p.change {
            Change::ProfileDisabled { profile, to } => rb.stage_profile_disabled(*profile, *to)?,
            Change::ReportRate { profile, to } => rb.stage_report_rate(*profile, *to)?,
            Change::Dpi { profile, slot, to } => rb.stage_slot_dpi(*profile, *slot, *to)?,
            Change::SlotDisabled { profile, slot, to } => rb.stage_slot_disabled(*profile, *slot, *to)?,
            Change::Button { profile, button, kind, value } => rb.stage_button(*profile, *button, *kind, value.as_ref())?,
            Change::Led { profile, led, mode, color, brightness } => {
                let rgb = color.as_deref().map(parse_color).transpose()?;
                rb.stage_led(*profile, *led, *mode, rgb, *brightness)?
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ratbag::{ButtonInfo, LedInfo, ProfileInfo, ResolutionInfo};

    fn button(index: u32, kind: u32, value: Option<RawValue>, action: &str) -> ButtonInfo {
        ButtonInfo { index, action_types: vec![0, 1, 2, 3, 4], action: action.into(), raw_type: kind, raw_value: value }
    }

    fn sample() -> Snapshot {
        Snapshot {
            name: "Test Mouse".into(),
            model: "usb:046d:c08b:0".into(),
            profiles: vec![ProfileInfo {
                index: 0,
                disabled: false,
                is_active: true,
                report_rate: 1000,
                report_rates: vec![125, 250, 500, 1000],
                capabilities: vec![102],
                buttons: vec![
                    button(0, 1, Some(RawValue::Number(1)), "mouse button 1"),
                    button(1, 4, Some(RawValue::MacroEvents(vec![[1, 183], [2, 183]])), "macro KEY_F13"),
                ],
                resolutions: vec![ResolutionInfo {
                    index: 0,
                    dpi: Some(1800),
                    is_active: true,
                    is_default: true,
                    is_disabled: false,
                    capabilities: vec![2],
                    supported_dpi: vec![1000, 1800, 2400],
                }],
                leds: vec![LedInfo { index: 0, mode: 1, modes: vec![0, 1, 2, 3], color: "0000ff".into(), brightness: 255, color_depth: 1 }],
            }],
        }
    }

    #[test]
    fn identical_snapshot_plans_nothing() {
        assert!(plan(&sample(), &sample()).unwrap().is_empty());
    }

    #[test]
    fn toml_round_trip_is_lossless() {
        let s = sample();
        let text = toml::to_string_pretty(&s).unwrap();
        let back: Snapshot = toml::from_str(&text).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn none_button_without_value_round_trips() {
        let mut s = sample();
        s.profiles[0].buttons[0] = button(0, 0, None, "none");
        let back: Snapshot = toml::from_str(&toml::to_string_pretty(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn plans_each_kind_of_difference() {
        let cur = sample();
        let mut t = sample();
        t.profiles[0].report_rate = 500;
        t.profiles[0].resolutions[0].dpi = Some(2400);
        t.profiles[0].buttons[0] = button(0, 3, Some(RawValue::Number(30)), "key KEY_A");
        t.profiles[0].leds[0].color = "ff0000".into();
        t.profiles[0].leds[0].brightness = 100;
        let p = plan(&cur, &t).unwrap();
        let changes: Vec<_> = p.iter().map(|p| p.change.clone()).collect();
        assert_eq!(
            changes,
            vec![
                Change::ReportRate { profile: 0, to: 500 },
                Change::Dpi { profile: 0, slot: 0, to: 2400 },
                Change::Button { profile: 0, button: 0, kind: 3, value: Some(RawValue::Number(30)) },
                Change::Led { profile: 0, led: 0, mode: None, color: Some("ff0000".into()), brightness: Some(100) },
            ]
        );
        assert!(p[2].summary.contains("mouse button 1 -> key KEY_A"), "{}", p[2].summary);
    }

    #[test]
    fn unchanged_parts_are_not_planned() {
        let mut t = sample();
        t.profiles[0].leds[0].color = "00ff00".into();
        assert_eq!(plan(&sample(), &t).unwrap().len(), 1);
    }

    #[test]
    fn refuses_other_models() {
        let mut t = sample();
        t.model = "usb:046d:c539:0".into();
        assert!(plan(&sample(), &t).unwrap_err().to_string().contains("snapshot is for"));
    }

    #[test]
    fn refuses_different_shape() {
        let mut t = sample();
        t.profiles[0].buttons.pop();
        assert!(plan(&sample(), &t).is_err());
        let mut t = sample();
        t.profiles.push(t.profiles[0].clone());
        assert!(plan(&sample(), &t).is_err());
    }

    #[test]
    fn refuses_values_the_device_does_not_support() {
        let mut t = sample();
        t.profiles[0].resolutions[0].dpi = Some(1234);
        assert!(plan(&sample(), &t).is_err());
        let mut t = sample();
        t.profiles[0].report_rate = 333;
        assert!(plan(&sample(), &t).is_err());
        let mut t = sample();
        t.profiles[0].leds[0].mode = 9;
        assert!(plan(&sample(), &t).is_err());
        let mut t = sample();
        t.profiles[0].leds[0].color = "nope".into();
        assert!(plan(&sample(), &t).is_err());
        let mut cur = sample();
        cur.profiles[0].buttons[0].action_types = vec![1];
        let mut t = sample();
        t.profiles[0].buttons[0] = button(0, 4, Some(RawValue::MacroEvents(vec![])), "macro");
        assert!(plan(&cur, &t).is_err());
    }

    fn three_profiles() -> Snapshot {
        let mut s = sample();
        for (i, (disabled, active)) in [(false, true), (false, false), (true, false)].into_iter().enumerate() {
            let mut p = s.profiles[0].clone();
            p.index = i as u32;
            p.disabled = disabled;
            p.is_active = active;
            if i == 0 {
                s.profiles.clear();
            }
            s.profiles.push(p);
        }
        s
    }

    #[test]
    fn disable_rules() {
        let s = three_profiles();
        assert!(check_can_disable(&s, &[], 1).is_ok());
        assert!(check_can_disable(&s, &[], 2).is_ok(), "already disabled is fine");
        let err = |p, dpi: &[u32]| check_can_disable(&s, dpi, p).unwrap_err().to_string();
        assert!(err(0, &[]).contains("active profile"));
        assert!(err(1, &[1]).contains("dpi.profiles"));
        assert!(err(0, &[0]).contains("dpi.profiles"), "sync rule is reported before the active rule");
        assert!(err(7, &[]).contains("no profile 7"));
    }

    #[test]
    fn never_disables_the_last_enabled_profile() {
        let mut s = three_profiles();
        s.profiles[1].disabled = true; // only profile 0 (active) is left
        s.profiles[0].is_active = false;
        assert!(check_can_disable(&s, &[], 0).unwrap_err().to_string().contains("only enabled profile"));
    }

    #[test]
    fn refuses_malformed_button_values() {
        let mut t = sample();
        t.profiles[0].buttons[0] = button(0, 4, Some(RawValue::Number(3)), "macro?");
        assert!(plan(&sample(), &t).is_err());
        let mut t = sample();
        t.profiles[0].buttons[0] = button(0, 1, None, "button?");
        assert!(plan(&sample(), &t).is_err());
    }
}
