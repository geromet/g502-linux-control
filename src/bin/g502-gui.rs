//! g502-gui: view the G502's profiles, buttons, DPI and LEDs, and edit button
//! actions. Build with `--features gui`.
//!
//! It reads and writes through the same library code as `g502ctl`: an edit shows
//! the exact diff, and Apply goes through `restore::write` (lock, backup, one
//! commit, verify). Nothing is written until Apply is pressed.
//!
//! Set G502_GUI_SCREENSHOT=file.png to have the window save a picture of
//! itself (only its own contents) once it has loaded, and exit. Used to check
//! the rendering without a display server session to look at. In that mode
//! G502_GUI_PROFILE=N picks the profile tab, G502_GUI_EDIT=P:B:KIND:VALUE opens
//! an editor (`P:B:KIND:VALUE` with KIND none|button|special|key|macro,
//! `led:P:L:MODE:COLOR:BRIGHTNESS`, `rate:P:HZ`,
//! `profile:P:enable|disable` or `slot:P:S:DPI:enabled|disabled`), G502_GUI_APPLY=1
//! presses Apply once loaded, and the window is tall enough to show everything.

use g502_linux_control::{
    apply::{overlay, with_button, with_led, with_profile_disabled, with_report_rate, with_slot},
    config::{Config, parse_color},
    ratbag::{LED_MODES, ProfileInfo, Ratbag, RawValue, Snapshot, led_mode_name, parse_led_mode, special_names},
    restore::{plan, write},
};
use iced::{
    Background, Border, Color, Element, Length, Task, Theme, window,
    widget::{Space, button, checkbox, column, container, pick_list, row, scrollable, text, text_input},
};
use std::{fmt, path::PathBuf, sync::Arc, time::Duration};

fn main() -> iced::Result {
    iced::application(App::boot, App::update, App::view)
        .title("G502 Linux Control")
        .theme(theme)
        // Wayland app id: matches packaging/g502-gui.desktop so the desktop shows its icon and name.
        .settings(iced::Settings { id: Some("g502-gui".into()), ..Default::default() })
        .window_size((920.0, if std::env::var_os("G502_GUI_SCREENSHOT").is_some() { 1500.0 } else { 760.0 }))
        .run()
}

fn theme(_: &App) -> Theme {
    Theme::Dark
}

/// Everything read from the device and config in one go.
struct Data {
    cfg: Config,
    snap: Snapshot,
    /// Shared DPI (first synced profile, shared slot), if readable.
    shared_dpi: Option<u32>,
    /// How many places the device differs from `[profiles.*]` in the config.
    /// `None` = the config names no profiles; `Some(Err)` = cannot be applied.
    config_diff: Option<Result<usize, String>>,
}

enum Load {
    Loading,
    Ready(Data),
    Failed(String),
}

struct App {
    load: Load,
    selected: usize,
    screenshot: Option<PathBuf>,
    editor: Option<Edit>,
    notice: Option<(bool, String)>,
    busy: bool,
    auto_apply: bool,
}

/// The kind of button action being edited (the words `g502ctl button set` takes).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    None,
    MouseButton,
    Special,
    Key,
    Macro,
}

const KINDS: [Kind; 5] = [Kind::MouseButton, Kind::Special, Kind::Key, Kind::Macro, Kind::None];

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Kind::None => "none (disable button)",
            Kind::MouseButton => "mouse button",
            Kind::Special => "special action",
            Kind::Key => "keyboard key",
            Kind::Macro => "key macro (press+release)",
        })
    }
}

impl Kind {
    fn word(self) -> &'static str {
        match self {
            Kind::None => "none",
            Kind::MouseButton => "button",
            Kind::Special => "special",
            Kind::Key => "key",
            Kind::Macro => "macro",
        }
    }

    fn from_word(w: &str) -> Option<Kind> {
        KINDS.iter().copied().find(|k| k.word() == w)
    }

    fn default_value(self) -> &'static str {
        match self {
            Kind::None => "",
            Kind::MouseButton => "1",
            Kind::Special => special_names()[0],
            Kind::Key | Kind::Macro => "KEY_A",
        }
    }
}

#[derive(Clone)]
struct ButtonEdit {
    profile: u32,
    button: u32,
    kind: Kind,
    value: String,
}

impl ButtonEdit {
    /// The words `parse_action` understands.
    fn words(&self) -> Vec<&str> {
        match self.kind {
            Kind::None => vec!["none"],
            k => vec![k.word(), self.value.trim()],
        }
    }
}

#[derive(Clone)]
struct LedEdit {
    profile: u32,
    led: u32,
    mode: u32,
    color: String,
    brightness: String,
}

#[derive(Clone)]
struct SlotEdit {
    profile: u32,
    slot: u32,
    dpi: String,
    enabled: bool,
}

/// What is being edited. Each variant knows how to build the snapshot the
/// device should end up with; the preview and the write both start from that.
#[derive(Clone)]
enum Edit {
    Button(ButtonEdit),
    Led(LedEdit),
    Rate { profile: u32, hz: u32 },
    /// Enable or disable a whole profile.
    Profile { profile: u32, disable: bool },
    Slot(SlotEdit),
}

impl Edit {
    fn profile(&self) -> u32 {
        match self {
            Edit::Button(e) => e.profile,
            Edit::Led(e) => e.profile,
            Edit::Rate { profile, .. } | Edit::Profile { profile, .. } => *profile,
            Edit::Slot(e) => e.profile,
        }
    }

    fn title(&self) -> String {
        match self {
            Edit::Button(e) => format!("profile {} button {}", e.profile, e.button),
            Edit::Led(e) => format!("profile {} LED {}", e.profile, e.led),
            Edit::Rate { profile, .. } => format!("profile {profile} report rate"),
            Edit::Profile { profile, disable } => format!("profile {profile}: {}", if *disable { "disable" } else { "enable" }),
            Edit::Slot(e) => format!("profile {} resolution slot {}", e.profile, e.slot),
        }
    }

    fn target(&self, snap: &Snapshot, cfg: &Config) -> anyhow::Result<Snapshot> {
        match self {
            Edit::Button(e) => with_button(snap, e.profile, e.button, &e.words()),
            Edit::Led(e) => {
                let b = e.brightness.trim();
                let brightness: u32 = b.parse().map_err(|_| anyhow::anyhow!("brightness must be a number from 0 to 255, got {b:?}"))?;
                with_led(snap, e.profile, e.led, Some(e.mode), Some(&e.color), Some(brightness))
            }
            Edit::Rate { profile, hz } => with_report_rate(snap, *profile, *hz),
            Edit::Profile { profile, disable } => with_profile_disabled(snap, &cfg.dpi.profiles, *profile, *disable),
            Edit::Slot(e) => {
                let d = e.dpi.trim();
                let dpi: u32 = d.parse().map_err(|_| anyhow::anyhow!("DPI must be a whole number, got {d:?}"))?;
                with_slot(snap, cfg, e.profile, e.slot, Some(dpi), Some(e.enabled))
            }
        }
    }
}

#[derive(Clone)]
enum Message {
    Refresh,
    Loaded(Result<Arc<Data>, String>),
    Select(usize),
    Edit(u32, u32),
    EditLed(u32, u32),
    EditRate(u32),
    EditProfile(u32, bool),
    EditSlot(u32, u32),
    SlotDpi(String),
    SlotEnabled(bool),
    EditKind(Kind),
    EditValue(String),
    LedMode(u32),
    LedColor(String),
    LedBrightness(String),
    RateHz(u32),
    CancelEdit,
    ApplyEdit,
    Applied(Result<String, String>),
    Capture,
    Captured(window::Screenshot),
    WindowId(Option<window::Id>),
}

// Iced needs `Message: Clone`; Data is shared behind an Arc for that.
impl std::fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Message")
    }
}

fn read_device() -> Result<Arc<Data>, String> {
    let run = || -> anyhow::Result<Data> {
        let (cfg, _) = Config::load()?;
        let rb = Ratbag::open(cfg.device.vendor, cfg.device.product)?;
        let snap = rb.snapshot()?;
        let shared_dpi = cfg.dpi.profiles.first().and_then(|&p| rb.slot_dpi(p, cfg.dpi.shared_slot).ok());
        let config_diff = (!cfg.profiles.is_empty())
            .then(|| overlay(&snap, &cfg).and_then(|t| plan(&snap, &t)).map(|p| p.len()).map_err(|e| format!("{e:#}")));
        Ok(Data { cfg, snap, shared_dpi, config_diff })
    };
    run().map(Arc::new).map_err(|e| format!("{e:#}"))
}

impl App {
    fn boot() -> (App, Task<Message>) {
        let screenshot = std::env::var_os("G502_GUI_SCREENSHOT").map(PathBuf::from);
        let selected = std::env::var("G502_GUI_PROFILE").ok().and_then(|p| p.parse().ok()).unwrap_or(0);
        // Dev hook: G502_GUI_EDIT=P:B:KIND:VALUE | led:P:L:MODE:COLOR:BRIGHTNESS | rate:P:HZ
        let editor = std::env::var("G502_GUI_EDIT").ok().and_then(|v| parse_edit_hook(&v));
        let auto_apply = screenshot.is_some() && std::env::var_os("G502_GUI_APPLY").is_some();
        let selected = editor.as_ref().map_or(selected, |e| e.profile() as usize);
        (
            App { load: Load::Loading, selected, screenshot, editor, notice: None, busy: false, auto_apply },
            Task::perform(async { read_device() }, Message::Loaded),
        )
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Refresh => {
                self.load = Load::Loading;
                Task::perform(async { read_device() }, Message::Loaded)
            }
            Message::Loaded(result) => {
                self.load = match result {
                    Ok(data) => {
                        // Arc is only shared with this message, so this always unwraps.
                        Load::Ready(Arc::try_unwrap(data).unwrap_or_else(|_| unreachable!()))
                    }
                    Err(e) => Load::Failed(e),
                };
                if self.auto_apply {
                    self.auto_apply = false;
                    return Task::done(Message::ApplyEdit);
                }
                self.after_load()
            }
            Message::Edit(profile, button) => {
                self.notice = None;
                if let Load::Ready(d) = &self.load
                    && let Some(b) = d.snap.profiles.get(profile as usize).and_then(|p| p.buttons.get(button as usize))
                {
                    self.editor = Some(Edit::Button(editor_for(profile, b.raw_type, &b.raw_value, button)));
                }
                Task::none()
            }
            Message::EditLed(profile, led) => {
                self.notice = None;
                if let Load::Ready(d) = &self.load
                    && let Some(l) = d.snap.profiles.get(profile as usize).and_then(|p| p.leds.get(led as usize))
                {
                    self.editor = Some(Edit::Led(LedEdit { profile, led, mode: l.mode, color: l.color.clone(), brightness: l.brightness.to_string() }));
                }
                Task::none()
            }
            Message::EditRate(profile) => {
                self.notice = None;
                if let Load::Ready(d) = &self.load
                    && let Some(p) = d.snap.profiles.get(profile as usize)
                {
                    self.editor = Some(Edit::Rate { profile, hz: p.report_rate });
                }
                Task::none()
            }
            Message::EditProfile(profile, disable) => {
                self.notice = None;
                self.editor = Some(Edit::Profile { profile, disable });
                Task::none()
            }
            Message::EditSlot(profile, slot) => {
                self.notice = None;
                if let Load::Ready(d) = &self.load
                    && let Some(r) = d.snap.profiles.get(profile as usize).and_then(|p| p.resolutions.get(slot as usize))
                {
                    self.editor = Some(Edit::Slot(SlotEdit { profile, slot, dpi: r.dpi.map_or(String::new(), |v| v.to_string()), enabled: !r.is_disabled }));
                }
                Task::none()
            }
            Message::SlotDpi(v) => {
                if let Some(Edit::Slot(e)) = &mut self.editor {
                    e.dpi = v;
                }
                Task::none()
            }
            Message::SlotEnabled(v) => {
                if let Some(Edit::Slot(e)) = &mut self.editor {
                    e.enabled = v;
                }
                Task::none()
            }
            Message::EditKind(kind) => {
                if let Some(Edit::Button(e)) = &mut self.editor {
                    e.kind = kind;
                    e.value = kind.default_value().to_string();
                }
                Task::none()
            }
            Message::EditValue(v) => {
                if let Some(Edit::Button(e)) = &mut self.editor {
                    e.value = v;
                }
                Task::none()
            }
            Message::LedMode(m) => {
                if let Some(Edit::Led(e)) = &mut self.editor {
                    e.mode = m;
                }
                Task::none()
            }
            Message::LedColor(c) => {
                if let Some(Edit::Led(e)) = &mut self.editor {
                    e.color = c;
                }
                Task::none()
            }
            Message::LedBrightness(b) => {
                if let Some(Edit::Led(e)) = &mut self.editor {
                    e.brightness = b;
                }
                Task::none()
            }
            Message::RateHz(hz) => {
                if let Some(Edit::Rate { hz: cur, .. }) = &mut self.editor {
                    *cur = hz;
                }
                Task::none()
            }
            Message::CancelEdit => {
                self.editor = None;
                Task::none()
            }
            Message::ApplyEdit => {
                let Some(edit) = self.editor.clone() else { return Task::none() };
                self.busy = true;
                Task::perform(async move { write_edit(&edit) }, Message::Applied)
            }
            Message::Applied(result) => {
                self.busy = false;
                match result {
                    Ok(msg) => {
                        self.editor = None;
                        self.notice = Some((true, msg));
                    }
                    Err(e) => self.notice = Some((false, e)),
                }
                // Re-read what is really on the device.
                Task::perform(async { read_device() }, Message::Loaded)
            }
            Message::Select(i) => {
                self.selected = i;
                Task::none()
            }
            Message::Capture => window::oldest().map(Message::WindowId),
            Message::WindowId(Some(id)) => window::screenshot(id).map(Message::Captured),
            Message::WindowId(None) => iced::exit(),
            Message::Captured(shot) => {
                if let Some(path) = &self.screenshot
                    && let Err(e) = save_png(path, &shot)
                {
                    eprintln!("g502-gui: could not save screenshot: {e}");
                }
                iced::exit()
            }
        }
    }

    /// After a (re)load: in screenshot mode, capture once things have settled.
    fn after_load(&self) -> Task<Message> {
        if self.screenshot.is_some() && !self.busy {
            // Give the new state a moment to be drawn, then capture.
            return Task::perform(async { std::thread::sleep(Duration::from_millis(600)) }, |_| Message::Capture);
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let body: Element<Message> = match &self.load {
            Load::Loading => text("Reading the device from ratbagd...").into(),
            Load::Failed(e) => column![
                text("Could not read the device").size(20),
                text(e.clone()),
                text("Is ratbagd running, and is the mouse connected?").size(14),
            ]
            .spacing(8)
            .into(),
            Load::Ready(d) => self.device_view(d),
        };
        container(scrollable(container(body).padding(20).width(Length::Fill))).into()
    }

    fn device_view<'a>(&'a self, d: &'a Data) -> Element<'a, Message> {
        let header = row![
            column![
                text(d.snap.name.clone()).size(24),
                text(format!("{}   |   edits are only written when you press Apply", d.snap.model)).size(13).style(muted),
            ]
            .spacing(4),
            Space::new().width(Length::Fill),
            button("Refresh").on_press(Message::Refresh),
        ]
        .align_y(iced::Center);

        let tabs = row(d.snap.profiles.iter().enumerate().map(|(i, p)| {
            let synced = d.cfg.dpi.profiles.contains(&p.index);
            let label = format!("Profile {}{}{}", p.index, if synced { "  (DPI sync)" } else { "" }, if p.disabled { "  off" } else { "" });
            let b = button(text(label).size(14)).on_press(Message::Select(i));
            b.style(if i == self.selected { button::primary } else { button::secondary }).into()
        }))
        .spacing(8);

        let mut page = column![header, tabs].spacing(18);
        if let Some((ok, msg)) = &self.notice {
            let msg = msg.clone();
            page = page.push(container(text(msg).size(14)).padding(12).width(Length::Fill).style(if *ok { container::success } else { container::danger }));
        }
        page = page.push(self.dpi_panel(d));
        if let Some(diff) = &d.config_diff {
            page = page.push(config_line(diff));
        }
        if let Some(p) = d.snap.profiles.get(self.selected) {
            page = page.push(self.profile_panel(d, p));
        }
        page.into()
    }

    fn dpi_panel<'a>(&'a self, d: &'a Data) -> Element<'a, Message> {
        let stages = row(d.cfg.dpi.values.iter().map(|&v| {
            let current = d.shared_dpi == Some(v);
            container(text(v.to_string()).size(15))
                .padding([6, 12])
                .style(move |t: &Theme| chip(t, current))
                .into()
        }))
        .spacing(8);
        let now = match d.shared_dpi {
            Some(v) => format!("{v} DPI"),
            None => "unknown".into(),
        };
        panel(
            "Shared DPI",
            column![
                text(format!("Current: {now}   (kept in sync across profiles {:?}, slot {})", d.cfg.dpi.profiles, d.cfg.dpi.shared_slot)).size(14),
                stages,
                text("Stages are stepped by the F13/F14 buttons of the DPI profile (marked below) through g502d.").size(12).style(muted),
            ]
            .spacing(8)
            .into(),
        )
    }

    fn editor_panel<'a>(&'a self, d: &'a Data, edit: &'a Edit, synced: bool) -> Element<'a, Message> {
        let controls: Element<Message> = match edit {
            Edit::Button(e) => {
                let value: Element<Message> = match e.kind {
                    Kind::None => text("The button will do nothing.").size(14).style(muted).into(),
                    Kind::Special => pick_list(
                        special_names(),
                        special_names().iter().copied().find(|n| *n == e.value),
                        |n| Message::EditValue(n.to_string()),
                    )
                    .width(260)
                    .into(),
                    Kind::MouseButton => text_input("button number, e.g. 3", &e.value).on_input(Message::EditValue).width(260).into(),
                    Kind::Key | Kind::Macro => text_input("evdev key name, e.g. KEY_A", &e.value).on_input(Message::EditValue).width(260).into(),
                };
                row![pick_list(KINDS, Some(e.kind), Message::EditKind).width(230), value].spacing(10).align_y(iced::Center).into()
            }
            Edit::Led(e) => {
                let swatch: Element<Message> = match parse_color(&e.color) {
                    Ok((r, g, b)) => container(Space::new())
                        .width(26)
                        .height(26)
                        .style(move |_: &Theme| container::Style {
                            background: Some(Background::Color(Color::from_rgb8(r, g, b))),
                            border: Border { radius: 5.0.into(), width: 1.0, color: Color::from_rgba(1.0, 1.0, 1.0, 0.35) },
                            ..Default::default()
                        })
                        .into(),
                    Err(_) => Space::new().width(26).into(),
                };
                row![
                    text("Mode").size(14),
                    pick_list(LED_MODES, led_mode_name(e.mode), |n: &'static str| Message::LedMode(parse_led_mode(n).unwrap_or(1))).width(150),
                    text("Color").size(14),
                    text_input("RRGGBB", &e.color).on_input(Message::LedColor).width(110),
                    swatch,
                    text("Brightness").size(14),
                    text_input("0-255", &e.brightness).on_input(Message::LedBrightness).width(80),
                ]
                .spacing(10)
                .align_y(iced::Center)
                .into()
            }
            Edit::Profile { disable, .. } => text(if *disable {
                "The profile will be disabled: the mouse can no longer switch to it."
            } else {
                "The profile will be enabled with the settings it currently has."
            })
            .size(14)
            .into(),
            Edit::Slot(e) => row![
                text("DPI").size(14),
                text_input("e.g. 2400", &e.dpi).on_input(Message::SlotDpi).width(100),
                checkbox(e.enabled).label("Slot enabled").on_toggle(Message::SlotEnabled),
            ]
            .spacing(12)
            .align_y(iced::Center)
            .into(),
            Edit::Rate { profile, hz } => {
                let rates: &[u32] = d.snap.profiles.get(*profile as usize).map_or(&[], |p| &p.report_rates);
                row![text("Report rate").size(14), pick_list(rates, Some(*hz), Message::RateHz).width(120), text("Hz").size(14)]
                    .spacing(10)
                    .align_y(iced::Center)
                    .into()
            }
        };

        // Exactly what would change, computed by the same code that writes it.
        let preview = edit.target(&d.snap, &d.cfg).and_then(|t| plan(&d.snap, &t));
        let (lines, valid): (Element<Message>, bool) = match &preview {
            Ok(p) if p.is_empty() => (text("No change: the device already has this.").size(14).style(muted).into(), false),
            Ok(p) => (column(p.iter().map(|c| text(c.summary.clone()).size(14).into())).spacing(4).into(), true),
            Err(err) => (text(format!("{err:#}")).size(14).style(text::danger).into(), false),
        };

        let touches_daemon = synced
            && matches!(edit, Edit::Button(e) if d.snap
                .profiles
                .get(e.profile as usize)
                .and_then(|p| p.buttons.get(e.button as usize))
                .is_some_and(|b| daemon_role(b.raw_type, &b.raw_value).is_some()));
        let warning: Element<Message> = if touches_daemon {
            text("This button sends KEY_F13/KEY_F14 to g502d. Changing it turns off that DPI button.").size(13).style(text::danger).into()
        } else if matches!(edit, Edit::Led(_)) {
            text("LED brightness is unreliable on the G502 HERO: ratbagd may report a value the LED does not follow.").size(12).style(muted).into()
        } else {
            Space::new().into()
        };

        panel(
            &format!("Edit {}", edit.title()),
            column![
                controls,
                lines,
                warning,
                row![
                    button("Apply").on_press_maybe((valid && !self.busy).then_some(Message::ApplyEdit)),
                    button("Cancel").on_press_maybe((!self.busy).then_some(Message::CancelEdit)).style(button::secondary),
                    text(if self.busy { "Writing to the mouse..." } else { "Nothing is written until you press Apply. A backup of the current state is saved first." })
                        .size(12)
                        .style(muted),
                ]
                .spacing(10)
                .align_y(iced::Center),
            ]
            .spacing(12)
            .into(),
        )
    }

    fn profile_panel<'a>(&'a self, d: &'a Data, p: &'a ProfileInfo) -> Element<'a, Message> {
        if p.disabled {
            let editor: Option<Element<Message>> = self.editor.as_ref().filter(|e| e.profile() == p.index).map(|e| self.editor_panel(d, e, false));
            return column![
                panel(
                    &format!("Profile {}", p.index),
                    row![
                        text("Disabled on the device.").size(14).width(Length::Fill),
                        button(text("Enable").size(13))
                            .on_press_maybe((!self.busy && !matches!(&self.editor, Some(Edit::Profile { .. }))).then_some(Message::EditProfile(p.index, false))),
                    ]
                    .align_y(iced::Center)
                    .into(),
                ),
                editor.unwrap_or_else(|| Space::new().into()),
            ]
            .spacing(18)
            .into();
        }
        let synced = d.cfg.dpi.profiles.contains(&p.index);

        // Buttons
        let mut buttons = column![row![
            text("Button").width(90).style(muted),
            text("Action").width(Length::Fill).style(muted),
            text("Where it runs").width(330).style(muted),
            Space::new().width(70),
        ]]
        .spacing(6);
        for b in &p.buttons {
            let handled_by = if synced { daemon_role(b.raw_type, &b.raw_value) } else { None };
            let where_it_runs: Element<Message> = match handled_by {
                Some(role) => text(format!("onboard macro, read by g502d: {role}")).size(13).style(accent).into(),
                None => text("onboard (stored in the mouse)").size(13).style(muted).into(),
            };
            let editing_this = matches!(&self.editor, Some(Edit::Button(e)) if e.profile == p.index && e.button == b.index);
            let edit = button(text("Edit").size(13))
                .on_press_maybe((!self.busy && !editing_this).then_some(Message::Edit(p.index, b.index)))
                .style(button::secondary);
            buttons = buttons.push(
                row![
                    text(format!("Button {}", b.index)).width(90),
                    text(b.action.clone()).width(Length::Fill),
                    container(where_it_runs).width(330),
                    container(edit).width(70),
                ]
                .align_y(iced::Center),
            );
        }
        let editor: Option<Element<Message>> = self.editor.as_ref().filter(|e| e.profile() == p.index).map(|e| self.editor_panel(d, e, synced));
        let buttons = column![
            buttons,
            text("Host-side actions (key combos, commands, macros run by a daemon) are not implemented yet.").size(12).style(muted),
        ]
        .spacing(10);

        // Resolutions
        let res = column(p.resolutions.iter().map(|r| {
            let shared = synced && r.index == d.cfg.dpi.shared_slot;
            let dpi = r.dpi.map_or("?".into(), |v| format!("{v} DPI"));
            let note = if shared {
                "  shared, managed by g502d"
            } else if r.is_disabled {
                "  disabled"
            } else {
                ""
            };
            let edit: Element<Message> = if shared {
                Space::new().into()
            } else {
                button(text("Edit").size(13))
                    .on_press_maybe((!self.busy && !matches!(&self.editor, Some(Edit::Slot(e)) if e.profile == p.index && e.slot == r.index)).then_some(Message::EditSlot(p.index, r.index)))
                    .style(button::secondary)
                    .into()
            };
            row![text(format!("Slot {}: {dpi}{note}", r.index)).size(14).width(Length::Fill), edit].align_y(iced::Center).into()
        }))
        .spacing(4);

        // LEDs
        let leds = column(p.leds.iter().map(|l| {
            let rgb = g502_linux_control::config::parse_color(&l.color).unwrap_or((128, 128, 128));
            let color = Color::from_rgb8(rgb.0, rgb.1, rgb.2);
            let mode = led_mode_name(l.mode).map_or(format!("mode {}", l.mode), str::to_string);
            row![
                container(Space::new())
                    .width(26)
                    .height(26)
                    .style(move |_: &Theme| container::Style {
                        background: Some(Background::Color(color)),
                        border: Border { radius: 5.0.into(), width: 1.0, color: Color::from_rgba(1.0, 1.0, 1.0, 0.35) },
                        ..Default::default()
                    }),
                text(format!("LED {}: {mode}, #{}", l.index, l.color)).size(14).width(Length::Fill),
                button(text("Edit").size(13))
                    .on_press_maybe((!self.busy && !matches!(&self.editor, Some(Edit::Led(e)) if e.profile == p.index && e.led == l.index)).then_some(Message::EditLed(p.index, l.index)))
                    .style(button::secondary),
            ]
            .spacing(10)
            .align_y(iced::Center)
            .into()
        }))
        .spacing(6);

        let toggle: Element<Message> = if synced {
            text(format!("Profile {} keeps the shared DPI in sync through g502d, so it cannot be disabled here.", p.index)).size(13).style(muted).into()
        } else {
            row![
                text("This profile is enabled.").size(14).width(Length::Fill),
                button(text("Disable").size(13))
                    .on_press_maybe((!self.busy && !matches!(&self.editor, Some(Edit::Profile { .. }))).then_some(Message::EditProfile(p.index, true)))
                    .style(button::secondary),
            ]
            .align_y(iced::Center)
            .into()
        };
        column![
            toggle,
            panel(&format!("Profile {} - buttons", p.index), buttons.into()),
            editor.unwrap_or_else(|| Space::new().into()),
            panel(
                "Resolution slots",
                column![
                    res,
                    row![
                        text(format!("Report rate: {} Hz (supports {:?})", p.report_rate, p.report_rates)).size(14).width(Length::Fill),
                        button(text("Edit").size(13))
                            .on_press_maybe((!self.busy && !matches!(&self.editor, Some(Edit::Rate { profile, .. }) if *profile == p.index)).then_some(Message::EditRate(p.index)))
                            .style(button::secondary),
                    ]
                    .align_y(iced::Center),
                ]
                .spacing(8)
                .into(),
            ),
            panel("LEDs", leds.into()),
        ]
        .spacing(18)
        .into()
    }
}

fn editor_for(profile: u32, kind: u32, value: &Option<RawValue>, button: u32) -> ButtonEdit {
    use g502_linux_control::ratbag::action_string;
    // Start from the current action when it has a config spelling.
    let current = action_string(kind, value);
    let words: Vec<&str> = current.as_deref().map(|s| s.split_whitespace().collect()).unwrap_or_default();
    match words[..] {
        [k, v] => match Kind::from_word(k) {
            Some(kind) => ButtonEdit { profile, button, kind, value: v.to_string() },
            None => ButtonEdit { profile, button, kind: Kind::MouseButton, value: "1".into() },
        },
        ["none"] => ButtonEdit { profile, button, kind: Kind::None, value: String::new() },
        _ => ButtonEdit { profile, button, kind: Kind::MouseButton, value: "1".into() },
    }
}

/// The GUI's only write: one edit, through the shared write path.
fn write_edit(edit: &Edit) -> Result<String, String> {
    let run = || -> anyhow::Result<String> {
        let (cfg, _) = Config::load()?;
        let rb = Ratbag::open(cfg.device.vendor, cfg.device.product)?;
        let target = edit.target(&rb.snapshot()?, &cfg)?;
        let done = write(&rb, &target)?;
        Ok(match done.backup {
            Some(b) => format!("Wrote {} change(s) to {}. Previous state saved to {}", done.written, edit.title(), b.display()),
            None => "Nothing to change: the device already has that.".to_string(),
        })
    };
    run().map_err(|e| format!("{e:#}"))
}

/// Dev hook syntax: `P:B:KIND:VALUE`, `led:P:L:MODE:COLOR:BRIGHTNESS`, `rate:P:HZ`,
/// `profile:P:enable|disable`, `slot:P:S:DPI:enabled|disabled`.
fn parse_edit_hook(v: &str) -> Option<Edit> {
    let parts: Vec<&str> = v.splitn(6, ':').collect();
    match parts[..] {
        ["led", p, l, mode, color, brightness] => Some(Edit::Led(LedEdit {
            profile: p.parse().ok()?,
            led: l.parse().ok()?,
            mode: parse_led_mode(mode).ok()?,
            color: color.to_string(),
            brightness: brightness.to_string(),
        })),
        ["rate", p, hz] => Some(Edit::Rate { profile: p.parse().ok()?, hz: hz.parse().ok()? }),
        ["profile", p, what] => Some(Edit::Profile { profile: p.parse().ok()?, disable: what == "disable" }),
        ["slot", p, sl, dpi, state] => Some(Edit::Slot(SlotEdit { profile: p.parse().ok()?, slot: sl.parse().ok()?, dpi: dpi.to_string(), enabled: state != "disabled" })),
        [p, b, kind, ..] => {
            let value = v.splitn(4, ':').nth(3).unwrap_or("").to_string();
            Some(Edit::Button(ButtonEdit { profile: p.parse().ok()?, button: b.parse().ok()?, kind: Kind::from_word(kind)?, value }))
        }
        _ => None,
    }
}

/// What g502d does with an onboard macro, if it is one of its two keys.
fn daemon_role(kind: u32, value: &Option<RawValue>) -> Option<&'static str> {
    match (kind, value) {
        (4, Some(RawValue::MacroEvents(ev))) => match ev[..] {
            [[1, 183], [2, 183]] => Some("DPI down (KEY_F13)"),
            [[1, 184], [2, 184]] => Some("DPI up (KEY_F14)"),
            _ => None,
        },
        _ => None,
    }
}

fn config_line(diff: &Result<usize, String>) -> Element<'_, Message> {
    let s = match diff {
        Ok(0) => "Config: the device matches the [profiles.*] settings in your config.".to_string(),
        Ok(n) => format!("Config: the device differs from your config in {n} place(s). `g502ctl apply --dry-run` lists them."),
        Err(e) => format!("Config cannot be applied to this device: {e}"),
    };
    text(s).size(14).into()
}

fn panel<'a>(title: &str, body: Element<'a, Message>) -> Element<'a, Message> {
    container(column![text(title.to_string()).size(17), body].spacing(12))
        .padding(16)
        .width(Length::Fill)
        .style(|t: &Theme| {
            let p = t.extended_palette();
            container::Style {
                background: Some(Background::Color(p.background.weak.color)),
                border: Border { radius: 8.0.into(), width: 1.0, color: p.background.strong.color },
                ..Default::default()
            }
        })
        .into()
}

fn chip(t: &Theme, current: bool) -> container::Style {
    let p = t.extended_palette();
    let (bg, fg) = if current { (p.primary.strong.color, p.primary.strong.text) } else { (p.background.strong.color, p.background.strong.text) };
    container::Style {
        text_color: Some(fg),
        background: Some(Background::Color(bg)),
        border: Border { radius: 14.0.into(), ..Default::default() },
        ..Default::default()
    }
}

fn muted(t: &Theme) -> text::Style {
    text::Style { color: Some(t.extended_palette().background.strong.text.scale_alpha(0.75)) }
}

fn accent(t: &Theme) -> text::Style {
    text::Style { color: Some(t.extended_palette().primary.strong.color) }
}

fn save_png(path: &PathBuf, shot: &window::Screenshot) -> Result<(), Box<dyn std::error::Error>> {
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut enc = png::Encoder::new(file, shot.size.width, shot.size.height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(&shot.rgba)?;
    Ok(())
}
