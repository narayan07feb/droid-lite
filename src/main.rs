mod app;
mod compose;
mod glass;
mod gradle;
mod highlight;
mod index;
mod kotlin;
mod lexer;
mod lsp;
mod project;
mod refactor;
mod watch;

fn main() -> eframe::Result {
    let open = std::env::args()
        .nth(1)
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_dir());
    let glass = glass::wanted();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Droid Lite")
            .with_inner_size([1440.0, 900.0])
            .with_drag_and_drop(true)
            .with_icon(icon())
            .with_transparent(glass)
            .with_fullsize_content_view(glass)
            .with_title_shown(!glass)
            .with_titlebar_shown(!glass),
        ..Default::default()
    };
    eframe::run_native(
        "Droid Lite",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc, open, glass)))),
    )
}

fn icon() -> egui::IconData {
    let img = image::load_from_memory(include_bytes!("../assets/icon-256.png"))
        .map(|i| i.to_rgba8())
        .unwrap_or_default();
    egui::IconData {
        width: img.width(),
        height: img.height(),
        rgba: img.into_raw(),
    }
}
