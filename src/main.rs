#[cfg(test)]
mod alloc_count;
mod app;
mod compose;
mod edit;
mod git;
mod glass;
mod gradle;
mod highlight;
mod icons;
mod index;
mod keymap;
mod kotlin;
mod lexer;
mod lsp;
mod project;
mod refactor;
mod theme;
mod watch;

fn main() -> eframe::Result {
    if let Some(dir) = std::env::args()
        .nth(1)
        .filter(|a| a == "--bench")
        .and(std::env::args().nth(2))
    {
        bench(std::path::Path::new(&dir));
        return Ok(());
    }
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

/// `droid-lite --bench <project>`: times opening, indexing and the searches the UI runs per keystroke.
fn bench(dir: &std::path::Path) {
    let time = |label: &str, f: &mut dyn FnMut() -> usize| {
        let t = std::time::Instant::now();
        let n = f();
        println!(
            "{label:<34} {:>8.2} ms  ({n})",
            t.elapsed().as_secs_f64() * 1000.0
        );
    };
    let mut project = None;
    time("open project (walk files)", &mut || {
        project = Some(project::Project::open(dir));
        project.as_ref().map_or(0, |p| p.files.len())
    });
    let p = project.unwrap();
    let mut idx = None;
    time("index all files", &mut || {
        idx = Some(index::Index::build(&p.files));
        idx.as_ref().map_or(0, |i| i.symbol_count())
    });
    let idx = idx.unwrap();
    for q in ["H", "HSVM", "HomeScreen", "viewmodel"] {
        time(&format!("go to symbol \"{q}\""), &mut || {
            idx.search(q, false, 60).len()
        });
    }
    time("go to class \"Repo\"", &mut || {
        idx.search("Repo", true, 60).len()
    });
    time("complete \"remem\"", &mut || {
        idx.complete("remem", 40).len()
    });
    time("definitions of \"Modifier\"", &mut || {
        idx.definitions("Modifier", std::path::Path::new("/")).len()
    });
    time("watch the project folder", &mut || {
        usize::from(watch::Watcher::start(&p.root, egui::Context::default()).is_some())
    });
    time("module of every file", &mut || {
        p.files.iter().filter(|f| p.module_of(f).is_some()).count()
    });
    let any = idx.files.keys().next().cloned().unwrap_or_default();
    let src = std::fs::read_to_string(&any).unwrap_or_default();
    let mut idx = idx;
    time("re-index one file (per keystroke)", &mut || {
        idx.update(&any, &src);
        src.len()
    });
}
