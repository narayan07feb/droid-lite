//! macOS Liquid Glass (macOS 26+): an NSGlassEffectView behind a transparent egui window.
//! Side panels and bars are see-through; the editor stays almost opaque so code is easy to read.

use egui::Color32;

/// Glass is on by default on macOS; `DROID_LITE_GLASS=0` turns it off.
pub fn wanted() -> bool {
    cfg!(target_os = "macos") && std::env::var("DROID_LITE_GLASS").map_or(true, |v| v != "0")
}

/// Puts the glass view behind the window content. False when the system has no Liquid Glass.
///
/// The glass goes into the window's frame view, *below* egui's view. It must not be a subview of
/// egui's view: AppKit draws subviews on top of their parent's layer, so it would cover everything
/// egui draws (the first version did that and showed an empty window).
pub fn apply(cc: &eframe::CreationContext) -> bool {
    #[cfg(target_os = "macos")]
    {
        macos(cc).is_some()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = cc;
        false
    }
}

#[cfg(target_os = "macos")]
fn macos(cc: &eframe::CreationContext) -> Option<()> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{
        NSAppKitVersionNumber, NSAutoresizingMaskOptions, NSGlassEffectView, NSView,
        NSWindowOrderingMode,
    };
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let mtm = MainThreadMarker::new()?;
    // NSGlassEffectView exists from AppKit 2685 (macOS 26)
    if unsafe { NSAppKitVersionNumber } < 2685.0 {
        return None;
    }
    let RawWindowHandle::AppKit(handle) = cc.window_handle().ok()?.as_raw() else {
        return None;
    };
    // SAFETY: winit gives a valid NSView for the window's lifetime; we are on the main thread.
    let egui_view: &NSView = unsafe { handle.ns_view.cast::<NSView>().as_ref() };
    let frame_view = unsafe { egui_view.superview() }?;
    let glass = NSGlassEffectView::initWithFrame(mtm.alloc(), frame_view.bounds());
    glass.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    // the frame view retains the glass view, so it lives as long as the window
    frame_view.addSubview_positioned_relativeTo(
        &glass,
        NSWindowOrderingMode::Below,
        Some(egui_view),
    );
    Some(())
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
