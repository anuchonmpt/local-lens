use eframe::egui::{self, Color32};
use std::{cell::Cell, path::Path};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Preference {
    #[default]
    System,
    Light,
    Dark,
}

impl Preference {
    #[cfg(test)]
    pub const ALL: [Self; 3] = [Self::System, Self::Light, Self::Dark];

    pub fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }

    pub fn resolve(self, system: Option<egui::Theme>) -> egui::Theme {
        match self {
            Self::Light => egui::Theme::Light,
            Self::Dark => egui::Theme::Dark,
            Self::System => system.unwrap_or(egui::Theme::Dark),
        }
    }

    pub fn load(dir: &Path) -> Self {
        match std::fs::read_to_string(dir.join("appearance"))
            .as_deref()
            .map(str::trim)
        {
            Ok("Light") => Self::Light,
            Ok("Dark") => Self::Dark,
            _ => Self::System,
        }
    }

    pub fn save(self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let mut file = tempfile::NamedTempFile::new_in(dir)?;
        std::io::Write::write_all(&mut file, self.label().as_bytes())?;
        file.persist(dir.join("appearance"))?;
        Ok(())
    }
}

// Custom egui painters do not all receive a Ui. Scope their shared color tokens
// to the window's render pass, so other contexts and background work are unaffected.
thread_local! { static DARK: Cell<bool> = const { Cell::new(true) }; }

pub fn paint<R>(theme: egui::Theme, draw: impl FnOnce() -> R) -> R {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            DARK.set(self.0);
        }
    }
    let _restore = Restore(DARK.replace(theme == egui::Theme::Dark));
    draw()
}

pub fn tone(light: Color32, dark: Color32) -> Color32 {
    if DARK.get() {
        dark
    } else {
        light
    }
}

macro_rules! token {
    ($name:ident, $light:expr, $dark:expr) => {
        pub fn $name() -> Color32 {
            tone(rgb($light), rgb($dark))
        }
    };
}

const fn rgb(value: u32) -> Color32 {
    Color32::from_rgb((value >> 16) as u8, (value >> 8) as u8, value as u8)
}

token!(bg, 0xF3F5F7, 0x101923);
token!(panel, 0xFFFFFF, 0x172330);
token!(side, 0xF8FAFC, 0x1B2938);
token!(border, 0xDAE1E8, 0x344657);
token!(blue, 0x1759B8, 0x82B8FF);
token!(green, 0x25654A, 0xA1DEBC);
token!(orange, 0x784309, 0xFFCC85);
token!(purple, 0x605091, 0xC6B1FF);
token!(muted, 0x4F6073, 0xA9BACD);
token!(text, 0x152639, 0xE6EDF6);
token!(selection, 0xE8F1FF, 0x1D3754);
token!(warning_fill, 0xFFF0D8, 0x3A2C1C);
token!(success_fill, 0xE8F4EC, 0x20392F);
token!(danger, 0xA52C3C, 0xFFA2AE);
token!(primary, 0x1759B8, 0x215EB2);
token!(disabled_control, 0xE5EAF0, 0x263646);
token!(primary_text, 0xFFFFFF, 0xF7FAFF);
token!(sidebar, 0x14293E, 0x0C1622);
token!(sidebar_text, 0xCBDAE7, 0xB9CDE0);
token!(sidebar_selected, 0x294664, 0x233B54);
token!(sidebar_divider, 0x2D4359, 0x293C50);

pub fn tint(foreground: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(foreground.r(), foreground.g(), foreground.b(), alpha)
}

pub fn install_fonts(ctx: &egui::Context) {
    use std::sync::Arc;

    let mut fonts = egui::FontDefinitions::default();
    for (name, bytes) in [
        (
            "Inter",
            include_bytes!("../assets/fonts/Inter-Regular.ttf").as_slice(),
        ),
        (
            "Inter SemiBold",
            include_bytes!("../assets/fonts/Inter-SemiBold.ttf").as_slice(),
        ),
    ] {
        fonts
            .font_data
            .insert(name.into(), Arc::new(egui::FontData::from_static(bytes)));
    }
    let proportional = fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default();
    proportional.insert(0, "Inter".into());
    let mut semibold = proportional.clone();
    semibold.insert(0, "Inter SemiBold".into());
    fonts
        .families
        .insert(egui::FontFamily::Name("Inter SemiBold".into()), semibold);
    ctx.set_fonts(fonts);
}

pub fn semibold(size: f32) -> egui::FontId {
    egui::FontId::new(size, egui::FontFamily::Name("Inter SemiBold".into()))
}

pub fn apply(ctx: &egui::Context, theme: egui::Theme) {
    ctx.set_theme(theme);
    paint(theme, || {
        let mut style = egui::Style::default();
        style.visuals = if theme == egui::Theme::Dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        };
        let v = &mut style.visuals;
        v.panel_fill = bg();
        v.window_fill = panel();
        v.window_stroke = egui::Stroke::new(1.0_f32, border());
        v.override_text_color = Some(text());
        v.extreme_bg_color = panel();
        v.faint_bg_color = side();
        v.code_bg_color = side();
        v.hyperlink_color = blue();
        v.warn_fg_color = orange();
        v.error_fg_color = danger();
        v.selection.bg_fill = selection();
        v.selection.stroke = egui::Stroke::new(1.0_f32, blue());
        for widget in [
            &mut v.widgets.inactive,
            &mut v.widgets.hovered,
            &mut v.widgets.active,
            &mut v.widgets.open,
        ] {
            widget.bg_fill = side();
            widget.weak_bg_fill = side();
            widget.bg_stroke = egui::Stroke::new(1.0_f32, border());
            widget.fg_stroke.color = text();
            widget.corner_radius = 5.into();
        }
        for widget in [
            &mut v.widgets.hovered,
            &mut v.widgets.active,
            &mut v.widgets.open,
        ] {
            widget.bg_fill = selection();
            widget.weak_bg_fill = selection();
        }
        v.widgets.hovered.bg_stroke.color = blue();
        v.widgets.active.bg_stroke.color = blue();
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.interact_size.y = 28.0;
        style
            .text_styles
            .insert(egui::TextStyle::Body, egui::FontId::proportional(14.0));
        style
            .text_styles
            .insert(egui::TextStyle::Heading, egui::FontId::proportional(26.0));
        ctx.set_style(style);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preference_round_trip_and_system_resolution() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Preference::load(dir.path()), Preference::System);
        for value in Preference::ALL {
            value.save(dir.path()).unwrap();
            assert_eq!(Preference::load(dir.path()), value);
        }
        assert_eq!(
            Preference::System.resolve(Some(egui::Theme::Light)),
            egui::Theme::Light
        );
        assert_eq!(
            Preference::Dark.resolve(Some(egui::Theme::Light)),
            egui::Theme::Dark
        );
        assert_eq!(
            Preference::Light.resolve(Some(egui::Theme::Dark)),
            egui::Theme::Light
        );
    }
}
