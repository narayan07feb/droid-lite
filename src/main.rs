#[cfg(any(test, feature = "memstats"))]
mod alloc_count;
mod app;
mod capture;
mod catalog;
mod classfile;
mod compose;
mod device;
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
mod libs;
mod logcat;
mod lsp;
mod project;
mod refactor;
mod resources;
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
    let glass = app::glass_setting();
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
    // time, and how much live heap the step left behind
    let time = |label: &str, f: &mut dyn FnMut() -> usize| {
        #[cfg(feature = "memstats")]
        let live = alloc_count::live_bytes;
        #[cfg(not(feature = "memstats"))]
        let live = || 0isize;
        let (t, before) = (std::time::Instant::now(), live());
        let n = f();
        println!(
            "{label:<34} {:>8.2} ms  {:>+8.1} MB heap  ({n})",
            t.elapsed().as_secs_f64() * 1000.0,
            (live() - before) as f64 / 1_048_576.0
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
    // a single search takes a few ms, so one sample is noise: report the median of 25
    let median = |label: &str, f: &dyn Fn() -> usize| {
        let mut ms: Vec<f64> = (0..25)
            .map(|_| {
                let t = std::time::Instant::now();
                std::hint::black_box(f());
                t.elapsed().as_secs_f64() * 1000.0
            })
            .collect();
        ms.sort_by(f64::total_cmp);
        println!(
            "{label:<34} {:>8.2} ms  {:>+8.1} MB heap  (median of 25)",
            ms[12], 0.0
        );
    };
    for q in ["H", "HSVM", "HomeScreen", "viewmodel"] {
        median(&format!("go to symbol \"{q}\""), &|| {
            idx.search(q, false, 60).len()
        });
    }
    median("go to class \"Repo\"", &|| {
        idx.search("Repo", true, 60).len()
    });
    median("complete \"remem\"", &|| idx.complete("remem", 40).len());
    time("definitions of \"Modifier\"", &mut || {
        idx.definitions("Modifier", std::path::Path::new("/")).len()
    });
    time("watch the project folder", &mut || {
        usize::from(watch::Watcher::start(&p.root, egui::Context::default()).is_some())
    });
    time("project tree (Android view)", &mut || app::bench_tree(&p));
    time("module of every file", &mut || {
        p.files.iter().filter(|f| p.module_of(f).is_some()).count()
    });
    let scripts: Vec<std::path::PathBuf> = p
        .files
        .iter()
        .filter(|f| f.extension().is_some_and(|e| e == "kts" || e == "gradle"))
        .cloned()
        .collect();
    let mut libs = None;
    time("index libraries (Gradle cache)", &mut || {
        libs = Some(libs::index(&p.root, &scripts));
        libs.as_ref().map_or(0, |l| l.syms.len())
    });
    let libs = libs.unwrap();
    {
        // every field of every library symbol, in order: equal fingerprints mean the same index
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for s in &libs.syms {
            format!(
                "{}|{:?}|{}|{}|{:?}|{}",
                s.qualified(),
                s.kind,
                s.composable,
                s.archive.display(),
                s.source_path(),
                s.artifact
            )
            .hash(&mut h);
        }
        println!("  library fingerprint {:016x}", h.finish());
    }
    println!(
        "  {} artifacts; Text: {:?}",
        libs.artifacts,
        libs.named("Text")
            .filter(|s| s.composable)
            .map(|s| s.qualified())
            .take(3)
            .collect::<Vec<_>>()
    );
    let any = idx.files.keys().next().cloned().unwrap_or_default();
    let src = std::fs::read_to_string(&any).unwrap_or_default();
    let mut idx = idx;
    time("re-index one file (per keystroke)", &mut || {
        idx.update(&any, &src);
        src.len()
    });
}
