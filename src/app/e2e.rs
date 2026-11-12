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
pub(super) static SERIAL: Mutex<()> = Mutex::new(());

const USER_KT: &str = "package com.demo.data\n\ndata class User(val name: String, var age: Int)\n\nclass UserRepository {\n    fun load(): List<User> = emptyList()\n}\n";
const CARD_KT: &str = "package com.demo.ui\n\nimport com.demo.data.User\n\n@Composable\nfun UserCard(user: User, modifier: Modifier) {\n    val count = mutableStateOf(0)\n    Column(modifier) {\n        Text(user.name)\n    }\n}\n\n@Preview\n@Composable\nprivate fun UserCardPreview() {\n    UserCard(User(\"a\", 1), Modifier)\n}\n";

/// Points the settings folder at a fresh temp folder for a unit test (serialised with the e2e tests).
pub(super) fn config_guard() -> MutexGuard<'static, ()> {
    let lock = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("droid-lite-cfg-unit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    // SAFETY: the SERIAL lock is held, so no other test reads the environment meanwhile.
    unsafe { std::env::set_var("DROID_LITE_CONFIG_DIR", &dir) };
    lock
}

struct Fixture {
    root: PathBuf,
    config: PathBuf,
    _lock: MutexGuard<'static, ()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_dir_all(&self.config);
        crate::theme::set_light(false);
        crate::theme::set_font_size(13.0);
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
            "plugins { id(\"com.android.application\"); id(\"app.cash.paparazzi\") }\n",
        );
        write("app/src/main/java/com/demo/data/User.kt", USER_KT);
        write("app/src/main/java/com/demo/ui/UserCard.kt", CARD_KT);
        let png = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/icon-256.png");
        write(
            "gradlew",
            &format!(
                "#!/bin/sh\necho \"> Task $1\"\ncase \"$1\" in\n  *recordPaparazziDebug) mkdir -p app/src/test/snapshots/images && cp '{}' app/src/test/snapshots/images/com.demo.ui_UserCardKt_UserCardPreview.png; echo 'BUILD SUCCESSFUL';;\n  *compileDebugKotlin) echo \"w: file://$PWD/app/src/main/java/com/demo/ui/UserCard.kt:6:18 Parameter 'user' is never used\"; echo \"e: file://$PWD/app/src/main/java/com/demo/data/User.kt:3:40 Unresolved reference 'Intt'.\"; echo 'BUILD FAILED'; exit 1;;\n  *failingTask) echo \"e: file://$PWD/app/src/main/java/com/demo/ui/UserCard.kt:7:9 Unresolved reference 'oops'.\"; echo 'BUILD FAILED'; exit 1;;\n  *) echo 'BUILD SUCCESSFUL';;\nesac\n",
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
        // every fixture gets its own settings folder (sessions, history, settings.json)
        let config =
            std::env::temp_dir().join(format!("droid-lite-cfg-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&config);
        // SAFETY: the SERIAL lock is held, so no other test reads the environment meanwhile.
        unsafe { std::env::set_var("DROID_LITE_CONFIG_DIR", &config) };
        // tests never start the kotlin-lsp installed on this Mac; the LSP test sets the fake one
        unsafe { std::env::set_var("DROID_LITE_LSP", "/nonexistent/droid-lite-no-lsp") };
        // nor any real phone: adb is missing unless a Logcat test sets the fake one
        unsafe { std::env::set_var("DROID_LITE_ADB", "/nonexistent/droid-lite-no-adb") };
        // and never index the real Gradle cache of this Mac
        unsafe {
            std::env::set_var(
                "DROID_LITE_GRADLE_HOME",
                "/nonexistent/droid-lite-no-gradle",
            )
        };
        Fixture {
            root,
            config,
            _lock: lock,
        }
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
    // stability is inline now: a badge after `fun UserCard(` says one parameter is unstable
    let a = tab(&h).annotations.as_ref().expect("annotations");
    let badge = a
        .badges
        .iter()
        .find(|b| b.line == 5)
        .expect("badge on the UserCard line");
    assert!(badge.text.ends_with("1 unstable"), "{}", badge.text);
    assert_eq!(
        badge.stability.params[0].verdict,
        crate::compose::Verdict::Unstable,
        "User has a var"
    );
    assert!(
        h.query_by_label("UNSTABLE").is_none(),
        "no separate stability section any more"
    );
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
    h.get_by_label(&sha).scroll_to_me();
    steps(&mut h, 3);
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

    assert!(
        h.state().lsp.is_none(),
        "kotlin-lsp waits until a code file opens"
    );
    let card = f.path("app/src/main/java/com/demo/ui/UserCard.kt");
    open(&mut h, &card);
    wait(&mut h, "lsp ready", |a| {
        a.lsp.as_ref().is_some_and(|l| l.ready)
    });
    // SAFETY: as above; the server has started, so the variable is no longer needed
    unsafe { std::env::remove_var("DROID_LITE_LSP") };
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
    // a second snapshot with gutter icons and an inline stability badge on one screen
    std::fs::write(f.path("app/src/main/java/com/demo/data/Repo.kt"), REPO_KT).unwrap();
    let both = format!(
        "{CARD_KT}\n{}",
        REPO_KT.replace("package com.demo.data\n", "")
    );
    std::fs::write(f.path("app/src/main/java/com/demo/ui/Both.kt"), both).unwrap();
    wait(&mut h, "index Both", |a| {
        a.index.find_type("FakeRepo").is_some()
    });
    open(&mut h, &f.path("app/src/main/java/com/demo/ui/Both.kt"));
    place_cursor(&mut h, "fun UserCard", 4);
    wait(&mut h, "blame", |a| a.tabs[a.active].git.blame.is_some());
    steps(&mut h, 5);
    if let Ok(dir) = std::env::var("DROID_LITE_SNAPSHOT_DIR") {
        h.render()
            .unwrap()
            .save(Path::new(&dir).join("e2e-gutter.png"))
            .unwrap();
    }
    // the same screen in the light theme
    let ctx = h.ctx.clone();
    comfort::Settings {
        light: true,
        ..Default::default()
    }
    .apply(&ctx, false);
    h.state_mut().tabs.iter_mut().for_each(|t| t.recolor = true);
    steps(&mut h, 5);
    if let Ok(dir) = std::env::var("DROID_LITE_SNAPSHOT_DIR") {
        h.render()
            .unwrap()
            .save(Path::new(&dir).join("e2e-light.png"))
            .unwrap();
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
    // every row sits exactly one list step below the last, so scrolling never jumps
    {
        let font = egui::FontId::monospace(crate::theme::font_size());
        let step =
            h.ctx.fonts_mut(|f| f.row_height(&font)) + h.ctx.global_style().spacing.item_spacing.y;
        let rows: Vec<f32> = h
            .get_all_by_role(egui::accesskit::Role::Link)
            .map(|n| n.rect().center().y)
            .collect();
        assert!(rows.len() >= 3, "{rows:?}");
        for w in rows.windows(2) {
            assert!((w[1] - w[0] - step).abs() < 0.5, "step {step}: {rows:?}");
        }
    }
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

#[test]
fn e2e_bulk_disk_changes_and_background_open_do_not_block() {
    let f = Fixture::new("bulk");
    let mut h = f.harness();
    // opening a project returns at once; the work happens on a worker thread
    let root = f.root.clone();
    let ctx = h.ctx.clone();
    let t = Instant::now();
    h.state_mut().open_project(&root, &ctx);
    assert!(
        t.elapsed().as_millis() < 50,
        "open_project blocked for {:?}",
        t.elapsed()
    );
    wait(&mut h, "reopen", |a| {
        a.project.is_some() && a.indexing.is_none()
    });
    // a "git checkout" of 300 files: all show up in the index, frames stay fast
    let dir = f.path("app/src/main/java/com/demo/gen");
    std::fs::create_dir_all(&dir).unwrap();
    for i in 0..300 {
        std::fs::write(
            dir.join(format!("Gen{i}.kt")),
            format!("package com.demo.gen\nclass Gen{i}\n"),
        )
        .unwrap();
    }
    let mut worst = 0u128;
    for _ in 0..1500 {
        let t = Instant::now();
        h.step();
        worst = worst.max(t.elapsed().as_millis());
        if (0..300).all(|i| h.state().index.find_type(&format!("Gen{i}")).is_some()) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        (0..300).all(|i| h.state().index.find_type(&format!("Gen{i}")).is_some()),
        "all 300 new files indexed"
    );
    assert!(
        worst < 100,
        "a frame took {worst} ms while 300 files changed"
    );
    assert!(
        h.state().slow.is_none(),
        "watchdog saw a slow frame: {:?}",
        h.state().slow
    );
    // deleting the folder drops them again
    std::fs::remove_dir_all(&dir).unwrap();
    wait(&mut h, "folder delete", |a| {
        a.index.find_type("Gen0").is_none()
    });
}

#[test]
fn watchdog_reports_only_slow_frames() {
    let mut w = Watchdog::start();
    w.stage("fast", || ());
    assert!(w.finish(10).is_none());
    let mut w = Watchdog::start();
    w.stage("fast", || ());
    w.stage("editor", || std::thread::sleep(Duration::from_millis(120)));
    let r = w.finish(10).unwrap();
    assert!(r.contains("mostly editor"), "{r}");
    let mut w = Watchdog::start();
    w.stage("startup", || std::thread::sleep(Duration::from_millis(120)));
    assert!(w.finish(1).is_none(), "first frames are skipped");
}

const REPO_KT: &str = "package com.demo.data\n\ninterface Repo {\n    fun load(): List<User>\n}\n\nclass RealRepo : Repo {\n    override fun load(): List<User> = emptyList()\n}\n\nclass FakeRepo : Repo {\n    override fun load(): List<User> = listOf(User(\"a\", 1))\n}\n";

/// Clicks a point the way a mouse would.
fn click_at(h: &mut Harness<'static, App>, pos: egui::Pos2, button: egui::PointerButton) {
    h.event(Event::PointerMoved(pos));
    h.event(Event::PointerButton {
        pos,
        button,
        pressed: true,
        modifiers: Modifiers::NONE,
    });
    steps(h, 1);
    h.event(Event::PointerButton {
        pos,
        button,
        pressed: false,
        modifiers: Modifiers::NONE,
    });
    steps(h, 3);
}

/// Screen position of `line` (0-based) in the gutter icon column of the active editor.
fn icon_pos(h: &mut Harness<'static, App>, line: usize) -> egui::Pos2 {
    let editor = h
        .get_by_role(egui::accesskit::Role::MultilineTextInput)
        .rect();
    let ctx = h.ctx.clone();
    let font = egui::FontId::monospace(crate::theme::font_size());
    let (row_h, char_w) = ctx.fonts_mut(|f| (f.row_height(&font), f.glyph_width(&font, '0')));
    let lines = tab(h).text.matches('\n').count() + 1;
    let gutter_w = char_w * lines.to_string().len().max(3) as f32 + 18.0;
    egui::pos2(
        editor.left() - gutter_w - gutter::ICON_WIDTH + 7.0,
        editor.top() + row_h * (line as f32 + 0.5),
    )
}

#[test]
fn e2e_gutter_icons_implementations_and_overrides() {
    let f = Fixture::new("gutter");
    std::fs::write(f.path("app/src/main/java/com/demo/data/Repo.kt"), REPO_KT).unwrap();
    let mut h = f.harness();
    let repo = f.path("app/src/main/java/com/demo/data/Repo.kt");
    open(&mut h, &repo);
    steps(&mut h, 3);
    let a = tab(&h).annotations.as_ref().unwrap();
    let marks: Vec<(usize, bool, usize)> = a
        .marks
        .iter()
        .map(|m| (m.line, m.up, m.targets.len()))
        .collect();
    // interface Repo (line 2): implemented by 2; fun load (3): implemented in 2;
    // RealRepo (6) / FakeRepo (10): implement Repo; their load (7, 11): override
    for want in [
        (2, false, 2),
        (3, false, 2),
        (6, true, 1),
        (7, true, 1),
        (10, true, 1),
        (11, true, 1),
    ] {
        assert!(marks.contains(&want), "missing {want:?} in {marks:?}");
    }
    // clicking the "implemented by" icon of `interface Repo` offers both classes
    let p = icon_pos(&mut h, 2);
    click_at(&mut h, p, egui::PointerButton::Primary);
    let s = h
        .state()
        .search
        .as_ref()
        .expect("picker after clicking the icon");
    assert_eq!(s.kind, SearchKind::Pick);
    assert_eq!(s.hits.len(), 2);
    key(&mut h, Modifiers::NONE, Key::Escape);
    // clicking the "overrides" icon of FakeRepo.load jumps to Repo.load
    let p = icon_pos(&mut h, 11);
    click_at(&mut h, p, egui::PointerButton::Primary);
    assert!(
        tab(&h).text[tab(&h).cursor..].starts_with("load(): List<User>\n}"),
        "at the interface method"
    );
    // ⌘T on the interface name lists the implementations too (Eclipse quick hierarchy)
    place_cursor(&mut h, "interface Repo", 10);
    key(&mut h, Modifiers::COMMAND, Key::T);
    assert_eq!(h.state().search.as_ref().map(|s| s.hits.len()), Some(2));
}

#[test]
fn e2e_right_click_menu_runs_commands() {
    let f = Fixture::new("menu");
    let mut h = f.harness();
    open(&mut h, &f.path("app/src/main/java/com/demo/ui/UserCard.kt"));
    let editor = h
        .get_by_role(egui::accesskit::Role::MultilineTextInput)
        .rect();
    let ctx = h.ctx.clone();
    let row_h =
        ctx.fonts_mut(|f| f.row_height(&egui::FontId::monospace(crate::theme::font_size())));
    // right-click on line 9 (`Text(user.name)`) moves the cursor there and opens the menu
    click_at(
        &mut h,
        egui::pos2(editor.left() + 120.0, editor.top() + row_h * 8.5),
        egui::PointerButton::Secondary,
    );
    for label in [
        "Go to declaration",
        "Go to implementations",
        "Find references",
        "Rename",
    ] {
        h.get_by_label_contains(label);
    }
    h.get_by_label_contains("Comment / uncomment lines").click();
    steps(&mut h, 3);
    assert!(
        tab(&h).text.contains("        // Text(user.name)"),
        "{}",
        tab(&h).text
    );
}

#[test]
fn e2e_find_replace_bar() {
    let f = Fixture::new("findbar");
    let mut h = f.harness();
    open(&mut h, &f.path("app/src/main/java/com/demo/ui/UserCard.kt"));
    key(&mut h, Modifiers::COMMAND, Key::F);
    assert!(h.state().find.is_some());
    type_text(&mut h, "User");
    steps(&mut h, 2);
    assert_eq!(
        h.state().find.as_ref().unwrap().matches.len(),
        8,
        "case-insensitive: user, User, UserCard…"
    );
    h.get_by_label("Aa").click();
    h.get_by_label("W").click();
    steps(&mut h, 2);
    assert_eq!(
        h.state().find.as_ref().unwrap().matches.len(),
        3,
        "whole word, case: import, parameter type, constructor"
    );
    key(&mut h, Modifiers::NONE, Key::Enter);
    let sel = tab(&h).selection.clone();
    assert_eq!(&tab(&h).text[sel], "User", "Enter selects the next match");
    // replace all
    let replace = h
        .get_by_role_and_label(egui::accesskit::Role::TextInput, "Replace with")
        .rect()
        .center();
    click_at(&mut h, replace, egui::PointerButton::Primary);
    type_text(&mut h, "Person");
    h.get_by_label("Replace all").click();
    steps(&mut h, 3);
    assert_eq!(tab(&h).text.matches("Person").count(), 3);
    assert!(
        tab(&h).text.contains("fun UserCard("),
        "whole word kept UserCard"
    );
    key(&mut h, Modifiers::COMMAND, Key::F); // focus the bar, then Esc closes it
    key(&mut h, Modifiers::NONE, Key::Escape);
    assert!(h.state().find.is_none());
}

#[test]
fn e2e_smart_typing_enter_and_pairs() {
    let f = Fixture::new("typing");
    let mut h = f.harness();
    open(&mut h, &f.path("app/src/main/java/com/demo/ui/UserCard.kt"));
    place_cursor(&mut h, "        Text(user.name)", 23);
    key(&mut h, Modifiers::NONE, Key::Enter);
    type_text(&mut h, "f");
    type_text(&mut h, "(");
    assert!(
        tab(&h)
            .text
            .contains("        Text(user.name)\n        f()"),
        "{}",
        tab(&h).text
    );
    type_text(&mut h, ")");
    assert!(
        !tab(&h).text.contains("f())"),
        "typing the closer steps over it"
    );
    key(&mut h, Modifiers::NONE, Key::ArrowLeft);
    key(&mut h, Modifiers::NONE, Key::Backspace);
    assert!(
        tab(&h).text.contains("        f\n"),
        "Backspace removes the empty pair: {}",
        tab(&h).text
    );
    type_text(&mut h, " ");
    type_text(&mut h, "{");
    key(&mut h, Modifiers::NONE, Key::Enter);
    assert!(
        tab(&h)
            .text
            .contains("        f {\n            \n        }"),
        "{}",
        tab(&h).text
    );
}

#[test]
fn e2e_cmd_hover_shows_declaration_and_usages() {
    let f = Fixture::new("hover");
    let mut h = f.harness();
    open(&mut h, &f.path("app/src/main/java/com/demo/ui/UserCard.kt"));
    let editor = h
        .get_by_role(egui::accesskit::Role::MultilineTextInput)
        .rect();
    let ctx = h.ctx.clone();
    let font = egui::FontId::monospace(crate::theme::font_size());
    let (row_h, char_w) = ctx.fonts_mut(|f| (f.row_height(&font), f.glyph_width(&font, '0')));
    // "fun UserCard(user: User" : hover the type `User` at column 20 of line 6 with ⌘ held
    let pos = egui::pos2(editor.left() + char_w * 21.5, editor.top() + row_h * 5.5);
    h.event(Event::PointerMoved(pos));
    h.event(Event::ModifiersChanged(Modifiers::COMMAND));
    for _ in 0..80 {
        h.step();
        if h.state().usages.get("User").is_some_and(|u| u.1.is_some()) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    steps(&mut h, 30); // tooltips appear after a short delay
    let found = h
        .state()
        .usages
        .get("User")
        .and_then(|u| u.1.clone())
        .expect("usages counted");
    assert_eq!(found.len(), 5, "User.kt: 2, UserCard.kt: 3");
    // the card leaves out the declaration `class User(` itself
    h.get_by_label_contains("4 usages in 2 files");
    // a usage line from the other file; the declaration line is not listed as a usage
    h.get_by_label_contains("User.kt:6");
    assert!(h.query_by_label_contains("User.kt:3").is_none());
    h.event(Event::ModifiersChanged(Modifiers::NONE));
}

#[test]
fn e2e_build_shows_compile_errors_in_editor_and_problems() {
    let f = Fixture::new("build");
    let mut h = f.harness();
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    open(&mut h, &user);
    key(&mut h, Modifiers::COMMAND, Key::B); // Eclipse: Build
    wait(&mut h, "build", |a| {
        a.gradle.is_none() && !a.build_problems.is_empty()
    });
    assert!(
        h.state()
            .console
            .iter()
            .any(|l| l == "> Task :app:compileDebugKotlin")
    );
    assert_eq!(
        h.state().build_problems[&user],
        vec![(3, 40, 1, "Unresolved reference 'Intt'.".to_string())]
    );
    assert_eq!(h.state().bottom, Bottom::Problems);
    h.get_by_label("Unresolved reference 'Intt'.").click();
    steps(&mut h, 3);
    assert_eq!(tab(&h).path, user);
    assert_eq!(
        edit::line_offset(&tab(&h).text, "3:40"),
        Some(tab(&h).cursor)
    );
}

#[test]
fn e2e_git_tab_lists_changes_stage_commit_and_branch() {
    let f = Fixture::new("gitchanges");
    let mut h = f.harness();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&f.root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    // a branch with one committed change, plus uncommitted work
    let base = String::from_utf8(
        std::process::Command::new("git")
            .arg("-C")
            .arg(&f.root)
            .args(["branch", "--show-current"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    git(&["checkout", "-q", "-b", "feature"]);
    std::fs::write(
        f.path("app/src/main/java/com/demo/data/User.kt"),
        USER_KT.replace("var age", "val age"),
    )
    .unwrap();
    git(&["commit", "-q", "-am", "age is val"]);
    git(&["update-ref", &format!("refs/remotes/origin/{base}"), &base]); // stand-in for the remote base
    std::fs::write(
        f.path("app/src/main/java/com/demo/ui/UserCard.kt"),
        CARD_KT.replace("count", "clicks"),
    )
    .unwrap();
    std::fs::write(
        f.path("app/src/main/java/com/demo/ui/New.kt"),
        "package com.demo.ui\n",
    )
    .unwrap();
    h.get_by_label(&format!("{}  Git", ic::GIT_COMMIT)).click();
    wait(&mut h, "changes", |a| {
        a.git
            .changes
            .as_ref()
            .is_some_and(|c| c.working.len() == 2 && c.branch.len() == 1)
    });
    h.get_by_label("Uncommitted changes (2)");
    h.get_by_label_contains("This branch: 1 files changed vs origin/");
    // the diff of the branch file
    h.get_by_label("app/src/main/java/com/demo/data/User.kt")
        .scroll_to_me();
    steps(&mut h, 3);
    h.get_by_label("app/src/main/java/com/demo/data/User.kt")
        .click();
    wait(&mut h, "diff", |a| {
        a.commit_lines().is_some_and(|l| {
            l.iter()
                .any(|l| l.contains("+data class User(val name: String, val age: Int)"))
        })
    });
    key(&mut h, Modifiers::NONE, Key::Escape);
    assert!(h.state().commit_lines().is_none(), "Esc closes the diff");
    steps(&mut h, 2);
    // stage one file and commit it
    h.get_by_label("app/src/main/java/com/demo/ui/UserCard.kt")
        .scroll_to_me();
    steps(&mut h, 3);
    let row = h
        .get_by_label("app/src/main/java/com/demo/ui/UserCard.kt")
        .rect();
    let stage = h
        .get_all_by_label("Stage")
        .find(|n| (n.rect().center().y - row.center().y).abs() < 4.0)
        .unwrap()
        .rect()
        .center();
    click_at(&mut h, stage, egui::PointerButton::Primary);
    wait(&mut h, "staged", |a| {
        a.git
            .changes
            .as_ref()
            .is_some_and(|c| c.working.iter().any(|w| w.staged))
    });
    h.get_by_role_and_label(egui::accesskit::Role::TextInput, "Commit message")
        .scroll_to_me();
    steps(&mut h, 3);
    let msg = h
        .get_by_role_and_label(egui::accesskit::Role::TextInput, "Commit message")
        .rect()
        .center();
    click_at(&mut h, msg, egui::PointerButton::Primary);
    type_text(&mut h, "Rename count");
    h.get_by_label("Commit staged").scroll_to_me();
    steps(&mut h, 2);
    h.get_by_label("Commit staged").click();
    wait(&mut h, "committed", |a| {
        a.git
            .changes
            .as_ref()
            .is_some_and(|c| c.working.len() == 1 && c.branch.len() == 2)
    });
    let log = String::from_utf8(
        std::process::Command::new("git")
            .arg("-C")
            .arg(&f.root)
            .args(["log", "-1", "--format=%s"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert_eq!(log.trim(), "Rename count");
}

#[test]
fn e2e_ctrl_and_cmd_click_open_the_declaration() {
    let f = Fixture::new("ctrlclick");
    let mut h = f.harness();
    let card = f.path("app/src/main/java/com/demo/ui/UserCard.kt");
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    for mods in [Modifiers::CTRL, Modifiers::COMMAND] {
        open(&mut h, &card);
        let editor = h
            .get_by_role(egui::accesskit::Role::MultilineTextInput)
            .rect();
        let ctx = h.ctx.clone();
        let font = egui::FontId::monospace(crate::theme::font_size());
        let (row_h, char_w) = ctx.fonts_mut(|f| (f.row_height(&font), f.glyph_width(&font, '0')));
        // the type `User` in "fun UserCard(user: User, …)" on line 6
        let pos = egui::pos2(editor.left() + char_w * 21.5, editor.top() + row_h * 5.5);
        h.event(Event::ModifiersChanged(mods));
        h.event(Event::PointerMoved(pos));
        steps(&mut h, 2);
        h.event(Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: mods,
        });
        steps(&mut h, 1);
        h.event(Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: mods,
        });
        steps(&mut h, 3);
        h.event(Event::ModifiersChanged(Modifiers::NONE));
        steps(&mut h, 2);
        assert_eq!(tab(&h).path, user, "{mods:?}-click on `User` opens User.kt");
        assert!(
            tab(&h).text[tab(&h).cursor..].starts_with("User("),
            "{mods:?}: cursor on the declaration"
        );
    }
}

#[test]
fn e2e_session_restore_history_autosave_and_settings() {
    let f = Fixture::new("comfort");
    let card = f.path("app/src/main/java/com/demo/ui/UserCard.kt");
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    {
        let mut h = f.harness();
        open(&mut h, &user);
        open(&mut h, &card);
        place_cursor(&mut h, "Column(", 0);
        h.state_mut().save_session(true);
    }
    // a new window on the same project reopens both tabs, the cursor where it was
    let mut h = f.harness();
    wait(&mut h, "session", |a| a.tabs.len() == 2);
    assert_eq!(tab(&h).path, card);
    assert!(tab(&h).text[tab(&h).cursor..].starts_with("Column("));
    // edits saved twice: two versions in local history
    type_text(&mut h, "// one\n");
    key(&mut h, Modifiers::COMMAND, Key::S);
    type_text(&mut h, "// two\n");
    key(&mut h, Modifiers::COMMAND, Key::S);
    let versions = comfort::history(&card);
    assert_eq!(versions.len(), 2, "the original and the first save");
    assert_eq!(std::fs::read_to_string(&versions[1].file).unwrap(), CARD_KT);
    // the History window restores the original into the editor
    h.state_mut().history_for = Some(card.clone());
    steps(&mut h, 3);
    h.get_by_label_contains("Local history: UserCard.kt");
    let restore = h
        .get_all_by_label("Restore")
        .last()
        .unwrap()
        .rect()
        .center();
    click_at(&mut h, restore, egui::PointerButton::Primary);
    assert_eq!(tab(&h).text, CARD_KT);
    assert!(tab(&h).dirty(), "restored text waits for ⌘S");
    // auto-save when the window loses focus
    h.input_mut().focused = true;
    steps(&mut h, 2);
    h.input_mut().focused = false; // the user switches to another app
    steps(&mut h, 3);
    assert_eq!(
        std::fs::read_to_string(&card).unwrap(),
        CARD_KT,
        "auto-saved"
    );
    assert!(!tab(&h).dirty());
    h.input_mut().focused = true;
    steps(&mut h, 2);
    // ⌘, Settings: the light theme applies at once and is saved
    key(&mut h, Modifiers::COMMAND, Key::Comma);
    assert!(h.state().show_settings, "⌘, opens Settings");
    h.get_by_label("Light").click();
    steps(&mut h, 2);
    assert!(h.state().settings.light, "the Light option is chosen");
    steps(&mut h, 3);
    assert!(std::ptr::eq(crate::theme::t(), &crate::theme::LIGHT));
    let saved = std::fs::read_to_string(f.config.join("settings.json")).unwrap();
    assert!(saved.contains("\"light\": true"), "{saved}");
}

#[test]
fn e2e_android_resources_catalog_and_swatches() {
    let f = Fixture::new("android");
    let w = |rel: &str, text: &str| {
        let p = f.path(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    };
    w(
        "app/src/main/res/values/strings.xml",
        "<resources>\n    <string name=\"app_name\">Shop</string>\n</resources>\n",
    );
    w(
        "app/src/main/res/values/colors.xml",
        "<resources>\n    <color name=\"brand\">#6200EE</color>\n</resources>\n",
    );
    w(
        "app/src/main/res/values-night/colors.xml",
        "<resources>\n    <color name=\"brand\">#BB86FC</color>\n</resources>\n",
    );
    w(
        "app/src/main/res/layout/main.xml",
        "<TextView android:id=\"@+id/title\" android:text=\"@string/app_name\"/>\n",
    );
    w(
        "gradle/libs.versions.toml",
        "[versions]\ncore = \"1.13\"\n\n[libraries]\nandroidx-core-ktx = { module = \"androidx.core:core-ktx\", version.ref = \"core\" }\n",
    );
    w(
        "app/build.gradle.kts",
        "plugins { id(\"com.android.application\") }\ndependencies {\n    implementation(libs.androidx.core.ktx)\n}\n",
    );
    w(
        "app/src/main/java/com/demo/ui/Res.kt",
        "package com.demo.ui\n\nfun res() {\n    val name = stringResource(R.string.app_name)\n    val brand = colorResource(R.color.brand)\n    val purple = Color(0xFF6200EE)\n}\n",
    );
    let mut h = f.harness();
    wait(&mut h, "resources", |a| a.res.len() >= 3);
    let res_kt = f.path("app/src/main/java/com/demo/ui/Res.kt");
    open(&mut h, &res_kt);
    steps(&mut h, 3);
    // swatches on the R.color line and the Color(0x…) line
    let sw: Vec<usize> = tab(&h)
        .annotations
        .as_ref()
        .unwrap()
        .swatches
        .iter()
        .map(|(l, _)| *l)
        .collect();
    assert_eq!(sw, vec![4, 5]);
    // F3 on R.string.app_name opens strings.xml at the name
    place_cursor(&mut h, "app_name)", 2);
    key(&mut h, Modifiers::NONE, Key::F3);
    assert!(tab(&h).path.ends_with("values/strings.xml"));
    assert!(tab(&h).text[tab(&h).cursor..].starts_with("app_name\""));
    // R.color.brand exists twice (default and night): a picker
    open(&mut h, &res_kt);
    place_cursor(&mut h, "brand)", 1);
    key(&mut h, Modifiers::NONE, Key::F3);
    assert_eq!(h.state().search.as_ref().map(|s| s.hits.len()), Some(2));
    key(&mut h, Modifiers::NONE, Key::Escape);
    // XML: @string/app_name in a layout
    open(&mut h, &f.path("app/src/main/res/layout/main.xml"));
    place_cursor(&mut h, "@string/app_name", 10);
    key(&mut h, Modifiers::NONE, Key::F3);
    assert!(tab(&h).path.ends_with("values/strings.xml"));
    // the catalog: libs.androidx.core.ktx in the build script
    open(&mut h, &f.path("app/build.gradle.kts"));
    place_cursor(&mut h, "core.ktx", 1);
    key(&mut h, Modifiers::NONE, Key::F3);
    assert!(tab(&h).path.ends_with("gradle/libs.versions.toml"));
    assert!(tab(&h).text[tab(&h).cursor..].starts_with("androidx-core-ktx = "));
    // ⌘-hover on R.string.app_name shows its value
    open(&mut h, &res_kt);
    let editor = h
        .get_by_role(egui::accesskit::Role::MultilineTextInput)
        .rect();
    let ctx = h.ctx.clone();
    let font = egui::FontId::monospace(crate::theme::font_size());
    let (row_h, char_w) = ctx.fonts_mut(|f| (f.row_height(&font), f.glyph_width(&font, '0')));
    let col = "    val name = stringResource(R.string.".len() as f32 + 2.0;
    let pos = egui::pos2(editor.left() + char_w * col, editor.top() + row_h * 3.5);
    h.event(Event::ModifiersChanged(Modifiers::COMMAND));
    h.event(Event::PointerMoved(pos));
    steps(&mut h, 30);
    h.get_by_label_contains("default:  Shop");
    h.event(Event::ModifiersChanged(Modifiers::NONE));
}

#[test]
fn e2e_library_classes_search_completion_and_sources() {
    let f = Fixture::new("libs");
    let home = f.config.join("gradle-home");
    if !crate::libs::tests::fake_cache(&home) {
        eprintln!("no javac: skipped");
        return;
    }
    // SAFETY: the fixture holds the SERIAL lock.
    unsafe { std::env::set_var("DROID_LITE_GRADLE_HOME", &home) };
    std::fs::create_dir_all(f.path("gradle")).unwrap();
    std::fs::write(
        f.path("gradle/libs.versions.toml"),
        "[libraries]\ndemo-ui = { module = \"demo.lib:ui\", version = \"1.0\" }\n",
    )
    .unwrap();
    std::fs::write(
        f.path("app/src/main/java/com/demo/ui/Lib.kt"),
        "package com.demo.ui\n\n@Composable\nfun Screen(repo: Repo) {\n    Text(\"hi\")\n}\n",
    )
    .unwrap();
    let mut h = f.harness();
    wait(&mut h, "libraries", |a| a.libs.artifacts == 2);
    // ⇧⌘T finds a library interface
    key(&mut h, Modifiers::COMMAND | Modifiers::SHIFT, Key::T);
    type_text(&mut h, "Modifier");
    let hit = h
        .state()
        .search
        .as_ref()
        .unwrap()
        .hits
        .first()
        .cloned()
        .expect("a hit");
    assert!(
        hit.detail.contains("demo.ui") && hit.detail.contains("demo.lib:ui-android"),
        "{}",
        hit.detail
    );
    key(&mut h, Modifiers::NONE, Key::Escape);
    // completion offers a library composable with its import
    let lib_kt = f.path("app/src/main/java/com/demo/ui/Lib.kt");
    open(&mut h, &lib_kt);
    place_cursor(&mut h, "Text(\"hi\")", "Text(\"hi\")".len());
    key(&mut h, Modifiers::NONE, Key::Enter);
    type_text(&mut h, "CoreBu");
    let c = h.state().completion.as_ref().expect("completion");
    let item = c
        .items
        .iter()
        .find(|i| i.label == "CoreButton")
        .expect("CoreButton offered");
    assert_eq!(item.import.as_deref(), Some("demo.core.CoreButton"));
    key(&mut h, Modifiers::NONE, Key::Escape);
    // the UI tree knows Text is a library composable
    place_cursor(&mut h, "Text(", 0);
    steps(&mut h, 3);
    assert_eq!(
        h.state()
            .compose_view
            .as_ref()
            .unwrap()
            .tree
            .iter()
            .map(|n| n.name.as_str())
            .collect::<Vec<_>>(),
        vec!["Text"]
    );
    // F3 on a library type with sources opens them read-only
    place_cursor(&mut h, "repo: Repo", 6);
    key(&mut h, Modifiers::NONE, Key::F3);
    assert!(
        crate::libs::is_archive_path(&tab(&h).path),
        "{}",
        tab(&h).path.display()
    );
    assert!(tab(&h).text.contains("interface Repo") && tab(&h).readonly);
    type_text(&mut h, "x");
    assert!(!tab(&h).dirty(), "typing does not change a library file");
    key(&mut h, Modifiers::COMMAND, Key::S);
    assert_eq!(h.state().status, "Library sources are read-only");
    // F3 on a library function without sources explains it
    key(&mut h, Modifiers::COMMAND, Key::OpenBracket);
    place_cursor(&mut h, "Text(", 1);
    key(&mut h, Modifiers::NONE, Key::F3);
    assert!(
        h.state().status.contains("no sources"),
        "{}",
        h.state().status
    );
}

#[test]
fn e2e_change_bars_show_and_revert_uncommitted_lines() {
    let f = Fixture::new("bars");
    let mut h = f.harness();
    let card = f.path("app/src/main/java/com/demo/ui/UserCard.kt");
    open(&mut h, &card);
    // modify line 7, add a line after line 9, delete line 13 (`@Preview`)
    let edited = CARD_KT
        .replace(
            "val count = mutableStateOf(0)",
            "val count = remember { mutableStateOf(0) }",
        )
        .replace(
            "        Text(user.name)\n",
            "        Text(user.name)\n        Spacer()\n",
        )
        .replace("@Preview\n", "");
    {
        let t = h.state_mut().tabs.last_mut().unwrap();
        t.text = edited.clone();
        t.mark_edited();
    }
    wait(&mut h, "bars", |a| {
        a.tabs[a.active]
            .git
            .hunks
            .as_ref()
            .is_some_and(|v| v.len() == 3)
    });
    let hunks = tab(&h).git.hunks.clone().unwrap();
    assert_eq!(
        hunks
            .iter()
            .map(|x| (x.kind(), x.new_start))
            .collect::<Vec<_>>(),
        vec![('M', 6), ('A', 9), ('D', 13)]
    );
    // the tree knows the file is modified once saved
    key(&mut h, Modifiers::COMMAND, Key::S);
    wait(&mut h, "status", |a| {
        a.git.changes.as_ref().is_some_and(|c| {
            c.working
                .iter()
                .any(|w| w.path.ends_with("UserCard.kt") && w.code == 'M')
        })
    });
    if let Ok(dir) = std::env::var("DROID_LITE_SNAPSHOT_DIR") {
        steps(&mut h, 3);
        h.render()
            .unwrap()
            .save(Path::new(&dir).join("e2e-bars.png"))
            .unwrap();
    }
    // click the blue bar of line 7: it goes back to HEAD
    let editor = h
        .get_by_role(egui::accesskit::Role::MultilineTextInput)
        .rect();
    let ctx = h.ctx.clone();
    let font = egui::FontId::monospace(crate::theme::font_size());
    let row_h = ctx.fonts_mut(|f| f.row_height(&font));
    click_at(
        &mut h,
        egui::pos2(editor.left() - 4.5, editor.top() + row_h * 6.5),
        egui::PointerButton::Primary,
    );
    assert!(
        tab(&h).text.contains("    val count = mutableStateOf(0)\n"),
        "{}",
        tab(&h).text
    );
    assert!(
        tab(&h).text.contains("Spacer()"),
        "only that change is reverted"
    );
    wait(&mut h, "bars after revert", |a| {
        a.tabs[a.active]
            .git
            .hunks
            .as_ref()
            .is_some_and(|v| v.len() == 2)
    });
}

/// Files for the usages tests: a Compose-navigation style extension function and its callers.
fn write_destination(f: &Fixture) -> (PathBuf, PathBuf) {
    let dest = f.path("app/src/main/java/com/demo/ui/StartCardDestination.kt");
    let host = f.path("app/src/main/java/com/demo/ui/StartCardFragment.kt");
    std::fs::write(
        &dest,
        "package com.demo.ui\n\ninternal fun NavGraphBuilder.startCardDestination(\n    onBack: () -> Unit,\n) {\n}\n",
    )
    .unwrap();
    std::fs::write(
        &host,
        "package com.demo.ui\n\nimport com.demo.ui.startCardDestination\n\nfun graph(b: NavGraphBuilder) {\n    b.startCardDestination(onBack = {})\n}\n",
    )
    .unwrap();
    (dest, host)
}

/// ⌘-clicks the name at (column, row) of the open editor.
fn cmd_click_at(h: &mut Harness<'static, App>, col: f32, row: f32) {
    let editor = h
        .get_by_role(egui::accesskit::Role::MultilineTextInput)
        .rect();
    let ctx = h.ctx.clone();
    let font = egui::FontId::monospace(crate::theme::font_size());
    let (row_h, char_w) = ctx.fonts_mut(|f| (f.row_height(&font), f.glyph_width(&font, '0')));
    let pos = egui::pos2(editor.left() + char_w * col, editor.top() + row_h * row);
    let mods = Modifiers::COMMAND;
    h.event(Event::ModifiersChanged(mods));
    h.event(Event::PointerMoved(pos));
    steps(h, 2);
    for pressed in [true, false] {
        h.event(Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: mods,
        });
        steps(h, 1);
    }
    h.event(Event::ModifiersChanged(Modifiers::NONE));
}

#[test]
fn e2e_cmd_click_on_a_declaration_shows_a_usages_dropdown() {
    let f = Fixture::new("declusages");
    let (dest, host) = write_destination(&f);
    let mut h = f.harness();
    open(&mut h, &dest);
    let bottom = h.state().bottom;
    // `startCardDestination` in "internal fun NavGraphBuilder.startCardDestination(" on line 3
    cmd_click_at(&mut h, 33.5, 2.5);
    wait(&mut h, "usages", |a| {
        a.find_rx.is_none() && !a.found.is_empty()
    });
    steps(&mut h, 3);
    assert!(
        h.state().usages_popup.is_some(),
        "the dropdown stays open after the click"
    );
    assert_eq!(tab(&h).path, dest, "the declaration stays open");
    assert_eq!(
        h.state().bottom,
        bottom,
        "the Find panel is not forced open"
    );
    // import and call, not the declaration
    h.get_by_label_contains("Usages of `startCardDestination`  2 in 1 files");
    h.get_by_label_contains("StartCardFragment.kt:6");
    assert!(
        h.query_by_label_contains("StartCardDestination.kt:3")
            .is_none()
    );
    // ↓ then Enter opens the second row: the call
    key(&mut h, Modifiers::NONE, Key::ArrowDown);
    key(&mut h, Modifiers::NONE, Key::Enter);
    steps(&mut h, 3);
    assert!(h.state().usages_popup.is_none());
    assert_eq!(tab(&h).path, host);
    assert!(tab(&h).text[tab(&h).cursor..].starts_with("startCardDestination(onBack"));
    assert!(
        tab(&h).text.contains("b.startCardDestination"),
        "the editor text is unchanged"
    );

    // Esc closes it; the footer link opens the Find panel
    open(&mut h, &dest);
    cmd_click_at(&mut h, 33.5, 2.5);
    wait(&mut h, "usages", |a| {
        a.find_rx.is_none() && a.usages_popup.is_some()
    });
    key(&mut h, Modifiers::NONE, Key::Escape);
    steps(&mut h, 2);
    assert!(h.state().usages_popup.is_none());
    cmd_click_at(&mut h, 33.5, 2.5);
    wait(&mut h, "usages", |a| {
        a.find_rx.is_none() && a.usages_popup.is_some()
    });
    steps(&mut h, 2);
    h.get_by_label_contains("Open in Find panel").click();
    steps(&mut h, 3);
    assert_eq!(h.state().bottom, Bottom::Find);
    assert!(h.state().usages_popup.is_none());
}

#[test]
fn e2e_cmd_click_on_a_declaration_with_one_usage_jumps_to_it() {
    let f = Fixture::new("oneusage");
    let dest = f.path("app/src/main/java/com/demo/ui/Single.kt");
    let host = f.path("app/src/main/java/com/demo/ui/SingleUser.kt");
    std::fs::write(&dest, "package com.demo.ui\n\nfun singleUseHelper() {}\n").unwrap();
    std::fs::write(
        &host,
        "package com.demo.ui\n\nfun caller() {\n    singleUseHelper()\n}\n",
    )
    .unwrap();
    let mut h = f.harness();
    open(&mut h, &dest);
    // `singleUseHelper` in "fun singleUseHelper() {}" on line 3
    cmd_click_at(&mut h, 6.5, 2.5);
    wait(&mut h, "jump", |a| {
        a.tabs[a.active].path.ends_with("SingleUser.kt")
    });
    assert!(h.state().usages_popup.is_none());
    assert_eq!(tab(&h).path, host);
}

#[test]
fn e2e_references_keep_project_hits_when_kotlin_lsp_knows_one_file() {
    let f = Fixture::new("lspusages");
    let (dest, host) = write_destination(&f);
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_lsp.py");
    // SAFETY: tests run one at a time (SERIAL), so no other thread reads the environment now.
    unsafe { std::env::set_var("DROID_LITE_LSP", format!("python3 {}", script.display())) };
    let mut h = f.harness();
    open(&mut h, &dest);
    wait(&mut h, "lsp ready", |a| {
        a.lsp.as_ref().is_some_and(|l| l.ready)
    });
    // SAFETY: as above; the server has started
    unsafe { std::env::remove_var("DROID_LITE_LSP") };
    // the fake server, like kotlin-lsp mid-import, only knows the open file
    place_cursor(&mut h, "startCardDestination(", 3);
    key(&mut h, Modifiers::COMMAND | Modifiers::SHIFT, Key::G);
    wait(&mut h, "references", |a| {
        a.find_rx.is_none() && a.status.contains("kotlin-lsp")
    });
    let found = &h.state().found;
    assert_eq!(found.iter().filter(|f| f.path == host).count(), 2);
    assert_eq!(
        found.iter().filter(|f| f.path == dest).count(),
        1,
        "the declaration once, not twice"
    );
}

#[test]
fn e2e_locate_file_opens_its_folders_in_the_project_tree() {
    let f = Fixture::new("locate");
    let mut h = f.harness();
    let card = f.path("app/src/main/java/com/demo/ui/UserCard.kt");
    open(&mut h, &card);
    steps(&mut h, 3);
    // the tree starts closed: only the editor tab shows the name
    let shown = |h: &Harness<'static, App>| h.query_all_by_label_contains("UserCard.kt").count();
    let before = shown(&h);
    key(&mut h, Modifiers::COMMAND | Modifiers::ALT, Key::W); // Eclipse "Show In"
    assert!(h.state().locate.is_some());
    steps(&mut h, 25);
    assert!(h.state().locate.is_none(), "stops after the folders opened");
    assert_eq!(shown(&h), before + 1, "UserCard.kt is now in the open tree");
    // the ⌖ button on the Project header does the same (here: nothing new to open)
    h.get_by_label(crate::icons::CROSSHAIR).click();
    steps(&mut h, 2);
    assert!(h.state().locate.is_some());
}

#[test]
fn e2e_logcat_streams_filters_opens_stack_frames_and_never_clears_the_phone() {
    let f = Fixture::new("logcat");
    std::fs::write(
        f.path("app/build.gradle.kts"),
        "plugins { id(\"com.android.application\") }\nandroid {\n    defaultConfig {\n        applicationId = \"com.demo.app\"\n    }\n}\n",
    )
    .unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_adb.sh");
    let calls = f.path("adb-calls.txt");
    // SAFETY: tests run one at a time (SERIAL), so no other thread reads the environment now.
    unsafe {
        std::env::set_var("DROID_LITE_ADB", &script);
        std::env::set_var("FAKE_ADB_LOG", &calls);
    }
    let mut h = f.harness();
    assert!(
        !calls.exists(),
        "no adb call before the Logcat tab is opened"
    );
    h.get_by_label_contains("Logcat").click();
    wait(&mut h, "log lines", |a| a.logcat.view.len() == 5);
    steps(&mut h, 3);
    assert_eq!(
        h.state().logcat.device.as_deref(),
        Some("emulator-5554"),
        "the only online device is picked"
    );
    h.get_by_label_contains("slow call");
    h.get_by_label_contains("ANR in com.other");
    // rows are exactly the step the virtual list uses, so dragging never jumps
    {
        let row_h = h
            .ctx
            .fonts_mut(|f| f.row_height(&egui::FontId::monospace(12.0)))
            + 4.0;
        let a = h.get_by_label_contains("fast call").rect();
        let b = h.get_by_label_contains("slow call").rect();
        let c = h.get_by_label_contains("ANR in").rect();
        // the list places each row at row height + item spacing
        let step = row_h + h.ctx.global_style().spacing.item_spacing.y;
        assert_eq!(b.top() - a.top(), step, "row step");
        assert_eq!(c.top() - b.top(), step, "row step");
        assert_eq!(a.height(), row_h);
    }

    if let Ok(dir) = std::env::var("DROID_LITE_SNAPSHOT_DIR") {
        steps(&mut h, 3);
        h.render()
            .unwrap()
            .save(Path::new(&dir).join("e2e-logcat.png"))
            .unwrap();
    }
    // Android Studio filter: my app, warnings and up
    // the filter field is the lowest text field: it sits in the bottom panel
    let input = h
        .get_all_by_role(egui::accesskit::Role::TextInput)
        .map(|n| n.rect())
        .max_by(|a, b| a.top().total_cmp(&b.top()))
        .expect("filter field");
    click_at(&mut h, input.center(), egui::PointerButton::Primary);
    steps(&mut h, 2);
    type_text(&mut h, "package:mine level:warn");
    wait(&mut h, "filtered", |a| a.logcat.view.len() == 3);
    steps(&mut h, 2);
    assert!(
        h.query_by_label_contains("ANR in com.other").is_none(),
        "system_server is not mine"
    );
    assert!(
        h.query_by_label_contains("fast call").is_none(),
        "debug is below warn"
    );

    // a stack frame opens the project file at its line
    h.get_by_label_contains("UserCard(UserCard.kt:7)").click();
    steps(&mut h, 3);
    assert!(tab(&h).path.ends_with("UserCard.kt"));
    let t = tab(&h);
    assert_eq!(
        t.text[..t.cursor].matches('\n').count() + 1,
        7,
        "cursor on line 7"
    );

    // Clear empties the view only
    h.get_by_label("Clear").click();
    steps(&mut h, 2);
    assert_eq!(h.state().logcat.view.len(), 0);

    // leaving the tab stops adb logcat
    h.get_by_label_contains("Problems").click();
    steps(&mut h, 2);
    assert!(h.state().logcat.stream.is_none());
    unsafe {
        std::env::remove_var("FAKE_ADB_LOG");
    }
    let log = std::fs::read_to_string(&calls).unwrap();
    assert!(
        log.contains("-s emulator-5554 logcat -v threadtime -T 2000"),
        "{log}"
    );
    for banned in [" -c", "-G", "--clear"] {
        assert!(!log.contains(banned), "never `logcat {banned}`: {log}");
    }
}

#[test]
fn e2e_usage_filters_hide_tests_and_imports_and_are_saved() {
    let f = Fixture::new("usagefilter");
    let (dest, _host) = write_destination(&f);
    let test = f.path("app/src/test/java/com/demo/ui/StartCardTest.kt");
    std::fs::create_dir_all(test.parent().unwrap()).unwrap();
    std::fs::write(
        &test,
        "package com.demo.ui\n\nfun t(b: NavGraphBuilder) {\n    b.startCardDestination(onBack = {})\n}\n",
    )
    .unwrap();
    let mut h = f.harness();
    open(&mut h, &dest);
    cmd_click_at(&mut h, 33.5, 2.5);
    wait(&mut h, "usages", |a| {
        a.find_rx.is_none() && a.usages_popup.is_some()
    });
    steps(&mut h, 3);
    h.get_by_label_contains("Usages of `startCardDestination`  3 in 2 files");
    h.get_by_label("Hide tests").click();
    steps(&mut h, 3);
    h.get_by_label_contains("Usages of `startCardDestination`  2 in 1 files");
    h.get_by_label_contains("1 hidden");
    assert!(h.query_by_label_contains("StartCardTest.kt").is_none());
    // one usage left after a filter change: the list stays, it does not jump
    h.get_by_label("Hide imports").click();
    steps(&mut h, 3);
    h.get_by_label_contains("Usages of `startCardDestination`  1 in 1 files");
    assert!(h.state().usages_popup.is_some());
    assert_eq!(tab(&h).path, dest);
    // saved with the settings
    let saved = comfort::Settings::load();
    assert!(saved.hide_tests && saved.hide_imports);
}
