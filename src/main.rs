#[cfg(any(test, feature = "memstats"))]
mod alloc_count;
mod android_kit;
mod app;
mod capture;
mod catalog;
mod classfile;
mod compose;
mod device;
mod edit;
mod git;
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
mod plugins;
mod preview;
mod preview_base;
mod preview_cache;
mod preview_spec;
mod preview_src;
mod project;
mod refactor;
mod resolve;
mod reports;
mod resources;
mod serve;
mod theme;
mod usage;
#[cfg(test)]
mod accuracy;
mod watch;

fn main() -> eframe::Result {
    // the engine without the window, for the VS Code / Cursor plugin
    if std::env::args().nth(1).as_deref() == Some("serve") {
        serve::run();
        return Ok(());
    }
    if let Some(dir) = std::env::args()
        .nth(1)
        .filter(|a| a == "--bench")
        .and(std::env::args().nth(2))
    {
        bench(std::path::Path::new(&dir));
        return Ok(());
    }
    app::install_crash_backup();
    let open = std::env::args()
        .nth(1)
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_dir());
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Droid Lite")
            .with_inner_size([1440.0, 900.0])
            .with_drag_and_drop(true)
            .with_icon(icon()),
        ..Default::default()
    };
    eframe::run_native(
        "Droid Lite",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc, open)))),
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
    // the editor's per-parameter stability hints, rebuilt after each edit: the Kotlin file with
    // the most composable parameters, median of 25
    let (big, big_src, params) = p
        .files
        .iter()
        .filter(|f| f.extension().is_some_and(|e| e == "kt"))
        .filter_map(|f| {
            let s = std::fs::read_to_string(f).ok()?;
            let n: usize = kotlin::scan(&s)
                .decls
                .iter()
                .filter(|d| d.is_composable())
                .map(|d| d.params.len())
                .sum();
            Some((f.clone(), s, n))
        })
        .max_by_key(|x| x.2)
        .unwrap_or_default();
    let syms = kotlin::scan(&big_src);
    let mut runs: Vec<f64> = (0..25)
        .map(|_| {
            let t = std::time::Instant::now();
            let newlines: Vec<usize> = big_src.match_indices('\n').map(|(i, _)| i).collect();
            let mut n = 0;
            for d in syms.decls.iter().filter(|d| d.is_composable()) {
                for (_, r) in compose::param_names(&big_src, d.offset) {
                    n += newlines.partition_point(|&x| x < r.start);
                }
            }
            std::hint::black_box(n);
            t.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    runs.sort_by(f64::total_cmp);
    println!(
        "{:<34} {:>8.3} ms  ({params} params in {})",
        "compose parameter hints (1 file)",
        runs[12],
        big.file_name().unwrap_or_default().to_string_lossy()
    );
    let any = keystroke_file(&idx, std::env::var_os("DROID_LITE_BENCH_FILE"));
    println!("  keystroke file: {}", any.display());
    let src = std::fs::read_to_string(&any).unwrap_or_default();
    let idx = std::cell::RefCell::new(idx);
    median("re-index one file (per keystroke)", &|| {
        idx.borrow_mut().update(&any, &src);
        src.len()
    });
}

/// The file the per-keystroke bench re-indexes, the same every run (a map's order changes per
/// process): `pinned` (the perf check names one for both builds it compares), else the largest.
fn keystroke_file(idx: &index::Index, pinned: Option<std::ffi::OsString>) -> std::path::PathBuf {
    pinned
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_file())
        .or_else(|| {
            idx.files
                .keys()
                .filter_map(|p| Some((std::fs::metadata(p).ok()?.len(), p.clone())))
                .max()
                .map(|(_, p)| p)
        })
        .unwrap_or_default()
}

#[cfg(all(test, unix))]
mod script_tests {
    use std::path::Path;

    /// `scripts/package-vsix.sh` with a fake `uname` (`os`, `cpu`) and a fake `cargo` that only
    /// notes it ran: (stderr, whether cargo ran).
    fn package_vsix(os: &str, cpu: &str, target: Option<&str>) -> (String, bool) {
        let bin = std::env::temp_dir().join(format!(
            "droid-lite-vsix-{os}-{cpu}-{}-{}",
            target.unwrap_or("none"),
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&bin);
        std::fs::create_dir_all(&bin).unwrap();
        let script = |name: &str, body: String| {
            let p = bin.join(name);
            std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
            let exe = std::os::unix::fs::PermissionsExt::from_mode(0o755);
            std::fs::set_permissions(&p, exe).unwrap();
        };
        script(
            "uname",
            format!("case \"$1\" in -s) echo {os} ;; -m) echo {cpu} ;; esac"),
        );
        let ran = bin.join("cargo-ran");
        script("cargo", format!("touch {:?}; exit 3", ran));
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
        let out = std::process::Command::new("/bin/sh")
            .arg(repo.join("scripts/package-vsix.sh"))
            .args(target)
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .output()
            .unwrap();
        assert!(!out.status.success(), "the fake cargo always fails");
        let result = (String::from_utf8_lossy(&out.stderr).into_owned(), ran.exists());
        let _ = std::fs::remove_dir_all(&bin);
        result
    }

    #[test]
    fn the_keystroke_bench_takes_the_pinned_file_else_the_largest() {
        let dir = std::env::temp_dir().join(format!("droid-lite-bench-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (small, big) = (dir.join("A.kt"), dir.join("B.kt"));
        std::fs::write(&small, "class A\n").unwrap();
        std::fs::write(&big, "class B\n// a longer file\n").unwrap();
        let idx = crate::index::Index::build(&[small.clone(), big.clone()]);
        let pick = |p: Option<&Path>| super::keystroke_file(&idx, p.map(|p| p.into()));
        let (pinned, largest, gone) = (
            pick(Some(&small)),
            pick(None),
            pick(Some(&dir.join("C.kt"))),
        );
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(pinned, small);
        assert_eq!(largest, big);
        assert_eq!(gone, big, "a pinned file that is gone");
    }

    /// Runs `scripts/perf_check.py old new` with fake bench builds (shell scripts, `old` and
    /// `new`): (its output, whether it passed, what each run of the old one was asked to pin).
    fn perf_check(name: &str, old: &str, new: &str) -> (String, bool, String) {
        perf_check_with(name, old, new, "")
    }

    /// [`perf_check`] with more arguments (`extra`, a Python list's items).
    fn perf_check_with(name: &str, old: &str, new: &str, extra: &str) -> (String, bool, String) {
        let dir =
            std::env::temp_dir().join(format!("droid-lite-perf-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("project")).unwrap();
        let script = |name: &str, body: &str| {
            let p = dir.join(name);
            std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
            let exe = std::os::unix::fs::PermissionsExt::from_mode(0o755);
            std::fs::set_permissions(&p, exe).unwrap();
            p
        };
        let log = dir.join("old-pins");
        let (old, new) = (
            script(
                "old",
                &format!("echo \"$DROID_LITE_BENCH_FILE\" >> {log:?}\n{old}"),
            ),
            script("new", new),
        );
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
        let driver = format!(
            "import sys\nsys.path.insert(0, {scripts:?})\nimport perf_check as p\n\
             p.git = lambda *a: a[1].split('^')[0]\n\
             p.build = lambda sha: {{'old': {old:?}, 'new': {new:?}}}[sha]\n\
             sys.argv = ['perf_check', 'old', 'new', '--project', {project:?}, '--rounds', '2', {extra}]\n\
             p.main()\n",
            scripts = repo.join("scripts").display().to_string(),
            old = old.display().to_string(),
            new = new.display().to_string(),
            project = dir.join("project").display().to_string(),
        );
        let out = std::process::Command::new("python3")
            .args(["-I", "-c", &driver])
            .current_dir(repo)
            .output()
            .unwrap();
        let pins = std::fs::read_to_string(&log).unwrap_or_default();
        let _ = std::fs::remove_dir_all(&dir);
        let text = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        (text, out.status.success(), pins)
    }

    /// A fake bench: the per-keystroke step at `ms`, and the file it re-indexed when `says`.
    fn bench_output(ms: &str, says: bool) -> String {
        let file = if says {
            "echo \"  keystroke file: ${DROID_LITE_BENCH_FILE:-/its/own/largest}\"\n"
        } else {
            ""
        };
        format!(
            "{file}printf '%-34s %8.2f ms  %+8.1f MB heap\\n' 're-index one file (per keystroke)' {ms} 0"
        )
    }

    #[test]
    fn the_perf_check_pins_one_keystroke_file_for_both_builds() {
        let (out, ok, pins) = perf_check(
            "pin",
            &bench_output("0.10", true),
            &bench_output("0.10", true),
        );
        assert!(ok, "{out}");
        let pins: Vec<&str> = pins.lines().collect();
        assert_eq!(pins.len(), 3, "a warm-up and two rounds: {out}");
        assert!(pins.iter().all(|p| *p == "/its/own/largest"), "{pins:?}");
        assert!(
            out.contains("per-keystroke file (both): /its/own/largest"),
            "{out}"
        );
        assert!(!out.contains("not compared"), "{out}");
    }

    #[test]
    fn the_perf_check_fails_keystroke_times_it_cannot_compare_unless_allowed() {
        let (old, new) = (bench_output("0.10", false), bench_output("9.40", true));
        let (out, ok, _) = perf_check("own", &old, &new);
        assert!(!ok, "an old build that picks its own file: {out}");
        assert!(out.contains("not compared"), "{out}");
        let (out, ok, _) = perf_check_with("own-allowed", &old, &new, "'--allow-uncompared'");
        assert!(ok, "a different file is no slowdown when allowed: {out}");
        assert!(out.contains("not compared"), "{out}");
    }

    #[test]
    fn the_perf_check_fails_a_sub_millisecond_step_that_grows_many_times() {
        let (out, ok, _) = perf_check(
            "short",
            &bench_output("0.10", true),
            &bench_output("1.90", true),
        );
        assert!(!ok, "{out}");
        assert!(out.contains("SLOWER"), "{out}");
        let (out, ok, _) = perf_check(
            "short-noise",
            &bench_output("0.10", true),
            &bench_output("0.14", true),
        );
        assert!(ok, "within the timer's noise: {out}");
    }

    #[test]
    fn package_vsix_builds_only_for_the_machine_it_runs_on() {
        assert!(package_vsix("Darwin", "arm64", None).1, "no target: this Mac's");
        assert!(package_vsix("Darwin", "arm64", Some("darwin-arm64")).1);
        assert!(package_vsix("Darwin", "x86_64", Some("darwin-x64")).1);
        assert!(package_vsix("Linux", "x86_64", Some("linux-x64")).1);
        let (err, ran) = package_vsix("Darwin", "arm64", Some("darwin-x64"));
        assert!(!ran, "an Intel package of an Apple silicon engine is refused before the build");
        assert!(err.contains("darwin-x64 is not this machine (darwin-arm64)"), "{err}");
        let (err, ran) = package_vsix("Darwin", "x86_64", Some("linux-x64"));
        assert!(!ran && err.contains("not this machine"), "{err}");
        let (err, ran) = package_vsix("SunOS", "sparc", Some("darwin-arm64"));
        assert!(!ran && err.contains("no VS Code target for SunOS sparc"), "{err}");
    }
}
