//! macOS Liquid Glass (macOS 26+): an NSGlassEffectView behind a transparent egui window.
//! Side panels and bars are see-through; the editor stays almost opaque so code is easy to read.

use egui::Color32;

/// Glass is on by default on macOS; `DROID_LITE_GLASS=0` turns it off.
pub fn wanted() -> bool {
    cfg!(target_os = "macos") && std::env::var("DROID_LITE_GLASS").map_or(true, |v| v != "0")
}

/// Puts the glass view behind the window content. False when the system has no Liquid Glass.
pub fn apply(cc: &eframe::CreationContext) -> bool {
    #[cfg(target_os = "macos")]
    {
        use window_vibrancy::{LiquidGlassOptions, NSGlassEffectViewStyle, apply_liquid_glass};
        apply_liquid_glass(cc, LiquidGlassOptions::new(NSGlassEffectViewStyle::Regular)).is_ok()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = cc;
        false
    }
}

/// The editor background: nearly opaque on glass so text keeps its contrast.
pub fn editor_fill(glass: bool) -> Color32 {
    let c = crate::theme::DARK.editor_bg;
    if glass {
        Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), 228)
    } else {
        c
    }
}
