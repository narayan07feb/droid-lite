//! Look and feel: a dark and a light palette (close to the new IntelliJ themes), the Mac system fonts
//! (SF Pro for the UI, SF Mono for code) and Phosphor icons as a fallback font.

use egui::{Color32, CornerRadius, FontData, FontDefinitions, FontFamily, Margin, Stroke, Visuals};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

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
    // syntax colours
    pub code: Color32,
    pub keyword: Color32,
    pub string: Color32,
    pub number: Color32,
    pub comment: Color32,
    pub annotation: Color32,
    pub call: Color32,
    pub type_: Color32,
}

pub static DARK: Theme = Theme {
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
    code: Color32::from_rgb(0xA9, 0xB7, 0xC6),
    keyword: Color32::from_rgb(0xCC, 0x78, 0x32),
    string: Color32::from_rgb(0x6A, 0x87, 0x59),
    number: Color32::from_rgb(0x68, 0x97, 0xBB),
    comment: Color32::from_rgb(0x80, 0x80, 0x80),
    annotation: Color32::from_rgb(0xBB, 0xB5, 0x29),
    call: Color32::from_rgb(0xFF, 0xC6, 0x6D),
    type_: Color32::from_rgb(0xC9, 0xD1, 0xDB),
};

pub static LIGHT: Theme = Theme {
    text: Color32::from_rgb(0x1E, 0x1F, 0x22),
    weak: Color32::from_rgb(0x6C, 0x70, 0x7E),
    gutter: Color32::from_rgb(0xAE, 0xB3, 0xC2),
    current_line: Color32::from_rgb(0xF5, 0xF8, 0xFE),
    accent: Color32::from_rgb(0x35, 0x74, 0xF0),
    editor_bg: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    panel_bg: Color32::from_rgb(0xF7, 0xF8, 0xFA),
    border: Color32::from_rgb(0xDF, 0xE1, 0xE5),
    selection: Color32::from_rgb(0xA6, 0xD2, 0xFF),
    hover: Color32::from_rgb(0xEB, 0xEC, 0xF0),
    code: Color32::from_rgb(0x08, 0x08, 0x08),
    keyword: Color32::from_rgb(0x00, 0x33, 0xB3),
    string: Color32::from_rgb(0x06, 0x7D, 0x17),
    number: Color32::from_rgb(0x17, 0x50, 0xEB),
    comment: Color32::from_rgb(0x8C, 0x8C, 0x8C),
    annotation: Color32::from_rgb(0x9E, 0x88, 0x0D),
    call: Color32::from_rgb(0x00, 0x62, 0x7A),
    type_: Color32::from_rgb(0x00, 0x00, 0x00),
};

static LIGHT_ON: AtomicBool = AtomicBool::new(false);
static FONT_BITS: AtomicU32 = AtomicU32::new(0x4150_0000); // 13.0

/// The palette in use (Settings → Theme).
pub fn t() -> &'static Theme {
    if LIGHT_ON.load(Ordering::Relaxed) {
        &LIGHT
    } else {
        &DARK
    }
}

pub fn set_light(light: bool) {
    LIGHT_ON.store(light, Ordering::Relaxed);
}

/// Editor font size (Settings → Font size).
pub fn font_size() -> f32 {
    f32::from_bits(FONT_BITS.load(Ordering::Relaxed))
}

pub fn set_font_size(size: f32) {
    FONT_BITS.store(size.clamp(9.0, 28.0).to_bits(), Ordering::Relaxed);
}

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
    let t = self::t();
    let mut v = if std::ptr::eq(t, &LIGHT) {
        Visuals::light()
    } else {
        Visuals::dark()
    };
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
    // egui draws bold ("strong") text in the active colour, so the light theme needs dark text there
    let light = std::ptr::eq(t, &LIGHT);
    let (strong, pressed) = if light {
        (t.text, Color32::from_rgb(0xD5, 0xE1, 0xFF))
    } else {
        (Color32::WHITE, t.accent)
    };
    let inactive = if light {
        Color32::from_rgb(0xEB, 0xEC, 0xF0)
    } else {
        Color32::from_rgb(0x3C, 0x3F, 0x44)
    };
    for (state, fill, text) in [
        (&mut w.noninteractive, t.panel_bg, t.text),
        (&mut w.inactive, inactive, t.text),
        (&mut w.hovered, t.hover, strong),
        (&mut w.active, pressed, strong),
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
            .color(crate::theme::t().weak)
            .strong(),
    );
    ui.add_space(2.0);
}
