//! Thin client for ratbagd's D-Bus API (org.freedesktop.ratbag1, API v2).
//!
//! Object tree: Manager -> Device -> Profile -> {Resolution, Button, Led}.
//! Property writes only stage changes (Profile.IsDirty); `Device.Commit()`
//! writes everything staged to the hardware in one go.

use crate::config::Config;
use crate::dpi::DpiBackend;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{fs::File, path::PathBuf};
use zbus::{
    blocking::{Connection, Proxy, proxy::Builder},
    proxy::CacheProperties,
    zvariant::{OwnedObjectPath, OwnedValue, StructureBuilder, Value},
};

const DEST: &str = "org.freedesktop.ratbag1";
const ROOT: &str = "/org/freedesktop/ratbag1";
const MANAGER: &str = "org.freedesktop.ratbag1.Manager";
const DEVICE: &str = "org.freedesktop.ratbag1.Device";
const PROFILE: &str = "org.freedesktop.ratbag1.Profile";
const RESOLUTION: &str = "org.freedesktop.ratbag1.Resolution";
const BUTTON: &str = "org.freedesktop.ratbag1.Button";
const LED: &str = "org.freedesktop.ratbag1.Led";

pub struct Ratbag {
    conn: Connection,
    device: OwnedObjectPath,
    pub name: String,
    /// e.g. "usb:046d:c08b:0"
    pub model: String,
}

impl Ratbag {
    /// Connect to ratbagd and find the device with this USB vendor:product.
    /// (ratbagd's nicknames like "warbling-mara" are not on D-Bus and differ
    /// per system, so we match on `Device.Model`.)
    pub fn open(vendor: u16, product: u16) -> Result<Ratbag> {
        let conn = Connection::system().context("connecting to the system bus")?;
        let manager = proxy(&conn, ROOT, MANAGER)?;
        let version: i32 = manager
            .get_property("APIVersion")
            .context("ratbagd not reachable (is ratbagd.service running?)")?;
        ensure!(version == 2, "unsupported ratbagd API version {version} (expected 2)");
        let devices: Vec<OwnedObjectPath> = manager.get_property("Devices")?;

        let want = format!("usb:{vendor:04x}:{product:04x}:");
        for path in devices {
            let dev = proxy(&conn, path.as_str(), DEVICE)?;
            let model: String = dev.get_property("Model")?;
            if model.to_lowercase().starts_with(&want) {
                let name = dev.get_property("Name")?;
                return Ok(Ratbag { conn, device: path, name, model });
            }
        }
        bail!("no ratbagd device with USB id {vendor:04x}:{product:04x}")
    }

    fn get<T>(&self, path: &str, iface: &str, prop: &str) -> Result<T>
    where
        T: TryFrom<OwnedValue>,
        T::Error: Into<zbus::Error>,
    {
        proxy(&self.conn, path, iface)?
            .get_property(prop)
            .with_context(|| format!("reading {iface}.{prop} of {path}"))
    }

    pub fn profile_paths(&self) -> Result<Vec<OwnedObjectPath>> {
        self.get(self.device.as_str(), DEVICE, "Profiles")
    }

    fn resolution_paths(&self, profile: &OwnedObjectPath) -> Result<Vec<OwnedObjectPath>> {
        self.get(profile.as_str(), PROFILE, "Resolutions")
    }

    fn resolution_path(&self, profile: u32, slot: u32) -> Result<OwnedObjectPath> {
        let profiles = self.profile_paths()?;
        let p = profiles.get(profile as usize).with_context(|| format!("device has no profile {profile}"))?;
        let mut slots = self.resolution_paths(p)?;
        ensure!((slot as usize) < slots.len(), "profile {profile} has no resolution slot {slot}");
        Ok(slots.swap_remove(slot as usize))
    }

    /// DPI of one resolution slot. Only the single-value form (`u`) is handled;
    /// devices with separate X/Y DPI (`(uu)`) are refused rather than guessed at.
    pub fn slot_dpi(&self, profile: u32, slot: u32) -> Result<u32> {
        let path = self.resolution_path(profile, slot)?;
        read_dpi(self, &path)
    }

    /// Stage a DPI change (does not touch the hardware until `commit`).
    fn stage_dpi(&self, path: &OwnedObjectPath, dpi: u32) -> Result<()> {
        // The property is typed `v`, so the value we send is itself a variant.
        proxy(&self.conn, path.as_str(), RESOLUTION)?
            .set_property("Resolution", Value::Value(Box::new(Value::U32(dpi))))
            .with_context(|| format!("staging {dpi} DPI on {path}"))
    }

    /// Write everything staged to the hardware. Returns ratbagd's status code.
    pub fn commit(&self) -> Result<()> {
        let reply = proxy(&self.conn, self.device.as_str(), DEVICE)?.call_method("Commit", &())?;
        let status: u32 = reply.body().deserialize()?;
        ensure!(status == 0, "Device.Commit failed with ratbagd status {status}");
        Ok(())
    }

    // ----------------------------------------------------------------- staging
    // Everything below only stages; nothing reaches the hardware until `commit`.

    fn stage(&self, path: &str, iface: &str, prop: &str, value: Value<'_>) -> Result<()> {
        proxy(&self.conn, path, iface)?
            .set_property(prop, value)
            .with_context(|| format!("staging {iface}.{prop} on {path}"))
    }

    fn profile_path(&self, profile: u32) -> Result<OwnedObjectPath> {
        let mut all = self.profile_paths()?;
        ensure!((profile as usize) < all.len(), "device has no profile {profile}");
        Ok(all.swap_remove(profile as usize))
    }

    fn child_path(&self, profile: u32, prop: &str, index: u32, what: &str) -> Result<OwnedObjectPath> {
        let p = self.profile_path(profile)?;
        let mut all: Vec<OwnedObjectPath> = self.get(p.as_str(), PROFILE, prop)?;
        ensure!((index as usize) < all.len(), "profile {profile} has no {what} {index}");
        Ok(all.swap_remove(index as usize))
    }

    pub fn stage_profile_disabled(&self, profile: u32, disabled: bool) -> Result<()> {
        let p = self.profile_path(profile)?;
        self.stage(p.as_str(), PROFILE, "Disabled", Value::Bool(disabled))
    }

    pub fn stage_report_rate(&self, profile: u32, hz: u32) -> Result<()> {
        let p = self.profile_path(profile)?;
        self.stage(p.as_str(), PROFILE, "ReportRate", Value::U32(hz))
    }

    pub fn stage_slot_dpi(&self, profile: u32, slot: u32, dpi: u32) -> Result<()> {
        self.stage_dpi(&self.resolution_path(profile, slot)?, dpi)
    }

    pub fn stage_slot_disabled(&self, profile: u32, slot: u32, disabled: bool) -> Result<()> {
        let r = self.resolution_path(profile, slot)?;
        self.stage(r.as_str(), RESOLUTION, "IsDisabled", Value::Bool(disabled))
    }

    /// `Button.Mapping` is `(uv)`: unlike `Resolution` (typed `v`) it takes a
    /// single variant layer around the value.
    pub fn stage_button(&self, profile: u32, button: u32, kind: u32, value: Option<&RawValue>) -> Result<()> {
        let inner = match value {
            Some(RawValue::Number(n)) => Value::U32(*n),
            Some(RawValue::MacroEvents(ev)) => {
                Value::from(ev.iter().map(|[a, b]| (*a, *b)).collect::<Vec<(u32, u32)>>())
            }
            None => Value::U32(0),
        };
        let mapping = StructureBuilder::new()
            .append_field(Value::U32(kind))
            .append_field(Value::Value(Box::new(inner)))
            .build()?;
        let b = self.child_path(profile, "Buttons", button, "button")?;
        self.stage(b.as_str(), BUTTON, "Mapping", Value::Structure(mapping))
    }

    pub fn stage_led(&self, profile: u32, led: u32, mode: Option<u32>, color: Option<(u8, u8, u8)>, brightness: Option<u32>) -> Result<()> {
        let l = self.child_path(profile, "Leds", led, "LED")?;
        if let Some(m) = mode {
            self.stage(l.as_str(), LED, "Mode", Value::U32(m))?;
        }
        if let Some((r, g, b)) = color {
            self.stage(l.as_str(), LED, "Color", Value::from((u32::from(r), u32::from(g), u32::from(b))))?;
        }
        if let Some(b) = brightness {
            self.stage(l.as_str(), LED, "Brightness", Value::U32(b))?;
        }
        Ok(())
    }

    // ---------------------------------------------------------------- snapshot

    pub fn snapshot(&self) -> Result<Snapshot> {
        let mut profiles = Vec::new();
        for p in self.profile_paths()? {
            let ps = p.as_str();
            let mut buttons = Vec::new();
            for b in self.get::<Vec<OwnedObjectPath>>(ps, PROFILE, "Buttons")? {
                let bs = b.as_str();
                let (kind, value): (u32, OwnedValue) = self.get(bs, BUTTON, "Mapping")?;
                buttons.push(ButtonInfo {
                    index: self.get(bs, BUTTON, "Index")?,
                    action_types: self.get(bs, BUTTON, "ActionTypes")?,
                    action: describe_mapping(kind, &value),
                    raw_type: kind,
                    raw_value: raw_value(&value),
                });
            }
            let mut resolutions = Vec::new();
            for r in self.resolution_paths(&p)? {
                let rs = r.as_str();
                resolutions.push(ResolutionInfo {
                    index: self.get(rs, RESOLUTION, "Index")?,
                    dpi: read_dpi(self, &r).ok(),
                    is_active: self.get(rs, RESOLUTION, "IsActive")?,
                    is_default: self.get(rs, RESOLUTION, "IsDefault")?,
                    is_disabled: self.get(rs, RESOLUTION, "IsDisabled")?,
                    capabilities: self.get(rs, RESOLUTION, "Capabilities")?,
                    supported_dpi: self.get(rs, RESOLUTION, "Resolutions")?,
                });
            }
            let mut leds = Vec::new();
            for l in self.get::<Vec<OwnedObjectPath>>(ps, PROFILE, "Leds")? {
                let ls = l.as_str();
                let (r, g, b): (u32, u32, u32) = self.get(ls, LED, "Color")?;
                leds.push(LedInfo {
                    index: self.get(ls, LED, "Index")?,
                    mode: self.get(ls, LED, "Mode")?,
                    modes: self.get(ls, LED, "Modes")?,
                    color: format!("{r:02x}{g:02x}{b:02x}"),
                    brightness: self.get(ls, LED, "Brightness")?,
                    color_depth: self.get(ls, LED, "ColorDepth")?,
                });
            }
            profiles.push(ProfileInfo {
                index: self.get(ps, PROFILE, "Index")?,
                disabled: self.get(ps, PROFILE, "Disabled")?,
                is_active: self.get(ps, PROFILE, "IsActive")?,
                report_rate: self.get(ps, PROFILE, "ReportRate")?,
                report_rates: self.get(ps, PROFILE, "ReportRates")?,
                capabilities: self.get(ps, PROFILE, "Capabilities")?,
                buttons,
                resolutions,
                leds,
            });
        }
        Ok(Snapshot { name: self.name.clone(), model: self.model.clone(), profiles })
    }
}

fn proxy(conn: &Connection, path: &str, iface: &str) -> Result<Proxy<'static>> {
    Ok(Builder::new(conn)
        .destination(DEST)?
        .path(path.to_owned())?
        .interface(iface.to_owned())?
        .cache_properties(CacheProperties::No)
        .build()?)
}

fn read_dpi(rb: &Ratbag, resolution: &OwnedObjectPath) -> Result<u32> {
    let v: OwnedValue = rb.get(resolution.as_str(), RESOLUTION, "Resolution")?;
    match unwrap(&v) {
        Value::U32(dpi) => Ok(*dpi),
        other => bail!("{resolution}: DPI is not a single u32 ({other:?})"),
    }
}

/// ratbagd's `v`-typed properties reach us as variants inside variants.
fn unwrap<'a>(mut v: &'a Value<'a>) -> &'a Value<'a> {
    while let Value::Value(inner) = v {
        v = inner;
    }
    v
}

// ------------------------------------------------------------------- snapshot

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub name: String,
    pub model: String,
    pub profiles: Vec<ProfileInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProfileInfo {
    pub index: u32,
    pub disabled: bool,
    pub is_active: bool,
    pub report_rate: u32,
    pub report_rates: Vec<u32>,
    pub capabilities: Vec<u32>,
    pub buttons: Vec<ButtonInfo>,
    pub resolutions: Vec<ResolutionInfo>,
    pub leds: Vec<LedInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ButtonInfo {
    pub index: u32,
    pub action_types: Vec<u32>,
    /// Human-readable, e.g. "macro KEY_F13" or "special resolution-alternate".
    pub action: String,
    /// ratbagd action type: 0 none, 1 button, 2 special, 3 key, 4 macro.
    pub raw_type: u32,
    /// Button number / special id / key code, or macro events as
    /// [event_type, key] pairs (1 = press, 2 = release).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_value: Option<RawValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum RawValue {
    Number(u32),
    MacroEvents(Vec<[u32; 2]>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolutionInfo {
    pub index: u32,
    pub dpi: Option<u32>,
    pub is_active: bool,
    pub is_default: bool,
    pub is_disabled: bool,
    pub capabilities: Vec<u32>,
    pub supported_dpi: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LedInfo {
    pub index: u32,
    pub mode: u32,
    pub modes: Vec<u32>,
    pub color: String,
    pub brightness: u32,
    pub color_depth: u32,
}

fn raw_value(v: &OwnedValue) -> Option<RawValue> {
    Some(match unwrap(v) {
        Value::U32(n) => RawValue::Number(*n),
        Value::Array(items) => {
            // a(uu): (event type, key) pairs
            let pair = |item: &Value| match item {
                Value::Structure(s) => match s.fields() {
                    [Value::U32(a), Value::U32(b)] => Some([*a, *b]),
                    _ => None,
                },
                _ => None,
            };
            RawValue::MacroEvents(items.iter().map(pair).collect::<Option<Vec<_>>>()?)
        }
        _ => return None,
    })
}

/// libratbag's button action types and special-action ids.
pub fn describe_mapping(kind: u32, v: &OwnedValue) -> String {
    describe(kind, &raw_value(v))
}

pub fn describe(kind: u32, value: &Option<RawValue>) -> String {
    match (kind, value) {
        (0, _) => "none".into(),
        (1, Some(RawValue::Number(n))) => format!("mouse button {n}"),
        (2, Some(RawValue::Number(n))) => format!("special {}", special_name(*n)),
        (3, Some(RawValue::Number(n))) => format!("key {}", key_name(*n)),
        (4, Some(RawValue::MacroEvents(ev))) => {
            // press+release of one key reads better as just the key
            if let [[1, a], [2, b]] = ev[..]
                && a == b
            {
                return format!("macro {}", key_name(a));
            }
            let steps: Vec<String> = ev
                .iter()
                .map(|[t, k]| match t {
                    1 => format!("+{}", key_name(*k)),
                    2 => format!("-{}", key_name(*k)),
                    3 => format!("wait {k}ms"),
                    _ => format!("?{t}:{k}"),
                })
                .collect();
            format!("macro [{}]", steps.join(" "))
        }
        (t, _) => format!("unknown type {t}"),
    }
}

fn key_name(code: u32) -> String {
    match u16::try_from(code) {
        Ok(c) => format!("{:?}", evdev::KeyCode::new(c)),
        Err(_) => format!("key#{code}"),
    }
}

/// libratbag `ActionSpecial` names, in id order starting at 1<<30.
const SPECIALS: [&str; 19] = [
    "unknown", "doubleclick", "wheel-left", "wheel-right", "wheel-up", "wheel-down",
    "ratchet-mode-switch", "resolution-cycle-up", "resolution-cycle-down", "resolution-up",
    "resolution-down", "resolution-alternate", "resolution-default", "profile-cycle-up",
    "profile-cycle-down", "profile-up", "profile-down", "second-mode", "battery-level",
];

fn special_name(id: u32) -> String {
    match id.checked_sub(1 << 30).and_then(|i| SPECIALS.get(i as usize)) {
        Some(n) => (*n).into(),
        None => format!("#{id:#x}"),
    }
}

fn special_id(name: &str) -> Option<u32> {
    let i = SPECIALS.iter().skip(1).position(|n| *n == name)? + 1;
    Some((1 << 30) + i as u32)
}

/// "KEY_A", "key_a" or "a" -> evdev key code.
fn key_code(name: &str) -> Result<u32> {
    let up = name.to_uppercase();
    let full = if up.starts_with("KEY_") { up } else { format!("KEY_{up}") };
    let key: evdev::KeyCode = full.parse().map_err(|_| anyhow::anyhow!("unknown key {name:?} (use evdev names such as KEY_A, KEY_F13, KEY_LEFTCTRL)"))?;
    Ok(u32::from(key.code()))
}

/// Parse a button action given on the command line into a raw ratbagd
/// (type, value): `none`, `button N`, `special NAME`, `key KEY_X`, `macro KEY_X`.
pub fn parse_action(words: &[&str]) -> Result<(u32, Option<RawValue>)> {
    match words {
        ["none"] => Ok((0, None)),
        ["button", n] => {
            let n: u32 = n.parse().map_err(|_| anyhow::anyhow!("not a button number: {n:?}"))?;
            ensure!(n >= 1, "mouse buttons are numbered from 1");
            Ok((1, Some(RawValue::Number(n))))
        }
        ["special", name] => {
            let id = special_id(name).with_context(|| format!("unknown special action {name:?}; known: {}", SPECIALS[1..].join(", ")))?;
            Ok((2, Some(RawValue::Number(id))))
        }
        ["key", k] => Ok((3, Some(RawValue::Number(key_code(k)?)))),
        ["macro", k] => {
            let c = key_code(k)?;
            Ok((4, Some(RawValue::MacroEvents(vec![[1, c], [2, c]]))))
        }
        _ => bail!("action must be one of: none | button N | special NAME | key KEY_X | macro KEY_X"),
    }
}

/// The inverse of `parse_action`: the config/CLI spelling of a mapping, or
/// `None` if it has no such spelling (multi-event macros, unknown ids/keys).
pub fn action_string(kind: u32, value: &Option<RawValue>) -> Option<String> {
    Some(match (kind, value) {
        (0, _) => "none".into(),
        (1, Some(RawValue::Number(n))) => format!("button {n}"),
        (2, Some(RawValue::Number(n))) => {
            let name = special_name(*n);
            special_id(&name)?;
            format!("special {name}")
        }
        (3, Some(RawValue::Number(n))) => format!("key {}", parseable_key_name(*n)?),
        (4, Some(RawValue::MacroEvents(ev))) => match ev[..] {
            [[1, a], [2, b]] if a == b => format!("macro {}", parseable_key_name(a)?),
            _ => return None,
        },
        _ => return None,
    })
}

fn parseable_key_name(code: u32) -> Option<String> {
    let name = key_name(code);
    (name.starts_with("KEY_") && key_code(&name).ok() == Some(code)).then_some(name)
}

/// libratbag `Led.Mode` values (confirmed against ratbagctl's own enum).
pub const LED_MODES: [&str; 4] = ["off", "on", "cycle", "breathing"];

pub fn led_mode_name(mode: u32) -> Option<&'static str> {
    LED_MODES.get(mode as usize).copied()
}

pub fn parse_led_mode(name: &str) -> Result<u32> {
    LED_MODES.iter().position(|m| *m == name).map(|i| i as u32).with_context(|| format!("unknown LED mode {name:?}; use one of: {}", LED_MODES.join(", ")))
}

// ----------------------------------------------------------------- controller

/// Cross-process lock so the daemon and `g502ctl` never interleave a
/// read-modify-write of the shared DPI. Held until the returned file drops.
pub fn lock_writes() -> Result<File> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let path = dir.join("g502-linux-control.lock");
    let f = File::options().create(true).write(true).truncate(false).open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    f.lock()?;
    Ok(f)
}

/// A validated connection that keeps the shared DPI in sync across profiles.
/// Constructing one is the only safety gate before we write to the mouse.
pub struct Controller {
    rb: Ratbag,
    /// Resolution objects (one per synced profile) holding the shared DPI.
    slots: Vec<OwnedObjectPath>,
    /// DPI values every synced slot accepts.
    supported: Vec<u32>,
}

impl Controller {
    /// Connect and check that this is the configured device and that every
    /// configured stage is a DPI the hardware accepts. Reads only.
    pub fn connect(cfg: &Config) -> Result<Controller> {
        let rb = Ratbag::open(cfg.device.vendor, cfg.device.product)?;
        let profiles = rb.profile_paths()?;
        let mut slots = Vec::new();
        let mut supported: Option<Vec<u32>> = None;
        for &p in &cfg.dpi.profiles {
            let path = profiles.get(p as usize).with_context(|| format!("{}: no profile {p}", rb.name))?;
            let disabled: bool = rb.get(path.as_str(), PROFILE, "Disabled")?;
            ensure!(!disabled, "profile {p} is disabled on the device");
            let slot = rb.resolution_path(p, cfg.dpi.shared_slot)?;
            read_dpi(&rb, &slot).with_context(|| format!("profile {p} slot {}", cfg.dpi.shared_slot))?;
            let here: Vec<u32> = rb.get(slot.as_str(), RESOLUTION, "Resolutions")?;
            if let Some(bad) = cfg.dpi.values.iter().find(|v| !here.contains(v)) {
                bail!("{bad} DPI is not supported by profile {p} slot {}", cfg.dpi.shared_slot);
            }
            supported = Some(match supported {
                None => here,
                Some(prev) => prev.into_iter().filter(|d| here.contains(d)).collect(),
            });
            slots.push(slot);
        }
        Ok(Controller { rb, slots, supported: supported.unwrap_or_default() })
    }

    pub fn is_supported(&self, dpi: u32) -> bool {
        self.supported.contains(&dpi)
    }

    pub fn device_name(&self) -> &str {
        &self.rb.name
    }
}

impl DpiBackend for Controller {
    fn get_dpi(&mut self) -> Result<u32> {
        read_dpi(&self.rb, &self.slots[0])
    }

    fn set_dpi(&mut self, dpi: u32) -> Result<()> {
        for slot in &self.slots {
            if read_dpi(&self.rb, slot)? != dpi {
                self.rb.stage_dpi(slot, dpi)?;
            }
        }
        self.rb.commit()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_actions() {
        assert_eq!(parse_action(&["none"]).unwrap(), (0, None));
        assert_eq!(parse_action(&["button", "3"]).unwrap(), (1, Some(RawValue::Number(3))));
        assert_eq!(parse_action(&["key", "KEY_A"]).unwrap(), (3, Some(RawValue::Number(30))));
        assert_eq!(parse_action(&["key", "a"]).unwrap(), (3, Some(RawValue::Number(30))));
        assert_eq!(
            parse_action(&["macro", "KEY_F13"]).unwrap(),
            (4, Some(RawValue::MacroEvents(vec![[1, 183], [2, 183]])))
        );
    }

    #[test]
    fn special_names_match_libratbag_ids() {
        // ids from ratbagctl's ActionSpecial enum
        assert_eq!(parse_action(&["special", "resolution-alternate"]).unwrap().1, Some(RawValue::Number((1 << 30) + 11)));
        assert_eq!(parse_action(&["special", "profile-cycle-up"]).unwrap().1, Some(RawValue::Number((1 << 30) + 13)));
        assert_eq!(parse_action(&["special", "wheel-left"]).unwrap().1, Some(RawValue::Number((1 << 30) + 2)));
        for name in &SPECIALS[1..] {
            let id = special_id(name).unwrap();
            assert_eq!(special_name(id), *name);
        }
        assert!(special_id("unknown").is_none());
    }

    #[test]
    fn rejects_bad_actions() {
        assert!(parse_action(&[]).is_err());
        assert!(parse_action(&["button", "0"]).is_err());
        assert!(parse_action(&["button", "x"]).is_err());
        assert!(parse_action(&["special", "nope"]).is_err());
        assert!(parse_action(&["key", "KEY_NOPE_NOT_REAL"]).is_err());
        assert!(parse_action(&["macro"]).is_err());
        assert!(parse_action(&["none", "extra"]).is_err());
    }

    #[test]
    fn led_modes() {
        assert_eq!(parse_led_mode("off").unwrap(), 0);
        assert_eq!(parse_led_mode("on").unwrap(), 1);
        assert_eq!(parse_led_mode("cycle").unwrap(), 2);
        assert_eq!(parse_led_mode("breathing").unwrap(), 3);
        assert!(parse_led_mode("blink").is_err());
    }

    #[test]
    fn describes_actions() {
        assert_eq!(describe(1, &Some(RawValue::Number(9))), "mouse button 9");
        assert_eq!(describe(4, &Some(RawValue::MacroEvents(vec![[1, 183], [2, 183]]))), "macro KEY_F13");
        assert_eq!(
            describe(4, &Some(RawValue::MacroEvents(vec![[1, 29], [1, 46], [3, 50], [2, 46], [2, 29]]))),
            "macro [+KEY_LEFTCTRL +KEY_C wait 50ms -KEY_C -KEY_LEFTCTRL]"
        );
        assert_eq!(describe(2, &Some(RawValue::Number((1 << 30) + 13))), "special profile-cycle-up");
    }
}
