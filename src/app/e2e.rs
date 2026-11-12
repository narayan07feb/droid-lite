//! End-to-end tests: the real `App` driven through egui_kittest with key presses and clicks,
//! on a fixture Android project with git history, a fake `gradlew` and a fake Kotlin LSP.
//! Run with `cargo test e2e -- --test-threads=1`.

use super::*;
use crate::icons as ic;
use egui::{Event, Key, Modifiers};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

/// Tests share env vars (DROID_LITE_LSP) and the file watcher, so they run one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

const USER_KT: &str = "package com.demo.data\n\ndata class User(val name: String, var age: Int)\n\nclass UserRepository {\n    fun load(): List<User> = emptyList()\n}\n";
const CARD_KT: &str = "package com.demo.ui\n\nimport com.demo.data.User\n\n@Composable\nfun UserCard(user: User, modifier: Modifier) {\n    val count = mutableStateOf(0)\n    Column(modifier) {\n        Text(user.name)\n    }\n}\n\n@Preview\n@Composable\nprivate fun UserCardPreview() {\n    UserCard(User(\"a\", 1), Modifier)\n}\n";

struct Fixture {
    root: PathBuf,
    _lock: MutexGuard<'static, ()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let lock = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let root =
            std::env::temp_dir().join(format!("droid-lite-e2e-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let write = |rel: &str, text: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        };
        write(
            "settings.gradle.kts",
            "rootProject.name = \"Demo\"\ninclude(\":app\")\n",
        );
        write("build.gradle.kts", "");
        write(
            "app/build.gradle.kts",
            "plugins { id(\"app.cash.paparazzi\") }\n",
        );
        write("app/src/main/java/com/demo/data/User.kt", USER_KT);
        write("app/src/main/java/com/demo/ui/UserCard.kt", CARD_KT);
        let png = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/icon-256.png");
        write(
            "gradlew",
            &format!(
                "#!/bin/sh\necho \"> Task $1\"\ncase \"$1\" in\n  *recordPaparazziDebug) mkdir -p app/src/test/snapshots/images && cp '{}' app/src/test/snapshots/images/com.demo.ui_UserCardKt_UserCardPreview.png; echo 'BUILD SUCCESSFUL';;\n  *failingTask) echo \"e: file://$PWD/app/src/main/java/com/demo/ui/UserCard.kt:7:9 Unresolved reference 'oops'.\"; echo 'BUILD FAILED'; exit 1;;\n  *) echo 'BUILD SUCCESSFUL';;\nesac\n",
                png.display()
            ),
        );
        let git = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(&root)
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success(),
                "git {args:?}"
            )
        };
        std::process::Command::new("chmod")
            .arg("+x")
            .arg(root.join("gradlew"))
            .status()
            .unwrap();
        git(&["init", "-q"]);
        git(&["config", "user.name", "Test Author"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "Initial commit"]);
        let root = root.canonicalize().unwrap();
        Fixture { root, _lock: lock }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn harness(&self) -> Harness<'static, App> {
        let root = self.root.clone();
        let mut h = Harness::builder()
            .with_size([1440.0, 900.0])
            .build_eframe(move |cc| App::new(cc, Some(root), false));
        wait(&mut h, "index", |a| {
            a.indexing.is_none() && a.index.symbol_count() > 0
        });
        h
    }
}

fn wait(h: &mut Harness<'static, App>, what: &str, done: impl Fn(&App) -> bool) {
    for _ in 0..1500 {
        h.step();
        if done(h.state()) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for: {what}");
}

fn steps(h: &mut Harness<'static, App>, n: usize) {
    for _ in 0..n {
        h.step();
    }
}

fn key(h: &mut Harness<'static, App>, mods: Modifiers, k: Key) {
    h.key_press_modifiers(mods, k);
    steps(h, 3);
}

fn type_text(h: &mut Harness<'static, App>, s: &str) {
    h.event(Event::Text(s.into()));
    steps(h, 3);
}

fn tab<'a>(h: &'a Harness<'static, App>) -> &'a Tab {
    &h.state().tabs[h.state().active]
}

/// Moves the editor cursor to the first `needle` (+ `extra` bytes) the way a click would.
fn place_cursor(h: &mut Harness<'static, App>, needle: &str, extra: usize) {
    let app = h.state_mut();
    let t = &mut app.tabs[app.active];
    let at = t
        .text
        .find(needle)
        .unwrap_or_else(|| panic!("{needle} not in {}", t.path.display()))
        + extra;
    t.goto = Some(at);
    steps(h, 3);
    assert_eq!(tab(h).cursor, at);
}

fn open(h: &mut Harness<'static, App>, path: &Path) {
    h.state_mut().open_file(path, None);
    steps(h, 3);
}

#[test]
fn e2e_open_project_tree_and_index() {
    let f = Fixture::new("open");
    let h = f.harness();
    let app = h.state();
    assert_eq!(app.project.as_ref().unwrap().name, "Demo");
    assert!(app.tree.iter().any(|(m, _)| m == ":app"));
    assert!(app.index.find_type("UserRepository").is_some());
    h.get_by_label_contains(":app"); // the module row is on screen
    assert_eq!(
        app.branch.as_deref().map(|b| b == "main" || b == "master"),
        Some(true)
    );
}

#[test]
fn e2e_navigation_search_goto_back() {
    let f = Fixture::new("nav");
    let mut h = f.harness();
    // ⌘O: go to class
    key(&mut h, Modifiers::COMMAND | Modifiers::SHIFT, Key::T); // Eclipse: Open Type
    assert!(
        h.state()
            .search
            .as_ref()
            .is_some_and(|s| s.kind == SearchKind::Class)
    );
    type_text(&mut h, "UsRe");
    assert_eq!(
        h.state().search.as_ref().unwrap().hits[0].name,
        "UserRepository"
    );
    key(&mut h, Modifiers::NONE, Key::Enter);
    assert!(h.state().search.is_none());
    assert_eq!(
        tab(&h).path,
        f.path("app/src/main/java/com/demo/data/User.kt")
    );
    assert_eq!(
        &tab(&h).text[tab(&h).cursor..tab(&h).cursor + 14],
        "UserRepository"
    );
    // ⇧⌘O: go to file
    key(&mut h, Modifiers::COMMAND | Modifiers::SHIFT, Key::R); // Eclipse: Open Resource
    type_text(&mut h, "UserCard.kt");
    key(&mut h, Modifiers::NONE, Key::Enter);
    assert_eq!(
        tab(&h).path,
        f.path("app/src/main/java/com/demo/ui/UserCard.kt")
    );
    // ⌘B on a usage jumps to the declaration in another file
    place_cursor(&mut h, "user: User", 7);
    key(&mut h, Modifiers::NONE, Key::F3); // Eclipse: Open Declaration
    assert_eq!(
        tab(&h).path,
        f.path("app/src/main/java/com/demo/data/User.kt")
    );
    assert!(tab(&h).text[tab(&h).cursor..].starts_with("User("));
    // ⌘[ goes back, ⌘] forward
    key(&mut h, Modifiers::COMMAND, Key::OpenBracket);
    assert_eq!(
        tab(&h).path,
        f.path("app/src/main/java/com/demo/ui/UserCard.kt")
    );
    key(&mut h, Modifiers::COMMAND, Key::CloseBracket);
    assert_eq!(
        tab(&h).path,
        f.path("app/src/main/java/com/demo/data/User.kt")
    );
    // ⌘E recent files lists both
    key(&mut h, Modifiers::COMMAND, Key::E);
    assert_eq!(h.state().search.as_ref().unwrap().hits.len(), 2);
    key(&mut h, Modifiers::NONE, Key::Escape);
    assert!(h.state().search.is_none());
    // ⌥F7 find usages of User
    place_cursor(&mut h, "class User(", 6);
    key(&mut h, Modifiers::COMMAND | Modifiers::SHIFT, Key::G); // Eclipse: References
    wait(&mut h, "usages", |a| {
        a.find_rx.is_none() && !a.found.is_empty()
    });
    assert!(
        h.state()
            .found
            .iter()
            .any(|f| f.path.ends_with("UserCard.kt"))
    );
}

#[test]
fn e2e_editing_completion_with_import_and_save() {
    let f = Fixture::new("complete");
    let mut h = f.harness();
    let card = f.path("app/src/main/java/com/demo/ui/UserCard.kt");
    open(&mut h, &card);
    let end = tab(&h).text.len();
    h.state_mut().tabs.last_mut().unwrap().goto = Some(end);
    steps(&mut h, 3);
    type_text(&mut h, "val repo = UserRe");
    let c = h.state().completion.as_ref().expect("completion popup");
    assert_eq!(c.items[0].label, "UserRepository");
    assert_eq!(
        c.items[0].import.as_deref(),
        Some("com.demo.data.UserRepository")
    );
    h.get_by_label("UserRepository"); // the popup row is visible
    key(&mut h, Modifiers::NONE, Key::Enter);
    let text = tab(&h).text.clone();
    assert!(text.ends_with("val repo = UserRepository"), "{text}");
    assert!(
        text.contains("import com.demo.data.User\nimport com.demo.data.UserRepository\n"),
        "{text}"
    );
    assert!(tab(&h).dirty());
    key(&mut h, Modifiers::COMMAND, Key::S);
    assert!(!tab(&h).dirty());
    assert_eq!(std::fs::read_to_string(&card).unwrap(), text);
}

#[test]
fn e2e_compose_panel_tree_stability_lints_preview_render() {
    let f = Fixture::new("compose");
    let mut h = f.harness();
    open(&mut h, &f.path("app/src/main/java/com/demo/ui/UserCard.kt"));
    place_cursor(&mut h, "Column(", 0);
    steps(&mut h, 3);
    for label in ["Column", "Text"] {
        h.get_by_label(label);
    }
    let view = h.state().compose_view.as_ref().unwrap();
    assert_eq!(view.tree[0].name, "Column");
    assert_eq!(
        view.stability.params[0].verdict,
        Verdict::Unstable,
        "User has a var"
    );
    h.get_by_label("UNSTABLE");
    let rules: Vec<&str> = tab(&h).lints.iter().map(|l| l.rule).collect();
    assert!(
        rules.contains(&"remember-state") && rules.contains(&"modifier-default"),
        "{rules:?}"
    );
    // Render previews runs the module's Paparazzi task and shows the image
    h.get_by_label(&format!("{}  Render previews", ic::IMAGE))
        .click();
    steps(&mut h, 2);
    wait(&mut h, "preview build", |a| {
        a.gradle.is_none() && a.console.iter().any(|l| l == "BUILD SUCCESSFUL")
    });
    assert!(
        h.state()
            .console
            .iter()
            .any(|l| l == "> Task :app:recordPaparazziDebug")
    );
    wait(&mut h, "preview image", |a| {
        a.compose_view.as_ref().is_some_and(|v| v.images.len() == 1) && a.textures.len() == 1
    });
}

#[test]
fn e2e_gradle_run_and_click_error() {
    let f = Fixture::new("gradle");
    let mut h = f.harness();
    h.get_by_label(&format!("{}  Run", ic::PLAY)).click();
    steps(&mut h, 2);
    wait(&mut h, "build", |a| {
        a.gradle.is_none() && a.console.iter().any(|l| l.contains("Gradle finished: OK"))
    });
    h.state_mut().gradle_task = "failingTask".into();
    steps(&mut h, 2);
    h.get_by_label(&format!("{}  Run", ic::PLAY)).click();
    steps(&mut h, 2);
    wait(&mut h, "failing build", |a| {
        a.gradle.is_none() && a.console.iter().any(|l| l.contains("exit 1"))
    });
    h.get_by_label_contains("Unresolved reference 'oops'")
        .click();
    steps(&mut h, 3);
    assert!(tab(&h).path.ends_with("UserCard.kt"));
    assert!(
        tab(&h).text[tab(&h).cursor..].starts_with("count ="),
        "line 7, column 9"
    );
}

#[test]
fn e2e_refactor_rename_preview_apply_file_rename_and_undo() {
    let f = Fixture::new("rename");
    let mut h = f.harness();
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    open(&mut h, &user);
    place_cursor(&mut h, "class User(", 6);
    key(&mut h, Modifiers::COMMAND | Modifiers::ALT, Key::R); // Eclipse: Rename
    assert!(matches!(
        h.state().refactor,
        Some(refactor_ui::Refactor::RenameInput { .. })
    ));
    for _ in 0.."User".len() {
        key(&mut h, Modifiers::NONE, Key::Backspace);
    }
    type_text(&mut h, "Person");
    key(&mut h, Modifiers::NONE, Key::Enter);
    wait(&mut h, "rename preview", |a| {
        matches!(a.refactor, Some(refactor_ui::Refactor::Preview(_)))
    });
    h.get_by_label_contains("Also rename file User.kt → Person.kt");
    h.get_by_label("Apply").click();
    steps(&mut h, 3);
    let person = f.path("app/src/main/java/com/demo/data/Person.kt");
    assert!(!user.exists() && person.exists(), "file renamed");
    assert!(
        std::fs::read_to_string(&person)
            .unwrap()
            .contains("data class Person(")
            && std::fs::read_to_string(&person)
                .unwrap()
                .contains("List<Person>")
    );
    let card =
        std::fs::read_to_string(f.path("app/src/main/java/com/demo/ui/UserCard.kt")).unwrap();
    assert!(
        card.contains("import com.demo.data.Person")
            && card.contains("user: Person")
            && card.contains("Person(\"a\", 1)"),
        "{card}"
    );
    assert!(
        card.contains("fun UserCard("),
        "a longer name that starts the same is not touched"
    );
    assert_eq!(tab(&h).path, person, "the open tab follows the file");
    // Refactor menu → Undo
    h.get_by_label("Refactor").click();
    steps(&mut h, 2);
    h.get_by_label_contains("Undo rename User → Person").click();
    steps(&mut h, 3);
    assert!(user.exists() && !person.exists());
    assert_eq!(std::fs::read_to_string(&user).unwrap(), USER_KT);
}

#[test]
fn e2e_refactor_extract_variable_and_optimize_imports() {
    let f = Fixture::new("extract");
    let mut h = f.harness();
    open(&mut h, &f.path("app/src/main/java/com/demo/ui/UserCard.kt"));
    place_cursor(&mut h, "user.name", 0);
    for _ in 0.."user.name".len() {
        key(&mut h, Modifiers::SHIFT, Key::ArrowRight);
    }
    assert_eq!(&tab(&h).text[tab(&h).selection.clone()], "user.name");
    key(&mut h, Modifiers::COMMAND | Modifiers::ALT, Key::L); // Eclipse: Extract Local Variable
    assert!(matches!(
        h.state().refactor,
        Some(refactor_ui::Refactor::Extract { .. })
    ));
    key(&mut h, Modifiers::NONE, Key::Enter);
    let text = tab(&h).text.clone();
    assert!(
        text.contains("        val name = user.name\n        Text(name)"),
        "{text}"
    );
    // optimize imports drops an unused one
    let at = text.find("import com.demo.data.User\n").unwrap();
    h.state_mut()
        .tabs
        .last_mut()
        .unwrap()
        .text
        .insert_str(at, "import java.util.UUID\n");
    h.state_mut().tabs.last_mut().unwrap().reanalyse();
    key(&mut h, Modifiers::COMMAND | Modifiers::SHIFT, Key::O); // Eclipse: Organize Imports
    assert!(
        !tab(&h).text.contains("java.util.UUID")
            && tab(&h).text.contains("import com.demo.data.User\n")
    );
}

#[test]
fn e2e_reload_when_file_changes_on_disk() {
    let f = Fixture::new("reload");
    let mut h = f.harness();
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    open(&mut h, &user);
    std::thread::sleep(Duration::from_millis(300));
    std::fs::write(&user, "package com.demo.data\n\nclass Changed\n").unwrap();
    wait(&mut h, "reload", |a| {
        a.tabs[a.active].text.contains("class Changed")
    });
    assert!(h.state().index.find_type("Changed").is_some());
    // with unsaved edits the user is asked instead
    type_text(&mut h, "x");
    std::fs::write(&user, "package com.demo.data\n\nclass Again\n").unwrap();
    wait(&mut h, "conflict bar", |a| a.tabs[a.active].disk_changed);
    h.get_by_label("Reload from disk").click();
    steps(&mut h, 3);
    assert!(tab(&h).text.contains("class Again") && !tab(&h).dirty());
    // a new file shows up in the tree and the index
    std::fs::write(
        f.path("app/src/main/java/com/demo/data/Extra.kt"),
        "package com.demo.data\nclass Extra\n",
    )
    .unwrap();
    wait(&mut h, "new file", |a| a.index.find_type("Extra").is_some());
}

#[test]
fn e2e_git_blame_history_and_commit_diff() {
    let f = Fixture::new("git");
    let mut h = f.harness();
    open(&mut h, &f.path("app/src/main/java/com/demo/data/User.kt"));
    wait(&mut h, "blame", |a| a.tabs[a.active].git.blame.is_some());
    let b = tab(&h).git.blame.clone().unwrap();
    assert_eq!(b.line(0).unwrap().author, "Test Author");
    assert_eq!(b.line(0).unwrap().summary, "Initial commit");
    // a new unsaved line is "not committed"
    place_cursor(&mut h, "data class", 0);
    type_text(&mut h, "// new\n");
    wait(&mut h, "re-blame", |a| {
        a.tabs[a.active]
            .git
            .blame
            .as_ref()
            .is_some_and(|b| b.line(2).is_some_and(|c| c.is_uncommitted()))
    });
    // Git tab: history and the commit's diff
    h.get_by_label(&format!("{}  Git", ic::GIT_COMMIT)).click();
    wait(&mut h, "history", |a| a.git_history_len() == 1);
    h.get_by_label("Initial commit");
    h.get_by_label("Blame column").click();
    steps(&mut h, 2);
    assert!(h.state().git.show_column);
    let sha = crate::git::history(&f.path("app/src/main/java/com/demo/data/User.kt"), 1)[0]
        .short()
        .to_string();
    h.get_by_label(&sha).click();
    wait(&mut h, "commit window", |a| {
        a.commit_lines()
            .is_some_and(|l| l.iter().any(|l| l == "+class UserRepository {"))
    });
}

#[test]
fn e2e_kotlin_lsp_completion_diagnostics_definition_rename() {
    let f = Fixture::new("lsp");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_lsp.py");
    // SAFETY: tests run one at a time (SERIAL), so no other thread reads the environment now.
    unsafe { std::env::set_var("DROID_LITE_LSP", format!("python3 {}", script.display())) };
    let mut h = f.harness();
    unsafe { std::env::remove_var("DROID_LITE_LSP") };
    wait(&mut h, "lsp ready", |a| {
        a.lsp.as_ref().is_some_and(|l| l.ready)
    });
    let card = f.path("app/src/main/java/com/demo/ui/UserCard.kt");
    open(&mut h, &card);
    wait(&mut h, "diagnostics", |a| {
        a.tabs[a.active]
            .diagnostics
            .iter()
            .any(|d| d.2 == "fake error from lsp")
    });
    // ⌃Space asks the server; its item replaces the local list
    place_cursor(&mut h, "Column", 3);
    key(&mut h, Modifiers::CTRL, Key::Space);
    wait(&mut h, "lsp completion", |a| {
        a.completion
            .as_ref()
            .is_some_and(|c| c.items.iter().any(|i| i.label == "lspSuggestion"))
    });
    key(&mut h, Modifiers::NONE, Key::Escape);
    // rename goes through the server
    place_cursor(&mut h, "val count", 4);
    key(&mut h, Modifiers::COMMAND | Modifiers::ALT, Key::R);
    for _ in 0.."count".len() {
        key(&mut h, Modifiers::NONE, Key::Backspace);
    }
    type_text(&mut h, "clicks");
    key(&mut h, Modifiers::NONE, Key::Enter);
    wait(
        &mut h,
        "lsp rename preview",
        |a| matches!(&a.refactor, Some(refactor_ui::Refactor::Preview(p)) if p.source_name() == "kotlin-lsp"),
    );
    h.get_by_label("Apply").click();
    steps(&mut h, 3);
    assert!(
        std::fs::read_to_string(&card)
            .unwrap()
            .contains("val clicks = mutableStateOf(0)")
    );
    // ⇧⌘G goes through the server too
    place_cursor(&mut h, "user.name", 1);
    key(&mut h, Modifiers::COMMAND | Modifiers::SHIFT, Key::G);
    wait(&mut h, "lsp references", |a| {
        a.found.len() == 2 && a.status.contains("kotlin-lsp")
    });
}

#[test]
fn e2e_memory_stays_flat_over_repeated_work() {
    let f = Fixture::new("memory");
    let mut h = f.harness();
    let files = [
        f.path("app/src/main/java/com/demo/data/User.kt"),
        f.path("app/src/main/java/com/demo/ui/UserCard.kt"),
    ];
    let cycle = |h: &mut Harness<'static, App>| {
        let root = f.root.clone();
        let ctx = h.ctx.clone();
        h.state_mut().open_project(&root, &ctx);
        wait(h, "index", |a| {
            a.indexing.is_none() && a.index.symbol_count() > 0
        });
        for p in &files {
            open(h, p);
            type_text(h, "abc");
            wait(h, "blame", |a| a.tabs[a.active].git.blame.is_some());
            key(h, Modifiers::COMMAND | Modifiers::SHIFT, Key::T);
            type_text(h, "User");
            key(h, Modifiers::NONE, Key::Escape);
            h.state_mut().revert();
            key(h, Modifiers::COMMAND, Key::W);
        }
        steps(h, 5);
    };
    for _ in 0..3 {
        cycle(&mut h); // warm-up: caches, thread pools, egui memory reach their size
    }
    let before = crate::alloc_count::live_bytes();
    for _ in 0..15 {
        cycle(&mut h);
    }
    let grown = crate::alloc_count::live_bytes() - before;
    assert!(
        grown < 1_000_000,
        "live heap grew by {grown} bytes over 15 cycles"
    );
}

#[test]
fn e2e_snapshot_for_review() {
    let f = Fixture::new("snapshot");
    let mut h = f.harness();
    open(&mut h, &f.path("app/src/main/java/com/demo/ui/UserCard.kt"));
    place_cursor(&mut h, "Column(", 0);
    wait(&mut h, "blame", |a| a.tabs[a.active].git.blame.is_some());
    steps(&mut h, 5);
    if let Ok(dir) = std::env::var("DROID_LITE_SNAPSHOT_DIR") {
        let img = h.render().expect("render needs the wgpu feature");
        img.save(Path::new(&dir).join("e2e-snapshot.png")).unwrap();
    }
}

#[test]
fn e2e_eclipse_line_commands_and_go_to_line() {
    let f = Fixture::new("lines");
    let mut h = f.harness();
    assert_eq!(
        h.state().keymap,
        Keymap::Eclipse,
        "Eclipse is the default keymap"
    );
    open(&mut h, &f.path("app/src/main/java/com/demo/ui/UserCard.kt"));
    // ⌘L go to line 9, column 9
    key(&mut h, Modifiers::COMMAND, Key::L);
    assert!(
        h.state()
            .search
            .as_ref()
            .is_some_and(|s| s.kind == SearchKind::GotoLine)
    );
    type_text(&mut h, "9:9");
    key(&mut h, Modifiers::NONE, Key::Enter);
    assert!(tab(&h).text[tab(&h).cursor..].starts_with("Text(user.name)"));
    // ⌘/ comments the line, again uncomments
    key(&mut h, Modifiers::COMMAND, Key::Slash);
    assert!(tab(&h).text.contains("        // Text(user.name)"));
    key(&mut h, Modifiers::COMMAND, Key::Slash);
    assert_eq!(tab(&h).text, CARD_KT);
    // ⌥↑ moves it above `Column(`; ⌥↓ moves it back
    key(&mut h, Modifiers::ALT, Key::ArrowUp);
    assert!(
        tab(&h)
            .text
            .contains("        Text(user.name)\n    Column(modifier) {"),
        "{}",
        tab(&h).text
    );
    key(&mut h, Modifiers::ALT, Key::ArrowDown);
    assert_eq!(tab(&h).text, CARD_KT);
    // ⌥⌘↓ duplicates, ⌘D deletes
    key(&mut h, Modifiers::COMMAND | Modifiers::ALT, Key::ArrowDown);
    assert_eq!(tab(&h).text.matches("Text(user.name)").count(), 2);
    key(&mut h, Modifiers::COMMAND, Key::D);
    assert_eq!(tab(&h).text, CARD_KT);
    // ⌘O quick outline lists this file's declarations
    key(&mut h, Modifiers::COMMAND, Key::O);
    let hits: Vec<String> = h
        .state()
        .search
        .as_ref()
        .unwrap()
        .hits
        .iter()
        .map(|h| h.name.clone())
        .collect();
    assert!(
        hits.contains(&"UserCard".to_string()) && hits.contains(&"UserCardPreview".to_string()),
        "{hits:?}"
    );
    key(&mut h, Modifiers::NONE, Key::Escape);
    // ⌘3 search everywhere, ⇧⌘L shortcut cheat sheet
    key(&mut h, Modifiers::COMMAND, Key::Num3);
    assert!(
        h.state()
            .search
            .as_ref()
            .is_some_and(|s| s.kind == SearchKind::Everywhere)
    );
    key(&mut h, Modifiers::NONE, Key::Escape);
    key(&mut h, Modifiers::COMMAND | Modifiers::SHIFT, Key::L);
    h.get_by_label("Open type / class");
    h.get_by_label("⇧⌘T");
}

#[test]
fn e2e_find_references_lists_code_and_xml_not_strings() {
    let f = Fixture::new("refs");
    std::fs::write(f.path("app/src/main/res/layout_user.xml"), "").ok();
    std::fs::create_dir_all(f.path("app/src/main/res/layout")).unwrap();
    std::fs::write(
        f.path("app/src/main/res/layout/user_card.xml"),
        "<com.demo.ui.UserCardView tools:context=\"User\" />\n",
    )
    .unwrap();
    std::fs::write(
        f.path("app/src/main/java/com/demo/ui/Strings.kt"),
        "package com.demo.ui\nval s = \"User\" // User\nval u: User? = null\n",
    )
    .unwrap();
    let mut h = f.harness();
    open(&mut h, &f.path("app/src/main/java/com/demo/data/User.kt"));
    place_cursor(&mut h, "class User(", 6);
    key(&mut h, Modifiers::COMMAND | Modifiers::SHIFT, Key::G);
    wait(&mut h, "references", |a| {
        a.find_rx.is_none() && !a.found.is_empty()
    });
    let found = &h.state().found;
    let in_file = |name: &str| found.iter().filter(|f| f.path.ends_with(name)).count();
    assert_eq!(in_file("User.kt"), 2, "declaration and List<User>");
    assert_eq!(
        in_file("UserCard.kt"),
        3,
        "import, parameter, constructor call: {:?}",
        found.iter().map(|f| &f.text).collect::<Vec<_>>()
    );
    assert_eq!(
        in_file("Strings.kt"),
        1,
        "only the type, not the string or the comment"
    );
    assert_eq!(in_file("user_card.xml"), 1, "XML is searched as text");
    h.get_by_label_contains("References to `User`");
    // clicking a result opens it
    h.get_by_label_contains("val u: User? = null").click();
    steps(&mut h, 3);
    assert!(tab(&h).path.ends_with("Strings.kt"));
}

#[test]
fn e2e_android_studio_keymap_from_help_menu() {
    let f = Fixture::new("keymap");
    let mut h = f.harness();
    h.get_by_label("Help").click();
    steps(&mut h, 2);
    h.get_by_label("Android Studio (macOS)").click();
    steps(&mut h, 2);
    assert_eq!(h.state().keymap, Keymap::AndroidStudio);
    key(&mut h, Modifiers::COMMAND, Key::O);
    assert!(
        h.state()
            .search
            .as_ref()
            .is_some_and(|s| s.kind == SearchKind::Class),
        "⌘O is Go to class in Android Studio"
    );
    type_text(&mut h, "UserRepository");
    key(&mut h, Modifiers::NONE, Key::Enter);
    place_cursor(&mut h, "List<User>", 5);
    key(&mut h, Modifiers::COMMAND, Key::B);
    assert!(tab(&h).text[tab(&h).cursor..].starts_with("User("));
    key(&mut h, Modifiers::ALT, Key::F7);
    wait(&mut h, "usages", |a| {
        a.find_rx.is_none() && !a.found.is_empty()
    });
}
