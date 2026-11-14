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
    /// The selected tab's pill, and the frame of popups.
    pub raised: Color32,
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
    border: Color32::from_rgb(0x1E, 0x1F, 0x22),
    selection: Color32::from_rgb(0x21, 0x4C, 0x8C),
    hover: Color32::from_rgb(0x36, 0x38, 0x3D),
    raised: Color32::from_rgb(0x43, 0x45, 0x4A),
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
    raised: Color32::from_rgb(0xDF, 0xE1, 0xE5),
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

/// Visuals for the palette.
pub fn visuals() -> Visuals {
    let t = self::t();
    let mut v = if std::ptr::eq(t, &LIGHT) {
        Visuals::light()
    } else {
        Visuals::dark()
    };
    v.panel_fill = t.panel_bg;
    v.window_fill = t.panel_bg;
    v.extreme_bg_color = t.editor_bg;
    v.faint_bg_color = Color32::from_rgba_unmultiplied(255, 255, 255, 6);
    v.code_bg_color = t.editor_bg;
    v.window_stroke = Stroke::new(1.0, t.raised);
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
        s.spacing.button_padding = egui::vec2(8.0, 4.0);
        s.animation_time = 0.15; // folders, popups and panels glide instead of jumping
        // jumps (go to declaration, search hits) glide 0.2-0.35 s, longer for longer distances
        s.scroll_animation =
            egui::style::ScrollAnimation::new(2400.0, egui::Rangef::new(0.2, 0.35));
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

/// Tool window title, like "Project": the icon dimmed, the name in normal case.
pub fn section(ui: &mut egui::Ui, icon: &str, title: &str) {
    ui.add_space(4.0);
    let mut job = egui::text::LayoutJob::default();
    let font = |color| egui::TextFormat {
        font_id: egui::FontId::proportional(12.5),
        color,
        ..Default::default()
    };
    job.append(icon, 0.0, font(t().weak));
    job.append(&format!("  {title}"), 0.0, font(t().text));
    ui.label(job);
    ui.add_space(2.0);
}

/// `id` glides to `target` in `secs` with a cubic ease-out, from wherever it is now.
/// Asks for the next frame only while it moves, so an idle window draws nothing.
pub fn ease(ctx: &egui::Context, id: egui::Id, target: f32, secs: f32) -> f32 {
    #[derive(Clone, Copy)]
    struct Glide {
        from: f32,
        to: f32,
        start: f64,
    }
    let at = |g: Glide, now: f64| {
        let p = (((now - g.start) as f32) / secs).clamp(0.0, 1.0);
        g.from + (g.to - g.from) * (1.0 - (1.0 - p).powi(3))
    };
    let now = ctx.input(|i| i.time);
    let g = ctx.data_mut(|d| {
        let g = d.get_temp_mut_or_insert_with(id, || Glide {
            from: target,
            to: target,
            start: now,
        });
        if g.to != target {
            *g = Glide {
                from: at(*g, now),
                to: target,
                start: now,
            };
        }
        *g
    });
    let v = at(g, now);
    if v != g.to {
        ctx.request_repaint();
    }
    v
}

/// Paints `text` in a click area with a fading hover fill; the shared base of the widgets below.
fn flat(
    ui: &mut egui::Ui,
    text: egui::WidgetText,
    enabled: bool,
    selected: Option<bool>,
) -> (egui::Response, f32, egui::Rect) {
    let pad = ui.spacing().button_padding;
    let galley = text.into_galley(
        ui,
        Some(egui::TextWrapMode::Extend),
        f32::INFINITY,
        egui::TextStyle::Button,
    );
    let size = egui::vec2(
        galley.size().x + 2.0 * pad.x,
        (galley.size().y + 2.0 * pad.y).max(ui.spacing().interact_size.y),
    );
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, r) = ui.allocate_exact_size(size, sense);
    let label = galley.text().to_owned();
    r.widget_info(|| match selected {
        Some(s) => egui::WidgetInfo::selected(egui::Role::Tab, enabled, s, &label),
        None => egui::WidgetInfo::labeled(egui::Role::Button, enabled, &label),
    });
    let hover = ease(ui.ctx(), r.id.with("hover"), r.hovered() as u8 as f32, 0.12);
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        if hover > 0.0 && selected != Some(true) {
            p.rect_filled(rect, CornerRadius::same(6), t().hover.gamma_multiply(hover));
        }
        let weak = !enabled || selected == Some(false);
        let color = if weak {
            t().weak
                .lerp_to_gamma(t().text, if enabled { hover } else { 0.0 })
        } else {
            t().text
        };
        let pos = rect.center() - galley.size() / 2.0;
        p.galley(pos, galley, color);
    }
    (r, hover, rect)
}

/// A flat toolbar button whose hover fill fades in and out.
pub fn button(
    ui: &mut egui::Ui,
    enabled: bool,
    text: impl Into<egui::WidgetText>,
) -> egui::Response {
    flat(ui, text.into(), enabled, None).0
}

/// A row of tabs whose selected pill slides to the clicked tab.
pub struct Tabs {
    id: egui::Id,
    slot: egui::layers::ShapeIdx,
    target: Option<egui::Rect>,
}

impl Tabs {
    pub fn new(ui: &egui::Ui, name: &str) -> Tabs {
        Tabs {
            id: egui::Id::unique(("tabs", name)),
            slot: ui.painter().add(egui::Shape::Noop),
            target: None,
        }
    }

    pub fn tab(
        &mut self,
        ui: &mut egui::Ui,
        selected: bool,
        text: impl Into<egui::WidgetText>,
    ) -> egui::Response {
        let (r, _, rect) = flat(ui, text.into(), true, Some(selected));
        if selected {
            self.target = Some(rect);
        }
        r
    }

    /// Paints the pill under the tabs (its slot was taken before them, so text stays on top).
    pub fn finish(self, ui: &egui::Ui) {
        let Some(to) = self.target else { return };
        let ctx = ui.ctx();
        let x = ease(ctx, self.id.with("x"), to.left(), 0.18);
        let w = ease(ctx, self.id.with("w"), to.width(), 0.18);
        let pill = egui::Rect::from_min_size(egui::pos2(x, to.top()), egui::vec2(w, to.height()));
        ui.painter().set(
            self.slot,
            egui::epaint::RectShape::filled(pill, CornerRadius::same(6), t().raised),
        );
    }
}

/// One clickable row of a problem list: icon, message (`code` in backticks shows as code) and a dim
/// detail. The hover fill fades; the whole row is the target.
pub fn problem_row(
    ui: &mut egui::Ui,
    icon: &str,
    tint: Color32,
    msg: &str,
    detail: &str,
) -> egui::Response {
    let fmt = |font_id, color| egui::TextFormat {
        font_id,
        color,
        valign: egui::Align::Center,
        ..Default::default()
    };
    let body = egui::FontId::proportional(13.0);
    let mut job = egui::text::LayoutJob::default();
    job.append(icon, 0.0, fmt(body.clone(), tint));
    job.append("  ", 0.0, fmt(body.clone(), t().text));
    for (i, part) in msg.split('`').enumerate() {
        if i % 2 == 1 {
            let mut code = fmt(egui::FontId::monospace(12.0), t().code);
            code.background = t().raised.gamma_multiply(0.6);
            job.append(part, 0.0, code);
        } else {
            job.append(part, 0.0, fmt(body.clone(), t().text));
        }
    }
    if !detail.is_empty() {
        job.append(
            &format!("   {detail}"),
            0.0,
            fmt(egui::FontId::proportional(11.5), t().weak),
        );
    }
    job.wrap.max_width = ui.available_width() - 16.0;
    let galley = ui.fonts_mut(|f| f.layout_job(job));
    let h = galley.size().y + 6.0;
    let (rect, r) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), h), egui::Sense::click());
    r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Link, true, msg));
    let hover = ease(ui.ctx(), r.id.with("hover"), r.hovered() as u8 as f32, 0.12);
    if ui.is_rect_visible(rect) {
        if hover > 0.0 {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(4), t().hover.gamma_multiply(hover));
        }
        ui.painter()
            .galley(rect.left_top() + egui::vec2(8.0, 3.0), galley, t().text);
    }
    if r.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    r
}

#[cfg(test)]
mod tests {
    #[test]
    fn ease_glides_to_a_new_target_with_ease_out_and_then_rests() {
        let ctx = egui::Context::default();
        let id = egui::Id::unique("glide");
        let at = |time: f64, target: f32| {
            let mut v = f32::NAN;
            let input = egui::RawInput {
                time: Some(time),
                ..Default::default()
            };
            let _ = ctx.run_ui(input, |ui| v = super::ease(ui.ctx(), id, target, 0.2));
            v
        };
        assert_eq!(at(0.0, 0.0), 0.0);
        assert_eq!(
            at(0.1, 10.0),
            0.0,
            "a new target starts from where the value is"
        );
        assert!(
            (at(0.2, 10.0) - 8.75).abs() < 1e-3,
            "half the time, 87.5% of the way"
        );
        assert_eq!(at(0.3, 10.0), 10.0);
        assert_eq!(at(5.0, 10.0), 10.0);
        at(5.0, 20.0);
        assert!((at(5.1, 20.0) - 18.75).abs() < 1e-3);
        assert!(
            (at(5.1, 0.0) - 18.75).abs() < 1e-3,
            "a target changed mid-glide turns back from the current value, no jump"
        );
    }
}
