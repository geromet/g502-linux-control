//! TOML config. A missing file means the defaults below (the tested setup).

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Debug, Clone, Deserialize, PartialEq)]
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

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProfileConfig {
    /// RRGGBB. Only compared against the device by `g502ctl check` for now.
    pub color: String,
}

impl Default for Config {
    fn default() -> Self {
        let color = |c: &str| ProfileConfig { color: c.into() };
        Config {
            device: DeviceConfig::default(),
            dpi: DpiConfig::default(),
            profiles: BTreeMap::from([(0, color("0000ff")), (1, color("ff0000"))]),
        }
    }
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
        for (i, p) in self.profiles.iter() {
            parse_color(&p.color).with_context(|| format!("profiles.{i}.color"))?;
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
        assert_eq!(cfg.profiles[&1].color, "#ff0000");
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
