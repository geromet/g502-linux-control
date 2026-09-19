//! g502-gui: read-only view of the G502's profiles, buttons, DPI and LEDs.
//!
//! First cut: it reads through the same library code as `g502ctl` and never
//! writes to the mouse. Build with `--features gui`.
//!
//! Set G502_GUI_SCREENSHOT=file.png to have the window save a picture of
//! itself (only its own contents) once it has loaded, and exit. Used to check
//! the rendering without a display server session to look at. In that mode
//! G502_GUI_PROFILE=N picks the profile tab and the window is tall enough to
//! show everything.

use g502_linux_control::{
    apply::overlay,
    config::Config,
    ratbag::{ProfileInfo, Ratbag, RawValue, Snapshot, led_mode_name},
    restore::plan,
};
use iced::{
    Background, Border, Color, Element, Length, Task, Theme, window,
    widget::{Space, button, column, container, row, scrollable, text},
};
use std::{path::PathBuf, time::Duration};

fn main() -> iced::Result {
    iced::application(App::boot, App::update, App::view)
        .title("G502 Linux Control")
        .theme(theme)
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
}

#[derive(Clone)]
enum Message {
    Refresh,
    Loaded(Result<std::sync::Arc<Data>, String>),
    Select(usize),
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

fn read_device() -> Result<std::sync::Arc<Data>, String> {
    let run = || -> anyhow::Result<Data> {
        let (cfg, _) = Config::load()?;
        let rb = Ratbag::open(cfg.device.vendor, cfg.device.product)?;
        let snap = rb.snapshot()?;
        let shared_dpi = cfg.dpi.profiles.first().and_then(|&p| rb.slot_dpi(p, cfg.dpi.shared_slot).ok());
        let config_diff = (!cfg.profiles.is_empty())
            .then(|| overlay(&snap, &cfg).and_then(|t| plan(&snap, &t)).map(|p| p.len()).map_err(|e| format!("{e:#}")));
        Ok(Data { cfg, snap, shared_dpi, config_diff })
    };
    run().map(std::sync::Arc::new).map_err(|e| format!("{e:#}"))
}

impl App {
    fn boot() -> (App, Task<Message>) {
        let screenshot = std::env::var_os("G502_GUI_SCREENSHOT").map(PathBuf::from);
        let selected = std::env::var("G502_GUI_PROFILE").ok().and_then(|p| p.parse().ok()).unwrap_or(0);
        (App { load: Load::Loading, selected, screenshot }, Task::perform(async { read_device() }, Message::Loaded))
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
                        Load::Ready(std::sync::Arc::try_unwrap(data).unwrap_or_else(|_| unreachable!()))
                    }
                    Err(e) => Load::Failed(e),
                };
                if self.screenshot.is_some() {
                    // Give the new state a moment to be drawn, then capture.
                    return Task::perform(async { std::thread::sleep(Duration::from_millis(600)) }, |_| Message::Capture);
                }
                Task::none()
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
                text(format!("{}   |   read-only view: nothing here writes to the mouse", d.snap.model)).size(13).style(muted),
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

    fn profile_panel<'a>(&'a self, d: &'a Data, p: &'a ProfileInfo) -> Element<'a, Message> {
        if p.disabled {
            return panel(
                &format!("Profile {}", p.index),
                text("Disabled on the device. Enable it with `g502ctl profile enable`.").size(14).into(),
            );
        }
        let synced = d.cfg.dpi.profiles.contains(&p.index);

        // Buttons
        let mut buttons = column![row![
            text("Button").width(90).style(muted),
            text("Action").width(Length::Fill).style(muted),
            text("Where it runs").width(330).style(muted),
        ]]
        .spacing(6);
        for b in &p.buttons {
            let handled_by = if synced { daemon_role(b.raw_type, &b.raw_value) } else { None };
            let where_it_runs: Element<Message> = match handled_by {
                Some(role) => text(format!("onboard macro, read by g502d: {role}")).size(13).style(accent).into(),
                None => text("onboard (stored in the mouse)").size(13).style(muted).into(),
            };
            buttons = buttons.push(row![
                text(format!("Button {}", b.index)).width(90),
                text(b.action.clone()).width(Length::Fill),
                container(where_it_runs).width(330),
            ]);
        }
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
            text(format!("Slot {}: {dpi}{note}", r.index)).size(14).into()
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
                text(format!("LED {}: {mode}, #{}", l.index, l.color)).size(14),
            ]
            .spacing(10)
            .align_y(iced::Center)
            .into()
        }))
        .spacing(6);

        column![
            panel(&format!("Profile {} - buttons", p.index), buttons.into()),
            panel(
                "Resolution slots",
                column![res, text(format!("Report rate: {} Hz (supports {:?})", p.report_rate, p.report_rates)).size(14)].spacing(8).into(),
            ),
            panel("LEDs", leds.into()),
        ]
        .spacing(18)
        .into()
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
