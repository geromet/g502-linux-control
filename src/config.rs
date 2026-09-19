//! TOML config. A missing file means the defaults below (the tested setup).

use crate::ratbag::{parse_action, parse_led_mode};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{collections::BTreeMap, path::PathBuf};

/// No profiles by default: without a config file `g502ctl apply` has nothing to write.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub device: DeviceConfig,
    pub dpi: DpiConfig,
    pub profiles: BTreeMap<u32, ProfileConfig>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct DeviceConfig {
    pub vendor: u16,
    pub product: u16,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct DpiConfig {
    /// Ascending DPI stages.
    pub values: Vec<u32>,
    /// Resolution slot that holds the shared DPI in every synced profile.
    pub shared_slot: u32,
    /// Profiles whose shared slot is kept in sync.
    pub profiles: Vec<u32>,
}

/// Everything is optional: `g502ctl apply` only touches what is named here.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileConfig {
    pub enabled: Option<bool>,
    pub report_rate: Option<u32>,
    /// RRGGBB shorthand for the colour of every LED in this profile
    /// (a per-LED `color` below wins).
    pub color: Option<String>,
    pub leds: BTreeMap<u32, LedConfig>,
    pub buttons: BTreeMap<u32, ButtonConfig>,
    /// Resolution slots. The shared DPI slot of a profile listed in
    /// `dpi.profiles` belongs to the daemon and cannot be set here.
    pub resolutions: BTreeMap<u32, ResolutionConfig>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct LedConfig {
    /// off | on | cycle | breathing
    pub mode: Option<String>,
    pub color: Option<String>,
    pub brightness: Option<u32>,
}

/// A button action stored in the mouse. Host-side actions (run by a daemon)
/// will get their own key later; `onboard` keeps the two apart.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ButtonConfig {
    /// none | button N | special NAME | key KEY_X | macro KEY_X
    pub onboard: String,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ResolutionConfig {
    pub dpi: Option<u32>,
    pub enabled: Option<bool>,
}

impl Default for DeviceConfig {
    fn default() -> Self {
        DeviceConfig { vendor: 0x046d, product: 0xc08b }
    }
}

impl Default for DpiConfig {
    fn default() -> Self {
        DpiConfig {
            values: vec![1000, 1800, 2400, 3200, 4000],
            shared_slot: 1,
            profiles: vec![0, 1],
        }
    }
}

impl Config {
    pub fn parse(text: &str) -> Result<Config> {
        let cfg: Config = toml::from_str(text)?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn default_path() -> PathBuf {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(|| PathBuf::from(".config"));
        base.join("g502-linux-control/config.toml")
    }

    /// Load `$G502_CONFIG` or the default path; defaults if the file is absent.
    pub fn load() -> Result<(Config, Option<PathBuf>)> {
        let path = std::env::var_os("G502_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(Config::default_path);
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let cfg = Config::parse(&text).with_context(|| format!("{}", path.display()))?;
                Ok((cfg, Some(path)))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((Config::default(), None)),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    fn validate(&self) -> Result<()> {
        let v = &self.dpi.values;
        if v.is_empty() {
            bail!("dpi.values must not be empty");
        }
        if v.contains(&0) || v.windows(2).any(|w| w[0] >= w[1]) {
            bail!("dpi.values must be positive and strictly ascending, got {v:?}");
        }
        if self.dpi.profiles.is_empty() {
            bail!("dpi.profiles must list at least one profile");
        }
        for (i, p) in &self.profiles {
            let at = |what: &str| format!("profiles.{i}.{what}");
            if let Some(c) = &p.color {
                parse_color(c).with_context(|| at("color"))?;
            }
            if let Some(hz) = p.report_rate
                && ![125, 250, 500, 1000].contains(&hz)
            {
                bail!("{}: {hz} is not a report rate (125, 250, 500 or 1000)", at("report_rate"));
            }
            for (l, led) in &p.leds {
                if let Some(m) = &led.mode {
                    parse_led_mode(m).with_context(|| at(&format!("leds.{l}.mode")))?;
                }
                if let Some(c) = &led.color {
                    parse_color(c).with_context(|| at(&format!("leds.{l}.color")))?;
                }
                if led.brightness.is_some_and(|b| b > 255) {
                    bail!("{}: brightness must be 0-255", at(&format!("leds.{l}.brightness")));
                }
            }
            for (b, btn) in &p.buttons {
                let words: Vec<&str> = btn.onboard.split_whitespace().collect();
                parse_action(&words).with_context(|| at(&format!("buttons.{b}.onboard")))?;
            }
            if let Some(slot) = p.resolutions.keys().find(|&&s| s == self.dpi.shared_slot)
                && self.dpi.profiles.contains(i)
            {
                bail!("{}: slot {slot} holds the shared DPI managed by g502d; set dpi.values instead", at(&format!("resolutions.{slot}")));
            }
        }
        Ok(())
    }
}

/// "RRGGBB" (optionally with a leading '#') to (r, g, b).
pub fn parse_color(s: &str) -> Result<(u8, u8, u8)> {
    let s = s.strip_prefix('#').unwrap_or(s);
    if s.len() != 6 || !s.is_ascii() {
        bail!("color must be RRGGBB, got {s:?}");
    }
    let byte = |i| u8::from_str_radix(&s[i..i + 2], 16).with_context(|| format!("bad color {s:?}"));
    Ok((byte(0)?, byte(2)?, byte(4)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_is_the_defaults() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
    }

    #[test]
    fn full_example_parses() {
        let cfg = Config::parse(
            r##"
            [device]
            vendor = 0x046d
            product = 0xc08b

            [dpi]
            values = [800, 1600]
            shared_slot = 2
            profiles = [0]

            [profiles.0]
            color = "0000ff"

            [profiles.1]
            color = "#ff0000"
            "##,
        )
        .unwrap();
        assert_eq!(cfg.device, DeviceConfig { vendor: 0x046d, product: 0xc08b });
        assert_eq!(cfg.dpi.values, vec![800, 1600]);
        assert_eq!(cfg.dpi.shared_slot, 2);
        assert_eq!(cfg.dpi.profiles, vec![0]);
        assert_eq!(cfg.profiles[&1].color.as_deref(), Some("#ff0000"));
    }

    #[test]
    fn partial_file_keeps_other_defaults() {
        let cfg = Config::parse("[dpi]\nvalues = [400, 800]\n").unwrap();
        assert_eq!(cfg.dpi.values, vec![400, 800]);
        assert_eq!(cfg.dpi.shared_slot, 1);
        assert_eq!(cfg.device, DeviceConfig::default());
    }

    #[test]
    fn rejects_bad_values() {
        assert!(Config::parse("[dpi]\nvalues = []").is_err());
        assert!(Config::parse("[dpi]\nvalues = [1800, 1000]").is_err());
        assert!(Config::parse("[dpi]\nvalues = [1000, 1000]").is_err());
        assert!(Config::parse("[dpi]\nvalues = [0, 1000]").is_err());
        assert!(Config::parse("[dpi]\nprofiles = []").is_err());
        assert!(Config::parse("[profiles.0]\ncolor = \"blue\"").is_err());
    }

    #[test]
    fn expansive_profile_config_parses() {
        let cfg = Config::parse(
            r#"
            [profiles.1]
            enabled = true
            report_rate = 500
            color = "ff0000"

            [profiles.1.leds.0]
            mode = "breathing"
            brightness = 200

            [profiles.1.buttons.6]
            onboard = "macro KEY_F13"

            [profiles.1.buttons.5]
            onboard = "special resolution-alternate"

            [profiles.1.resolutions.2]
            dpi = 2400
            enabled = true
            "#,
        )
        .unwrap();
        let p = &cfg.profiles[&1];
        assert_eq!((p.enabled, p.report_rate), (Some(true), Some(500)));
        assert_eq!(p.leds[&0].mode.as_deref(), Some("breathing"));
        assert_eq!(p.buttons[&6].onboard, "macro KEY_F13");
        assert_eq!(p.resolutions[&2].dpi, Some(2400));
    }

    #[test]
    fn rejects_bad_profile_settings() {
        for bad in [
            "[profiles.0]\nreport_rate = 333",
            "[profiles.0.leds.0]\nmode = \"blink\"",
            "[profiles.0.leds.0]\ncolor = \"red\"",
            "[profiles.0.leds.0]\nbrightness = 300",
            "[profiles.0.buttons.1]\nonboard = \"special nope\"",
            "[profiles.0.buttons.1]\nonboard = \"key KEY_NOT_A_KEY\"",
            "[profiles.0.buttons.1]\nhost = \"x\"",
            "[profiles.0.buttons.1]",
            // shared slot of a synced profile belongs to the daemon
            "[profiles.0.resolutions.1]\ndpi = 2000",
            "[profiles.0]\nunknown = 1",
        ] {
            assert!(Config::parse(bad).is_err(), "should reject: {bad}");
        }
        // ...but other slots, and slots of profiles the daemon does not sync, are fine
        assert!(Config::parse("[profiles.0.resolutions.2]\ndpi = 2000").is_ok());
        assert!(Config::parse("[profiles.3.resolutions.1]\ndpi = 2000").is_ok());
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(Config::parse("[dpi]\nvalus = [1000]").is_err());
    }

    #[test]
    fn colors() {
        assert_eq!(parse_color("ff8000").unwrap(), (255, 128, 0));
        assert_eq!(parse_color("#0000ff").unwrap(), (0, 0, 255));
        assert!(parse_color("fff").is_err());
        assert!(parse_color("gg0000").is_err());
    }
}
