mod app;
mod compose;
mod gradle;
mod highlight;
mod index;
mod kotlin;
mod lexer;
mod lsp;
mod project;

fn main() -> eframe::Result {
    let open = std::env::args().nth(1).map(std::path::PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Droid Lite")
            .with_inner_size([1440.0, 900.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "Droid Lite",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc, open)))),
    )
}
