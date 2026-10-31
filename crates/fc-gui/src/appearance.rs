use std::path::PathBuf;

use chrono::Timelike;
use egui::{Color32, Context, RichText, Stroke, Theme, Visuals};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Palette {
    #[default]
    System,
    Light,
    Dark,
    LowContrast,
    HighContrast,
}

impl Palette {
    const ALL: [(Self, &'static str); 5] = [
        (Self::System, "System (auto-switch)"),
        (Self::Light, "Light"),
        (Self::Dark, "Dark"),
        (Self::LowContrast, "Low-contrast"),
        (Self::HighContrast, "High-contrast"),
    ];
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Night {
    #[default]
    Off,
    On,
    Scheduled,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Preferences {
    theme: Palette,
    warmth: u8,
    night: Night,
    night_warmth: u8,
    start: String,
    end: String,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            theme: Palette::System,
            warmth: 0,
            night: Night::Off,
            night_warmth: 35,
            start: "20:00".into(),
            end: "07:00".into(),
        }
    }
}

fn minutes(text: &str) -> Option<u16> {
    let bytes = text.as_bytes();
    if bytes.len() != 5
        || bytes[2] != b':'
        || ![0, 1, 3, 4].iter().all(|i| bytes[*i].is_ascii_digit())
    {
        return None;
    }
    let hour: u16 = text[..2].parse().ok()?;
    let minute: u16 = text[3..].parse().ok()?;
    (hour < 24 && minute < 60).then_some(hour * 60 + minute)
}

fn in_schedule(now: u16, start: u16, end: u16) -> bool {
    if start < end {
        now >= start && now < end
    } else {
        now >= start || now < end
    }
}

impl Preferences {
    fn validate(&self) -> Result<(), String> {
        if self.warmth > 60 || self.night_warmth > 60 {
            return Err("Warmth must be between 0 and 60%.".into());
        }
        let start = minutes(&self.start).ok_or("Start time must be HH:MM (24-hour).")?;
        let end = minutes(&self.end).ok_or("End time must be HH:MM (24-hour).")?;
        if start == end {
            return Err("Night Shift start and end must be different.".into());
        }
        Ok(())
    }

    fn effective_warmth(&self, now: u16) -> u8 {
        let active = match self.night {
            Night::Off => false,
            Night::On => true,
            Night::Scheduled => minutes(&self.start)
                .zip(minutes(&self.end))
                .is_some_and(|(start, end)| start != end && in_schedule(now, start, end)),
        };
        self.warmth
            .max(if active { self.night_warmth } else { 0 })
            .min(60)
    }
}

pub struct Appearance {
    preferences: Preferences,
    path: PathBuf,
    notice: Option<String>,
}

impl Default for Appearance {
    fn default() -> Self {
        Self::open(fc_remote::config::default_client_dir().join("appearance.toml"))
    }
}

impl Appearance {
    fn open(path: PathBuf) -> Self {
        let result = match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str::<Preferences>(&text)
                .map_err(|e| e.to_string())
                .and_then(|p| {
                    p.validate()?;
                    Ok(p)
                }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(Preferences::default())
            }
            Err(error) => Err(error.to_string()),
        };
        match result {
            Ok(preferences) => Self {
                preferences,
                path,
                notice: None,
            },
            Err(error) => Self {
                preferences: Preferences::default(),
                path,
                notice: Some(format!(
                    "Appearance could not be loaded: {error}. Defaults are previewed; the saved file has not been changed."
                )),
            },
        }
    }

    pub fn apply(&self, ctx: &Context) {
        let palette = match self.preferences.theme {
            Palette::System => {
                if ctx.system_theme() == Some(Theme::Light) {
                    Palette::Light
                } else {
                    Palette::Dark
                }
            }
            palette => palette,
        };
        ctx.set_theme(if palette == Palette::Light {
            Theme::Light
        } else {
            Theme::Dark
        });
        ctx.set_visuals(visuals(palette));
        ctx.request_repaint_after(std::time::Duration::from_secs(30));
    }

    pub fn overlay(&self, ctx: &Context) {
        let now = chrono::Local::now();
        let warmth = self
            .preferences
            .effective_warmth((now.hour() * 60 + now.minute()) as u16);
        if warmth > 0 {
            ctx.layer_painter(egui::LayerId::new(
                egui::Order::Tooltip,
                egui::Id::new("warmth"),
            ))
            .rect_filled(
                ctx.screen_rect(),
                0.0,
                Color32::from_rgba_unmultiplied(
                    255,
                    170,
                    60,
                    (u16::from(warmth) * 255 / 100) as u8,
                ),
            );
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui) {
        ui.heading("Appearance");
        ui.label("Changes preview immediately. Save to keep them on this device.");
        egui::ComboBox::from_label("Theme")
            .selected_text(
                Palette::ALL
                    .iter()
                    .find(|(p, _)| *p == self.preferences.theme)
                    .map_or("System", |(_, label)| *label),
            )
            .show_ui(ui, |ui| {
                for (palette, label) in Palette::ALL {
                    ui.selectable_value(&mut self.preferences.theme, palette, label);
                }
            });
        ui.label("System follows your OS light/dark setting; Dark is used if unavailable.");
        ui.add(
            egui::Slider::new(&mut self.preferences.warmth, 0..=60)
                .text("Blue-light filter warmth (%)"),
        );
        ui.horizontal_wrapped(|ui| {
            ui.label("Night Shift");
            ui.selectable_value(&mut self.preferences.night, Night::Off, "Off");
            ui.selectable_value(&mut self.preferences.night, Night::On, "On");
            ui.selectable_value(&mut self.preferences.night, Night::Scheduled, "Scheduled");
        });
        ui.add(
            egui::Slider::new(&mut self.preferences.night_warmth, 0..=60)
                .text("Night Shift warmth (%)"),
        );
        ui.horizontal_wrapped(|ui| {
            ui.label("Local time, 24-hour: from");
            ui.add(egui::TextEdit::singleline(&mut self.preferences.start).desired_width(60.0));
            ui.label("to");
            ui.add(egui::TextEdit::singleline(&mut self.preferences.end).desired_width(60.0));
        });
        ui.label("Warmth is an app-only tint, not a display/OS blue-light filter. It changes color accuracy and may reduce contrast. No health benefit is implied.");
        if let Err(error) = self.preferences.validate() {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        if ui.button("Save appearance").clicked() {
            self.notice = Some(match self.save() {
                Ok(()) => "Appearance saved on this device.".into(),
                Err(error) => format!("Appearance was not saved: {error}"),
            });
        }
        if let Some(notice) = &self.notice {
            ui.label(RichText::new(notice).strong());
        }
    }

    fn save(&self) -> Result<(), String> {
        self.preferences.validate()?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options
            .open(self.path.with_extension("lock"))
            .map_err(|e| e.to_string())?;
        fs4::FileExt::lock(&lock).map_err(|e| e.to_string())?;
        let text = toml::to_string(&self.preferences).map_err(|e| e.to_string())?;
        fc_remote::config::write_private(&self.path, text.as_bytes()).map_err(|e| e.to_string())
    }
}

fn visuals(palette: Palette) -> Visuals {
    let light = palette == Palette::Light;
    let (base, panel, raised, text, dim, line, accent) = match palette {
        Palette::Light => (
            0xf3f4f6, 0xffffff, 0xe7e9ed, 0x18202b, 0x4b5563, 0x929aa6, 0xb83c08,
        ),
        Palette::LowContrast => (
            0x25282c, 0x2d3035, 0x363a40, 0xd7d9de, 0xb2b7c0, 0x676f7a, 0xffb078,
        ),
        Palette::HighContrast => (
            0x000000, 0x000000, 0x171717, 0xffffff, 0xffffff, 0xffffff, 0xffb86b,
        ),
        _ => (
            0x111315, 0x191c20, 0x25292f, 0xf3f4f6, 0xaab2be, 0x596370, 0xffa15c,
        ),
    };
    let color = |hex: u32| Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8);
    let mut v = if light {
        Visuals::light()
    } else {
        Visuals::dark()
    };
    v.panel_fill = color(base);
    v.window_fill = color(panel);
    v.extreme_bg_color = color(base);
    v.faint_bg_color = color(panel);
    v.code_bg_color = color(raised);
    v.override_text_color = Some(color(text));
    v.weak_text_color = Some(color(dim));
    v.hyperlink_color = color(accent);
    v.warn_fg_color = if light {
        color(0x865400)
    } else {
        color(0xffd060)
    };
    v.error_fg_color = if light {
        color(0xb42318)
    } else {
        color(0xff9990)
    };
    v.selection.bg_fill = color(if light { 0xf6d8c8 } else { 0x613418 });
    v.selection.stroke = Stroke::new(1.0_f32, color(text));
    v.window_stroke = Stroke::new(1.0_f32, color(line));
    for widget in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        widget.bg_fill = color(raised);
        widget.weak_bg_fill = color(raised);
        widget.bg_stroke = Stroke::new(1.0_f32, color(line));
        widget.fg_stroke = Stroke::new(1.0_f32, color(text));
    }
    v.widgets.hovered.bg_stroke = Stroke::new(2.0_f32, color(accent));
    v.widgets.active.bg_stroke = Stroke::new(2.0_f32, color(accent));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedules_use_local_minutes_and_handle_midnight_boundaries() {
        let mut p = Preferences {
            night: Night::Scheduled,
            ..Default::default()
        };
        for (now, expected) in [
            (0, 35),
            (419, 35),
            (420, 0),
            (1199, 0),
            (1200, 35),
            (1439, 35),
        ] {
            assert_eq!(p.effective_warmth(now), expected);
        }
        p.start = "08:00".into();
        p.end = "17:00".into();
        assert_eq!(p.effective_warmth(480), 35);
        assert_eq!(p.effective_warmth(1020), 0);
        p.warmth = 50;
        assert_eq!(
            p.effective_warmth(500),
            50,
            "filters take the stronger warmth, not an additive washout"
        );
        for time in ["24:00", "12:60", "8:00", "xx:yy", "é:00"] {
            assert!(minutes(time).is_none());
        }
        p.end = p.start.clone();
        assert!(p.validate().is_err());
    }

    #[test]
    fn preferences_persist_and_corrupt_files_are_not_silently_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new-client").join("appearance.toml");
        let mut settings = Appearance::open(path.clone());
        settings.preferences.theme = Palette::HighContrast;
        settings.preferences.night = Night::Scheduled;
        settings.preferences.warmth = 18;
        settings.save().unwrap();
        let reopened = Appearance::open(path.clone());
        assert_eq!(reopened.preferences.theme, Palette::HighContrast);
        assert_eq!(reopened.preferences.warmth, 18);
        assert_eq!(reopened.preferences.night, Night::Scheduled);
        let before = std::fs::read(&path).unwrap();
        std::fs::create_dir(path.with_extension("tmp")).unwrap();
        assert!(settings.save().is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        std::fs::remove_dir(path.with_extension("tmp")).unwrap();
        std::fs::write(&path, "broken = [").unwrap();
        let broken = Appearance::open(path.clone());
        assert!(broken.notice.is_some());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "broken = [");
    }

    #[test]
    fn system_theme_tracks_os_updates_and_explicit_themes_override_them() {
        let dir = tempfile::tempdir().unwrap();
        let mut settings = Appearance::open(dir.path().join("appearance.toml"));
        let ctx = Context::default();
        for theme in [Theme::Light, Theme::Dark, Theme::Light] {
            let _ = ctx.run(
                egui::RawInput {
                    system_theme: Some(theme),
                    ..Default::default()
                },
                |ctx| settings.apply(ctx),
            );
            assert_eq!(ctx.style().visuals.dark_mode, theme == Theme::Dark);
        }
        settings.preferences.theme = Palette::HighContrast;
        let _ = ctx.run(
            egui::RawInput {
                system_theme: Some(Theme::Light),
                ..Default::default()
            },
            |ctx| {
                settings.apply(ctx);
                egui::CentralPanel::default().show(ctx, |ui| settings.show(ui));
                settings.overlay(ctx);
            },
        );
        assert!(ctx.style().visuals.dark_mode);
        assert_eq!(ctx.style().visuals.panel_fill, Color32::BLACK);
    }

    #[test]
    fn readable_palette_text_and_statuses_meet_contrast_thresholds_without_tint() {
        fn luminance(c: Color32) -> f64 {
            let channel = |n: u8| {
                let n = f64::from(n) / 255.0;
                if n <= 0.04045 {
                    n / 12.92
                } else {
                    ((n + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * channel(c.r()) + 0.7152 * channel(c.g()) + 0.0722 * channel(c.b())
        }
        for (palette, _) in Palette::ALL {
            let v = visuals(palette);
            for background in [v.panel_fill, v.window_fill, v.widgets.inactive.bg_fill] {
                for foreground in [
                    v.text_color(),
                    v.weak_text_color(),
                    v.hyperlink_color,
                    v.error_fg_color,
                    v.warn_fg_color,
                ] {
                    let a = luminance(background);
                    let b = luminance(foreground);
                    let ratio = (a.max(b) + 0.05) / (a.min(b) + 0.05);
                    let required = if palette == Palette::HighContrast {
                        7.0
                    } else {
                        4.5
                    };
                    assert!(
                        ratio >= required,
                        "{palette:?}: {foreground:?} on {background:?} is only {ratio:.2}:1"
                    );
                }
            }
        }
    }
}
