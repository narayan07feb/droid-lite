//! Look and feel: one dark palette (close to the new IntelliJ dark theme), the Mac system fonts
//! (SF Pro for the UI, SF Mono for code) and Phosphor icons as a fallback font.

use egui::{Color32, CornerRadius, FontData, FontDefinitions, FontFamily, Margin, Stroke, Visuals};
use std::sync::Arc;

pub struct Theme {
    pub text: Color32,
    pub weak: Color32,
    pub gutter: Color32,
    pub current_line: Color32,
    pub accent: Color32,
    pub editor_bg: Color32,
    pub panel_bg: Color32,
    pub border: Color32,
    pub selection: Color32,
    pub hover: Color32,
}

pub const DARK: Theme = Theme {
    text: Color32::from_rgb(0xDF, 0xE1, 0xE5),
    weak: Color32::from_rgb(0x8C, 0x8F, 0x94),
    gutter: Color32::from_rgb(0x5A, 0x5D, 0x63),
    current_line: Color32::from_rgb(0x26, 0x28, 0x2E),
    accent: Color32::from_rgb(0x35, 0x74, 0xF0),
    editor_bg: Color32::from_rgb(0x1E, 0x1F, 0x22),
    panel_bg: Color32::from_rgb(0x2B, 0x2D, 0x30),
    border: Color32::from_rgb(0x39, 0x3B, 0x40),
    selection: Color32::from_rgb(0x21, 0x4C, 0x8C),
    hover: Color32::from_rgb(0x36, 0x38, 0x3D),
};

/// SF Pro and SF Mono from macOS, with Phosphor icons behind both. Missing files keep egui's fonts.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let mut add = |name: &str, family: FontFamily, data: Option<Vec<u8>>, first: bool| {
        let Some(bytes) = data else { return };
        fonts
            .font_data
            .insert(name.into(), Arc::new(FontData::from_owned(bytes)));
        let list = fonts.families.entry(family).or_default();
        if first {
            list.insert(0, name.into())
        } else {
            list.push(name.into())
        }
    };
    let read = |p: &str| std::fs::read(p).ok();
    add(
        "sf-pro",
        FontFamily::Proportional,
        read("/System/Library/Fonts/SFNS.ttf"),
        true,
    );
    add(
        "sf-mono",
        FontFamily::Monospace,
        read("/System/Library/Fonts/SFNSMono.ttf"),
        true,
    );
    let icons = include_bytes!("../assets/Phosphor.ttf").to_vec();
    add(
        "phosphor",
        FontFamily::Proportional,
        Some(icons.clone()),
        false,
    );
    add("phosphor-mono", FontFamily::Monospace, Some(icons), false);
    ctx.set_fonts(fonts);
}

/// Visuals for the palette. With Liquid Glass the panels let the glass through.
pub fn visuals(glass: bool) -> Visuals {
    let t = &DARK;
    let mut v = Visuals::dark();
    let alpha = |c: Color32, a: u8| Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a);
    v.panel_fill = if glass {
        alpha(t.panel_bg, 120)
    } else {
        t.panel_bg
    };
    v.window_fill = if glass {
        alpha(t.panel_bg, 240)
    } else {
        t.panel_bg
    };
    v.extreme_bg_color = t.editor_bg;
    v.faint_bg_color = Color32::from_rgba_unmultiplied(255, 255, 255, 6);
    v.code_bg_color = t.editor_bg;
    v.window_stroke = Stroke::new(1.0, t.border);
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(8);
    v.selection.bg_fill = t.selection;
    v.selection.stroke = Stroke::new(1.0, t.text);
    v.hyperlink_color = Color32::from_rgb(0x58, 0x9D, 0xF6);
    v.override_text_color = None;
    let w = &mut v.widgets;
    for (state, fill, text) in [
        (&mut w.noninteractive, t.panel_bg, t.text),
        (&mut w.inactive, Color32::from_rgb(0x3C, 0x3F, 0x44), t.text),
        (&mut w.hovered, t.hover, Color32::WHITE),
        (&mut w.active, t.accent, Color32::WHITE),
        (&mut w.open, t.hover, t.text),
    ] {
        state.corner_radius = CornerRadius::same(6);
        state.fg_stroke = Stroke::new(1.0, text);
        state.weak_bg_fill = fill;
        state.bg_fill = fill;
    }
    w.noninteractive.bg_stroke = Stroke::new(1.0, t.border);
    w.noninteractive.fg_stroke = Stroke::new(1.0, t.weak);
    w.inactive.weak_bg_fill = Color32::TRANSPARENT; // flat buttons until hovered, like a toolbar
    w.inactive.bg_stroke = Stroke::NONE;
    w.hovered.bg_stroke = Stroke::NONE;
    w.active.bg_stroke = Stroke::NONE;
    v
}

pub fn style(ctx: &egui::Context) {
    ctx.global_style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 4.0);
        s.spacing.button_padding = egui::vec2(8.0, 3.0);
        s.spacing.window_margin = Margin::same(12);
        s.spacing.menu_margin = Margin::same(6);
        s.spacing.interact_size.y = 22.0;
        s.text_styles
            .insert(egui::TextStyle::Body, egui::FontId::proportional(13.0));
        s.text_styles
            .insert(egui::TextStyle::Button, egui::FontId::proportional(13.0));
        s.text_styles
            .insert(egui::TextStyle::Small, egui::FontId::proportional(11.0));
        s.text_styles
            .insert(egui::TextStyle::Heading, egui::FontId::proportional(14.0));
        s.text_styles
            .insert(egui::TextStyle::Monospace, egui::FontId::monospace(13.0));
    });
}

/// Small grey section title for a tool window, like "PROJECT".
pub fn section(ui: &mut egui::Ui, icon: &str, title: &str) {
    ui.add_space(2.0);
    ui.label(
        egui::RichText::new(format!("{icon}  {}", title.to_uppercase()))
            .size(11.0)
            .color(DARK.weak)
            .strong(),
    );
    ui.add_space(2.0);
}
