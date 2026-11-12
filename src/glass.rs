//! macOS Liquid Glass (macOS 26+): an NSGlassEffectView behind a transparent egui window.
//! Side panels and bars are see-through; the editor stays almost opaque so code is easy to read.

use egui::{Color32, Visuals};

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

pub fn visuals(glass: bool) -> Visuals {
    let mut v = Visuals::dark();
    if glass {
        v.panel_fill = Color32::from_rgba_unmultiplied(22, 24, 28, 110);
        v.window_fill = Color32::from_rgba_unmultiplied(30, 32, 38, 235);
        v.extreme_bg_color = Color32::from_rgba_unmultiplied(10, 12, 14, 150);
        v.faint_bg_color = Color32::from_rgba_unmultiplied(255, 255, 255, 8);
        v.window_corner_radius = 14.into();
        v.menu_corner_radius = 10.into();
    }
    v
}

/// The editor background: nearly opaque on glass so text keeps its contrast.
pub fn editor_fill(glass: bool) -> Color32 {
    if glass {
        Color32::from_rgba_unmultiplied(24, 26, 30, 225)
    } else {
        Visuals::dark().panel_fill
    }
}
