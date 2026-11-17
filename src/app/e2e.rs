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

/// Points the settings folder at a fresh temp folder for a unit test (serialised with the e2e
/// tests): one of its own, so work a test left running never touches the next test's.
pub(super) fn config_guard() -> MutexGuard<'static, ()> {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let lock = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("droid-lite-cfg-unit-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    // SAFETY: the SERIAL lock is held, so no other test reads the environment meanwhile.
    unsafe { std::env::set_var("DROID_LITE_CONFIG_DIR", &dir) };
    unsafe { std::env::set_var("DROID_LITE_UNSAVED_DIR", dir.join("unsaved")) };
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
                "#!/bin/sh\necho \"> Task $1\"\ncase \"$1\" in\n  *recordPaparazziDebug) mkdir -p app/src/test/snapshots/images && cp '{}' app/src/test/snapshots/images/com.demo.ui_UserCardKt_UserCardPreview.png; echo 'BUILD SUCCESSFUL';;\n  *compileDebugKotlin) echo \"w: file://$PWD/app/src/main/java/com/demo/ui/UserCard.kt:6:18 Parameter 'user' is never used\"; echo \"e: file://$PWD/app/src/main/java/com/demo/data/User.kt:3:40 Unresolved reference 'Intt'.\"; echo 'BUILD FAILED'; exit 1;;\n  *failingTask) echo \"e: file://$PWD/app/src/main/java/com/demo/ui/UserCard.kt:7:9 Unresolved reference 'oops'.\"; echo 'BUILD FAILED'; exit 1;;\n  *installDebug) echo \"install on $ANDROID_SERIAL\"; echo 'BUILD SUCCESSFUL';;\n  *) echo 'BUILD SUCCESSFUL';;\nesac\n",
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
        // and its own crash backup folder, never ~/Library/Application Support
        unsafe { std::env::set_var("DROID_LITE_UNSAVED_DIR", config.join("unsaved")) };
        // tests never start the kotlin-lsp installed on this Mac; the LSP test sets the fake one
        unsafe { std::env::set_var("DROID_LITE_LSP", "/nonexistent/droid-lite-no-lsp") };
        // nor any real phone: adb is missing unless a Logcat test sets the fake one
        unsafe { std::env::set_var("DROID_LITE_ADB", "/nonexistent/droid-lite-no-adb") };
        // nor GitHub: gh is missing unless a pull request test sets the fake one
        unsafe { std::env::set_var("DROID_LITE_GH", "/nonexistent/droid-lite-no-gh") };
        // previews draw by themselves: every test gets the stand-in worker and its own preview folder,
        // never the real JVM worker or ~/Library/Caches
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        unsafe {
            std::env::set_var(
                "DROID_LITE_PREVIEW_DIR",
                std::env::temp_dir().join(format!("dlpv-{name}-{}", std::process::id())),
            );
            std::env::set_var(
                "DROID_LITE_PREVIEW_WORKER",
                format!(
                    "python3 {}",
                    manifest
                        .join("tests/fixtures/fake_preview_worker.py")
                        .display()
                ),
            );
            std::env::set_var("FAKE_PREVIEW_PNG", manifest.join("assets/icon-256.png"));
            std::env::set_var("DROID_LITE_MAC_GUARD", "/nonexistent/mac-guard");
        }
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
            .build_eframe(move |cc| App::new(cc, Some(root)));
        wait(&mut h, "index", |a| {
            a.indexing.is_none() && a.index.read().symbol_count() > 0
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

/// Waits until the project's watcher is live (FSEvents starts a moment after the window opens)
/// and has delivered what happened before: rewrites `file` unchanged until the window applies an
/// event for it.
fn watcher_live(h: &mut Harness<'static, App>, file: &Path) {
    let text = std::fs::read(file).unwrap();
    super::disk::SEEN.take();
    for _ in 0..100 {
        std::fs::write(file, &text).unwrap();
        for _ in 0..10 {
            h.step();
            if super::disk::SEEN
                .with_borrow(|s| s.iter().any(|p| p.file_name() == file.file_name()))
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    panic!("the watcher never reported {}", file.display());
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
    assert!(app.tree.iter().any(|n| n.label == "app"));
    assert!(app.index.read().find_type("UserRepository").is_some());
    h.get_by_label(&format!("{}  app", ic::PACKAGE)); // the module row is on screen
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
    h.state_mut().wait_saves();
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
    // only the previews by default: the UI tree is behind "Structure", which is remembered
    assert!(
        h.query_by_label("Column").is_none(),
        "the UI tree is hidden by default"
    );
    h.get_by_label(&format!("{}  Structure", ic::TREE_STRUCTURE))
        .click();
    steps(&mut h, 3);
    assert!(
        h.state().show_structure,
        "the toggle turns the structure on"
    );
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
    // a hint per parameter: on a one-line signature an underline under each name
    fn name_at(text: &str, b: &super::gutter::Badge, i: usize) -> String {
        let r = &b.params[i].chars;
        text.chars().skip(r.start).take(r.len()).collect()
    }
    assert_eq!(badge.params.len(), 2);
    assert!(badge.params.iter().all(|p| !p.own_line && p.line == 5));
    assert_eq!(name_at(&tab(&h).text, badge, 0), "user");
    assert_eq!(name_at(&tab(&h).text, badge, 1), "modifier");
    // one parameter per line: a chip at the end of each line
    let multi = f.path("app/src/main/java/com/demo/ui/Multi.kt");
    std::fs::write(
        &multi,
        "package com.demo.ui\n\nimport com.demo.data.User\n\n@Composable\nfun Multi(\n    user: User,\n    tags: List<String>,\n    modifier: Modifier = Modifier,\n) {}\n",
    )
    .unwrap();
    open(&mut h, &multi);
    steps(&mut h, 3);
    let a = tab(&h).annotations.as_ref().expect("annotations");
    let badge = a
        .badges
        .iter()
        .find(|b| b.line == 5)
        .expect("badge on fun Multi(");
    let hints: Vec<(usize, bool, String, crate::compose::Verdict)> = badge
        .params
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let st = &badge.stability.params[p.param];
            (
                p.line,
                p.own_line,
                name_at(&tab(&h).text, badge, i),
                st.verdict,
            )
        })
        .collect();
    use crate::compose::Verdict::{Stable, Unstable};
    assert_eq!(
        hints,
        vec![
            (6, true, "user".into(), Unstable),
            (7, true, "tags".into(), Unstable),
            (8, true, "modifier".into(), Stable),
        ]
    );
    if let Ok(dir) = std::env::var("DROID_LITE_SNAPSHOT_DIR") {
        h.render()
            .unwrap()
            .save(Path::new(&dir).join("e2e-param-stability.png"))
            .unwrap();
    }
    open(&mut h, &f.path("app/src/main/java/com/demo/ui/UserCard.kt"));
    steps(&mut h, 3);
    let rules: Vec<&str> = tab(&h).lints.iter().map(|l| l.rule).collect();
    assert!(
        rules.contains(&"remember-state") && rules.contains(&"modifier-default"),
        "{rules:?}"
    );
    // previews draw by themselves (no click) through the preview worker (the fixture's stand-in), no Gradle
    wait(
        &mut h,
        "preview picture",
        |a| matches!(a.previews.cards.as_slice(), [(name, Ok(_))] if name == "UserCardPreview"),
    );
    assert!(h.state().gradle.is_none(), "no Gradle run for a preview");
    assert!(
        h.query_all_by_label("UserCardPreview").count() >= 2,
        "the card title beside the list link"
    );
    // a file that does not compile shows the compiler's text on the card
    let broken = f.path("app/src/main/java/com/demo/ui/BrokenPreview.kt");
    std::fs::write(
        &broken,
        "package com.demo.ui\n\nimport androidx.compose.ui.tooling.preview.Preview\n\n@Preview\n@Composable\nfun BrokenCardPreview() { Oops() }\n",
    )
    .unwrap();
    open(&mut h, &broken);
    wait(&mut h, "error card", |a| {
        a.previews.file.as_deref() == Some(broken.as_path())
            && matches!(a.previews.cards.as_slice(), [(_, Err(e))] if e.contains("unresolved reference 'Oops'"))
    });
}

#[test]
fn e2e_gradle_run_and_click_error() {
    let f = Fixture::new("gradle");
    let mut h = f.harness();
    h.get_by_label(&format!("{}  Run task", ic::PLAY)).click();
    steps(&mut h, 2);
    wait(&mut h, "build", |a| {
        a.gradle.is_none() && a.console.iter().any(|l| l.contains("Gradle finished: OK"))
    });
    h.state_mut().gradle_task = "failingTask".into();
    steps(&mut h, 2);
    h.get_by_label(&format!("{}  Run task", ic::PLAY)).click();
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
    let card_kt = f.path("app/src/main/java/com/demo/ui/UserCard.kt");
    open(&mut h, &card_kt);
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
    // the open files' symbols are read once, for their tabs and the index
    h.state().index.settle();
    assert!(
        index_shares_tab_symbols(h.state(), &card_kt),
        "a rewritten file"
    );
    assert!(index_shares_tab_symbols(h.state(), &person), "a moved file");
    // typing since then, which the undo replaces, stays in local history
    let a = h.state().active;
    h.state_mut().tabs[a].text.push_str("// later\n");
    let typed = tab(&h).text.clone();
    // Refactor menu → Undo
    h.get_by_label("Refactor").click();
    steps(&mut h, 2);
    h.get_by_label_contains("Undo rename User → Person").click();
    steps(&mut h, 3);
    assert!(user.exists() && !person.exists());
    assert_eq!(std::fs::read_to_string(&user).unwrap(), USER_KT);
    assert_eq!(newest_version(&user), Some(typed));
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
    watcher_live(&mut h, &user);
    std::fs::write(&user, "package com.demo.data\n\nclass Changed\n").unwrap();
    wait(&mut h, "reload", |a| {
        a.tabs[a.active].text.contains("class Changed")
    });
    assert!(h.state().index.read().find_type("Changed").is_some());
    // with unsaved edits the user is asked instead
    type_text(&mut h, "x");
    std::fs::write(&user, "package com.demo.data\n\nclass Again\n").unwrap();
    wait(&mut h, "conflict bar", |a| a.tabs[a.active].disk_changed);
    let typed = tab(&h).text.clone();
    h.get_by_label("Reload from disk").click();
    steps(&mut h, 3);
    assert!(tab(&h).text.contains("class Again") && !tab(&h).dirty());
    assert_eq!(
        newest_version(&user),
        Some(typed),
        "the dropped edits are in local history"
    );
    // a new file shows up in the tree and the index
    std::fs::write(
        f.path("app/src/main/java/com/demo/data/Extra.kt"),
        "package com.demo.data\nclass Extra\n",
    )
    .unwrap();
    wait(&mut h, "new file", |a| {
        a.index.read().find_type("Extra").is_some()
    });
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
    // Current changes tab: history and the commit's diff
    h.get_by_label(&format!("{}  Current changes", ic::GIT_PULL_REQUEST))
        .click();
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
    steps(&mut h, 5);
    assert!(
        h.state().found.iter().all(|l| !l.path.ends_with("Gone.kt")),
        "a place the server knows in a deleted file is dropped"
    );
    // a file deleted on disk is reported to the server
    std::fs::remove_file(&card).unwrap();
    wait(&mut h, "server told of the delete", |a| {
        a.tabs
            .iter()
            .any(|t| t.path == card && t.diagnostics.iter().any(|d| d.2 == "watched 3"))
    });
    // its tab stays open, but its places no longer count as references
    wait(&mut h, "tab marked deleted", |a| {
        a.tabs.iter().any(|t| t.path == card && t.deleted)
    });
    h.state_mut().found.clear();
    place_cursor(&mut h, "user.name", 1);
    key(&mut h, Modifiers::COMMAND | Modifiers::SHIFT, Key::G);
    wait(&mut h, "search done", |a| a.find_rx.is_none());
    steps(&mut h, 20);
    assert!(
        h.state().found.iter().all(|l| l.path != card),
        "{:?}",
        h.state().found.iter().map(|l| &l.path).collect::<Vec<_>>()
    );
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
            a.indexing.is_none() && a.index.read().symbol_count() > 0
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
        a.index.read().find_type("FakeRepo").is_some()
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
    .apply(&ctx);
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
        "package com.demo.ui\nimport com.demo.data.User\nval s = \"User\" // User\nval u: User? = null\n",
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
        2,
        "the import and the type, not the string or the comment"
    );
    assert_eq!(in_file("user_card.xml"), 1, "XML is searched as text");
    h.get_by_label_contains("References to `User`");
    // every row sits exactly one list step below the last, so scrolling never jumps
    {
        let font = egui::FontId::monospace(crate::theme::font_size());
        let step =
            h.ctx.fonts_mut(|f| f.row_height(&font)) + h.ctx.global_style().spacing.item_spacing.y;
        // the result rows, not the links above the code
        let editor = h
            .get_by_role(egui::accesskit::Role::MultilineTextInput)
            .rect();
        let rows: Vec<f32> = h
            .get_all_by_role(egui::accesskit::Role::Link)
            .map(|n| n.rect().center().y)
            .filter(|&y| y > editor.bottom())
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
        if (0..300).all(|i| {
            h.state()
                .index
                .read()
                .find_type(&format!("Gen{i}"))
                .is_some()
        }) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        (0..300).all(|i| h
            .state()
            .index
            .read()
            .find_type(&format!("Gen{i}"))
            .is_some()),
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
        a.index.read().find_type("Gen0").is_none()
    });
}

#[test]
fn watchdog_reports_only_slow_frames() {
    let mut w = Watchdog::start();
    w.stage("fast", || ());
    assert!(w.finish(10).2.is_none());
    let mut w = Watchdog::start();
    w.stage("fast", || ());
    w.stage("editor", || std::thread::sleep(Duration::from_millis(120)));
    let r = w.finish(10).2.unwrap();
    assert!(r.contains("mostly editor"), "{r}");
    let mut w = Watchdog::start();
    w.stage("startup", || std::thread::sleep(Duration::from_millis(120)));
    assert!(w.finish(1).2.is_none(), "first frames are skipped");
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

/// The link rows above the active editor's lines: (line, texts).
/// Screen y of the middle of `line` (0-based) in the active editor, link rows included.
fn line_y(h: &Harness<'static, App>, editor: egui::Rect, line: usize) -> f32 {
    let font = egui::FontId::monospace(crate::theme::font_size());
    let row_h = h.ctx.fonts_mut(|f| f.row_height(&font));
    let rows = &tab(h)
        .cache
        .as_ref()
        .unwrap()
        .shown
        .as_ref()
        .unwrap()
        .1
        .rows;
    editor.top() + rows[line].pos.y + row_h * 0.5
}

/// The links of the active tab, as (line, full words). Usage counts arrive in the background
/// and are left out (`usage_links` reads them).
fn lenses(h: &Harness<'static, App>) -> Vec<(usize, Vec<String>)> {
    link_items(h, false)
        .into_iter()
        .map(|(l, i)| (l, i.into_iter().map(|(_, tip)| tip).collect()))
        .filter(|(_, i): &(usize, Vec<String>)| !i.is_empty())
        .collect()
}

/// (line, [(shown, full words)]) of the usage links, or of all the others.
fn link_items(h: &Harness<'static, App>, usages: bool) -> Vec<(usize, Vec<(String, String)>)> {
    tab(h)
        .annotations
        .as_ref()
        .unwrap()
        .lenses
        .iter()
        .map(|l| {
            let items = l
                .items
                .iter()
                .filter(|i| i.text.starts_with(ic::LINK_SIMPLE) == usages)
                .map(|i| (i.text.clone(), i.tip.clone()))
                .collect();
            (l.line, items)
        })
        .collect()
}

/// Clicks the link `label`; the lowest one on screen when several have it.
fn click_link(h: &mut Harness<'static, App>, label: &str, lowest: bool) {
    let rects: Vec<egui::Rect> = h.get_all_by_label(label).map(|n| n.rect()).collect();
    let r = if lowest {
        rects.iter().max_by(|a, b| a.top().total_cmp(&b.top()))
    } else {
        rects.iter().min_by(|a, b| a.top().total_cmp(&b.top()))
    };
    let p = r.unwrap_or_else(|| panic!("no link {label}")).center();
    click_at(h, p, egui::PointerButton::Primary);
}

#[test]
fn e2e_usage_counts_above_declarations() {
    let f = Fixture::new("usage-links");
    let write = |rel: &str, text: &str| std::fs::write(f.path(rel), text).unwrap();
    write(
        "app/src/main/java/com/demo/data/Dao.kt",
        "package com.demo.data\n\nclass Dao {\n    fun insert(u: User) {}\n    fun unused() {}\n}\n\nclass Cache {\n    fun insert(k: String) {}\n}\n\nopen class Base {\n    open fun close() {}\n}\n\nclass Impl : Base() {\n    override fun close() {}\n}\n",
    );
    write(
        "app/src/main/java/com/demo/data/Saver.kt",
        "package com.demo.data\n\nclass Saver(private val dao: Dao, private val cache: Cache) {\n    fun save(u: User) {\n        dao.insert(u)\n        dao.insert(u)\n        cache.insert(\"k\")\n    }\n}\n",
    );
    let mut h = f.harness();
    open(&mut h, &f.path("app/src/main/java/com/demo/data/Dao.kt"));
    wait(&mut h, "usage counts", |a| a.decl_usages.is_some());
    steps(&mut h, 3);
    let link = |n: usize, tip: &str| vec![(format!("{}  {n}", ic::LINK_SIMPLE), tip.to_string())];
    assert_eq!(
        link_items(&h, true)
            .into_iter()
            .filter(|(_, i)| !i.is_empty())
            .collect::<Vec<_>>(),
        [
            (2, link(1, "1 usage")),   // class Dao: the Saver parameter
            (3, link(2, "2 usages")),  // Dao.insert, not Cache.insert
            (4, link(0, "No usages")), // unused
            (7, link(1, "1 usage")),   // class Cache
            (8, link(1, "1 usage")),   // Cache.insert
            (11, link(1, "1 usage")),  // open class Base: Impl extends it
            (12, link(0, "No usages")),
            (15, link(0, "No usages")), // and none on Impl's override
        ]
    );
    // a click lists the places
    click_link(&mut h, "2 usages", false);
    steps(&mut h, 3);
    let s = h.state().search.as_ref().unwrap();
    assert_eq!(s.title.as_deref(), Some("Usages of insert"));
    let mut places: Vec<&str> = s.pinned.iter().map(|x| x.detail.as_str()).collect();
    places.sort();
    assert_eq!(
        places,
        [
            "app/src/main/java/com/demo/data/Saver.kt:5",
            "app/src/main/java/com/demo/data/Saver.kt:6"
        ]
    );
    key(&mut h, Modifiers::NONE, Key::Escape);
    // a new use, saved or not, is counted after the edit settles
    open(&mut h, &f.path("app/src/main/java/com/demo/data/Saver.kt"));
    steps(&mut h, 3);
    let on_class_line: Vec<_> = link_items(&h, true)
        .into_iter()
        .filter(|(l, _)| *l == 2)
        .flat_map(|(_, i)| i)
        .collect();
    assert_eq!(
        on_class_line.len(),
        1,
        "one count for `class Saver(val dao, val cache)`"
    );
    place_cursor(&mut h, "cache.insert", 0);
    type_text(&mut h, "dao.unused(); ");
    h.state_mut().settle(true);
    open(&mut h, &f.path("app/src/main/java/com/demo/data/Dao.kt"));
    wait(&mut h, "recount", |a| {
        a.decl_usages.as_ref().is_some_and(|u| {
            u.by_decl
                .get(&(Some("Dao".to_string()), "unused".to_string()))
                .is_some_and(|v| v.len() == 1)
        })
    });
}

#[test]
fn e2e_links_above_declarations_implementations_and_overrides() {
    let f = Fixture::new("gutter");
    std::fs::write(f.path("app/src/main/java/com/demo/data/Repo.kt"), REPO_KT).unwrap();
    let mut h = f.harness();
    let repo = f.path("app/src/main/java/com/demo/data/Repo.kt");
    open(&mut h, &repo);
    steps(&mut h, 3);
    // interface Repo (line 2) and its load (3): implemented in 2 classes;
    // RealRepo (6) / FakeRepo (10): implement Repo; their load (7, 11): override it
    let want = |s: &str| vec![s.to_string()];
    assert_eq!(
        lenses(&h),
        [
            (2, want("Implemented in 2 classes")),
            (3, want("Implemented in 2 classes")),
            (6, want("Implements / extends Repo")),
            (7, want("Overrides Repo.load")),
            (10, want("Implements / extends Repo")),
            (11, want("Overrides Repo.load")),
        ]
    );
    assert_eq!(
        h.get_all_by_label("Implemented in 2 classes").count(),
        2,
        "links are drawn"
    );
    // they show an icon and a count or name; the words are the hover tip
    let shown: Vec<Vec<String>> = link_items(&h, false)
        .into_iter()
        .map(|(_, i)| i.into_iter().map(|(text, _)| text).collect())
        .filter(|i: &Vec<String>| !i.is_empty())
        .collect();
    let (down, up) = (
        |s: &str| format!("{}  {s}", ic::ARROW_DOWN),
        |s: &str| format!("{}  {s}", ic::ARROW_UP),
    );
    assert_eq!(
        shown,
        [
            vec![down("2")],
            vec![down("2")],
            vec![up("Repo")],
            vec![up("Repo.load")],
            vec![up("Repo")],
            vec![up("Repo.load")],
        ]
    );
    // the links sit in their own row: the code lines below them move down
    let rows = &tab(&h)
        .cache
        .as_ref()
        .unwrap()
        .shown
        .as_ref()
        .unwrap()
        .1
        .rows;
    assert!(rows[2].pos.y - rows[1].pos.y > rows[1].pos.y - rows[0].pos.y + 5.0);
    // clicking "Implemented in 2 classes" of `interface Repo` offers both classes
    click_link(&mut h, "Implemented in 2 classes", false);
    let s = h
        .state()
        .search
        .as_ref()
        .expect("picker after clicking the icon");
    assert_eq!(s.kind, SearchKind::Pick);
    assert_eq!(s.hits.len(), 2);
    key(&mut h, Modifiers::NONE, Key::Escape);
    // clicking "Overrides load" of FakeRepo.load jumps to Repo.load
    click_link(&mut h, "Overrides Repo.load", true);
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
    // right-click on line 9 (`Text(user.name)`) moves the cursor there and opens the menu
    let pos = egui::pos2(editor.left() + 120.0, line_y(&h, editor, 8));
    click_at(&mut h, pos, egui::PointerButton::Secondary);
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
    let char_w = ctx.fonts_mut(|f| f.glyph_width(&font, '0'));
    // "fun UserCard(user: User" : hover the type `User` at column 20 of line 6 with ⌘ held
    let pos = egui::pos2(editor.left() + char_w * 21.5, line_y(&h, editor, 5));
    h.event(Event::PointerMoved(pos));
    h.event(Event::ModifiersChanged(Modifiers::COMMAND));
    for _ in 0..80 {
        h.step();
        if user_usages(&h).is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    steps(&mut h, 30); // tooltips appear after a short delay
    let found = user_usages(&h).expect("usages counted");
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
    h.get_by_label(&format!("{}  Current changes", ic::GIT_PULL_REQUEST))
        .click();
    wait(&mut h, "changes", |a| {
        a.git
            .changes
            .as_ref()
            .is_some_and(|c| c.working.len() == 2 && c.branch.len() == 1)
    });
    h.get_by_label("Uncommitted changes (2)");
    h.get_by_label("This branch");
    h.get_by_label_contains("1 files changed vs origin/");
    // a click on the name opens the file in the editor, with no diff window
    h.get_by_label("User.kt").scroll_to_me();
    steps(&mut h, 3);
    h.get_by_label("User.kt").click();
    wait(&mut h, "opened", |a| {
        a.tabs
            .get(a.active)
            .is_some_and(|t| t.path.ends_with("data/User.kt"))
    });
    assert!(h.state().commit_lines().is_none(), "no diff window");
    // "Diff" shows the diff of the branch file
    click_in_row(&mut h, "User.kt", "Diff");
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
    h.get_by_label("UserCard.kt").scroll_to_me();
    steps(&mut h, 3);
    let row = h.get_by_label("UserCard.kt").rect();
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
fn e2e_pull_request_tab_compares_with_the_picked_base() {
    let f = Fixture::new("prbase");
    let mut h = f.harness();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&f.root)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
    };
    let main = String::from_utf8(
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
    git(&["update-ref", &format!("refs/remotes/origin/{main}"), &main]);
    // stacked work: `first` changes User.kt, `second` (on top of it) changes UserCard.kt
    git(&["checkout", "-q", "-b", "first"]);
    std::fs::write(
        f.path("app/src/main/java/com/demo/data/User.kt"),
        USER_KT.replace("var age", "val age"),
    )
    .unwrap();
    git(&["commit", "-q", "-am", "first PR"]);
    git(&["checkout", "-q", "-b", "second"]);
    std::fs::write(
        f.path("app/src/main/java/com/demo/ui/UserCard.kt"),
        CARD_KT.replace("count", "clicks"),
    )
    .unwrap();
    git(&["commit", "-q", "-am", "second PR"]);
    h.state_mut().branch = Some("second".into());
    h.state_mut().git.changes_stale = true;
    h.get_by_label(&format!("{}  Current changes", ic::GIT_PULL_REQUEST))
        .click();
    let files = |a: &App| a.git.changes.as_ref().map(|c| c.branch.len());
    wait(&mut h, "automatic base", |a| files(a) == Some(2));
    steps(&mut h, 2);
    h.get_by_label_contains(&format!("2 files changed vs origin/{main}"));
    // type the base branch (a PR number works the same through gh)
    let input = h
        .get_by_role_and_label(egui::accesskit::Role::TextInput, "or")
        .rect()
        .center();
    click_at(&mut h, input, egui::PointerButton::Primary);
    type_text(&mut h, "first");
    h.get_by_label("Use").click();
    wait(&mut h, "picked base", |a| {
        files(a) == Some(1) && a.git.changes.as_ref().unwrap().base.as_deref() == Some("first")
    });
    steps(&mut h, 2);
    h.get_by_label("UserCard.kt");
    h.get_by_label_contains("1 files changed vs first");
    assert!(
        h.query_by_label("User.kt").is_none(),
        "the base PR's file is not listed"
    );
    let saved = std::fs::read_to_string(f.config.join("settings.json")).unwrap();
    assert!(saved.contains(r#"\tsecond": "first""#), "{saved}");
    // an unknown base says so and falls back to the automatic base
    let input = h
        .get_by_role_and_label(egui::accesskit::Role::TextInput, "or")
        .rect()
        .center();
    click_at(&mut h, input, egui::PointerButton::Primary);
    type_text(&mut h, "no-such-branch");
    h.get_by_label("Use").click();
    wait(&mut h, "bad base", |a| {
        a.git
            .changes
            .as_ref()
            .is_some_and(|c| c.base_error.is_some())
    });
    steps(&mut h, 2);
    h.get_by_label_contains("no-such-branch not found: using the automatic base");
    assert_eq!(files(h.state()), Some(2));
    // dim bars: what this branch changed since its parent; nothing is uncommitted
    let bars = |a: &App| {
        let g = &a.tabs[a.active].git;
        g.branch_hunks
            .as_ref()
            .map(|v| v.len())
            .zip(g.hunks.as_ref().map(|v| v.len()))
    };
    h.state_mut()
        .open_file(&f.path("app/src/main/java/com/demo/ui/UserCard.kt"), None);
    wait(&mut h, "branch bars", |a| bars(a) == Some((1, 0)));
    // automatic base: User.kt (changed in the parent PR) is part of the branch
    h.state_mut()
        .open_file(&f.path("app/src/main/java/com/demo/data/User.kt"), None);
    wait(&mut h, "parent's change vs the automatic base", |a| {
        bars(a) == Some((1, 0))
    });
    // with the parent branch set, only this PR's work has bars
    let input = h
        .get_by_role_and_label(egui::accesskit::Role::TextInput, "or")
        .rect()
        .center();
    click_at(&mut h, input, egui::PointerButton::Primary);
    type_text(&mut h, "first");
    h.get_by_label("Use").click();
    wait(&mut h, "no bars for the parent's change", |a| {
        bars(a) == Some((0, 0))
    });
    // an uncommitted edit adds a bright bar on top
    {
        let t = h
            .state_mut()
            .tabs
            .iter_mut()
            .find(|t| t.path.ends_with("User.kt"))
            .unwrap();
        t.text = t.text.replace("class UserRepository", "class UserStore");
        t.mark_edited();
    }
    wait(&mut h, "bright and dim bars", |a| bars(a) == Some((1, 1)));
}

/// Clicks the button `button` in the row of the label `row` (the first such row).
fn click_in_row(h: &mut Harness<'static, App>, row: &str, button: &str) {
    h.get_all_by_label(row).next().unwrap().scroll_to_me();
    steps(h, 2);
    let y = h.get_all_by_label(row).next().unwrap().rect().center().y;
    let at = h
        .get_all_by_label(button)
        .find(|n| (n.rect().center().y - y).abs() < 4.0)
        .unwrap_or_else(|| panic!("no {button} in the {row} row"))
        .rect()
        .center();
    click_at(h, at, egui::PointerButton::Primary);
    steps(h, 2);
}

#[test]
fn e2e_revert_each_file() {
    let f = Fixture::new("revert");
    let mut h = f.harness();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&f.root)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
    };
    let main = String::from_utf8(
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
    git(&["update-ref", &format!("refs/remotes/origin/{main}"), &main]);
    git(&["checkout", "-q", "-b", "feature"]);
    let card = f.path("app/src/main/java/com/demo/ui/UserCard.kt");
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    let new = f.path("app/src/main/java/com/demo/ui/New.kt");
    std::fs::write(&card, CARD_KT.replace("count", "clicks")).unwrap();
    git(&["commit", "-q", "-am", "clicks"]);
    std::fs::write(&user, USER_KT.replace("var age", "val age")).unwrap();
    std::fs::write(&new, "package com.demo.ui\n").unwrap();
    h.state_mut().branch = Some("feature".into());
    h.state_mut().git.changes_stale = true;
    h.get_by_label(&format!("{}  Current changes", ic::GIT_PULL_REQUEST))
        .click();
    let counts = |a: &App| {
        a.git
            .changes
            .as_ref()
            .map(|c| (c.branch.len(), c.working.len()))
    };
    wait(&mut h, "changes", |a| counts(a) == Some((1, 2)));
    steps(&mut h, 2);
    // Revert asks first; No keeps the file
    click_in_row(&mut h, "New.kt", "Revert");
    h.get_by_label("Discard?");
    click_in_row(&mut h, "New.kt", "No");
    assert!(new.exists() && h.query_by_label("Discard?").is_none());
    // an untracked file is deleted
    click_in_row(&mut h, "New.kt", "Revert");
    click_in_row(&mut h, "New.kt", "Yes");
    assert!(!new.exists());
    // a changed file goes back to the last commit
    wait(&mut h, "untracked gone", |a| counts(a) == Some((1, 1)));
    steps(&mut h, 2);
    click_in_row(&mut h, "User.kt", "Revert");
    click_in_row(&mut h, "User.kt", "Yes");
    assert_eq!(std::fs::read_to_string(&user).unwrap(), USER_KT);
    // a PR file goes back to the base version, as an uncommitted change
    wait(&mut h, "working clean", |a| counts(a) == Some((1, 0)));
    steps(&mut h, 2);
    click_in_row(&mut h, "UserCard.kt", "Revert");
    click_in_row(&mut h, "UserCard.kt", "Yes");
    assert_eq!(std::fs::read_to_string(&card).unwrap(), CARD_KT);
    wait(&mut h, "base version", |a| counts(a) == Some((1, 1)));
    let log = std::process::Command::new("git")
        .arg("-C")
        .arg(&f.root)
        .args(["log", "-1", "--format=%s"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&log.stdout).trim(),
        "clicks",
        "commits stay"
    );
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
        let char_w = ctx.fonts_mut(|f| f.glyph_width(&font, '0'));
        // the type `User` in "fun UserCard(user: User, …)" on line 6
        let pos = egui::pos2(editor.left() + char_w * 21.5, line_y(&h, editor, 5));
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
    h.state_mut().wait_saves();
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
    // typing not saved yet, which the restore replaces, is kept as a new version first (after
    // the watcher saw the save: a late event would mark the edited tab as changed on disk)
    watcher_live(&mut h, &card);
    steps(&mut h, 3);
    let a = h.state().active;
    h.state_mut().tabs[a].text.push_str("// three\n");
    let typed = tab(&h).text.clone();
    click_at(&mut h, restore, egui::PointerButton::Primary);
    assert_eq!(tab(&h).text, CARD_KT);
    assert!(tab(&h).dirty(), "restored text waits for ⌘S");
    assert_eq!(newest_version(&card), Some(typed));
    // Revert (the bar's button) drops the restored text; it is kept too
    h.state_mut().revert();
    assert!(!tab(&h).dirty());
    assert_eq!(newest_version(&card), Some(CARD_KT.to_string()));
    h.state_mut().tabs[a].text = CARD_KT.into();
    // auto-save when the window loses focus
    h.input_mut().focused = true;
    steps(&mut h, 2);
    h.input_mut().focused = false; // the user switches to another app
    steps(&mut h, 3);
    h.state_mut().wait_saves();
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
    let char_w = ctx.fonts_mut(|f| f.glyph_width(&font, '0'));
    let col = "    val name = stringResource(R.string.".len() as f32 + 2.0;
    let pos = egui::pos2(editor.left() + char_w * col, line_y(&h, editor, 3));
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
    wait(&mut h, "log lines", |a| a.logcat.view.len() == 7);
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
    wait(&mut h, "filtered", |a| a.logcat.view.len() == 5);
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

    // the app's crash got a note, even with Logcat on screen; it opens the crash line
    open(&mut h, &f.path("app/src/main/java/com/demo/data/User.kt"));
    let note = h.get_by_label_contains("App crashed: IllegalStateException: boom at UserCard.kt:7");
    note.click();
    steps(&mut h, 3);
    assert!(tab(&h).path.ends_with("UserCard.kt"));
    let t = tab(&h);
    assert_eq!(t.text[..t.cursor].matches('\n').count() + 1, 7);
    assert!(h.state().toasts.is_empty(), "a clicked note goes");

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

#[test]
fn e2e_tree_android_and_project_views_nested_modules_and_filter() {
    let f = Fixture::new("treeviews");
    std::fs::write(
        f.path("settings.gradle.kts"),
        "rootProject.name = \"Demo\"\ninclude(\":app\")\ninclude(\":feature:card:data\")\n",
    )
    .unwrap();
    let write = |rel: &str, text: &str| {
        let p = f.path(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    };
    write("feature/card/data/build.gradle.kts", "");
    write(
        "feature/card/data/src/main/kotlin/com/demo/card/data/CardRepo.kt",
        "package com.demo.card.data\nclass CardRepo\n",
    );
    let test = f.path("feature/card/data/src/test/kotlin/com/demo/card/data/CardRepoTest.kt");
    write(
        "feature/card/data/src/test/kotlin/com/demo/card/data/CardRepoTest.kt",
        "package com.demo.card.data\nclass CardRepoTest\n",
    );
    let mut h = f.harness();
    // Android view: feature › card › data (a module), with kotlin and kotlin (test)
    {
        let tree = &h.state().tree;
        let feature = tree
            .iter()
            .find(|n| n.label == "feature")
            .expect("feature group");
        let data = &feature
            .dirs
            .iter()
            .find(|n| n.label == "card")
            .unwrap()
            .dirs[0];
        assert_eq!(data.label, "data");
        let groups: Vec<&str> = data.dirs.iter().map(|n| n.label.as_str()).collect();
        assert_eq!(groups, ["kotlin", "kotlin (test)"]);
        assert_eq!(data.dirs[0].dirs[0].label, "com.demo.card.data");
    }
    // locating the test file opens feature › card › data › kotlin (test) › package
    open(&mut h, &test);
    key(&mut h, Modifiers::COMMAND | Modifiers::ALT, Key::W);
    steps(&mut h, 25);
    h.get_by_label_contains("kotlin (test)");
    h.get_by_label_contains("com.demo.card.data");
    if let Ok(dir) = std::env::var("DROID_LITE_SNAPSHOT_DIR") {
        h.render()
            .unwrap()
            .save(Path::new(&dir).join("e2e-tree.png"))
            .unwrap();
    }

    // Project view: source sets under the module, src/ left out; remembered for the project
    h.get_by_label("Project").click();
    steps(&mut h, 3);
    assert_eq!(h.state().tree_view, tree::View::Project);
    {
        let feature = h
            .state()
            .tree
            .iter()
            .find(|n| n.label == "feature")
            .unwrap();
        let data = &feature.dirs[0].dirs[0];
        let sets: Vec<&str> = data.dirs.iter().map(|n| n.label.as_str()).collect();
        assert_eq!(sets, ["main", "test"]);
        assert_eq!(data.dirs[0].dirs[0].label, "kotlin · com.demo.card.data");
    }
    drop(h);
    let mut h = f.harness();
    steps(&mut h, 3);
    assert_eq!(
        h.state().tree_view,
        tree::View::Project,
        "restored for this project"
    );

    // the filter box lists matching files with their folder
    let input = h
        .get_all_by_role(egui::accesskit::Role::TextInput)
        .map(|n| n.rect())
        .min_by(|a, b| a.left().total_cmp(&b.left()))
        .expect("filter field");
    click_at(&mut h, input.center(), egui::PointerButton::Primary);
    steps(&mut h, 2);
    type_text(&mut h, "cardrepo");
    steps(&mut h, 3);
    h.get_by_label_contains("CardRepo.kt");
    h.get_by_label_contains("feature/card/data/src/main/kotlin/com/demo/card/data");
    // the test file is also open in a tab (restored session); click the row in the Project panel
    let row = h
        .get_all_by_label_contains("CardRepoTest.kt")
        .map(|n| n.rect())
        .min_by(|a, b| a.left().total_cmp(&b.left()))
        .unwrap();
    open(&mut h, &f.path("app/src/main/java/com/demo/data/User.kt"));
    click_at(&mut h, row.center(), egui::PointerButton::Primary);
    steps(&mut h, 3);
    assert_eq!(tab(&h).path, test);
}

#[test]
fn e2e_kmp_expect_actual_gutter_and_jump() {
    let f = Fixture::new("kmp");
    std::fs::write(
        f.path("settings.gradle.kts"),
        "rootProject.name = \"Demo\"\ninclude(\":app\")\ninclude(\":shared\")\n",
    )
    .unwrap();
    let write = |rel: &str, text: &str| {
        let p = f.path(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, text).unwrap();
        p
    };
    write("shared/build.gradle.kts", "");
    let common = write(
        "shared/src/commonMain/kotlin/com/demo/Platform.kt",
        "package com.demo\n\nexpect fun platformName(): String\n",
    );
    let android = write(
        "shared/src/androidMain/kotlin/com/demo/Platform.android.kt",
        "package com.demo\n\nactual fun platformName(): String = \"Android\"\n",
    );
    write(
        "shared/src/iosMain/kotlin/com/demo/Platform.ios.kt",
        "package com.demo\n\nactual fun platformName(): String = \"iOS\"\n",
    );
    let mut h = f.harness();
    open(&mut h, &common);
    steps(&mut h, 3);
    assert_eq!(
        lenses(&h),
        [(2, vec!["Actual in androidMain, iosMain".to_string()])],
        "the expect on line 3 links both actuals"
    );
    // ⌥⌘B / ⌘T on the expect lists the actuals with their source sets
    place_cursor(&mut h, "platformName", 3);
    key(&mut h, Modifiers::COMMAND, Key::T); // Eclipse: implementations
    steps(&mut h, 3);
    h.get_by_label_contains("androidMain · ");
    h.get_by_label_contains("iosMain · ");
    key(&mut h, Modifiers::NONE, Key::Escape);
    // from an actual, the jump goes straight to the expect
    open(&mut h, &android);
    steps(&mut h, 3);
    assert_eq!(
        lenses(&h),
        [(2, vec!["Expected in commonMain".to_string()])]
    );
    place_cursor(&mut h, "platformName", 3);
    key(&mut h, Modifiers::COMMAND, Key::T);
    steps(&mut h, 3);
    assert_eq!(tab(&h).path, common);
}

#[test]
fn e2e_run_app_and_preview_on_a_device() {
    let f = Fixture::new("runapp");
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
    // ▶ App: install on the device, start the launcher activity, show the app's log
    h.get_by_label(&format!("{}  App", ic::PLAY)).click();
    wait(&mut h, "app started", |a| a.status.starts_with("Started"));
    steps(&mut h, 2);
    assert!(
        h.state()
            .console
            .iter()
            .any(|l| l == "install on emulator-5554"),
        "installDebug ran for the picked device: {:?}",
        h.state().console
    );
    assert!(
        h.state()
            .console
            .iter()
            .any(|l| l.starts_with("> ./gradlew :app:installDebug"))
    );
    assert_eq!(h.state().bottom, Bottom::Logcat);
    assert_eq!(h.state().logcat.query, "package:mine");
    let log = std::fs::read_to_string(&calls).unwrap();
    assert!(
        log.contains("-s emulator-5554 shell am start -n com.demo.app.debug/com.demo.MainActivity"),
        "{log}"
    );

    // ▶ next to the @Preview: the same install, then PreviewActivity with that preview
    let card = f.path("app/src/main/java/com/demo/ui/UserCard.kt");
    open(&mut h, &card);
    steps(&mut h, 3);
    let run = "Run preview on the device".to_string();
    // above `@Preview`, not the fun line under it
    assert!(
        lenses(&h).contains(&(12, vec![run.clone()])),
        "{:?}",
        lenses(&h)
    );
    h.get_by_label(&run).click();
    wait(&mut h, "preview shown", |a| {
        a.status.starts_with("Showing preview")
    });
    let log = std::fs::read_to_string(&calls).unwrap();
    assert!(
        log.contains("am start -n com.demo.app.debug/androidx.compose.ui.tooling.PreviewActivity -a android.intent.action.MAIN --es composable com.demo.ui.UserCardKt.UserCardPreview"),
        "{log}"
    );

    // without ui-tooling in the debug app, the message says what to add
    unsafe { std::env::set_var("FAKE_NO_PREVIEW", "1") };
    h.get_by_label(&run).click();
    wait(&mut h, "preview error", |a| a.status.contains("ui-tooling"));
    assert_eq!(h.state().bottom, Bottom::Build);
    unsafe {
        std::env::remove_var("FAKE_NO_PREVIEW");
        std::env::remove_var("FAKE_ADB_LOG");
    }
    for banned in [" install ", " uninstall", "logcat -c", " -G"] {
        assert!(
            !log.contains(banned),
            "adb {banned} is Gradle's job or banned: {log}"
        );
    }
}

#[test]
fn e2e_device_screenshot_and_recording() {
    let f = Fixture::new("capture");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_adb.sh");
    // the recording's size on the device ("fake mp4"): it is deleted there only once all of it came
    let script = f.path("adb");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\ncase \"$*\" in *\" shell stat -c %s \"*) echo 8;; *) exec {} \"$@\";; esac\n",
            fixture.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let calls = f.path("adb-calls.txt");
    let out = f.path("captures");
    std::fs::create_dir_all(&out).unwrap();
    // SAFETY: tests run one at a time (SERIAL), so no other thread reads the environment now.
    unsafe {
        std::env::set_var("DROID_LITE_ADB", &script);
        std::env::set_var("FAKE_ADB_LOG", &calls);
        std::env::set_var("DROID_LITE_CAPTURE_DIR", &out);
    }
    let mut h = f.harness();
    h.state_mut().logcat.refresh_devices_now();
    steps(&mut h, 2);

    // 📷: a PNG lands in the capture folder and shows in the result window
    h.get_by_label(ic::CAMERA).click();
    wait(&mut h, "screenshot saved", |a| a.capture.last.is_some());
    let shot = h.state().capture.last.clone().unwrap();
    assert_eq!(shot.parent(), Some(out.as_path()));
    let name = shot.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        name.starts_with("Screenshot emulator-5554 ") && name.ends_with(".png"),
        "{name}"
    );
    assert!(std::fs::read(&shot).unwrap().starts_with(b"\x89PNG"));
    steps(&mut h, 2);
    h.get_by_label(&format!("{}  Show in Finder", ic::FOLDER_OPEN));

    // ⏺ then ⏹: stopped by its own pid, pulled, and only that file deleted on the device
    h.get_by_label(ic::RECORD).click();
    wait(&mut h, "recording", |a| a.capture.recording.is_some());
    steps(&mut h, 5);
    h.get_by_label(&format!("{}  0:00", ic::STOP_CIRCLE))
        .click();
    wait(&mut h, "video saved", |a| {
        a.capture
            .last
            .as_ref()
            .is_some_and(|p| p.extension().is_some_and(|e| e == "mp4"))
    });
    let video = h.state().capture.last.clone().unwrap();
    assert_eq!(std::fs::read(&video).unwrap(), b"fake mp4");
    let log = std::fs::read_to_string(&calls).unwrap();
    let remote = log
        .lines()
        .find_map(|l| l.split("exec screenrecord --time-limit 180 ").nth(1))
        .expect("screenrecord started")
        .to_string();
    assert!(
        remote.starts_with("/sdcard/droid-lite-") && remote.ends_with(".mp4"),
        "{remote}"
    );
    let kill = log
        .lines()
        .find(|l| l.contains("shell kill -INT "))
        .expect("stopped by pid");
    assert!(
        kill.rsplit(' ')
            .next()
            .unwrap()
            .bytes()
            .all(|b| b.is_ascii_digit()),
        "{kill}"
    );
    assert!(
        log.contains(&format!("pull {remote} {}", video.display())),
        "{log}"
    );
    assert!(log.contains(&format!("shell rm -f {remote}")), "{log}");
    for banned in ["pkill", "killall", "rm -rf", "*", "logcat -c", " -G"] {
        assert!(!log.contains(banned), "adb {banned}: {log}");
    }
    unsafe {
        std::env::remove_var("FAKE_ADB_LOG");
        std::env::remove_var("DROID_LITE_CAPTURE_DIR");
    }
}

/// Drags from `from` to `to` with the primary button.
fn drag(h: &mut Harness<'static, App>, from: egui::Pos2, to: egui::Pos2) {
    h.event(Event::PointerMoved(from));
    steps(h, 1);
    h.event(Event::PointerButton {
        pos: from,
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: Modifiers::NONE,
    });
    steps(h, 1);
    for i in 1..=10 {
        h.event(Event::PointerMoved(from + (to - from) * (i as f32 / 10.0)));
        steps(h, 1);
    }
    h.event(Event::PointerButton {
        pos: to,
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: Modifiers::NONE,
    });
    steps(h, 3);
}

#[test]
fn e2e_current_changes_panel_resizes_both_ways() {
    let f = Fixture::new("prwidth");
    let mut h = f.harness();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&f.root)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
    };
    // long names are what used to hold the panel wide
    let branch = "feature/a-very-long-branch-name-for-the-pull-request-panel-width";
    git(&["checkout", "-q", "-b", branch]);
    let long =
        f.path("app/src/main/java/com/demo/ui/AVeryLongComposableScreenFileNameThatIsWide.kt");
    std::fs::write(&long, "package com.demo.ui\n").unwrap();
    // staged: the commit message row shows too
    git(&["add", "."]);
    h.state_mut().branch = Some(branch.into());
    h.state_mut().git.changes_stale = true;
    h.get_by_label(&format!("{}  Current changes", ic::GIT_PULL_REQUEST))
        .click();
    wait(&mut h, "changes", |a| {
        a.git.changes.as_ref().is_some_and(|c| c.working.len() == 1)
    });
    steps(&mut h, 3);
    let edge = |h: &Harness<'static, App>| {
        h.get_by_label(&format!("{}  Compose", ic::CUBE))
            .rect()
            .left()
    };
    let y = 450.0;
    // the right panel's resize handle: the splitter with the largest x
    let handle = |h: &Harness<'static, App>| {
        h.get_all_by_role(egui::accesskit::Role::Splitter)
            .map(|n| n.rect())
            .filter(|r| r.height() > r.width())
            .max_by(|a, b| a.left().total_cmp(&b.left()))
            .expect("right panel handle")
            .center()
            .x
    };
    for (dx, what) in [
        (100.0, "narrower"), // the tab row (Compose, Current changes) sets the narrowest
        (-300.0, "wider"),
        (200.0, "narrower again"),
    ] {
        let before = edge(&h);
        let at = egui::pos2(handle(&h), y);
        drag(&mut h, at, at + egui::vec2(dx, 0.0));
        let after = edge(&h);
        assert!(
            (after - before - dx).abs() < 4.0,
            "{what}: the edge moved {} of {dx}",
            after - before
        );
    }
    if let Ok(dir) = std::env::var("DROID_LITE_SNAPSHOT_DIR") {
        steps(&mut h, 3);
        h.render()
            .unwrap()
            .save(Path::new(&dir).join("e2e-pull-request-narrow.png"))
            .unwrap();
    }
}

#[test]
fn e2e_performance_tab_shows_frames_memory_and_open_time() {
    let f = Fixture::new("perf");
    let mut h = f.harness();
    h.get_by_label(&format!("{}  Performance", ic::GAUGE))
        .click();
    steps(&mut h, 5);
    h.get_by_label_contains("Frames: median");
    h.get_by_label_contains("Memory");
    h.get_by_label_contains("Project opened in");
    // the parts of the last frame, by name
    h.get_by_label_contains("editor ");
    h.get_by_label_contains("project tree ");
    let p = &h.state().perf;
    assert!(p.frames.len() > 5, "frames are recorded");
    assert!(p.opened_in.is_some() && p.opening.is_none());
    let mb = perf::footprint().unwrap() / 1_048_576;
    assert!((10..4096).contains(&mb), "footprint {mb} MB");
    if let Ok(dir) = std::env::var("DROID_LITE_SNAPSHOT_DIR") {
        h.render()
            .unwrap()
            .save(Path::new(&dir).join("e2e-performance.png"))
            .unwrap();
    }
}

#[test]
fn e2e_a_project_plugin_runs_nothing_until_allowed_and_asks_again_after_a_change() {
    let f = Fixture::new("plugin-ok");
    let dir = f.path(".droid-lite/plugins/shared");
    std::fs::create_dir_all(&dir).unwrap();
    let manifest = r#"{"name":"Shared tests","list":["sh","{plugin}/list.sh"]}"#;
    std::fs::write(dir.join("plugin.json"), manifest).unwrap();
    std::fs::write(
        dir.join("list.sh"),
        "touch \"$DROID_LITE_PROJECT/listed\"\necho '[{\"id\":\"a\",\"label\":\"Case A\"}]'\n",
    )
    .unwrap();
    let mut h = f.harness();
    h.get_by_label(&format!("{}  Tests", ic::CHECK_CIRCLE))
        .click();
    wait(&mut h, "plugins found", |a| a.tests.found());
    steps(&mut h, 3);
    h.get_by_label(&format!(
        "Needs your OK: runs [\"sh\", \"{{plugin}}/list.sh\"] from {}\nAllowing trusts this \
         plugin's folder. Commands may also run project code outside it (for example a script in \
         the repo); changes to that code do not ask again.",
        dir.display()
    ));
    for _ in 0..20 {
        h.step();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!f.path("listed").exists(), "nothing ran before the OK");
    assert!(h.state().tests.plugins[0].loading.is_none());
    h.get_by_label("Allow").click();
    wait(&mut h, "cases listed", |a| {
        a.tests.plugins[0].cases.len() == 1
    });
    assert!(f.path("listed").exists());
    let store: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(f.config.join("approved-plugins.json")).unwrap(),
    )
    .unwrap();
    let (root, real) = (f.root.canonicalize().unwrap(), dir.canonicalize().unwrap());
    assert_eq!(store["version"], 2, "{store}");
    let entry = &store["approved"][root.to_str().unwrap()][real.to_str().unwrap()];
    assert!(entry.as_str().is_some_and(|h| h.len() == 64), "{store}");
    // allowed once: the next load lists it at once
    let ctx = h.ctx.clone();
    std::fs::remove_file(f.path("listed")).unwrap();
    h.state_mut().load_plugins(&ctx);
    wait(&mut h, "plugins found", |a| a.tests.found());
    wait(&mut h, "listed again", |a| {
        a.tests.plugins[0].cases.len() == 1
    });
    assert!(f.path("listed").exists());
    // a changed plugin.json asks again, and runs nothing meanwhile
    std::fs::remove_file(f.path("listed")).unwrap();
    std::fs::write(
        dir.join("plugin.json"),
        manifest.replace("list.sh", "list.sh\",\"x"),
    )
    .unwrap();
    h.state_mut().load_plugins(&ctx);
    wait(&mut h, "plugins found", |a| a.tests.found());
    steps(&mut h, 3);
    assert!(!h.state().tests.plugins[0].m.allowed);
    h.get_by_label_contains("Needs your OK: runs ");
    assert!(!f.path("listed").exists());
}

#[test]
fn e2e_a_project_plugin_changed_after_the_ok_runs_nothing_and_asks_again() {
    let f = Fixture::new("plugin-recheck");
    let dir = f.path(".droid-lite/plugins/shared");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("plugin.json"),
        r#"{"name":"Shared","list":["sh","{plugin}/list.sh"],"run":["sh","{plugin}/run.sh","{id}"]}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("list.sh"),
        "touch \"$DROID_LITE_PROJECT/listed\"\necho '[{\"id\":\"a\",\"label\":\"Case A\"}]'\n",
    )
    .unwrap();
    std::fs::write(dir.join("run.sh"), "touch \"$DROID_LITE_PROJECT/ran\"\n").unwrap();
    let mut h = f.harness();
    h.get_by_label(&format!("{}  Tests", ic::CHECK_CIRCLE))
        .click();
    wait(&mut h, "plugins found", |a| a.tests.found());
    steps(&mut h, 3);
    h.get_by_label("Allow").click();
    wait(&mut h, "cases listed", |a| {
        a.tests.plugins[0].cases.len() == 1
    });
    // allowed and loaded; then a file of the folder changes (no reload)
    std::fs::write(dir.join("run.sh"), "touch \"$DROID_LITE_PROJECT/pwned\"\n").unwrap();
    click_in_row(&mut h, "Case A", ic::PLAY);
    // the folder looks changed: it is read in full on a thread, then refused
    wait(&mut h, "the check", |a| !a.tests.plugins[0].m.allowed);
    steps(&mut h, 3);
    let why = "Shared changed since you allowed it; allow it again";
    assert_eq!(h.state().status, why);
    assert!(h.state().tests.plugins[0].run.is_none(), "nothing runs");
    assert!(!h.state().tests.plugins[0].m.allowed);
    // in the tab (and the status bar)
    assert!(h.get_all_by_label(why).count() >= 2);
    h.get_by_label_contains("Needs your OK: runs ");
    for _ in 0..20 {
        h.step();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!f.path("pwned").exists() && !f.path("ran").exists());
    // Allow covers the folder as it is now: the case runs again, once the cases were listed
    // again (maybe after a check of the folder) and the list stopped moving
    let listed = f.path("listed");
    let _ = std::fs::remove_file(&listed);
    h.get_by_label("Allow").click();
    wait(&mut h, "listed again", |a| {
        let p = &a.tests.plugins[0];
        p.m.allowed && listed.exists() && p.settled()
    });
    steps(&mut h, 3);
    click_in_row(&mut h, "Case A", ic::PLAY);
    wait(&mut h, "run", |a| {
        a.tests.plugins[0].run.is_none() && a.tests.plugins[0].status.contains_key("a")
    });
    assert!(f.path("pwned").exists());
}

#[test]
fn e2e_a_running_watch_stops_when_its_folder_changes_and_one_that_cannot_be_checked_is_an_error() {
    let f = Fixture::new("plugin-watch-recheck");
    let dir = f.path(".droid-lite/plugins/live");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("plugin.json"),
        r#"{"name":"Live","watch":["sh","{plugin}/watch.sh"]}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("watch.sh"),
        "echo $$ > \"$DROID_LITE_PROJECT/watch.pid\"\nwhile :; do echo '{\"title\":\"tick\"}'; sleep 0.2; done\n",
    )
    .unwrap();
    let alive = |pid: &str| {
        let kill = std::process::Command::new("kill")
            .args(["-0", pid])
            .stderr(std::process::Stdio::null())
            .status();
        kill.is_ok_and(|s| s.success())
    };
    let mut h = f.harness();
    h.get_by_label(&format!("{}  Tests", ic::CHECK_CIRCLE))
        .click();
    wait(&mut h, "plugins found", |a| a.tests.found());
    steps(&mut h, 3);
    h.get_by_label("Allow").click();
    wait(&mut h, "the watch", |a| a.tests.plugins[0].live.is_some());
    let pid = std::fs::read_to_string(f.path("watch.pid"))
        .unwrap()
        .trim()
        .to_owned();
    assert!(alive(&pid));
    // a new file in the folder while the watch runs, and no new command: a check finds it
    std::fs::write(dir.join("notes.txt"), "new").unwrap();
    wait(&mut h, "the check", |a| !a.tests.plugins[0].m.allowed);
    assert!(
        h.state().tests.plugins[0].watch.is_none() && h.state().tests.plugins[0].live.is_none()
    );
    assert_eq!(
        h.state().status,
        "Live changed since you allowed it; allow it again"
    );
    // the stopped watch is reaped by the next frames (it is a zombie until then)
    for _ in 0..100 {
        h.step();
        if !alive(&pid) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!alive(&pid), "the watch process ended");
    // allowed again, it watches again; then the folder grows too big to check: an error, no Allow
    steps(&mut h, 3);
    h.get_by_label("Allow").click();
    wait(&mut h, "the watch again", |a| {
        a.tests.plugins[0].live.is_some()
    });
    std::fs::write(dir.join("blob"), vec![0u8; 8 << 20]).unwrap();
    wait(&mut h, "the check", |a| {
        a.tests.plugins[0].m.broken.is_some()
    });
    assert!(h.state().tests.plugins[0].watch.is_none());
    steps(&mut h, 3);
    // in the tab and the status bar
    let why = "Live changed since you allowed it and cannot run now (too big";
    assert_eq!(h.get_all_by_label_contains(why).count(), 2);
    assert!(h.query_by_label("Allow").is_none(), "no OK to give");
}

#[test]
fn e2e_a_watch_that_ends_at_once_starts_again_after_a_growing_pause() {
    let f = Fixture::new("plugin-watch-backoff");
    let dir = f.path(".droid-lite/plugins/quick");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("plugin.json"),
        r#"{"name":"Quick","watch":["sh","-c","echo x >> \"$DROID_LITE_PROJECT/starts\""]}"#,
    )
    .unwrap();
    let mut h = f.harness();
    h.get_by_label(&format!("{}  Tests", ic::CHECK_CIRCLE))
        .click();
    wait(&mut h, "plugins found", |a| a.tests.found());
    steps(&mut h, 3);
    h.get_by_label("Allow").click();
    let t0 = std::time::Instant::now();
    while t0.elapsed() < Duration::from_millis(1500) {
        h.step();
        std::thread::sleep(Duration::from_millis(10));
    }
    let starts = std::fs::read_to_string(f.path("starts"))
        .unwrap_or_default()
        .lines()
        .count();
    assert!((1..=2).contains(&starts), "{starts} starts in 1.5 s");
}

#[test]
fn e2e_allow_waits_for_the_lock_off_the_ui_thread() {
    let f = Fixture::new("plugin-allow-lock");
    let dir = f.path(".droid-lite/plugins/shared");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("plugin.json"),
        r#"{"name":"Shared","list":["sh","-c","echo '[{\"id\":\"a\"}]'"]}"#,
    )
    .unwrap();
    let mut h = f.harness();
    h.get_by_label(&format!("{}  Tests", ic::CHECK_CIRCLE))
        .click();
    wait(&mut h, "plugins found", |a| a.tests.found());
    steps(&mut h, 3);
    // the VS Code extension holds the lock
    std::fs::create_dir_all(&f.config).unwrap();
    let lock = f.config.join("approved-plugins.json.lock");
    std::fs::write(&lock, "4242 the extension").unwrap();
    let t0 = std::time::Instant::now();
    h.get_by_label("Allow").click();
    steps(&mut h, 5);
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "a frame waited for the lock"
    );
    assert!(!h.state().tests.plugins[0].m.allowed);
    std::fs::remove_file(&lock).unwrap();
    wait(&mut h, "allowed and listed", |a| {
        a.tests.plugins[0].cases.len() == 1
    });
    assert!(h.state().tests.plugins[0].m.allowed);
}

#[test]
fn e2e_a_project_plugin_not_allowed_shows_no_links_and_only_web_links_ever_show() {
    let f = Fixture::new("plugin-links");
    let dir = f.path(".droid-lite/plugins/linky");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("plugin.json"),
        r#"{"name":"Linky","list":["sh","-c","echo []"],"links":[{"name":"PluginWebLink","url":"https://example.com"}]}"#,
    )
    .unwrap();
    // a link that is not http(s) makes the whole plugin an error, as in the VS Code extension
    let bad = f.path(".droid-lite/plugins/sharey");
    std::fs::create_dir_all(&bad).unwrap();
    std::fs::write(
        bad.join("plugin.json"),
        r#"{"name":"Sharey","links":[{"name":"PluginShareLink","url":"smb://evil/share"}]}"#,
    )
    .unwrap();
    let mut h = f.harness();
    h.get_by_label(&format!("{}  Tests", ic::CHECK_CIRCLE))
        .click();
    wait(&mut h, "plugins found", |a| a.tests.found());
    steps(&mut h, 3);
    assert_eq!(h.state().tests.plugins.len(), 1);
    h.get_by_label_contains("Needs your OK: runs ");
    assert!(
        h.query_by_label("PluginWebLink").is_none(),
        "a link shows before the OK"
    );
    h.get_by_label("Allow").click();
    wait(&mut h, "allowed", |a| a.tests.plugins[0].m.allowed);
    steps(&mut h, 3);
    h.get_by_label("PluginWebLink");
    assert!(h.query_by_label("PluginShareLink").is_none());
    let errors = &h.state().tests.errors;
    assert!(
        errors.len() == 1
            && errors[0].ends_with(
                "sharey/plugin.json: links: PluginShareLink: url must be an http or https link"
            ),
        "{errors:?}"
    );
}

#[test]
fn e2e_a_watch_that_cannot_start_waits_before_trying_again_and_says_why() {
    let f = Fixture::new("plugin-watch-spawn");
    let dir = f.config.join("plugins/nowatch");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("plugin.json"),
        r#"{"name":"No watch","watch":["/nonexistent/droid-lite-watch"]}"#,
    )
    .unwrap();
    let mut h = f.harness();
    h.get_by_label(&format!("{}  Tests", ic::CHECK_CIRCLE))
        .click();
    wait(&mut h, "plugins found", |a| a.tests.found());
    let t0 = std::time::Instant::now();
    while t0.elapsed() < Duration::from_millis(1300) {
        h.step();
        std::thread::sleep(Duration::from_millis(10));
    }
    let p = &h.state().tests.plugins[0];
    assert!(p.watch.is_none());
    assert!(
        p.watch_backoff >= Duration::from_secs(1),
        "{:?}",
        p.watch_backoff
    );
    assert!(p.watch_next.is_some_and(|t| t > std::time::Instant::now()));
    h.get_by_label_contains("watch: could not start /nonexistent/droid-lite-watch");
}

#[test]
fn e2e_a_refused_plugin_stops_its_list_and_kills_a_watch_that_ignores_sigterm() {
    let f = Fixture::new("plugin-refused-kill");
    let dir = f.path(".droid-lite/plugins/hang");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("plugin.json"),
        r#"{"name":"Hang","list":["sh","-c","sleep 30 & echo $! > \"$DROID_LITE_PROJECT/list.pid\"; wait"],
            "watch":["sh","-c","trap '' TERM; echo $$ > \"$DROID_LITE_PROJECT/watch.pid\"; while :; do echo '{\"title\":\"t\"}'; sleep 0.2; done"]}"#,
    )
    .unwrap();
    let alive = |pid: &str| unsafe { libc::kill(pid.trim().parse().unwrap(), 0) } == 0;
    let mut h = f.harness();
    h.get_by_label(&format!("{}  Tests", ic::CHECK_CIRCLE))
        .click();
    wait(&mut h, "plugins found", |a| a.tests.found());
    steps(&mut h, 3);
    h.get_by_label("Allow").click();
    wait(&mut h, "the list and the watch", |a| {
        a.tests.plugins[0].live.is_some() && f.path("list.pid").exists()
    });
    let list = std::fs::read_to_string(f.path("list.pid")).unwrap();
    let watch = std::fs::read_to_string(f.path("watch.pid")).unwrap();
    assert!(alive(&list) && alive(&watch));
    // a new file in its folder: the check while the watch runs refuses it
    std::fs::write(dir.join("notes.txt"), "new").unwrap();
    wait(&mut h, "the check", |a| !a.tests.plugins[0].m.allowed);
    let refused = std::time::Instant::now();
    let mut list_gone = None;
    while refused.elapsed() < Duration::from_secs(9) && (list_gone.is_none() || alive(&watch)) {
        h.step();
        if list_gone.is_none() && !alive(&list) {
            list_gone = Some(refused.elapsed());
        }
        if refused.elapsed() > Duration::from_millis(2500)
            && refused.elapsed() < Duration::from_secs(3)
        {
            assert!(
                alive(&watch),
                "SIGTERM is ignored: still there before its 5 s"
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        list_gone.is_some_and(|t| t < Duration::from_secs(2)),
        "the list's group was killed: {list_gone:?}"
    );
    assert!(!alive(&watch), "SIGKILL after 5 s");
    assert!(refused.elapsed() >= Duration::from_secs(4));
}

#[test]
fn e2e_add_plugin_selects_the_added_folder_not_the_first_of_that_name() {
    let f = Fixture::new("plugin-add-same-name");
    let own = f.config.join("plugins/aaa");
    std::fs::create_dir_all(&own).unwrap();
    std::fs::write(own.join("plugin.json"), r#"{"name":"Same"}"#).unwrap();
    let tool = f.path("tool");
    std::fs::create_dir_all(&tool).unwrap();
    std::fs::write(tool.join("plugin.json"), r#"{"name":"Same"}"#).unwrap();
    let mut h = f.harness();
    let ctx = h.ctx.clone();
    h.state_mut().add_plugin(&tool, &ctx);
    wait(&mut h, "plugins found", |a| a.tests.found());
    let t = &h.state().tests;
    assert_eq!(t.plugins.len(), 2);
    assert_eq!(t.plugins[t.active].m.real, tool.canonicalize().unwrap());
}

#[test]
fn e2e_a_plugin_folder_is_read_in_full_only_off_the_ui_thread() {
    let f = Fixture::new("plugin-off-ui");
    let dir = f.path(".droid-lite/plugins/shared");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("plugin.json"),
        r#"{"name":"Shared","list":["sh","{plugin}/list.sh"],"run":["sh","{plugin}/run.sh","{id}"]}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("list.sh"),
        "echo '[{\"id\":\"a\",\"label\":\"Case A\"}]'\n",
    )
    .unwrap();
    std::fs::write(dir.join("run.sh"), "touch \"$DROID_LITE_PROJECT/ran\"\n").unwrap();
    let reads = || crate::plugins::tests::READS.with(|r| r.get());
    let start = reads();
    let mut h = f.harness();
    let ctx = h.ctx.clone();
    h.get_by_label(&format!("{}  Tests", ic::CHECK_CIRCLE))
        .click();
    wait(&mut h, "plugins found", |a| a.tests.found());
    steps(&mut h, 3);
    h.get_by_label("Allow").click();
    wait(&mut h, "cases listed", |a| {
        a.tests.plugins[0].cases.len() == 1
    });
    // a reload reads the folder in full (on a thread), and lists
    h.state_mut().load_plugins(&ctx);
    wait(&mut h, "plugins found", |a| a.tests.found());
    wait(&mut h, "listed again", |a| {
        a.tests.plugins.first().is_some_and(|p| p.cases.len() == 1)
    });
    // the same bytes written again: it looks changed, the ▶ waits for a full read on a thread
    std::fs::write(dir.join("run.sh"), "touch \"$DROID_LITE_PROJECT/ran\"\n").unwrap();
    steps(&mut h, 3);
    click_in_row(&mut h, "Case A", ic::PLAY);
    wait(&mut h, "run", |a| {
        a.tests.plugins[0].run.is_none() && a.tests.plugins[0].status.contains_key("a")
    });
    assert!(f.path("ran").exists());
    assert_eq!(
        reads(),
        start,
        "no full read of the folder on the UI thread"
    );
}

#[test]
fn e2e_plugin_tests_tab_runs_cases_from_list_gutter_views_and_actions() {
    let f = Fixture::new("plugins");
    std::fs::create_dir_all(f.path("flows")).unwrap();
    std::fs::write(f.path("flows/login.yaml"), "appId: demo\n- tapOn: Login\n").unwrap();
    std::fs::write(f.path("flows/solo.yaml"), "appId: demo\n").unwrap();
    let mut h = f.harness();
    // no plugin yet: the tab says how to add one
    h.get_by_label(&format!("{}  Tests", ic::CHECK_CIRCLE))
        .click();
    wait(&mut h, "plugins found", |a| a.tests.found());
    steps(&mut h, 2);
    h.get_by_label(&format!("{}  Add plugin…", ic::FOLDER_OPEN));
    // Add plugin with the example of docs/PLUGINS.md (the dialog's pick), so the documented
    // format stays tested
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/plugin-example");
    let ctx = h.ctx.clone();
    h.state_mut().add_plugin(&example, &ctx);
    assert!(f.config.join("plugins/example-tests").is_symlink());
    wait(&mut h, "cases listed", |a| a.tests.marks.len() == 3);
    wait(&mut h, "live state", |a| {
        a.tests.plugins[0]
            .live
            .as_ref()
            .is_some_and(|l| l.title == "Phone idle")
    });
    steps(&mut h, 2);
    h.get_by_label("Phone idle");
    h.get_by_label("fake-1");

    // ▶ in the list: no status line for the case itself, so exit 0 means passed
    click_in_row(&mut h, "solo case", ic::PLAY);
    wait(&mut h, "solo run", |a| a.tests.plugins[0].run.is_none());
    let st =
        |h: &Harness<'static, App>, id: &str| h.state().tests.plugins[0].status.get(id).copied();
    assert_eq!(st(&h, "solo"), Some(crate::plugins::Status::Passed));
    steps(&mut h, 2);
    h.get_by_label("step 1");
    assert!(
        h.state().tests.plugins[0]
            .log
            .iter()
            .any(|l| l == "starting solo")
    );

    // gutter ▶ on a line with two cases: the list shows just those to choose
    let login = f.path("flows/login.yaml");
    h.state_mut().open_file(&login, None);
    steps(&mut h, 3);
    h.get_by_label_contains("Run a case: ").click();
    steps(&mut h, 3);
    assert_eq!(h.state().tests.only.len(), 2);
    h.get_by_label("Cases on this line");
    click_in_row(&mut h, "fails", ic::PLAY);
    wait(&mut h, "failing run", |a| a.tests.plugins[0].run.is_none());
    assert_eq!(st(&h, "bad"), Some(crate::plugins::Status::Failed));
    steps(&mut h, 2);
    // a result with a file opens it at its line
    h.state_mut()
        .open_file(&f.path("app/src/main/java/com/demo/data/User.kt"), None);
    steps(&mut h, 2);
    h.get_by_label("step 1  button missing").click();
    steps(&mut h, 3);
    assert!(tab(&h).path.ends_with("flows/login.yaml"));
    assert!(tab(&h).text[tab(&h).cursor..].starts_with("- tapOn"));

    // gutter ▶ on a line with one case runs it at once
    h.state_mut().open_file(&f.path("flows/solo.yaml"), None);
    steps(&mut h, 3);
    h.state_mut().tests.plugins[0].status.clear();
    h.get_by_label("Run solo case").click();
    steps(&mut h, 2);
    wait(&mut h, "gutter run", |a| {
        a.tests.plugins[0].run.is_none() && a.tests.plugins[0].status.contains_key("solo")
    });
    assert_eq!(st(&h, "solo"), Some(crate::plugins::Status::Passed));

    // a view lists rows; an action reports like a run
    h.get_by_label("Failures").click();
    wait(&mut h, "failures view", |a| {
        a.tests.plugins[0].rows.len() == 1
    });
    steps(&mut h, 2);
    h.get_by_label("F-1  stuck on Home");
    h.get_by_label("Check").click();
    wait(&mut h, "action", |a| {
        a.tests.plugins[0].run.is_none() && !a.tests.plugins[0].results.is_empty()
    });
    steps(&mut h, 2);
    h.get_by_label("tool  ready");
    if let Ok(dir) = std::env::var("DROID_LITE_SNAPSHOT_DIR") {
        h.render()
            .unwrap()
            .save(Path::new(&dir).join("e2e-tests-tab.png"))
            .unwrap();
    }

    // the live state runs only while the tab shows
    h.get_by_label(&format!("{}  Performance", ic::GAUGE))
        .click();
    steps(&mut h, 2);
    assert!(h.state().tests.plugins[0].watch.is_none());
    assert!(h.state().tests.plugins[0].live.is_none());

    // Remove drops the link only
    h.get_by_label(&format!("{}  Tests", ic::CHECK_CIRCLE))
        .click();
    wait(&mut h, "plugins found", |a| a.tests.found());
    steps(&mut h, 2);
    h.get_by_label("Remove").click();
    wait(&mut h, "plugins found", |a| a.tests.found());
    steps(&mut h, 2);
    assert!(h.state().tests.plugins.is_empty());
    assert!(!f.config.join("plugins/example-tests").exists());
    assert!(example.join("plugin.json").is_file());
}

#[test]
fn e2e_action_palette_focus_mode_panel_toggles_and_build_toasts() {
    let f = Fixture::new("gradle");
    let mut h = f.harness();
    let compose_tab = format!("{}  Compose", ic::CUBE);
    h.get_by_label(&compose_tab);

    // ⇧⌘A lists commands; Enter runs the picked one
    key(&mut h, Modifiers::COMMAND | Modifiers::SHIFT, Key::A);
    assert!(
        h.state()
            .search
            .as_ref()
            .is_some_and(|s| s.kind == SearchKind::Actions)
    );
    {
        let s = h.state_mut().search.as_mut().unwrap();
        s.query = "focus mode".into();
        s.stale = true;
    }
    steps(&mut h, 2);
    key(&mut h, Modifiers::NONE, Key::Enter);
    assert!(h.state().focus, "the palette ran Focus mode");
    assert!(h.state().search.is_none());
    steps(&mut h, 40); // the panels slide away
    assert!(
        h.query_by_label(&compose_tab).is_none(),
        "focus mode hides the right panel"
    );

    // ⌘1 in focus mode leaves it with only the Project panel open
    key(&mut h, Modifiers::COMMAND, Key::Num1);
    let a = h.state();
    assert!(!a.focus && a.show_project && !a.show_right && !a.show_bottom);

    // the bottom panel is closed, so a finished build shows a note; a click opens the Build tab
    h.state_mut().bottom = Bottom::Problems;
    key(&mut h, Modifiers::CTRL | Modifiers::SHIFT, Key::R);
    wait(&mut h, "build", |a| {
        a.gradle.is_none() && !a.toasts.is_empty()
    });
    steps(&mut h, 3);
    h.get_by_label_contains("assembleDebug finished in").click();
    steps(&mut h, 3);
    let a = h.state();
    assert!(a.show_bottom && a.bottom == Bottom::Build && a.toasts.is_empty());

    // with the Build tab on screen, the note would only cover it: none is shown
    key(&mut h, Modifiers::CTRL | Modifiers::SHIFT, Key::R);
    wait(&mut h, "second build", |a| {
        a.gradle.is_none() && a.console.iter().any(|l| l.contains("Gradle finished: OK"))
    });
    assert!(h.state().toasts.is_empty());

    // the status bar's panel buttons toggle too
    h.get_by_label(ic::CUBE).click();
    steps(&mut h, 2);
    assert!(h.state().show_right);
}

#[test]
fn e2e_welcome_screen_lists_recent_projects_and_opens_one() {
    let f = Fixture::new("welcome");
    save_setting("recent-projects", &f.root.to_string_lossy());
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .build_eframe(|cc| App::new(cc, None));
    steps(&mut h, 3);
    h.get_by_label(&format!("{}  Open project…", ic::FOLDER_OPEN));
    if let Ok(dir) = std::env::var("DROID_LITE_SNAPSHOT_DIR") {
        let img = h.render().unwrap();
        img.save(Path::new(&dir).join("e2e-welcome.png")).unwrap();
    }
    let name = f.root.file_name().unwrap().to_string_lossy().into_owned();
    h.get_by_label(&name).click();
    wait(&mut h, "project", |a| {
        a.indexing.is_none() && a.project.is_some()
    });
    assert_eq!(h.state().project.as_ref().unwrap().root, f.root);
}

#[test]
fn e2e_kotlin_android_helpers_fixes_templates_new_file_variants_tests_and_manifest() {
    let f = Fixture::new("kotlin-helpers");
    let write = |rel: &str, text: &str| {
        let p = f.path(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    };
    write(
        "app/build.gradle.kts",
        "plugins { id(\"com.android.application\") }\nandroid {\n    namespace = \"com.demo\"\n    defaultConfig { applicationId = \"com.demo.app\" }\n    buildTypes {\n        create(\"staging\") { }\n    }\n}\n",
    );
    write(
        "app/src/main/AndroidManifest.xml",
        "<manifest xmlns:android=\"http://schemas.android.com/apk/res/android\">\n    <application>\n        <activity android:name=\".MainActivity\" android:exported=\"true\">\n            <intent-filter>\n                <action android:name=\"android.intent.action.MAIN\" />\n                <category android:name=\"android.intent.category.LAUNCHER\" />\n            </intent-filter>\n        </activity>\n    </application>\n</manifest>\n",
    );
    write(
        "app/src/main/java/com/demo/MainActivity.kt",
        "package com.demo\n\nclass MainActivity : ComponentActivity()\n",
    );
    write(
        "app/src/main/java/com/demo/ui/Counter.kt",
        "package com.demo.ui\n\nimport androidx.compose.runtime.Composable\n\n@Composable\nfun Counter(label: String) {\n    val count = mutableStateOf(0)\n    Text(\"Hello there\")\n}\n",
    );
    write(
        "app/src/test/java/com/demo/MathTest.kt",
        "package com.demo\n\nclass MathTest {\n    @Test\n    fun adds() {}\n}\n",
    );
    let mut h = f.harness();
    wait(&mut h, "manifest scan", |a| {
        a.res.component("com.demo.MainActivity").is_some()
    });

    // build variants of the app module, in the toolbar
    assert_eq!(h.state().variants, ["debug", "release", "staging"]);
    h.get_by_label("Build variant");

    // ⌥Enter on a Compose check: wrap in remember, with its import
    open(&mut h, &f.path("app/src/main/java/com/demo/ui/Counter.kt"));
    place_cursor(&mut h, "mutableStateOf", 2);
    key(&mut h, Modifiers::ALT, Key::Enter);
    h.get_by_label("Wrap in remember { }");
    key(&mut h, Modifiers::NONE, Key::Enter);
    assert!(
        tab(&h)
            .text
            .contains("val count = remember { mutableStateOf(0) }"),
        "{}",
        tab(&h).text
    );
    assert!(
        tab(&h)
            .text
            .contains("import androidx.compose.runtime.remember\n")
    );
    assert!(h.state().fix_popup.is_none());

    // extract a string: strings.xml gets it, the code uses stringResource and imports the module's R
    place_cursor(&mut h, "Hello there", 3);
    key(&mut h, Modifiers::ALT, Key::Enter);
    if let Ok(dir) = std::env::var("DROID_LITE_SNAPSHOT_DIR") {
        steps(&mut h, 3);
        h.render()
            .unwrap()
            .save(Path::new(&dir).join("e2e-quick-fix.png"))
            .unwrap();
    }
    h.get_by_label("Extract string resource `hello_there`")
        .click();
    steps(&mut h, 3);
    let text = tab(&h).text.clone();
    assert!(
        text.contains("Text(stringResource(R.string.hello_there))"),
        "{text}"
    );
    assert!(
        text.contains("import com.demo.R\n")
            && text.contains("import androidx.compose.ui.res.stringResource\n")
    );
    let strings = std::fs::read_to_string(f.path("app/src/main/res/values/strings.xml")).unwrap();
    assert!(
        strings.contains("<string name=\"hello_there\">Hello there</string>"),
        "{strings}"
    );

    // a live template from the completion list: comp + Tab
    let end = tab(&h).text.len();
    h.state_mut()
        .tabs
        .iter_mut()
        .for_each(|t| t.goto = Some(t.text.len()));
    steps(&mut h, 3);
    assert_eq!(tab(&h).cursor, end);
    type_text(&mut h, "\n");
    type_text(&mut h, "comp");
    key(&mut h, Modifiers::NONE, Key::Tab);
    let text = tab(&h).text.clone();
    assert!(
        text.ends_with("@Composable\nfun (modifier: Modifier = Modifier) {\n}"),
        "{text}"
    );
    assert!(text.contains("import androidx.compose.ui.Modifier\n"));
    let t = tab(&h);
    assert!(
        t.text[..t.cursor].ends_with("fun "),
        "the cursor waits for the name"
    );

    // ▶ on a JUnit test runs it with Gradle for the variant; with the bottom panel hidden a note says how it went
    key(&mut h, Modifiers::COMMAND, Key::Num4);
    assert!(!h.state().show_bottom);
    open(&mut h, &f.path("app/src/test/java/com/demo/MathTest.kt"));
    h.get_by_label("Run test").click();
    wait(&mut h, "gradle", |a| {
        a.gradle.is_none()
            && !a.console.is_empty()
            && a.console.iter().any(|l| l.starts_with("Gradle finished"))
    });
    assert_eq!(
        h.state().console[0],
        "> ./gradlew :app:testDebugUnitTest --tests com.demo.MathTest.adds"
    );
    steps(&mut h, 2);
    h.get_by_label_contains("Tests passed in");
    h.get_by_label("Run all tests");

    // the launcher Activity links to its manifest entry
    open(
        &mut h,
        &f.path("app/src/main/java/com/demo/MainActivity.kt"),
    );
    if let Ok(dir) = std::env::var("DROID_LITE_SNAPSHOT_DIR") {
        steps(&mut h, 3);
        h.render()
            .unwrap()
            .save(Path::new(&dir).join("e2e-manifest-chip.png"))
            .unwrap();
    }
    h.get_by_label_contains("Launcher activity").click();
    steps(&mut h, 3);
    assert!(tab(&h).path.ends_with("AndroidManifest.xml"));
    assert!(tab(&h).text[tab(&h).cursor..].starts_with("<activity android:name=\".MainActivity\""));

    // ⌘N: a ViewModel next to the open file, with its package
    open(
        &mut h,
        &f.path("app/src/main/java/com/demo/MainActivity.kt"),
    );
    key(&mut h, Modifiers::COMMAND, Key::N);
    h.get_by_label("package com.demo");
    type_text(&mut h, "Home");
    h.get_by_label("ViewModel (with UI state)").click();
    steps(&mut h, 2);
    h.get_by_label("→ HomeViewModel.kt");
    if let Ok(dir) = std::env::var("DROID_LITE_SNAPSHOT_DIR") {
        steps(&mut h, 3);
        h.render()
            .unwrap()
            .save(Path::new(&dir).join("e2e-new-file.png"))
            .unwrap();
    }
    h.get_by_label("Create").click();
    steps(&mut h, 3);
    let vm = f.path("app/src/main/java/com/demo/HomeViewModel.kt");
    assert!(tab(&h).path.ends_with("HomeViewModel.kt"));
    assert!(
        std::fs::read_to_string(&vm)
            .unwrap()
            .starts_with("package com.demo\n\nimport androidx.lifecycle.ViewModel\n")
    );
    // an Activity goes into the manifest too
    key(&mut h, Modifiers::COMMAND, Key::N);
    type_text(&mut h, "Settings");
    h.get_by_label("Activity (Compose, added to the manifest)")
        .click();
    steps(&mut h, 2);
    h.get_by_label("Create").click();
    steps(&mut h, 3);
    assert!(
        f.path("app/src/main/java/com/demo/SettingsActivity.kt")
            .exists()
    );
    let manifest = std::fs::read_to_string(f.path("app/src/main/AndroidManifest.xml")).unwrap();
    assert!(
        manifest.contains("android:name=\"com.demo.SettingsActivity\""),
        "{manifest}"
    );
    // an existing name is refused
    key(&mut h, Modifiers::COMMAND, Key::N);
    type_text(&mut h, "Settings");
    h.get_by_label("Activity (Compose, added to the manifest)")
        .click();
    h.get_by_label("Create").click();
    steps(&mut h, 2);
    h.get_by_label("SettingsActivity.kt is there already");
}

#[test]
fn e2e_hilt_links_lint_in_problems_translations_emulator_and_deep_links() {
    let f = Fixture::new("android-tools");
    let write = |rel: &str, text: &str| {
        let p = f.path(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    };
    write(
        "app/build.gradle.kts",
        "plugins { id(\"com.android.application\") }\nandroid {\n    namespace = \"com.demo\"\n    defaultConfig { applicationId = \"com.demo.app\" }\n}\n",
    );
    write(
        "app/src/main/AndroidManifest.xml",
        "<manifest xmlns:android=\"http://schemas.android.com/apk/res/android\">\n    <application>\n        <activity android:name=\".MainActivity\">\n            <intent-filter>\n                <action android:name=\"android.intent.action.VIEW\" />\n                <data android:scheme=\"demo\" android:host=\"home\" />\n            </intent-filter>\n        </activity>\n    </application>\n</manifest>\n",
    );
    write(
        "app/src/main/java/com/demo/di/AppModule.kt",
        "package com.demo.di\n\n@Module\nobject AppModule {\n    @Provides\n    fun provideRepo(): Repo = RealRepo()\n}\n",
    );
    write(
        "app/src/main/java/com/demo/HomeViewModel.kt",
        "package com.demo\n\n@HiltViewModel\nclass HomeViewModel @Inject constructor(private val repo: Repo) : ViewModel()\n",
    );
    write(
        "app/src/main/java/com/demo/HomeFragment.kt",
        "package com.demo\n\nclass HomeFragment : Fragment() {\n    @Inject lateinit var repo: Repo\n}\n",
    );
    write(
        "app/src/main/res/values/strings.xml",
        "<resources>\n    <string name=\"hello\">Hello</string>\n    <string name=\"bye\">Bye</string>\n    <string name=\"brand\" translatable=\"false\">Demo</string>\n</resources>\n",
    );
    write(
        "app/src/main/res/values-hi/strings.xml",
        "<resources>\n    <string name=\"hello\">Namaste</string>\n</resources>\n",
    );
    // a Gradle whose lint and detekt tasks write their reports
    write(
        "gradlew",
        "#!/bin/sh\necho \"> Task $1\"\ncase \"$1\" in\n  lint) mkdir -p app/build/reports && printf '<issues>\\n<issue id=\"HardcodedText\" severity=\"Warning\" message=\"Hardcoded string &quot;Hi&quot;\">\\n<location file=\"app/src/main/java/com/demo/HomeFragment.kt\" line=\"3\" column=\"5\"/>\\n</issue>\\n</issues>\\n' > app/build/reports/lint-results-debug.xml;;\n  detekt) mkdir -p app/build/reports/detekt && printf '<checkstyle>\\n<file name=\"app/src/main/java/com/demo/HomeFragment.kt\">\\n<error line=\"4\" column=\"5\" severity=\"error\" message=\"Too long.\" source=\"detekt.LongMethod\"/>\\n</file>\\n</checkstyle>\\n' > app/build/reports/detekt/detekt.xml;;\nesac\necho 'BUILD SUCCESSFUL'\n",
    );
    let emulator_log = f.path("emulator-calls.txt");
    write(
        "fake-emulator",
        &format!(
            "#!/bin/sh\nif [ \"$1\" = -list-avds ]; then echo 'INFO    | Storing crashdata'; echo Pixel_8_API_35; else echo \"$@\" >> '{}'; fi\n",
            emulator_log.display()
        ),
    );
    for x in ["gradlew", "fake-emulator"] {
        std::process::Command::new("chmod")
            .arg("+x")
            .arg(f.path(x))
            .status()
            .unwrap();
    }
    let adb_log = f.path("adb-calls.txt");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_adb.sh");
    // SAFETY: tests run one at a time (SERIAL), so no other thread reads the environment now.
    unsafe {
        std::env::set_var("DROID_LITE_ADB", &script);
        std::env::set_var("FAKE_ADB_LOG", &adb_log);
        std::env::set_var("DROID_LITE_EMULATOR", f.path("fake-emulator"));
    }
    let mut h = f.harness();
    wait(&mut h, "manifest scan", |a| !a.res.links.is_empty());
    assert_eq!(h.state().res.links, ["demo://home"]);

    // Hilt: the provider lists where Repo is injected; the injected places link back to it
    open(
        &mut h,
        &f.path("app/src/main/java/com/demo/di/AppModule.kt"),
    );
    let texts = |h: &Harness<'static, App>| -> Vec<String> {
        lenses(h).into_iter().flat_map(|(_, t)| t).collect()
    };
    // the links come from the worker's pass over the file
    for _ in 0..500 {
        if !texts(&h).is_empty() {
            break;
        }
        steps(&mut h, 1);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(texts(&h), ["Injected in 2 places"]);
    h.get_by_label("Injected in 2 places").click();
    steps(&mut h, 3);
    assert_eq!(h.state().search.as_ref().map(|s| s.pinned.len()), Some(2));
    key(&mut h, Modifiers::NONE, Key::Escape);
    open(
        &mut h,
        &f.path("app/src/main/java/com/demo/HomeViewModel.kt"),
    );
    // the links come from the worker's pass over the file
    for _ in 0..500 {
        if !texts(&h).is_empty() {
            break;
        }
        steps(&mut h, 1);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(texts(&h), ["Dependencies: Repo"]);
    h.get_by_label("Dependencies: Repo").click();
    steps(&mut h, 3);
    assert!(
        tab(&h).path.ends_with("AppModule.kt"),
        "one provider opens at once"
    );
    assert!(tab(&h).text[tab(&h).cursor..].starts_with("provideRepo"));
    open(
        &mut h,
        &f.path("app/src/main/java/com/demo/HomeFragment.kt"),
    );
    assert_eq!(texts(&h), ["Provided by provideRepo"]);

    // lint and detekt reports land in Problems
    for task in ["lint", "detekt"] {
        h.state_mut().gradle_task = task.into();
        let ctx = h.ctx.clone();
        h.state_mut().run(Action::RunTask, &ctx);
        wait(&mut h, task, |a| {
            a.gradle.is_none() && a.console.iter().any(|l| l.starts_with("Gradle finished"))
        });
    }
    steps(&mut h, 3);
    let problems = &h.state().build_problems[&f.path("app/src/main/java/com/demo/HomeFragment.kt")];
    assert_eq!(
        problems,
        &[(4, 5, 1, "[detekt LongMethod] Too long.".to_string())],
        "only the last run's report"
    );
    assert_eq!(h.state().bottom, Bottom::Problems);
    h.get_by_label_contains("[detekt LongMethod] Too long.");
    h.state_mut().gradle_task = "lint".into();
    let ctx = h.ctx.clone();
    h.state_mut().run(Action::RunTask, &ctx);
    wait(&mut h, "lint again", |a| {
        a.gradle.is_none() && a.console.iter().any(|l| l.starts_with("Gradle finished"))
    });
    steps(&mut h, 3);
    h.get_by_label_contains("[lint HardcodedText] Hardcoded string \"Hi\"");

    // translations: the missing Hindi `bye` is marked; typing it writes values-hi
    let ctx = h.ctx.clone();
    h.state_mut().run(Action::Translations, &ctx);
    steps(&mut h, 3);
    h.get_by_label("3 strings · 1 language · 1 missing");
    h.get_by_label("not translated");
    if let Ok(dir) = std::env::var("DROID_LITE_SNAPSHOT_DIR") {
        steps(&mut h, 3);
        h.render()
            .unwrap()
            .save(Path::new(&dir).join("e2e-translations.png"))
            .unwrap();
    }

    h.get_by_label("missing").click();
    steps(&mut h, 3);
    type_text(&mut h, "Alvida");
    key(&mut h, Modifiers::NONE, Key::Enter);
    steps(&mut h, 3);
    let hi = std::fs::read_to_string(f.path("app/src/main/res/values-hi/strings.xml")).unwrap();
    assert!(hi.contains("<string name=\"bye\">Alvida</string>"), "{hi}");
    wait(&mut h, "rescan", |a| a.res.get("string", "bye").len() == 2);
    steps(&mut h, 3);
    h.get_by_label("3 strings · 1 language · 0 missing");
    h.state_mut().translations.open = false;

    // the device picker lists the emulators and starts one
    h.get_by_label("Run device").click();
    steps(&mut h, 3);
    h.get_by_label(&format!("{}  Pixel_8_API_35", ic::PLAY))
        .click();
    steps(&mut h, 3);
    wait(&mut h, "emulator started", |_| emulator_log.exists());
    assert_eq!(
        std::fs::read_to_string(&emulator_log).unwrap().trim(),
        "-avd Pixel_8_API_35"
    );

    // a deep link from the manifest opens in the app on the device
    h.get_by_label("Run device").click();
    steps(&mut h, 3);
    if let Ok(dir) = std::env::var("DROID_LITE_SNAPSHOT_DIR") {
        steps(&mut h, 3);
        h.render()
            .unwrap()
            .save(Path::new(&dir).join("e2e-device-picker.png"))
            .unwrap();
    }
    h.get_by_label("Open a deep link…").click();
    steps(&mut h, 3);
    h.get_by_label("Open on the device").click();
    wait(&mut h, "link opened", |a| {
        a.status.starts_with("Opened demo://home")
    });
    unsafe {
        std::env::remove_var("FAKE_ADB_LOG");
        std::env::remove_var("DROID_LITE_EMULATOR");
    }
    let log = std::fs::read_to_string(&adb_log).unwrap();
    assert!(
        log.contains(
            "shell am start -W -a android.intent.action.VIEW -d 'demo://home' com.demo.app.debug"
        ),
        "{log}"
    );
}

/// The ⌘-hover count of `User`: by the declaration it means once the file is read, by name before.
fn user_usages(h: &Harness<'static, App>) -> Option<Vec<FoundLine>> {
    h.state()
        .usages
        .iter()
        .find(|(k, _)| *k == "User" || k.starts_with("User\u{0}"))
        .and_then(|(_, u)| u.1.clone())
}

#[test]
fn e2e_usages_drop_a_file_deleted_on_disk() {
    let f = Fixture::new("deleted-use");
    let write = |rel: &str, text: &str| std::fs::write(f.path(rel), text).unwrap();
    write(
        "app/src/main/java/com/demo/data/Dao.kt",
        "package com.demo.data\n\ninterface Dao {\n    fun insert(u: User)\n}\n",
    );
    for name in ["A", "B"] {
        write(
            &format!("app/src/main/java/com/demo/data/{name}.kt"),
            &format!(
                "package com.demo.data\n\nclass {name}(val dao: Dao) {{\n    fun go(u: User) = dao.insert(u)\n}}\n"
            ),
        );
    }
    let mut h = f.harness();
    let b = f.path("app/src/main/java/com/demo/data/B.kt");
    open(&mut h, &b);
    let at = tab(&h).text.find("dao.insert").unwrap() + 4;
    let count = |h: &mut Harness<'static, App>| {
        for _ in 0..500 {
            if let Some(v) = h.state_mut().usages_of("insert", at) {
                let mut v: Vec<String> = v.iter().map(|l| l.path.display().to_string()).collect();
                v.sort();
                return v;
            }
            steps(h, 1);
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("no usages");
    };
    assert_eq!(count(&mut h).len(), 3, "the declaration, A and B");
    std::fs::remove_file(f.path("app/src/main/java/com/demo/data/A.kt")).unwrap();
    wait(&mut h, "the delete is seen", |a| {
        a.project
            .as_ref()
            .is_some_and(|p| p.files.iter().all(|x| !x.ends_with("A.kt")))
    });
    let left = count(&mut h);
    assert!(left.iter().all(|p| !p.ends_with("A.kt")), "{left:?}");
    assert_eq!(left.len(), 2);
}

#[test]
fn e2e_usages_of_a_member_skip_same_named_members_of_other_classes() {
    let f = Fixture::new("same-name");
    let write = |rel: &str, text: &str| {
        let p = f.path(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    };
    write(
        "app/src/main/java/com/demo/data/UserDao.kt",
        "package com.demo.data\n\ninterface UserDao {\n    fun insert(u: User)\n}\n",
    );
    write(
        "app/src/main/java/com/demo/data/Cache.kt",
        "package com.demo.data\n\nclass Cache {\n    fun insert(k: String) {}\n}\n",
    );
    write(
        "app/src/main/java/com/demo/data/Saver.kt",
        "package com.demo.data\n\nclass Saver(private val dao: UserDao, private val cache: Cache) {\n    fun save(u: User) {\n        dao.insert(u)\n        cache.insert(\"k\")\n    }\n}\n",
    );
    let mut h = f.harness();
    let saver = f.path("app/src/main/java/com/demo/data/Saver.kt");
    open(
        &mut h,
        &f.path("app/src/main/java/com/demo/data/UserDao.kt"),
    );
    place_cursor(&mut h, "insert", 0);
    key(&mut h, Modifiers::COMMAND | Modifiers::SHIFT, Key::G);
    wait(&mut h, "references", |a| {
        a.find_rx.is_none() && !a.found.is_empty()
    });
    let mut texts: Vec<String> = h
        .state()
        .found
        .iter()
        .map(|l| l.text.trim().to_string())
        .collect();
    texts.sort();
    assert_eq!(
        texts,
        ["dao.insert(u)", "fun insert(u: User)"],
        "not cache.insert"
    );
    // ⌘-hover counts the same way, and from a use it knows which insert is meant
    open(&mut h, &saver);
    let at = tab(&h).text.find("cache.insert").unwrap() + 6;
    for _ in 0..500 {
        if h.state_mut().usages_of("insert", at).is_some() {
            break;
        }
        steps(&mut h, 1);
        std::thread::sleep(Duration::from_millis(10));
    }
    let found: Vec<String> = h
        .state_mut()
        .usages_of("insert", at)
        .unwrap()
        .iter()
        .map(|l| l.text.trim().to_string())
        .collect();
    let mut found = found;
    found.sort();
    assert_eq!(found, ["cache.insert(\"k\")", "fun insert(k: String) {}"]);
    // rename without kotlin-lsp: the other class's insert is listed, but unticked
    open(&mut h, &f.path("app/src/main/java/com/demo/data/Cache.kt"));
    place_cursor(&mut h, "insert", 0);
    key(&mut h, Modifiers::COMMAND | Modifiers::ALT, Key::R);
    for _ in 0.."insert".len() {
        key(&mut h, Modifiers::NONE, Key::Backspace);
    }
    type_text(&mut h, "put");
    key(&mut h, Modifiers::NONE, Key::Enter);
    wait(&mut h, "rename preview", |a| {
        matches!(a.refactor, Some(refactor_ui::Refactor::Preview(_)))
    });
    let Some(refactor_ui::Refactor::Preview(p)) = &h.state().refactor else {
        unreachable!()
    };
    let mut lines = p.lines();
    lines.sort();
    assert_eq!(
        lines,
        [
            ("cache.[put](\"k\")".to_string(), true),
            ("dao.[put](u)".to_string(), false),
            ("fun [put](k: String) {}".to_string(), true),
            ("fun [put](u: User)".to_string(), false),
        ]
    );
    // an unsaved use counts too: the count reads open tabs, not the files on disk
    h.state_mut().refactor = None;
    open(&mut h, &saver);
    place_cursor(&mut h, "cache.insert", 0);
    type_text(&mut h, "cache.insert(\"j\"); ");
    h.state_mut().settle(true);
    let at = tab(&h).text.find("cache.insert").unwrap() + 6;
    for _ in 0..500 {
        if h.state_mut().usages_of("insert", at).is_some() {
            break;
        }
        steps(&mut h, 1);
        std::thread::sleep(Duration::from_millis(10));
    }
    let found = h.state_mut().usages_of("insert", at).unwrap().len();
    assert_eq!(found, 3, "the unsaved cache.insert(\"j\") is counted");
}

/// UX audit (not a check): renders every main screen and state at three widths in both themes into
/// `DROID_LITE_AUDIT_DIR`. `DROID_LITE_AUDIT_PROJECT` adds a real project (opened read-only).
/// Run: DROID_LITE_AUDIT_DIR=/tmp/a cargo test --release e2e_ux_audit -- --ignored --test-threads=1
#[test]
#[ignore]
fn e2e_ux_audit() {
    let Ok(dir) = std::env::var("DROID_LITE_AUDIT_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    let f = Fixture::new("audit");
    let mut projects: Vec<(&str, Option<PathBuf>, Option<PathBuf>)> = vec![
        ("empty", None, None),
        (
            "small",
            Some(f.root.clone()),
            Some(f.path("app/src/main/java/com/demo/ui/UserCard.kt")),
        ),
    ];
    if let Ok(p) = std::env::var("DROID_LITE_AUDIT_PROJECT") {
        let file = std::env::var("DROID_LITE_AUDIT_FILE")
            .ok()
            .map(PathBuf::from);
        projects.push(("real", Some(PathBuf::from(p)), file));
    }
    for (name, root, file) in projects {
        for width in [1280.0, 1600.0, 1920.0] {
            for light in [false, true] {
                let r = root.clone();
                let mut h = Harness::builder()
                    .with_size([width, 900.0])
                    .build_eframe(move |cc| App::new(cc, r));
                h.state_mut().settings.light = light;
                let ctx = h.ctx.clone();
                h.state().settings.apply(&ctx);
                if root.is_some() {
                    wait(&mut h, "index", |a| {
                        a.indexing.is_none() && a.index.read().symbol_count() > 0
                    });
                }
                if let Some(file) = &file {
                    open(&mut h, file);
                }
                steps(&mut h, 8);
                let theme = if light { "light" } else { "dark" };
                let shot = |h: &mut Harness<'static, App>, state: &str| {
                    steps(h, 4);
                    let img = h.render().expect("render needs the wgpu feature");
                    img.save(dir.join(format!("{name}-{}-{theme}-{state}.png", width as u32)))
                        .unwrap();
                };
                shot(&mut h, "1-main");
                if root.is_none() {
                    continue;
                }
                h.state_mut().right = Right::Changes;
                h.state_mut().show_right = true;
                shot(&mut h, "2-current-changes");
                h.state_mut().right = Right::Compose;
                for (i, b) in [
                    Bottom::Build,
                    Bottom::Find,
                    Bottom::Logcat,
                    Bottom::Tests,
                    Bottom::Performance,
                ]
                .into_iter()
                .enumerate()
                {
                    h.state_mut().bottom = b;
                    h.state_mut().show_bottom = true;
                    shot(&mut h, &format!("3{}-bottom-{b:?}", i + 1).to_lowercase());
                }
                h.state_mut().bottom = Bottom::Problems;
                if width as u32 == 1600 {
                    h.get_by_label("Help").click();
                    shot(&mut h, "4-help-menu");
                    key(&mut h, Modifiers::NONE, Key::Escape);
                    key(&mut h, Modifiers::COMMAND, Key::O);
                    type_text(&mut h, "Card");
                    shot(&mut h, "5-search");
                    key(&mut h, Modifiers::NONE, Key::Escape);
                }
            }
        }
    }
}

#[test]
fn e2e_toolbar_items_never_overlap_at_any_width() {
    let f = Fixture::new("toolbar");
    for width in [1024.0, 1280.0, 1600.0, 1920.0] {
        let root = f.root.clone();
        let mut h = Harness::builder()
            .with_size([width, 900.0])
            .build_eframe(move |cc| App::new(cc, Some(root)));
        wait(&mut h, "index", |a| {
            a.indexing.is_none() && a.index.read().symbol_count() > 0
        });
        h.state_mut().branch = Some(
            "narayansoni/gro-13003-android-remove-india-p2p-screens-and-rename-shared-p2p".into(),
        );
        steps(&mut h, 6); // the layout uses the widths of the last frame
        let mut rects = Vec::new();
        for label in [
            "Refactor",
            "Help",
            "»",
            "Search",
            "Gradle task",
            "Run task",
            "Run device",
            "App",
        ] {
            if let Some(n) = h.query_by_label(label) {
                rects.push((label, n.rect()));
            }
        }
        assert!(
            rects.iter().any(|(l, _)| *l == "Refactor") || rects.iter().any(|(l, _)| *l == "»"),
            "{width}: the menus are shown, in the bar or in »"
        );
        for (i, (a, ra)) in rects.iter().enumerate() {
            assert!(
                ra.right() <= width + 0.5,
                "{width}: {a} goes past the window edge ({ra:?})"
            );
            for (b, rb) in &rects[i + 1..] {
                assert!(
                    !ra.intersects(*rb),
                    "{width}: {a} {ra:?} overlaps {b} {rb:?}"
                );
            }
        }
        // where it does not fit, the long branch name is shortened instead of pushing the rest
        if width <= 1600.0 {
            assert!(
                h.query_by_label_contains("rename-shared-p2p").is_none(),
                "{width}: the branch name is elided"
            );
        }
    }
}

/// Real preview check (not run by default): opens `DROID_LITE_SHOT_PROJECT`, the Kotlin file
/// `DROID_LITE_SHOT_FILE`, presses Render previews with the real worker and base pack, and saves the
/// window to `DROID_LITE_SHOT_OUT` once the cards are there (up to 3 min). Headless, no window.
#[test]
#[ignore]
fn e2e_real_preview_shot() {
    let (Ok(project), Ok(file), Ok(out)) = (
        std::env::var("DROID_LITE_SHOT_PROJECT"),
        std::env::var("DROID_LITE_SHOT_FILE"),
        std::env::var("DROID_LITE_SHOT_OUT"),
    ) else {
        return;
    };
    let _f = Fixture::new("real-preview"); // the lock and a throwaway config dir
    // SAFETY: the SERIAL lock is held
    unsafe {
        std::env::remove_var("DROID_LITE_PREVIEW_WORKER");
        std::env::remove_var("DROID_LITE_PREVIEW_DIR");
        std::env::remove_var("DROID_LITE_MAC_GUARD");
        std::env::remove_var("DROID_LITE_GRADLE_HOME"); // the real Gradle cache, read only
    }
    let root = PathBuf::from(project);
    let mut h = Harness::builder()
        .with_size([1600.0, 1000.0])
        .build_eframe(move |cc| App::new(cc, Some(root)));
    wait(&mut h, "index", |a| {
        a.indexing.is_none() && a.index.read().symbol_count() > 0
    });
    open(&mut h, Path::new(&file));
    steps(&mut h, 5);
    let started = std::time::Instant::now(); // drawn by itself once the file is open
    let loading_out = std::env::var("DROID_LITE_SHOT_LOADING_OUT").ok();
    let mut loading_saved = false;
    while started.elapsed() < Duration::from_secs(180) {
        h.step();
        // DROID_LITE_SHOT_LOADING_OUT: the window while the loading cards show (1.5 s in)
        if let Some(o) = &loading_out
            && !loading_saved
            && h.state().previews.busy
            && started.elapsed() > Duration::from_millis(1500)
        {
            h.render().expect("render").save(o).unwrap();
            loading_saved = true;
        }
        if !h.state().previews.busy && !h.state().previews.cards.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let p = &h.state().previews;
    eprintln!(
        "preview: {:.1} s, note {:?}, cards {:?}",
        started.elapsed().as_secs_f32(),
        p.note,
        p.cards
            .iter()
            .map(|(n, c)| (n.clone(), c.as_ref().err().cloned()))
            .collect::<Vec<_>>()
    );
    steps(&mut h, 4);
    h.render()
        .expect("render needs the wgpu feature")
        .save(&out)
        .unwrap();
}

#[test]
fn e2e_tab_close_button_closes_the_tab() {
    let f = Fixture::new("tabclose");
    let mut h = f.harness();
    open(&mut h, &f.path("app/src/main/java/com/demo/data/User.kt"));
    open(&mut h, &f.path("app/src/main/java/com/demo/ui/UserCard.kt"));
    assert_eq!(h.state().tabs.len(), 2);
    // the × of the active tab (UserCard.kt): the tab closes, the other stays
    let xs: Vec<_> = h.query_all_by_label(ic::X).collect();
    assert_eq!(xs.len(), 2, "one × per tab");
    xs[1].click();
    steps(&mut h, 3);
    assert_eq!(h.state().tabs.len(), 1, "the × closed its tab");
    assert!(h.state().tabs[0].path.ends_with("User.kt"));
    // ⌘W closes the active one too
    key(&mut h, Modifiers::COMMAND, Key::W);
    assert!(h.state().tabs.is_empty(), "⌘W closed the last tab");
}

#[test]
fn e2e_tab_of_a_deleted_file_closes() {
    let f = Fixture::new("tabgone");
    let mut h = f.harness();
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    let card = f.path("app/src/main/java/com/demo/ui/UserCard.kt");
    open(&mut h, &user);
    open(&mut h, &card);
    // UserCard.kt has an edit that was never saved, User.kt has none
    place_cursor(&mut h, "fun ", 0);
    type_text(&mut h, "// edit\n");
    std::fs::remove_file(&user).unwrap();
    std::fs::remove_file(&card).unwrap();
    wait(&mut h, "both tabs marked deleted", |a| {
        a.tabs.iter().filter(|t| t.deleted).count() == 2
    });
    let typed = tab(&h).text.clone();
    // the × of a deleted file's tab closes it, edited or not
    for _ in 0..2 {
        let xs: Vec<_> = h.query_all_by_label(ic::X).collect();
        xs[0].click();
        steps(&mut h, 3);
    }
    assert!(h.state().tabs.is_empty(), "{:?}", h.state().status);
    // the edits stay in local history
    assert_eq!(newest_version(&card), Some(typed));
    assert_eq!(
        newest_version(&user),
        None,
        "a tab without edits keeps nothing"
    );
}

#[test]
fn e2e_sidebar_pull_requests_open_a_file_diff_beside_the_editor() {
    let f = Fixture::new("prsidebar");
    let mut h = f.harness();
    // gh needs GitHub: the lists and the PR's files come in as gh would send them
    let item = |n, t: &str| crate::git::PrItem {
        number: n,
        title: t.into(),
        author: "ana".into(),
        draft: false,
        url: String::new(),
        review: if n == 7 {
            "APPROVED".into()
        } else {
            String::new()
        },
        approvals: u32::from(n == 7) * 2,
        head: String::new(),
        head_oid: String::new(),
    };
    {
        let s = h.state_mut();
        s.prs
            .lists
            .insert(crate::git::PrFilter::Mine, Ok(vec![item(7, "Fix login")]));
        s.prs
            .lists
            .insert(crate::git::PrFilter::Review, Ok(vec![item(9, "Add cards")]));
    }
    h.get_by_label(&format!("{}  Pull requests", ic::GIT_PULL_REQUEST))
        .click();
    steps(&mut h, 3);
    assert!(h.query_by_label("#7 Fix login").is_some(), "your PRs first");
    // an approved PR shows it at a glance, and the Approved tab lists only those
    assert!(
        h.query_by_label("Approved by 2").is_some(),
        "approvals counted"
    );
    h.state_mut().prs.lists.insert(
        crate::git::PrFilter::Approved,
        Ok(vec![item(7, "Fix login")]),
    );
    h.get_by_label("Approved").click();
    steps(&mut h, 3);
    assert!(
        h.query_by_label("#7 Fix login").is_some(),
        "the approved filter"
    );
    h.get_by_label("Awaiting your review").click();
    steps(&mut h, 3);
    assert!(h.query_by_label("#7 Fix login").is_none());
    assert!(
        h.query_by_label("#9 Add cards").is_some(),
        "the review filter"
    );
    // an opened PR lists its files; a file's diff opens in the side panel, side by side
    let diff = "diff --git a/A.kt b/A.kt\n@@ -1 +1 @@\n-val a = 1\n+val a = 2\n";
    h.state_mut().prs.open = Some((9, Some(Ok(vec![("app/A.kt".into(), diff.into())]))));
    steps(&mut h, 3);
    h.get_by_label("A.kt").click();
    wait(&mut h, "diff panel", |a| {
        a.commit_lines()
            .is_some_and(|l| l.iter().any(|l| l == "+val a = 2"))
    });
    assert!(
        h.query_by_label("Side by side").is_some(),
        "docked panel, not a window"
    );
    key(&mut h, Modifiers::NONE, Key::Escape);
    assert!(h.state().commit_lines().is_none(), "Esc closes the diff");
    // back to the Project tree
    h.get_by_label(&format!("{}  Project", ic::TREE_STRUCTURE))
        .click();
    steps(&mut h, 2);
    assert!(!h.state().prs.shown);
}

/// Clicks the Check out button of the PR titled `title` (on the line below the title).
fn click_check_out(h: &mut Harness<'static, App>, title: &str) {
    steps(h, 2); // the layout settles first (a note above the list moves the rows)
    let y = h.get_by_label(title).rect().center().y;
    let pos = h
        .get_all_by_label("Check out")
        .map(|n| n.rect().center())
        .filter(|p| p.y > y)
        .min_by(|a, b| a.y.total_cmp(&b.y))
        .expect("a Check out button below the title");
    click_at(h, pos, egui::PointerButton::Primary);
}

#[test]
fn e2e_check_out_a_pull_request_from_the_sidebar() {
    let f = Fixture::new("prcheckout");
    let git = |args: &[&str]| crate::git::run(&f.root, args).unwrap();
    // the PR's branch: User.kt differs there
    let start = crate::git::branch(&f.root).unwrap();
    git(&["checkout", "-q", "-b", "pr-7"]);
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    std::fs::write(&user, USER_KT.replace("var age", "val age")).unwrap();
    git(&["commit", "-q", "-am", "PR 7"]);
    git(&["checkout", "-q", &start]);
    // a fake gh: lists PR 7 (branch pr-7), 8, and 9 from a fork whose branch has the name of
    // the current one but another commit; checks out 7, fails on 8 after making a branch
    let bin = std::env::temp_dir().join(format!("droid-lite-fake-gh-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&bin);
    std::fs::create_dir_all(&bin).unwrap();
    let gh = bin.join("gh");
    let list = r#"[{"number":7,"title":"Fix login","headRefName":"pr-7","headRefOid":"'"$(git rev-parse pr-7)"'"},{"number":8,"title":"Add cards","headRefName":"pr-8","headRefOid":"1111111111111111111111111111111111111111"},{"number":9,"title":"Fork main","headRefName":"START","headRefOid":"2222222222222222222222222222222222222222"}]"#
        .replace("START", &start);
    std::fs::write(
        &gh,
        format!("#!/bin/sh\necho \"$*\" >> \"$0.log\"\ncase \"$1 $2 $3\" in\n  'pr list '*) echo '{list}';;\n  'pr checkout 7') git checkout -q pr-7;;\n  'pr checkout 8') git checkout -q -b pr-8-half; echo 'From github.com:demo/demo' >&2; echo 'GraphQL: Could not resolve to a PullRequest with the number of 8.' >&2; exit 1;;\n  *) exit 2;;\nesac\n"),
    )
    .unwrap();
    std::process::Command::new("chmod")
        .arg("+x")
        .arg(&gh)
        .status()
        .unwrap();
    // SAFETY: the SERIAL lock (held by the fixture) keeps other tests out of the environment.
    unsafe { std::env::set_var("DROID_LITE_GH", &gh) };
    let mut h = f.harness();
    open(&mut h, &user);
    let checkout_done = |a: &App| {
        a.prs
            .note
            .as_ref()
            .is_some_and(|(t, _)| !t.starts_with("Checking"))
    };
    // the renamed right-panel tab
    assert!(
        h.query_by_label(&format!("{}  Pull request", ic::GIT_PULL_REQUEST))
            .is_none()
    );
    h.get_by_label(&format!("{}  Current changes", ic::GIT_PULL_REQUEST))
        .click();
    steps(&mut h, 3);
    assert_eq!(h.state().right, Right::Changes);
    h.get_by_label(&format!("{}  Pull requests", ic::GIT_PULL_REQUEST))
        .click();
    wait(
        &mut h,
        "the PR list from gh",
        |a| matches!(a.prs.lists.get(&crate::git::PrFilter::Mine), Some(Ok(l)) if l.len() == 3),
    );
    steps(&mut h, 3);
    assert!(h.query_by_label("#7 Fix login").is_some());
    assert!(
        h.query_by_label("● current").is_none(),
        "pr-7 is not checked out yet, and the fork's {start} is another commit"
    );
    let refused = |h: &mut Harness<'static, App>, title: &str| {
        h.state_mut().prs.note = None;
        click_check_out(h, title);
        steps(h, 2);
        let note = h.state().prs.note.clone();
        let (text, err) = note.expect("a note");
        assert!(err, "{text}");
        text
    };
    // unsaved edits in an editor tab: they would follow onto the other branch
    h.state_mut().tabs[0].text.push_str("// unsaved\n");
    assert_eq!(
        refused(&mut h, "#7 Fix login"),
        "Save or discard your unsaved edits first:\napp/src/main/java/com/demo/data/User.kt"
    );
    let saved = tab(&h).saved.clone();
    h.state_mut().tabs[0].text = saved;
    // a build, a test run or a preview draw of this project still running
    let ctx = h.ctx.clone();
    let sleeper = || {
        let mut c = std::process::Command::new("sleep");
        c.arg("30");
        crate::gradle::Run::spawn(c, ctx.clone()).unwrap()
    };
    h.state_mut().gradle = Some(sleeper());
    assert_eq!(
        refused(&mut h, "#7 Fix login"),
        "Wait for the Gradle build to finish"
    );
    h.state_mut().gradle = None;
    let mut plugin = super::tests_ui::Plugin::default();
    plugin.run = Some((String::new(), "a case".into(), sleeper()));
    h.state_mut().tests.plugins.push(plugin);
    assert_eq!(
        refused(&mut h, "#7 Fix login"),
        "Wait for the test run to finish"
    );
    if let Some((_, _, r)) = h.state_mut().tests.plugins[0].run.as_mut() {
        r.stop();
    }
    h.state_mut().tests.plugins.clear();
    h.state_mut().previews.busy = true;
    assert_eq!(
        refused(&mut h, "#7 Fix login"),
        "Wait for the preview draw to finish"
    );
    h.state_mut().previews.busy = false;
    h.state_mut().prs.note = None;
    // uncommitted work in a tracked file: refused, the branch stays, the file is listed
    let card = f.path("app/src/main/java/com/demo/ui/UserCard.kt");
    std::fs::write(&card, format!("{CARD_KT}// mine\n")).unwrap();
    click_check_out(&mut h, "#7 Fix login");
    wait(&mut h, "refusal", |a| {
        a.prs.note.as_ref().is_some_and(|(_, err)| *err)
    });
    let (note, _) = h.state().prs.note.clone().unwrap();
    assert_eq!(
        note,
        "Commit or stash your changes first:\napp/src/main/java/com/demo/ui/UserCard.kt"
    );
    steps(&mut h, 2);
    assert!(
        h.query_by_label(&note).is_some(),
        "the refusal is on screen"
    );
    assert_eq!(crate::git::branch(&f.root).as_deref(), Some(start.as_str()));
    assert!(tab(&h).text.contains("var age"));
    // a clean tree: gh checks out, the branch, the open file and the list follow, also while
    // the sidebar shows the Project tree (the reply is read every frame)
    git(&["checkout", "-q", "--", "."]);
    h.state_mut().prs.note = None;
    click_check_out(&mut h, "#7 Fix login");
    h.get_by_label(&format!("{}  Project", ic::TREE_STRUCTURE))
        .click();
    wait(&mut h, "checked out", checkout_done);
    assert!(!h.state().prs.shown);
    assert_eq!(
        h.state().prs.note,
        Some(("Checked out #7 (pr-7)".to_string(), false))
    );
    assert_eq!(h.state().branch.as_deref(), Some("pr-7"));
    assert_eq!(h.state().status, "Checked out #7 (pr-7)");
    wait(&mut h, "the open file reloads from disk", |a| {
        a.tabs[a.active].text.contains("val age")
    });
    h.get_by_label(&format!("{}  Pull requests", ic::GIT_PULL_REQUEST))
        .click();
    wait(&mut h, "the list reloads", |a| {
        a.prs.lists.contains_key(&crate::git::PrFilter::Mine)
    });
    steps(&mut h, 3);
    let calls = std::fs::read_to_string(bin.join("gh.log")).unwrap();
    assert_eq!(
        calls.lines().filter(|l| l.contains("--author @me")).count(),
        2,
        "the list reloads after the checkout: {calls}"
    );
    // the list asks gh for each PR's branch, which the current marker compares with
    assert!(
        calls
            .lines()
            .filter(|l| l.contains("--author @me"))
            .all(|l| l.contains("headRefName") && l.contains("headRefOid")),
        "{calls}"
    );
    assert_eq!(
        h.get_all_by_label("● current").count(),
        1,
        "the checked-out PR is marked, by its commit"
    );
    let below = |h: &Harness<'static, App>, a: &str, b: &str| {
        h.get_by_label(b).rect().center().y > h.get_by_label(a).rect().center().y
    };
    assert!(below(&h, "#7 Fix login", "● current") && below(&h, "● current", "#8 Add cards"));
    // gh fails after it made a branch: its error line, not git's progress; the branch name and
    // the Current changes lists follow anyway
    h.state_mut().git.changes = Some(Default::default());
    h.state_mut().prs.note = None;
    click_check_out(&mut h, "#8 Add cards");
    wait(&mut h, "failure", checkout_done);
    assert_eq!(
        h.state().prs.note,
        Some((
            "GraphQL: Could not resolve to a PullRequest with the number of 8.".to_string(),
            true
        ))
    );
    assert_eq!(h.state().branch.as_deref(), Some("pr-8-half"));
    wait(&mut h, "Current changes reload", |a| {
        a.git
            .changes
            .as_ref()
            .is_some_and(|c| c.branches.iter().any(|b| b == "pr-8-half"))
    });
    let _ = std::fs::remove_dir_all(&bin);
}

#[test]
fn e2e_pull_requests_of_one_project_never_show_in_another() {
    let f = Fixture::new("prswitch");
    let a_branch = crate::git::branch(&f.root);
    // project B: another repository
    let b = std::env::temp_dir().join(format!("droid-lite-e2e-prswitch-b-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&b);
    std::fs::create_dir_all(b.join("app/src/main/java/b")).unwrap();
    std::fs::write(b.join("settings.gradle.kts"), "include(\":app\")\n").unwrap();
    std::fs::write(b.join("app/src/main/java/b/B.kt"), "package b\nclass B\n").unwrap();
    let b = b.canonicalize().unwrap();
    let gb = |args: &[&str]| crate::git::run(&b, args).unwrap();
    gb(&["init", "-q", "-b", "trunk"]);
    gb(&["config", "user.name", "T"]);
    gb(&["config", "user.email", "t@example.com"]);
    gb(&["add", "."]);
    gb(&["commit", "-q", "-m", "B"]);
    // a fake gh: each project its own list; a checkout that waits and ignores SIGTERM, with a
    // child in its group
    let bin = std::env::temp_dir().join(format!("droid-lite-fake-gh-sw-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&bin);
    std::fs::create_dir_all(&bin).unwrap();
    let gh = bin.join("gh");
    std::fs::write(
        &gh,
        "#!/bin/sh\ncase \"$1 $2 $PWD\" in\n  'pr list '*prswitch-b*) echo '[{\"number\":1,\"title\":\"Only in B\"}]';;\n  'pr list '*) echo '[{\"number\":7,\"title\":\"Only in A\"}]';;\n  'pr checkout '*) trap '' TERM; sleep 30 & echo $! > \"$0.pid\"; wait; git checkout -q -b pr-7;;\nesac\n",
    )
    .unwrap();
    std::process::Command::new("chmod")
        .arg("+x")
        .arg(&gh)
        .status()
        .unwrap();
    // SAFETY: the SERIAL lock (held by the fixture) keeps other tests out of the environment.
    unsafe { std::env::set_var("DROID_LITE_GH", &gh) };
    let mut h = f.harness();
    h.get_by_label(&format!("{}  Pull requests", ic::GIT_PULL_REQUEST))
        .click();
    wait(&mut h, "A's list", |a| {
        a.prs.lists.contains_key(&crate::git::PrFilter::Mine)
    });
    // A's checkout is still running when B opens: it is stopped (SIGTERM, then SIGKILL after
    // the grace time, as this gh ignores SIGTERM)
    click_check_out(&mut h, "#7 Only in A");
    assert!(h.state().prs.note.is_some());
    let pid_file = bin.join("gh.pid");
    // the shell creates the file before it writes the pid
    let pid = || {
        std::fs::read_to_string(&pid_file)
            .ok()
            .and_then(|t| t.trim().parse::<i32>().ok())
    };
    wait(&mut h, "gh runs", |_| pid().is_some());
    let child = pid().unwrap();
    let ctx = h.ctx.clone();
    h.state_mut().open_project(&b, &ctx);
    wait(&mut h, "B open", |a| {
        a.indexing.is_none() && a.project.as_ref().is_some_and(|p| p.root == b)
    });
    assert_eq!(h.state().branch.as_deref(), Some("trunk"));
    assert!(h.state().prs.note.is_none(), "A's checkout note is gone");
    assert!(
        h.state().prs.shown,
        "the sidebar still shows the pull requests"
    );
    assert_eq!(
        h.state().prs.stopping_count(),
        1,
        "A's checkout is being stopped"
    );
    assert!(
        !h.state().checkout_busy(),
        "nothing in B waits for A's checkout"
    );
    wait(&mut h, "B's list", |a| {
        matches!(
            a.prs.lists.get(&crate::git::PrFilter::Mine),
            Some(Ok(l)) if l.len() == 1 && l[0].number == 1
        )
    });
    steps(&mut h, 3);
    assert!(h.query_by_label("#1 Only in B").is_some());
    assert!(h.query_by_label("#7 Only in A").is_none());
    // a reply that names A's root changes nothing in B, even if it got through
    h.state_mut().checked_out(super::prs_ui::Checkout {
        root: f.root.clone(),
        number: 7,
        result: Ok(("pr-7".into(), false)),
        branch: Some("pr-7".into()),
        head: None,
    });
    assert_eq!(h.state().branch.as_deref(), Some("trunk"));
    assert!(h.state().prs.note.is_none());
    assert!(
        h.state()
            .prs
            .lists
            .contains_key(&crate::git::PrFilter::Mine)
    );
    // A's checkout ends: nothing of it reaches B, and A never switched
    wait(&mut h, "A's checkout stopped", |a| {
        a.prs.stopping_count() == 0
    });
    steps(&mut h, 5);
    assert_eq!(h.state().branch.as_deref(), Some("trunk"));
    assert!(h.state().prs.note.is_none());
    assert_eq!(crate::git::branch(&f.root), a_branch, "A did not switch");
    // SAFETY: signal 0 only checks whether the process lives (a killed child is gone once
    // launchd has reaped it)
    let gone = || unsafe { libc::kill(child, 0) } != 0;
    let until = std::time::Instant::now() + Duration::from_secs(1);
    while !gone() && std::time::Instant::now() < until {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(gone(), "gh's child was killed too");
    // back in A while an old checkout of A is still being stopped: a new one waits for it
    let open = |h: &mut Harness<'static, App>, root: &Path| {
        let ctx = h.ctx.clone();
        h.state_mut().open_project(root, &ctx);
        wait(h, "project open", |a| {
            a.indexing.is_none() && a.project.as_ref().is_some_and(|p| p.root == root)
        });
    };
    let _ = std::fs::remove_file(&pid_file);
    open(&mut h, &f.root);
    wait(&mut h, "A's list", |a| {
        a.prs.lists.contains_key(&crate::git::PrFilter::Mine)
    });
    click_check_out(&mut h, "#7 Only in A");
    wait(&mut h, "gh runs", |_| pid().is_some());
    open(&mut h, &b);
    open(&mut h, &f.root);
    assert!(h.state().checkout_busy(), "A's files may still change");
    wait(&mut h, "A's list", |a| {
        a.prs.lists.contains_key(&crate::git::PrFilter::Mine)
    });
    click_check_out(&mut h, "#7 Only in A");
    steps(&mut h, 2);
    assert_eq!(
        h.state().prs.note,
        Some(("Wait for the check out of #7 to stop".to_string(), true))
    );
    // its end shows in A, the project it belongs to
    wait(&mut h, "the old checkout stopped", |a| !a.checkout_busy());
    assert_eq!(
        h.state().prs.note,
        Some(("Check out of #7 was stopped".to_string(), true))
    );
    assert_eq!(crate::git::branch(&f.root), a_branch);
    drop(h);
    let _ = std::fs::remove_dir_all(&bin);
    let _ = std::fs::remove_dir_all(&b);
}

#[test]
fn e2e_autosave_never_overwrites_a_file_changed_on_disk() {
    let f = Fixture::new("autosavedisk");
    let mut h = f.harness();
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    open(&mut h, &user);
    assert!(h.state().settings.autosave);
    watcher_live(&mut h, &user);
    type_text(&mut h, "mine");
    std::fs::write(&user, "package com.demo.data\n\nclass Theirs\n").unwrap();
    wait(&mut h, "conflict bar", |a| a.tabs[a.active].disk_changed);
    // the window loses focus: the other version stays on disk, the edits stay in the editor
    h.input_mut().focused = true;
    steps(&mut h, 2);
    h.input_mut().focused = false;
    steps(&mut h, 3);
    let theirs = "package com.demo.data\n\nclass Theirs\n";
    assert_eq!(std::fs::read_to_string(&user).unwrap(), theirs);
    assert!(tab(&h).dirty() && tab(&h).text.contains("mine"));
    assert!(tab(&h).disk_changed, "the bar still asks");
    // nor on quit; the edits go to local history instead, so a forced quit loses nothing
    eframe::App::on_exit(h.state_mut());
    assert_eq!(std::fs::read_to_string(&user).unwrap(), theirs);
    let kept = super::comfort::history(&user);
    assert_eq!(
        kept.first()
            .map(|v| std::fs::read_to_string(&v.file).unwrap()),
        Some(tab(&h).text.clone()),
        "the unsaved edits are in local history"
    );
    // Keep mine: from then on auto-save writes the edits
    h.input_mut().focused = true;
    steps(&mut h, 2);
    h.get_by_label("Keep mine").click();
    steps(&mut h, 2);
    h.input_mut().focused = false;
    steps(&mut h, 3);
    h.state_mut().wait_saves();
    assert!(std::fs::read_to_string(&user).unwrap().contains("mine"));
}

/// Puts the keyboard focus in the active editor, as a click in it does.
fn focus_editor(h: &mut Harness<'static, App>) {
    let id = egui::Id::unique(("editor", &tab(h).path));
    h.ctx.memory_mut(|m| m.request_focus(id));
    steps(h, 2);
    assert!(
        h.ctx.memory(|m| m.has_focus(id)),
        "the editor has the focus"
    );
}

/// Asks the window to close, as its close button does; whether the app cancelled it.
fn ask_to_close(h: &mut Harness<'static, App>) -> bool {
    h.input_mut()
        .viewports
        .entry(egui::ViewportId::ROOT)
        .or_default()
        .events
        .push(egui::ViewportEvent::Close);
    h.step();
    h.output()
        .viewport_output
        .get(&egui::ViewportId::ROOT)
        .is_some_and(|v| {
            v.commands
                .iter()
                .any(|c| matches!(c, egui::ViewportCommand::CancelClose))
        })
}

#[test]
fn e2e_closing_with_edits_auto_save_will_not_write_asks_first() {
    let f = Fixture::new("closeask");
    let mut h = f.harness();
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    let card = f.path("app/src/main/java/com/demo/ui/UserCard.kt");
    open(&mut h, &user);
    watcher_live(&mut h, &user);
    type_text(&mut h, "mine");
    assert!(tab(&h).dirty());
    // edits auto-save writes: the window waits for that save without blocking, then closes
    assert!(ask_to_close(&mut h), "the close waits for the save");
    wait(&mut h, "closed once saved", |a| a.close_ok);
    assert!(std::fs::read_to_string(&user).unwrap().contains("mine"));
    // (a test window stays open) typing goes on
    h.state_mut().close_ok = false;
    type_text(&mut h, "more");
    assert!(tab(&h).dirty());
    // the file changed on disk meanwhile: the close waits for a choice
    let theirs = "package com.demo.data\n\nclass Theirs\n";
    std::fs::write(&user, theirs).unwrap();
    wait(&mut h, "conflict bar", |a| a.tabs[a.active].disk_changed);
    assert!(ask_to_close(&mut h), "the close is cancelled");
    steps(&mut h, 2);
    let list =
        "These files have edits that are not saved:\napp/src/main/java/com/demo/data/User.kt";
    assert!(h.query_by_label(list).is_some(), "the files are listed");
    h.get_by_label("Cancel").click();
    steps(&mut h, 3);
    assert!(h.query_by_label(list).is_none());
    assert!(!h.state().close_ok);
    assert_eq!(std::fs::read_to_string(&user).unwrap(), theirs);
    // Keep a copy and close: the edits go to local history, the file on disk stays
    assert!(ask_to_close(&mut h));
    steps(&mut h, 2);
    h.get_by_label("Keep a copy and close").click();
    steps(&mut h, 3);
    assert!(h.state().close_ok && !h.state().close_asked);
    let newest = |p: &Path| {
        super::comfort::history(p)
            .first()
            .map(|v| std::fs::read_to_string(&v.file).unwrap())
    };
    assert_eq!(newest(&user), Some(tab(&h).text.clone()));
    assert_eq!(std::fs::read_to_string(&user).unwrap(), theirs);
    assert!(!ask_to_close(&mut h), "then the close goes through");
    // Keep my version (save): written, then closed
    h.state_mut().close_ok = false;
    assert!(ask_to_close(&mut h));
    steps(&mut h, 2);
    h.get_by_label("Keep my version (save)").click();
    steps(&mut h, 3);
    h.state_mut().wait_saves();
    steps(&mut h, 2); // closes once written
    assert!(std::fs::read_to_string(&user).unwrap().contains("mine"));
    assert!(h.state().close_ok && !h.state().close_asked);
    // a file deleted on disk while it has edits asks too
    h.state_mut().close_ok = false;
    open(&mut h, &card);
    watcher_live(&mut h, &card);
    type_text(&mut h, "x");
    std::fs::remove_file(&card).unwrap();
    wait(&mut h, "deleted", |a| a.tabs[a.active].deleted);
    assert!(ask_to_close(&mut h));
    steps(&mut h, 2);
    assert!(
        h.query_by_label(
            "These files have edits that are not saved:\napp/src/main/java/com/demo/ui/UserCard.kt"
        )
        .is_some()
    );
    // auto-save off: every edit counts
    h.get_by_label("Keep a copy and close").click();
    steps(&mut h, 3);
    assert_eq!(newest(&card), Some(tab(&h).text.clone()));
    h.state_mut().close_ok = false;
    h.state_mut().settings.autosave = false;
    let active = h.state().active;
    h.state_mut().close_tab(active); // a deleted file's tab closes even with edits
    assert_eq!(tab(&h).path, user);
    let a = h.state().active;
    h.state_mut().tabs[a].text.push_str("// y\n");
    assert!(ask_to_close(&mut h), "with auto-save off, any edit asks");
}

/// The newest local history version of `path`.
fn newest_version(path: &Path) -> Option<String> {
    super::comfort::history(path)
        .first()
        .map(|v| std::fs::read_to_string(&v.file).unwrap())
}

/// A folder dropped on the window.
#[derive(Debug)]
struct DroppedFolder(PathBuf);

impl egui::DroppedFile for DroppedFolder {
    fn path(&self) -> &Path {
        &self.0
    }

    fn bytes(&self) -> Result<Vec<u8>, String> {
        Err("a folder".into())
    }
}

#[test]
fn e2e_switching_project_with_unsaved_edits_asks_first() {
    let f = Fixture::new("switchask");
    // project B: another folder
    let b = std::env::temp_dir().join(format!("droid-lite-e2e-switch-b-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&b);
    std::fs::create_dir_all(b.join("app/src/main/java/b")).unwrap();
    std::fs::write(b.join("settings.gradle.kts"), "include(\":app\")\n").unwrap();
    std::fs::write(b.join("app/src/main/java/b/B.kt"), "package b\nclass B\n").unwrap();
    let b = b.canonicalize().unwrap();
    let mut h = f.harness();
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    let switch = |h: &mut Harness<'static, App>, root: &Path| {
        let ctx = h.ctx.clone();
        h.state_mut().open_project(root, &ctx);
        steps(h, 2);
    };
    let opened = |h: &mut Harness<'static, App>, root: &Path| {
        wait(h, "project open", |a| {
            a.indexing.is_none() && a.project.as_ref().is_some_and(|p| p.root == root)
        });
    };
    let edit = |h: &mut Harness<'static, App>, s: &str| {
        open(h, &user);
        let a = h.state().active;
        h.state_mut().tabs[a].text.push_str(s);
    };
    // edits auto-save writes: written, then the switch goes through at once
    edit(&mut h, "// one\n");
    switch(&mut h, &b);
    opened(&mut h, &b);
    assert!(
        std::fs::read_to_string(&user)
            .unwrap()
            .ends_with("// one\n")
    );
    switch(&mut h, &f.root);
    opened(&mut h, &f.root);
    // the file changed on disk meanwhile: a folder dropped on the window waits for a choice
    edit(&mut h, "// two\n");
    watcher_live(&mut h, &user);
    let theirs = "package com.demo.data\n\nclass Theirs\n";
    std::fs::write(&user, theirs).unwrap();
    wait(&mut h, "conflict bar", |a| a.tabs[a.active].disk_changed);
    let typed = tab(&h).text.clone();
    let drop_b = |h: &mut Harness<'static, App>| {
        let folder = std::sync::Arc::new(DroppedFolder(b.clone()));
        h.input_mut().dropped_files.push(folder);
        steps(h, 3);
    };
    drop_b(&mut h);
    let list =
        "These files have edits that are not saved:\napp/src/main/java/com/demo/data/User.kt";
    assert!(h.query_by_label(list).is_some(), "the files are listed");
    assert_eq!(h.state().project.as_ref().unwrap().root, f.root);
    // Cancel: the project, the tab and its edits stay; the file on disk is not touched
    h.get_by_label("Cancel").click();
    steps(&mut h, 3);
    assert!(h.query_by_label(list).is_none());
    assert!(h.state().switch_to.is_none());
    assert_eq!(h.state().project.as_ref().unwrap().root, f.root);
    assert_eq!(tab(&h).text, typed);
    assert_eq!(std::fs::read_to_string(&user).unwrap(), theirs);
    // Keep a copy and switch: the edits go to local history, the file on disk stays
    drop_b(&mut h);
    h.get_by_label("Keep a copy and switch").click();
    opened(&mut h, &b);
    assert_eq!(newest_version(&user), Some(typed));
    assert_eq!(std::fs::read_to_string(&user).unwrap(), theirs);
    // auto-save off: Save writes the edits, then the switch goes on
    switch(&mut h, &f.root);
    opened(&mut h, &f.root);
    h.state_mut().settings.autosave = false;
    edit(&mut h, "// three\n");
    switch(&mut h, &b);
    assert!(
        h.query_by_label(list).is_some(),
        "with auto-save off, any edit asks"
    );
    h.get_by_label("Save").click();
    opened(&mut h, &b);
    assert_eq!(
        std::fs::read_to_string(&user).unwrap(),
        format!("{theirs}// three\n")
    );
    // a switch that does not ask (as the dialog's own) still keeps unsaved edits in local history
    switch(&mut h, &f.root);
    opened(&mut h, &f.root);
    edit(&mut h, "// four\n");
    let typed = tab(&h).text.clone();
    let ctx = h.ctx.clone();
    h.state_mut().switch_project(&b, &ctx);
    opened(&mut h, &b);
    assert_eq!(newest_version(&user), Some(typed));
    let _ = std::fs::remove_dir_all(&b);
}

#[test]
fn e2e_a_running_check_out_locks_edits_and_commands_and_quit_stops_it() {
    let f = Fixture::new("cobusy");
    let git = |args: &[&str]| crate::git::run(&f.root, args).unwrap();
    let start = crate::git::branch(&f.root).unwrap();
    // a fake gh: checking out #7 waits for gh.go, then switches to a branch without UserCard.kt
    let bin = std::env::temp_dir().join(format!("droid-lite-fake-gh-busy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&bin);
    std::fs::create_dir_all(&bin).unwrap();
    let gh = bin.join("gh");
    std::fs::write(
        &gh,
        "#!/bin/sh\ncase \"$1 $2 $3\" in\n  'pr list '*) echo '[{\"number\":7,\"title\":\"Fix login\"}]';;\n  'pr checkout 7') echo $$ > \"$0.pid\"; while [ ! -f \"$0.go\" ]; do sleep 0.05; done; git checkout -q -b pr-7 && git rm -q app/src/main/java/com/demo/ui/UserCard.kt && git commit -qm rm;;\n  *) exit 2;;\nesac\n",
    )
    .unwrap();
    std::process::Command::new("chmod")
        .arg("+x")
        .arg(&gh)
        .status()
        .unwrap();
    // SAFETY: the SERIAL lock (held by the fixture) keeps other tests out of the environment.
    unsafe { std::env::set_var("DROID_LITE_GH", &gh) };
    let (pid_file, go) = (bin.join("gh.pid"), bin.join("gh.go"));
    let mut h = f.harness();
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    let card = f.path("app/src/main/java/com/demo/ui/UserCard.kt");
    open(&mut h, &user);
    watcher_live(&mut h, &user);
    h.get_by_label(&format!("{}  Pull requests", ic::GIT_PULL_REQUEST))
        .click();
    wait(&mut h, "the PR list", |a| {
        a.prs.lists.contains_key(&crate::git::PrFilter::Mine)
    });
    click_check_out(&mut h, "#7 Fix login");
    wait(&mut h, "gh runs", |_| pid_file.exists());
    assert!(h.state().checkout_busy());
    let wait_note = super::prs_ui::WAIT_CHECKOUT;
    // typing changes nothing: plain text, nor a bracket pair
    let before = tab(&h).text.clone();
    place_cursor(&mut h, "emptyList()", 11); // a line end, where ( gets its )
    focus_editor(&mut h);
    type_text(&mut h, "z");
    type_text(&mut h, "(");
    assert_eq!(tab(&h).text, before);
    // nor do line commands
    let ctx = h.ctx.clone();
    h.state_mut().run(Action::DuplicateLine, &ctx);
    assert_eq!(tab(&h).text, before);
    assert_eq!(h.state().status, wait_note);
    // an edit made before is neither auto-saved when the window loses focus nor saved
    let a = h.state().active;
    h.state_mut().tabs[a].text.push_str("// later\n");
    h.input_mut().focused = true;
    steps(&mut h, 2);
    h.input_mut().focused = false;
    steps(&mut h, 3);
    assert_eq!(std::fs::read_to_string(&user).unwrap(), USER_KT);
    h.state_mut().status.clear();
    h.state_mut().save();
    h.state_mut().wait_saves();
    assert_eq!(h.state().status, wait_note);
    assert_eq!(std::fs::read_to_string(&user).unwrap(), USER_KT);
    h.state_mut().tabs[a].text = before.clone();
    // builds, test runs and Git commands wait
    h.state_mut().status.clear();
    h.state_mut().run_gradle("assembleDebug".into(), &ctx);
    assert!(h.state().gradle.is_none());
    assert_eq!(h.state().status, wait_note);
    h.state_mut().tests.plugins.push(Default::default());
    h.state_mut().status.clear();
    h.state_mut().run_case(0, "a case", None, &ctx);
    assert_eq!(h.state().status, wait_note);
    h.state_mut().tests.plugins.clear();
    let head = crate::git::head_sha(&f.root);
    h.state_mut().status.clear();
    h.state_mut()
        .git_command(&["commit", "-q", "--allow-empty", "-m", "during"]);
    assert_eq!(h.state().status, wait_note);
    assert_eq!(crate::git::head_sha(&f.root), head);
    // a change bar's revert, a completion, a run on a device and a Current changes revert wait too
    h.state_mut().status.clear();
    let hunk = crate::git::Hunk {
        new_start: 0,
        new_len: 1,
        old: vec!["package other".into()],
    };
    h.state_mut().revert_hunk(&hunk);
    assert_eq!(
        (tab(&h).text.clone(), h.state().status.as_str()),
        (before.clone(), wait_note)
    );
    h.state_mut().status.clear();
    h.state_mut().completion = Some(Completion {
        start: 0,
        items: vec![Item {
            label: "zzz".into(),
            detail: String::new(),
            insert: "zzz".into(),
            import: None,
            template: None,
        }],
        selected: 0,
    });
    h.state_mut().accept_completion();
    assert_eq!(
        (tab(&h).text.clone(), h.state().status.as_str()),
        (before.clone(), wait_note)
    );
    h.state_mut().status.clear();
    h.state_mut()
        .run_on_device(crate::device::Launch::App, &ctx);
    assert_eq!(h.state().status, wait_note, "not: no device");
    h.state_mut().status.clear();
    std::fs::write(&card, format!("{CARD_KT}// mine\n")).unwrap();
    let root = f.root.clone();
    h.state_mut().revert_file(
        &root,
        super::git_ui::Revert::Working {
            path: card.clone(),
            untracked: false,
        },
    );
    assert_eq!(h.state().status, wait_note);
    assert!(
        std::fs::read_to_string(&card)
            .unwrap()
            .ends_with("// mine\n")
    );
    std::fs::write(&card, CARD_KT).unwrap();
    // a file with previews opened now is not drawn
    open(&mut h, &card);
    steps(&mut h, 5);
    assert!(h.state().previews.sent.is_none() && !h.state().previews.busy);
    // the checkout ends: the deleted file's clean tab closes, editing works again
    std::fs::write(&go, "").unwrap();
    wait(&mut h, "checked out", |a| !a.checkout_busy());
    assert_eq!(
        h.state().prs.note,
        Some(("Checked out #7 (pr-7)".to_string(), false))
    );
    assert!(
        h.state().tabs.iter().all(|t| t.path != card),
        "no ⌘S writes it back"
    );
    assert_eq!(tab(&h).path, user);
    focus_editor(&mut h);
    type_text(&mut h, "z");
    assert_ne!(tab(&h).text, before);
    h.state_mut().tabs[a].text = before.clone();
    // HEAD moves outside Droid Lite: "● current" follows with the next refresh
    assert_eq!(h.state().prs.head, crate::git::head_sha(&f.root));
    git(&["commit", "-q", "--allow-empty", "-m", "outside"]);
    h.state_mut().git.changes_stale = true;
    let moved = crate::git::head_sha(&f.root);
    wait(&mut h, "HEAD refreshed", |a| a.prs.head == moved);
    // quit during a checkout: gh stops first; edits go to local history, not over the files
    git(&["checkout", "-q", &start]);
    let _ = std::fs::remove_file(&go);
    let _ = std::fs::remove_file(&pid_file);
    wait(&mut h, "the PR list", |a| {
        a.prs.lists.contains_key(&crate::git::PrFilter::Mine)
    });
    h.state_mut().prs.note = None;
    click_check_out(&mut h, "#7 Fix login");
    wait(&mut h, "gh runs", |_| pid_file.exists());
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    h.state_mut().tabs[a].text.push_str("// quit\n");
    eframe::App::on_exit(h.state_mut());
    // SAFETY: signal 0 only checks whether the process lives
    assert!(unsafe { libc::kill(pid, 0) } != 0, "gh was stopped");
    assert!(!h.state().checkout_busy());
    assert_eq!(std::fs::read_to_string(&user).unwrap(), USER_KT);
    let kept = super::comfort::history(&user);
    assert_eq!(
        kept.first()
            .map(|v| std::fs::read_to_string(&v.file).unwrap()),
        Some(tab(&h).text.clone())
    );
    drop(h);
    let _ = std::fs::remove_dir_all(&bin);
}

fn set_mode(p: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// Another small project next to the fixture, for project switches.
fn other_project(name: &str) -> PathBuf {
    let b = std::env::temp_dir().join(format!("droid-lite-e2e-{name}-b-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&b);
    std::fs::create_dir_all(b.join("app/src/main/java/b")).unwrap();
    std::fs::write(b.join("settings.gradle.kts"), "include(\":app\")\n").unwrap();
    std::fs::write(b.join("app/src/main/java/b/B.kt"), "package b\nclass B\n").unwrap();
    b.canonicalize().unwrap()
}

/// The text of each backup in the unsaved folder of fixture `f`.
fn unsaved_backups(f: &Fixture) -> Vec<String> {
    std::fs::read_dir(f.config.join("unsaved"))
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let v: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(e.ok()?.path()).ok()?).ok()?;
            Some(v["text"].as_str()?.to_string())
        })
        .collect()
}

#[test]
fn e2e_a_copy_that_cannot_be_kept_keeps_the_window_and_the_project_open() {
    let f = Fixture::new("copyfail");
    let mut h = f.harness();
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    open(&mut h, &user);
    h.state_mut().settings.autosave = false;
    let a = h.state().active;
    h.state_mut().tabs[a].text.push_str("// typed\n");
    let typed = tab(&h).text.clone();
    // local history cannot be written
    let history = f.config.join("history");
    std::fs::create_dir_all(&history).unwrap();
    set_mode(&history, 0o555);
    let why = format!("Could not keep a copy of {}: ", user.display());
    // Keep a copy and close: the window stays open and says why
    assert!(ask_to_close(&mut h));
    steps(&mut h, 2);
    h.get_by_label("Keep a copy and close").click();
    steps(&mut h, 3);
    assert!(
        !h.state().close_ok && h.state().close_asked,
        "the window stays open"
    );
    assert!(h.state().status.starts_with(&why), "{}", h.state().status);
    assert!(
        h.state()
            .copy_error
            .as_deref()
            .is_some_and(|e| e.starts_with(&why))
    );
    assert!(
        h.query_all_by_label_contains(&why).count() >= 2,
        "the window says why too"
    );
    h.get_by_label("Cancel").click();
    steps(&mut h, 2);
    assert!(h.state().copy_error.is_none());
    // Keep a copy and switch: this project stays open
    let b = other_project("copyfail");
    let ctx = h.ctx.clone();
    h.state_mut().open_project(&b, &ctx);
    steps(&mut h, 2);
    h.get_by_label("Keep a copy and switch").click();
    steps(&mut h, 3);
    assert_eq!(
        h.state().project.as_ref().map(|p| p.root.clone()),
        Some(f.root.clone())
    );
    assert!(h.state().indexing.is_none() && h.state().switch_to.is_some());
    assert_eq!(tab(&h).text, typed);
    assert!(h.state().status.starts_with(&why), "{}", h.state().status);
    h.get_by_label("Cancel").click();
    steps(&mut h, 2);
    // Revert and closing the tab keep the edits as well
    h.state_mut().revert();
    assert_eq!(tab(&h).text, typed);
    let a = h.state().active;
    h.state_mut().tabs[a].deleted = true;
    h.state_mut().close_tab(a);
    assert_eq!(tab(&h).text, typed, "the tab stays open");
    h.state_mut().tabs[a].deleted = false;
    // a forced quit: the edits go to the unsaved folder instead
    eframe::App::on_exit(h.state_mut());
    assert_eq!(unsaved_backups(&f), vec![typed.clone()]);
    set_mode(&history, 0o755);
    // the next start puts them back in their tab
    drop(h);
    let mut h = f.harness();
    wait(&mut h, "recovered", |a| {
        a.toast_texts()
            .contains(&"Recovered unsaved edits for User.kt")
    });
    let t = h.state().tabs.iter().find(|t| t.path == user).unwrap();
    assert_eq!(t.text, typed);
    assert!(t.dirty(), "recovered edits are not saved yet");
    assert_eq!(
        std::fs::read_to_string(&user).unwrap(),
        USER_KT,
        "the file is not touched"
    );
    let _ = std::fs::remove_dir_all(&b);
}

#[test]
fn e2e_save_never_overwrites_another_version_it_could_not_keep() {
    let f = Fixture::new("savecopyfail");
    let mut h = f.harness();
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    open(&mut h, &user);
    h.state_mut().settings.autosave = false;
    let history = f.config.join("history");
    std::fs::create_dir_all(&history).unwrap();
    set_mode(&history, 0o555);
    // the disk has what was loaded: no copy is needed, the save goes on
    let a = h.state().active;
    h.state_mut().tabs[a].text.push_str("// one\n");
    h.state_mut().save();
    h.state_mut().wait_saves();
    assert!(
        std::fs::read_to_string(&user)
            .unwrap()
            .ends_with("// one\n")
    );
    // another version on disk that cannot be kept: not overwritten
    h.state_mut().tabs[a].text.push_str("// two\n");
    let theirs = "package com.demo.data\n\nclass Theirs\n";
    std::fs::write(&user, theirs).unwrap();
    h.state_mut().save();
    h.state_mut().wait_saves();
    assert_eq!(std::fs::read_to_string(&user).unwrap(), theirs);
    assert!(
        h.state()
            .status
            .starts_with(&format!("Could not keep a copy of {}", user.display())),
        "{}",
        h.state().status
    );
    assert!(tab(&h).dirty());
    set_mode(&history, 0o755);
    h.state_mut().save();
    h.state_mut().wait_saves();
    assert!(
        std::fs::read_to_string(&user)
            .unwrap()
            .ends_with("// two\n")
    );
    assert_eq!(newest_version(&user).as_deref(), Some(theirs));
}

#[test]
fn e2e_save_of_a_clean_tab_never_overwrites_a_change_on_disk() {
    let f = Fixture::new("savecleanchanged");
    let mut h = f.harness();
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    open(&mut h, &user);
    // changed on disk; the watcher has not told the tab yet (nor will it here)
    h.state_mut().watcher = None;
    let theirs = "package com.demo.data\n\nclass Theirs\n";
    std::fs::write(&user, theirs).unwrap();
    h.state_mut().save();
    h.state_mut().wait_saves();
    assert_eq!(std::fs::read_to_string(&user).unwrap(), theirs);
    assert_eq!(
        h.state().status,
        "File changed on disk; reload or keep yours"
    );
    assert!(tab(&h).disk_changed);
    // Keep mine: then ⌘S writes it, and the other version is in local history
    steps(&mut h, 2);
    h.get_by_label("Keep mine").click();
    steps(&mut h, 2);
    h.state_mut().save();
    h.state_mut().wait_saves();
    assert_eq!(std::fs::read_to_string(&user).unwrap(), USER_KT);
    assert_eq!(newest_version(&user).as_deref(), Some(theirs));
}

#[test]
fn e2e_a_file_that_cannot_be_read_never_blanks_its_tab() {
    let f = Fixture::new("unreadable");
    let mut h = f.harness();
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    open(&mut h, &user);
    watcher_live(&mut h, &user);
    std::fs::write(&user, [0xff, 0xfe, b'x']).unwrap();
    wait(&mut h, "unreadable", |a| {
        a.tabs[a.active].unreadable.is_some()
    });
    assert_eq!(tab(&h).text, USER_KT, "the tab keeps its text");
    assert!(tab(&h).disk_changed);
    steps(&mut h, 2);
    h.get_by_label_contains("cannot be read as text now (it is not UTF-8 text)");
    // ⌘S does not write over it unasked
    h.state_mut().save();
    h.state_mut().wait_saves();
    assert_eq!(std::fs::read(&user).unwrap(), [0xff, 0xfe, b'x']);
    // a version in local history that is not UTF-8 is not restored: it says why
    super::comfort::record_text(&user, &[0xff, 0xfe]).unwrap();
    h.state_mut().history_for = Some(user.clone());
    steps(&mut h, 2);
    h.get_all_by_label("Restore").next().unwrap().click();
    steps(&mut h, 2);
    assert!(
        h.state().status.contains("is not UTF-8 text"),
        "{}",
        h.state().status
    );
    assert_eq!(tab(&h).text, USER_KT);
}

#[test]
fn e2e_disk_changes_apply_in_the_order_they_happened() {
    let f = Fixture::new("diskorder");
    let mut h = f.harness();
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    open(&mut h, &user);
    let (tx1, rx1) = channel();
    let (tx2, rx2) = channel();
    h.state_mut().disk_rx.extend([rx1, rx2]);
    let change = |t: &str| vec![super::disk::DiskChange::File(user.clone(), t.into(), None)];
    // the newer batch is read first: it waits for the older one
    tx2.send(change("// newer\n")).unwrap();
    steps(&mut h, 2);
    assert_eq!(tab(&h).text, USER_KT);
    tx1.send(change("// older\n")).unwrap();
    steps(&mut h, 2);
    assert_eq!(tab(&h).text, "// newer\n");
    assert!(h.state().disk_rx.is_empty());
}

#[test]
fn e2e_own_late_save_event_is_not_a_change_on_disk() {
    let f = Fixture::new("latesave");
    let mut h = f.harness();
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    open(&mut h, &user);
    let a = h.state().active;
    h.state_mut().tabs[a].text.push_str("// one\n");
    h.state_mut().save();
    h.state_mut().wait_saves();
    let saved = tab(&h).text.clone();
    h.state_mut().tabs[a].text.push_str("// two\n");
    // the watcher reports that save after more typing
    let (tx, rx) = channel();
    h.state_mut().disk_rx.push(rx);
    tx.send(vec![super::disk::DiskChange::File(
        user.clone(),
        saved,
        None,
    )])
    .unwrap();
    steps(&mut h, 2);
    assert!(!tab(&h).disk_changed, "no changed-on-disk bar");
    assert!(tab(&h).text.ends_with("// two\n"));
    // a successful save clears the bars
    h.state_mut().tabs[a].disk_changed = true;
    h.state_mut().tabs[a].deleted = true;
    h.state_mut().save();
    h.state_mut().wait_saves();
    assert!(!tab(&h).disk_changed && !tab(&h).deleted);
}

#[test]
fn e2e_a_late_watcher_event_for_a_save_keeps_the_typed_symbols_in_the_index() {
    let f = Fixture::new("latesaveindex");
    let mut h = f.harness();
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    open(&mut h, &user);
    let a = h.state().active;
    h.state_mut().save();
    h.state_mut().wait_saves();
    let saved = tab(&h).text.clone();
    h.state_mut().tabs[a].text.push_str("class Typed\n");
    h.state_mut().tabs[a].mark_edited();
    h.state_mut().settle(true);
    let has_typed = |h: &Harness<'static, App>| {
        let app = h.state();
        app.index.settle();
        let index = app.index.read();
        index.files[user.as_path()]
            .decls
            .iter()
            .any(|d| d.name == "Typed")
    };
    assert!(has_typed(&h), "the index has the typed class");
    // the watcher reports the earlier save now
    let (tx, rx) = channel();
    h.state_mut().disk_rx.push(rx);
    let syms = crate::kotlin::scan_file(&user, &saved);
    tx.send(vec![super::disk::DiskChange::File(
        user.clone(),
        saved,
        Some(syms),
    )])
    .unwrap();
    wait(&mut h, "the event applied", |a| a.disk_rx.is_empty());
    assert!(has_typed(&h), "still there after the event");
}

#[test]
fn e2e_a_stuck_save_never_freezes_the_window_and_a_revert_runs_once_it_lands() {
    let f = Fixture::new("stucksave");
    let mut h = f.harness();
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    open(&mut h, &user);
    // the save thread hangs (a stuck network disk) with a save of User.kt behind it
    let (unstick, stuck) = channel::<()>();
    saver::run(move || {
        let _ = stuck.recv_timeout(Duration::from_secs(30));
    });
    let a = h.state().active;
    h.state_mut().tabs[a].text.push_str("// typed\n");
    h.state_mut().save_tab(false);
    h.state_mut().tabs[a].text.push_str("// more\n");
    h.state_mut().revert();
    assert!(
        h.state()
            .status
            .starts_with("Revert User.kt runs when the save on its way is done"),
        "{}",
        h.state().status
    );
    assert!(tab(&h).text.ends_with("// more\n"), "not reverted yet");
    unstick.send(()).unwrap();
    h.state_mut().wait_saves();
    let disk = std::fs::read_to_string(&user).unwrap();
    assert!(disk.ends_with("// typed\n"));
    assert_eq!(tab(&h).text, disk, "reverted to what was saved");
}

#[test]
fn e2e_an_action_waiting_for_a_save_does_not_run_after_more_typing() {
    let f = Fixture::new("stucksave-typed");
    let mut h = f.harness();
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    open(&mut h, &user);
    let (unstick, stuck) = channel::<()>();
    saver::run(move || {
        let _ = stuck.recv_timeout(Duration::from_secs(30));
    });
    let a = h.state().active;
    h.state_mut().tabs[a].text.push_str("// typed\n");
    h.state_mut().save_tab(false);
    h.state_mut().revert();
    h.state_mut().tabs[a]
        .text
        .push_str("// typed after the revert\n");
    h.state_mut().tabs[a].mark_edited();
    unstick.send(()).unwrap();
    h.state_mut().wait_saves();
    assert!(
        tab(&h).text.ends_with("// typed after the revert\n"),
        "not reverted"
    );
    assert!(
        h.state()
            .status
            .starts_with("Revert User.kt did not run: User.kt changed meanwhile"),
        "{}",
        h.state().status
    );
}

/// Hangs the save thread (a stuck network disk) until the returned sender sends or is dropped
/// (a test that fails first lets it go as it unwinds).
fn stuck_disk() -> std::sync::mpsc::Sender<()> {
    let (unstick, stuck) = channel::<()>();
    saver::run(move || {
        let _ = stuck.recv_timeout(Duration::from_secs(30));
    });
    unstick
}

/// Opens User.kt, types at its end and saves it behind a stuck disk: (User.kt, the sender that
/// lets the disk go).
fn save_on_a_stuck_disk(
    f: &Fixture,
    h: &mut Harness<'static, App>,
) -> (PathBuf, std::sync::mpsc::Sender<()>) {
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    open(h, &user);
    let unstick = stuck_disk();
    let a = h.state_mut();
    a.settings.autosave = false;
    let i = a.active;
    a.tabs[i].text.push_str("// typed\n");
    a.tabs[i].mark_edited();
    a.save_tab(false);
    (user, unstick)
}

/// An action that counts its runs.
fn counted(n: &std::rc::Rc<std::cell::Cell<u32>>) -> impl FnOnce(&mut App) + 'static {
    let n = n.clone();
    move |_| n.set(n.get() + 1)
}

#[test]
fn e2e_an_action_asked_for_twice_while_a_save_is_on_its_way_runs_once() {
    let f = Fixture::new("after-saves-twice");
    let mut h = f.harness();
    let (_, unstick) = save_on_a_stuck_disk(&f, &mut h);
    let runs = std::rc::Rc::new(std::cell::Cell::new(0));
    let a = h.state_mut();
    assert!(!a.settle_saves("git stash", counted(&runs)));
    assert!(!a.settle_saves("git stash", counted(&runs)));
    unstick.send(()).unwrap();
    a.wait_saves();
    assert_eq!(runs.get(), 1);
}

#[test]
fn e2e_actions_waiting_for_a_save_are_dropped_when_another_project_opens() {
    let f = Fixture::new("after-saves-switch");
    let mut h = f.harness();
    let (_, unstick) = save_on_a_stuck_disk(&f, &mut h);
    let runs = std::rc::Rc::new(std::cell::Cell::new(0));
    h.state_mut().settle_saves("git stash", counted(&runs));
    let (other, ctx) = (f.path("app"), h.ctx.clone());
    h.state_mut().switch_project(&other, &ctx);
    assert!(h.state().after_saves.is_empty());
    unstick.send(()).unwrap();
    h.state_mut().wait_saves();
    assert_eq!(runs.get(), 0);
}

#[test]
fn e2e_a_checkout_that_waited_for_a_save_drops_the_actions_queued_after_it() {
    let f = Fixture::new("after-saves-checkout");
    let mut h = f.harness();
    let (_, unstick) = save_on_a_stuck_disk(&f, &mut h);
    let runs = std::rc::Rc::new(std::cell::Cell::new(0));
    let a = h.state_mut();
    a.git_command(&["checkout", "-q", "-b", "other"]);
    a.settle_saves("Rename A → B", counted(&runs));
    unstick.send(()).unwrap();
    a.wait_saves();
    assert_eq!(a.branch.as_deref(), Some("other"), "the checkout ran");
    assert_eq!(runs.get(), 0, "asked for on the old branch");
}

#[test]
fn e2e_a_git_action_waiting_for_a_save_is_dropped_when_head_moved_meanwhile() {
    let f = Fixture::new("after-saves-head");
    let mut h = f.harness();
    let (user, unstick) = save_on_a_stuck_disk(&f, &mut h);
    let root = f.root.clone();
    h.state_mut().revert_file(
        &root,
        git_ui::Revert::Working {
            path: user.clone(),
            untracked: false,
        },
    );
    // the state it was asked in is read before the commit below
    h.state_mut().wait_asked();
    // a commit made outside Droid Lite while the disk is stuck
    let made = std::process::Command::new("git")
        .args(["commit", "-q", "--allow-empty", "-m", "elsewhere"])
        .current_dir(&root)
        .status()
        .unwrap();
    assert!(made.success());
    unstick.send(()).unwrap();
    h.state_mut().wait_saves();
    assert!(
        std::fs::read_to_string(&user)
            .unwrap()
            .ends_with("// typed\n"),
        "not reverted"
    );
    assert!(
        h.state().status.starts_with(
            "Git revert User.kt did not run: the branch or the staged files changed meanwhile"
        ),
        "{}",
        h.state().status
    );
}

#[test]
fn e2e_an_action_waiting_for_a_save_shows_and_can_be_cancelled() {
    let f = Fixture::new("after-saves-cancel");
    let mut h = f.harness();
    let (user, unstick) = save_on_a_stuck_disk(&f, &mut h);
    let i = h.state().active;
    h.state_mut().tabs[i].text.push_str("// more\n");
    h.state_mut().tabs[i].mark_edited();
    h.state_mut().revert();
    steps(&mut h, 2);
    assert!(
        h.query_by_label("Waiting for save: Revert User.kt")
            .is_some()
    );
    h.get_by_label("Cancel Revert User.kt").click();
    steps(&mut h, 2);
    assert!(
        h.query_by_label("Waiting for save: Revert User.kt")
            .is_none()
    );
    unstick.send(()).unwrap();
    h.state_mut().wait_saves();
    assert!(tab(&h).text.ends_with("// more\n"), "not reverted");
    assert!(
        std::fs::read_to_string(&user)
            .unwrap()
            .ends_with("// typed\n")
    );
}

#[test]
fn e2e_an_edit_undone_while_an_action_waits_for_a_save_still_counts_as_a_change() {
    let f = Fixture::new("after-saves-revs");
    let mut h = f.harness();
    let (_, unstick) = save_on_a_stuck_disk(&f, &mut h);
    let runs = std::rc::Rc::new(std::cell::Cell::new(0));
    let a = h.state_mut();
    a.settle_saves("git stash", counted(&runs));
    let i = a.active;
    // typed and typed away again: the same text, a newer version
    a.tabs[i].text.push('x');
    a.tabs[i].mark_edited();
    a.tabs[i].text.pop();
    a.tabs[i].mark_edited();
    unstick.send(()).unwrap();
    a.wait_saves();
    assert_eq!(runs.get(), 0);
    assert!(
        a.status.contains("User.kt changed meanwhile"),
        "{}",
        a.status
    );
}

/// The repository state ([`App::stamp`]) a test holds back: its root, how many more are
/// worked out for it before that one, whether it is read before or after the hold, "held" and
/// "go on".
#[allow(clippy::type_complexity)]
static HELD: Mutex<Option<(PathBuf, usize, bool, std::sync::mpsc::Sender<()>, Receiver<()>)>> =
    Mutex::new(None);

/// [`crate::git::state_stamp`], except the one [`hold_stamp`] holds back until the test's go.
fn held_stamp(root: &Path) -> Option<String> {
    let mut held = HELD.lock().unwrap_or_else(|e| e.into_inner());
    let due = match held.as_mut() {
        Some((r, n, ..)) if r.as_path() == root => {
            *n -= 1;
            *n == 0
        }
        _ => false,
    };
    let gate = if due { held.take() } else { None };
    drop(held);
    let Some((_, _, read_first, held, go)) = gate else {
        return crate::git::state_stamp(root);
    };
    let s = read_first.then(|| crate::git::state_stamp(root));
    let _ = held.send(());
    let _ = go.recv_timeout(Duration::from_secs(30));
    s.unwrap_or_else(|| crate::git::state_stamp(root))
}

/// Holds back the `nth` (from 1) repository state the app works out from now on, read from git
/// before the hold when `read_first`, else after it: (it is held, go on).
fn hold_stamp(
    a: &mut App,
    nth: usize,
    read_first: bool,
) -> (Receiver<()>, std::sync::mpsc::Sender<()>) {
    let root = a.project.as_ref().unwrap().root.clone();
    let (held_tx, held) = channel();
    let (go, go_rx) = channel();
    *HELD.lock().unwrap_or_else(|e| e.into_inner()) =
        Some((root, nth, read_first, held_tx, go_rx));
    a.stamp = held_stamp;
    (held, go)
}

/// What git prints in `root`.
fn git_out(root: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Lets the stuck save land and applies it, without running anything that waits on a git state.
fn land_saves(a: &mut App) {
    while a.saves_pending > 0 {
        saver::wait();
        a.poll_saves();
    }
}

#[test]
fn e2e_git_add_then_commit_queued_behind_a_save_both_run() {
    let f = Fixture::new("after-saves-add-commit");
    let mut h = f.harness();
    let (_, unstick) = save_on_a_stuck_disk(&f, &mut h);
    let a = h.state_mut();
    a.git_command(&["add", "app/src/main/java/com/demo/data/User.kt"]);
    a.git_command(&["commit", "-q", "-m", "typed"]);
    assert_eq!(a.after_saves.len(), 2);
    unstick.send(()).unwrap();
    a.wait_saves();
    assert_eq!(git_out(&f.root, &["log", "-1", "--format=%s"]), "typed\n", "{}", a.status);
    assert!(
        git_out(&f.root, &["show", "--name-only", "--format=", "HEAD"]).contains("User.kt"),
        "the commit has the file the add staged"
    );
}

#[test]
fn e2e_an_action_asked_for_again_keeps_its_place_before_the_ones_behind_it() {
    let f = Fixture::new("after-saves-again");
    let mut h = f.harness();
    let (_, unstick) = save_on_a_stuck_disk(&f, &mut h);
    let a = h.state_mut();
    a.git_command(&["add", "app/src/main/java/com/demo/data/User.kt"]);
    a.git_command(&["commit", "-q", "-m", "typed"]);
    // the add clicked again while both wait
    a.git_command(&["add", "app/src/main/java/com/demo/data/User.kt"]);
    let order: Vec<&str> = a.after_saves.iter().map(|w| w.what.as_str()).collect();
    assert_eq!(order.len(), 2, "{order:?}");
    assert!(order[0].starts_with("git add"), "{order:?}");
    unstick.send(()).unwrap();
    a.wait_saves();
    assert_eq!(git_out(&f.root, &["log", "-1", "--format=%s"]), "typed\n", "{}", a.status);
    assert!(
        git_out(&f.root, &["show", "--name-only", "--format=", "HEAD"]).contains("User.kt"),
        "the add ran before the commit"
    );
}

#[test]
fn e2e_actions_on_same_named_files_or_of_two_kinds_wait_side_by_side() {
    let f = Fixture::new("after-saves-same-name");
    let other = f.path("app/src/main/java/com/demo/other/User.kt");
    std::fs::create_dir_all(other.parent().unwrap()).unwrap();
    std::fs::write(&other, "package com.demo.other\n").unwrap();
    let mut h = f.harness();
    let (user, unstick) = save_on_a_stuck_disk(&f, &mut h);
    open(&mut h, &other);
    let a = h.state_mut();
    let o = a.active;
    a.tabs[o].text.push_str("// typed\n");
    a.tabs[o].mark_edited();
    let u = a.tabs.iter().position(|t| t.path == user).unwrap();
    a.active = u;
    a.revert();
    a.active = o;
    a.revert();
    a.close_tab(u);
    let keys: Vec<&str> = a.after_saves.iter().map(|w| w.key.as_str()).collect();
    assert_eq!(keys.len(), 3, "{keys:?}");
    unstick.send(()).unwrap();
    a.wait_saves();
    assert!(a.tabs.iter().all(|t| t.path != user), "closed: {}", a.status);
    let t = a.tabs.iter().find(|t| t.path == other).unwrap();
    assert_eq!(t.text, "package com.demo.other\n", "reverted");
}

#[test]
fn e2e_a_git_action_waiting_for_a_save_is_dropped_when_files_were_staged_meanwhile() {
    let f = Fixture::new("after-saves-staged");
    let mut h = f.harness();
    let (_, unstick) = save_on_a_stuck_disk(&f, &mut h);
    std::fs::write(f.path("notes.txt"), "notes\n").unwrap();
    let a = h.state_mut();
    a.git_command(&["commit", "-q", "--allow-empty", "-m", "mine"]);
    a.wait_asked();
    // staged outside Droid Lite while the disk is stuck
    git_out(&f.root, &["add", "notes.txt"]);
    unstick.send(()).unwrap();
    a.wait_saves();
    assert_ne!(git_out(&f.root, &["log", "-1", "--format=%s"]), "mine\n");
    assert!(
        a.status.starts_with(
            "git commit -q --allow-empty -m mine did not run: the branch or the staged files changed meanwhile"
        ),
        "{}",
        a.status
    );
}

#[test]
fn e2e_an_action_asked_for_while_others_wait_runs_after_them() {
    let f = Fixture::new("after-saves-order");
    let mut h = f.harness();
    let (_, unstick) = save_on_a_stuck_disk(&f, &mut h);
    std::fs::write(f.path("notes.txt"), "notes\n").unwrap();
    let a = h.state_mut();
    // the commit's state when it is to run: read, then held back
    let (read, go) = hold_stamp(a, 2, true);
    a.git_command(&["commit", "-q", "--allow-empty", "-m", "first"]);
    a.wait_asked();
    unstick.send(()).unwrap();
    land_saves(a);
    read.recv_timeout(Duration::from_secs(30)).unwrap();
    // no save on its way, the commit still waits: a new click waits behind it
    a.git_command(&["add", "notes.txt"]);
    assert_eq!(a.after_saves.len(), 2, "{}", a.status);
    assert_eq!(git_out(&f.root, &["diff", "--cached", "--name-only"]), "");
    go.send(()).unwrap();
    a.wait_saves();
    assert_eq!(git_out(&f.root, &["log", "-1", "--format=%s"]), "first\n", "{}", a.status);
    assert!(!git_out(&f.root, &["show", "--name-only", "--format=", "HEAD"]).contains("notes"));
    assert_eq!(
        git_out(&f.root, &["diff", "--cached", "--name-only"]),
        "notes.txt\n",
        "the add ran after"
    );
}

#[test]
fn e2e_a_git_state_worked_out_while_git_ran_is_worked_out_again() {
    let f = Fixture::new("after-saves-moved");
    let mut h = f.harness();
    let (_, unstick) = save_on_a_stuck_disk(&f, &mut h);
    std::fs::write(f.path("notes.txt"), "notes\n").unwrap();
    let a = h.state_mut();
    let (read, go) = hold_stamp(a, 2, true);
    a.git_command(&["commit", "-q", "--allow-empty", "-m", "first"]);
    a.wait_asked();
    unstick.send(()).unwrap();
    land_saves(a);
    read.recv_timeout(Duration::from_secs(30)).unwrap();
    // git run by this app after that state was read
    crate::git::run(&f.root, &["add", "notes.txt"]).unwrap();
    go.send(()).unwrap();
    a.wait_saves();
    assert_ne!(git_out(&f.root, &["log", "-1", "--format=%s"]), "first\n");
    assert!(
        a.status.ends_with("did not run: the branch or the staged files changed meanwhile"),
        "{}",
        a.status
    );
}

#[test]
fn e2e_a_git_action_whose_asked_state_was_read_while_git_ran_is_dropped() {
    let f = Fixture::new("after-saves-asked-moved");
    let mut h = f.harness();
    let (_, unstick) = save_on_a_stuck_disk(&f, &mut h);
    std::fs::write(f.path("notes.txt"), "notes\n").unwrap();
    let a = h.state_mut();
    // the commit's state when asked: read only after the add below
    let (held, go) = hold_stamp(a, 1, false);
    a.git_command(&["commit", "-q", "-m", "first"]);
    held.recv_timeout(Duration::from_secs(30)).unwrap();
    crate::git::run(&f.root, &["add", "notes.txt"]).unwrap();
    go.send(()).unwrap();
    unstick.send(()).unwrap();
    a.wait_saves();
    assert_ne!(git_out(&f.root, &["log", "-1", "--format=%s"]), "first\n");
    assert!(a.status.ends_with("did not run: git ran meanwhile"), "{}", a.status);
}

#[test]
fn e2e_an_action_ahead_of_a_git_action_runs_while_the_git_state_is_worked_out() {
    let f = Fixture::new("after-saves-ahead");
    let mut h = f.harness();
    let (_, unstick) = save_on_a_stuck_disk(&f, &mut h);
    let runs = std::rc::Rc::new(std::cell::Cell::new(0));
    let a = h.state_mut();
    // the commit's state when asked: read, then held back
    let (read, go) = hold_stamp(a, 1, true);
    a.settle_saves("Rename A → B", counted(&runs));
    a.git_command(&["commit", "-q", "--allow-empty", "-m", "first"]);
    read.recv_timeout(Duration::from_secs(30)).unwrap();
    unstick.send(()).unwrap();
    land_saves(a);
    assert_eq!(runs.get(), 1, "{}", a.status);
    assert_eq!(a.after_saves.len(), 1, "the commit waits");
    go.send(()).unwrap();
    a.wait_saves();
    assert_eq!(git_out(&f.root, &["log", "-1", "--format=%s"]), "first\n", "{}", a.status);
}

#[test]
fn e2e_an_action_behind_one_that_changed_a_text_still_runs() {
    let f = Fixture::new("after-saves-revert-then");
    let mut h = f.harness();
    let (_, unstick) = save_on_a_stuck_disk(&f, &mut h);
    let runs = std::rc::Rc::new(std::cell::Cell::new(0));
    let a = h.state_mut();
    let i = a.active;
    a.tabs[i].text.push_str("// more\n");
    a.tabs[i].mark_edited();
    a.revert();
    a.settle_saves("git stash", counted(&runs));
    unstick.send(()).unwrap();
    a.wait_saves();
    assert!(!a.tabs[i].text.ends_with("// more\n"), "reverted");
    assert_eq!(runs.get(), 1, "{}", a.status);
}

#[test]
fn e2e_every_action_dropped_in_a_round_stays_in_the_notices() {
    let f = Fixture::new("after-saves-notices");
    let mut h = f.harness();
    let (_, unstick) = save_on_a_stuck_disk(&f, &mut h);
    let runs = std::rc::Rc::new(std::cell::Cell::new(0));
    let a = h.state_mut();
    a.settle_saves("Rename A → B", counted(&runs));
    a.settle_saves("Rename C → D", counted(&runs));
    let i = a.active;
    a.tabs[i].text.push_str("// more\n");
    a.tabs[i].mark_edited();
    unstick.send(()).unwrap();
    a.wait_saves();
    assert_eq!(runs.get(), 0);
    let notices: Vec<&str> = a.notices.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        notices,
        [
            "Rename A → B did not run: User.kt changed meanwhile",
            "Rename C → D did not run: User.kt changed meanwhile"
        ]
    );
}

#[test]
fn e2e_opening_or_closing_other_tabs_keeps_an_action_waiting_for_a_save() {
    let f = Fixture::new("after-saves-tabs");
    let mut h = f.harness();
    let card = f.path("app/src/main/java/com/demo/ui/UserCard.kt");
    open(&mut h, &card);
    let (_, unstick) = save_on_a_stuck_disk(&f, &mut h);
    let runs = std::rc::Rc::new(std::cell::Cell::new(0));
    h.state_mut().settle_saves("git stash", counted(&runs));
    // a clean tab closed and another file opened while it waits
    let c = h.state().tabs.iter().position(|t| t.path == card).unwrap();
    h.state_mut().close_tab(c);
    open(&mut h, &f.path("app/build.gradle.kts"));
    unstick.send(()).unwrap();
    h.state_mut().wait_saves();
    assert_eq!(runs.get(), 1, "{}", h.state().status);
}

#[test]
fn e2e_a_notice_seen_again_is_counted_and_only_the_last_ones_stay() {
    let f = Fixture::new("after-saves-notice-cap");
    let mut h = f.harness();
    for i in 0..7 {
        h.state_mut().notice(format!("Rename {i} did not run"));
    }
    h.state_mut().notice("Rename 6 did not run".into());
    let kept: Vec<(&str, u32)> = h
        .state()
        .notices
        .iter()
        .map(|(n, k)| (n.as_str(), *k))
        .collect();
    assert_eq!(
        kept,
        [
            ("Rename 2 did not run", 1),
            ("Rename 3 did not run", 1),
            ("Rename 4 did not run", 1),
            ("Rename 5 did not run", 1),
            ("Rename 6 did not run", 2)
        ]
    );
    steps(&mut h, 2);
    assert!(h.query_by_label("Rename 6 did not run (×2)").is_some());
    assert!(h.query_by_label("Rename 1 did not run").is_none());
}

#[test]
fn e2e_waiting_actions_are_told_apart_by_their_place() {
    let f = Fixture::new("after-saves-keys");
    let mut h = f.harness();
    let (user, unstick) = save_on_a_stuck_disk(&f, &mut h);
    let card = f.path("app/src/main/java/com/demo/ui/UserCard.kt");
    let a = h.state_mut();
    a.ask_rename("name", "title", std::slice::from_ref(&user));
    a.ask_rename("name", "title", std::slice::from_ref(&card));
    a.git_command(&["status"]);
    a.write_translation("app_name".into(), 0, "Demo");
    let keys: Vec<String> = a.after_saves.iter().map(|w| w.key.clone()).collect();
    assert_eq!(keys.len(), 4, "{keys:?}");
    let root = f.root.display().to_string();
    assert!(keys[2].ends_with(&root), "{}", keys[2]);
    assert!(keys[3].ends_with(&root), "{}", keys[3]);
    unstick.send(()).unwrap();
    a.wait_saves();
}

#[test]
fn e2e_a_pr_check_out_is_refused_until_the_actions_waiting_for_a_save_ran() {
    let f = Fixture::new("after-saves-pr");
    let mut h = f.harness();
    let (_, unstick) = save_on_a_stuck_disk(&f, &mut h);
    let a = h.state_mut();
    let (read, go) = hold_stamp(a, 1, true);
    a.git_command(&["commit", "-q", "--allow-empty", "-m", "first"]);
    read.recv_timeout(Duration::from_secs(30)).unwrap();
    unstick.send(()).unwrap();
    land_saves(a);
    assert_eq!(
        a.checkout_refusal(&f.root).as_deref(),
        Some("Wait for the actions waiting for a save to finish")
    );
    go.send(()).unwrap();
    a.wait_saves();
    assert_eq!(a.checkout_refusal(&f.root), None);
}

#[test]
fn e2e_a_close_waits_for_a_stuck_save_and_a_second_close_quits_now() {
    let f = Fixture::new("close-stuck");
    let mut h = f.harness();
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    open(&mut h, &user);
    let (unstick, stuck) = channel::<()>();
    saver::run(move || {
        let _ = stuck.recv_timeout(Duration::from_secs(30));
    });
    let a = h.state().active;
    h.state_mut().tabs[a].text.push_str("// typed\n");
    assert!(
        ask_to_close(&mut h),
        "the close waits for auto-save's write"
    );
    assert!(!h.state().close_ok && !h.state().quit_now);
    assert!(
        h.state().status.starts_with("Closing when"),
        "{}",
        h.state().status
    );
    assert!(!ask_to_close(&mut h), "asked again: it closes now");
    assert!(h.state().close_ok && h.state().quit_now);
    unstick.send(()).unwrap();
    h.state_mut().wait_saves();
}

#[test]
fn e2e_undo_of_a_rename_never_overwrites_a_new_file_at_the_old_name() {
    let f = Fixture::new("undorename");
    let mut h = f.harness();
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    let person = f.path("app/src/main/java/com/demo/data/Person.kt");
    std::fs::write(&person, "class Person\n").unwrap();
    // User.kt was made again after the rename to Person.kt
    h.state_mut().undo = Some(refactor_ui::Undo::of(&user, &person, Vec::new()));
    h.state_mut().undo_refactor();
    assert!(
        h.state().status.contains("exists again"),
        "{}",
        h.state().status
    );
    assert_eq!(std::fs::read_to_string(&user).unwrap(), USER_KT);
    assert_eq!(std::fs::read_to_string(&person).unwrap(), "class Person\n");
    assert!(h.state().undo.is_some(), "the undo can run later");
    // a file it cannot write back is reported, not "Undid"
    std::fs::remove_file(&user).unwrap();
    let card = f.path("app/src/main/java/com/demo/ui/UserCard.kt");
    set_mode(&card, 0o444);
    let files = vec![(card.clone(), "x".into())];
    h.state_mut().undo = Some(refactor_ui::Undo::of(&user, &person, files));
    h.state_mut().undo_refactor();
    set_mode(&card, 0o644);
    assert!(user.exists() && !person.exists());
    assert!(
        h.state()
            .status
            .starts_with("Undid rename test only in part: Cannot write"),
        "{}",
        h.state().status
    );
    assert_eq!(std::fs::read_to_string(&card).unwrap(), CARD_KT);
}

#[test]
fn e2e_unsaved_edits_are_backed_up_and_recovered_after_a_crash() {
    let f = Fixture::new("crashbackup");
    let mut h = f.harness();
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    open(&mut h, &user);
    h.state_mut().settings.autosave = false;
    assert!(unsaved_backups(&f).is_empty(), "nothing without edits");
    let a = h.state().active;
    h.state_mut().tabs[a].text.push_str("// one\n");
    h.state_mut().tabs[a].mark_edited();
    steps(&mut h, 2);
    let one = tab(&h).text.clone();
    assert_eq!(unsaved_backups(&f), vec![one.clone()]);
    // more typing is written at most every 30 s
    h.state_mut().tabs[a].text.push_str("// two\n");
    h.state_mut().tabs[a].mark_edited();
    steps(&mut h, 2);
    assert_eq!(unsaved_backups(&f), vec![one.clone()]);
    h.state_mut().backup.written_at = None; // 30 s later
    steps(&mut h, 2);
    let two = tab(&h).text.clone();
    assert_eq!(unsaved_backups(&f), vec![two.clone()]);
    // the app ends without on_exit (a crash): the next start puts the edits back
    drop(h);
    let mut h = f.harness();
    wait(&mut h, "recovered", |a| {
        a.toast_texts()
            .contains(&"Recovered unsaved edits for User.kt")
    });
    assert_eq!(
        h.state().tabs.iter().find(|t| t.path == user).unwrap().text,
        two
    );
    // once saved, the backup goes
    let i = h.state().tabs.iter().position(|t| t.path == user).unwrap();
    h.state_mut().active = i;
    h.state_mut().save();
    h.state_mut().wait_saves();
    h.state_mut().backup.written_at = None;
    steps(&mut h, 2);
    assert!(unsaved_backups(&f).is_empty());
}

#[test]
fn e2e_a_backup_older_than_the_file_on_disk_is_offered_with_a_diff_not_put_back() {
    let f = Fixture::new("stalebackup");
    let mut h = f.harness();
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    open(&mut h, &user);
    h.state_mut().settings.autosave = false;
    let a = h.state().active;
    h.state_mut().tabs[a]
        .text
        .push_str("// typed before the crash\n");
    h.state_mut().tabs[a].mark_edited();
    steps(&mut h, 2);
    let typed = tab(&h).text.clone();
    assert_eq!(unsaved_backups(&f), vec![typed.clone()]);
    // a crash, then the file is saved from another editor
    drop(h);
    let theirs = "package com.demo.data\n\nclass Theirs\n";
    std::fs::write(&user, theirs).unwrap();
    let mut h = f.harness();
    wait(&mut h, "offered", |a| {
        a.toast_texts()
            .iter()
            .any(|t| t.starts_with("Changed on disk since their unsaved backup"))
    });
    assert!(
        h.state()
            .tabs
            .iter()
            .filter(|t| t.path == user)
            .all(|t| t.text == theirs)
    );
    assert_eq!(
        std::fs::read_to_string(&user).unwrap(),
        theirs,
        "the newer file stays"
    );
    assert_eq!(
        newest_version(&user).as_deref(),
        Some(typed.as_str()),
        "the backup is in Local history"
    );
    let (title, diff) = h.state().git.commit.clone().expect("a diff is shown");
    assert!(title.contains("Unsaved backup of User.kt"), "{title}");
    assert!(
        diff.iter().any(|l| l == "+// typed before the crash"),
        "{diff:?}"
    );
    assert!(unsaved_backups(&f).is_empty(), "not offered again");
}

#[test]
fn e2e_another_running_window_keeps_its_backups() {
    let f = Fixture::new("otherwindow");
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    let dir = f.config.join("unsaved");
    let typed = format!("{USER_KT}// typed in the other window\n");
    // a backup the other window (a live process) wrote
    let file = super::backup::write_one(&dir, &user, &typed).unwrap();
    let mut other = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let mut v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    v["pid"] = other.id().into();
    v["start"] = super::backup::started(other.id()).unwrap().into();
    let theirs = dir.join(format!("other-{}.json", other.id()));
    std::fs::write(&theirs, v.to_string()).unwrap();
    std::fs::remove_file(&file).unwrap();
    let mut h = f.harness();
    steps(&mut h, 3);
    assert!(
        h.state()
            .tabs
            .iter()
            .filter(|t| t.path == user)
            .all(|t| t.text == USER_KT)
    );
    assert!(theirs.exists(), "the other window's backup stays");
    assert!(
        !h.state()
            .toast_texts()
            .iter()
            .any(|t| t.contains("Recovered"))
    );
    // that window crashed: the next start takes it
    other.kill().unwrap();
    other.wait().unwrap();
    drop(h);
    let mut h = f.harness();
    wait(&mut h, "recovered", |a| {
        a.toast_texts()
            .contains(&"Recovered unsaved edits for User.kt")
    });
    assert_eq!(
        h.state().tabs.iter().find(|t| t.path == user).unwrap().text,
        typed
    );
    assert!(!theirs.exists(), "this window's own backup replaces it");
    assert_eq!(unsaved_backups(&f), vec![typed]);
}

#[test]
fn e2e_a_panic_time_backup_goes_once_its_tab_is_saved() {
    let f = Fixture::new("panicbackup");
    let mut h = f.harness();
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    open(&mut h, &user);
    h.state_mut().settings.autosave = false;
    // the folder was written a moment ago: the next write waits 30 s
    h.state_mut().backup.written_at = Some(std::time::Instant::now());
    let a = h.state().active;
    h.state_mut().tabs[a].text.push_str("// typed\n");
    h.state_mut().tabs[a].mark_edited();
    steps(&mut h, 2);
    assert!(unsaved_backups(&f).is_empty());
    // a panic on a worker thread writes the snapshot; the window goes on
    assert_eq!(super::backup::write_snapshot(true), 1);
    assert_eq!(unsaved_backups(&f).len(), 1);
    h.state_mut().save();
    h.state_mut().wait_saves();
    steps(&mut h, 2);
    assert!(unsaved_backups(&f).is_empty(), "saved: the backup goes");
}

#[test]
fn e2e_autosave_never_writes_over_a_change_on_disk_the_watcher_has_not_reported() {
    let f = Fixture::new("autosaverace");
    let mut h = f.harness();
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    open(&mut h, &user);
    h.state_mut().watcher = None;
    let a = h.state().active;
    h.state_mut().tabs[a].text.push_str("// mine\n");
    h.state_mut().tabs[a].mark_edited();
    let theirs = "package com.demo.data\n\nclass Theirs\n";
    std::fs::write(&user, theirs).unwrap();
    h.state_mut().autosave();
    h.state_mut().wait_saves();
    assert_eq!(std::fs::read_to_string(&user).unwrap(), theirs);
    assert!(tab(&h).disk_changed && tab(&h).dirty(), "the bar asks");
    assert!(
        h.state().status.starts_with("Auto-save did not write"),
        "{}",
        h.state().status
    );
    // Keep mine: auto-save writes it
    steps(&mut h, 2);
    h.get_by_label("Keep mine").click();
    steps(&mut h, 2);
    h.state_mut().autosave();
    h.state_mut().wait_saves();
    assert!(
        std::fs::read_to_string(&user)
            .unwrap()
            .ends_with("// mine\n")
    );
    // deleted meanwhile: auto-save does not bring it back
    std::fs::remove_file(&user).unwrap();
    h.state_mut().tabs[a].text.push_str("// more\n");
    h.state_mut().autosave();
    h.state_mut().wait_saves();
    assert!(!user.exists());
    assert!(tab(&h).deleted);
}

#[test]
fn e2e_translations_and_history_restore_wait_for_a_checkout() {
    let f = Fixture::new("checkoutlock");
    let strings = |dir: &str, text: &str| {
        let p = f.path(&format!("app/src/main/res/{dir}/strings.xml"));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let xml = format!("<resources>\n    <string name=\"hi\">{text}</string>\n</resources>\n");
        std::fs::write(&p, xml).unwrap();
        p
    };
    strings("values", "Hi");
    let hindi = strings("values-hi", "Namaste");
    let mut h = f.harness();
    wait(&mut h, "resources", |a| {
        a.res.get("string", "hi").len() == 2
    });
    let ctx = h.ctx.clone();
    h.state_mut().run(Action::Translations, &ctx);
    steps(&mut h, 3);
    let root = f.root.clone();
    let _reply = h.state_mut().prs.add_stopping(&root, 7);
    assert!(h.state().checkout_busy());
    // the columns are the sorted folders: values, values-hi
    h.state_mut().write_translation("hi".into(), 1, "Alvida");
    assert_eq!(h.state().status, super::prs_ui::WAIT_CHECKOUT);
    assert!(std::fs::read_to_string(&hindi).unwrap().contains("Namaste"));
    // local history Restore waits too
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    super::comfort::record_text(&user, b"old").unwrap();
    open(&mut h, &user);
    h.state_mut().translations.open = false;
    h.state_mut().history_for = Some(user.clone());
    steps(&mut h, 2);
    h.get_all_by_label("Restore").next().unwrap().click();
    steps(&mut h, 2);
    assert_eq!(h.state().status, super::prs_ui::WAIT_CHECKOUT);
    assert_eq!(tab(&h).text, USER_KT);
}

#[test]
fn e2e_a_stopped_checkout_reply_waits_while_the_project_opens() {
    let f = Fixture::new("heldreply");
    let mut h = f.harness();
    let root = f.root.clone();
    let reply = h.state_mut().prs.add_stopping(&root, 7);
    // the project is opening again when the stop is confirmed
    let project = h.state_mut().project.take();
    let (_opening, rx) = channel();
    h.state_mut().indexing = Some(rx);
    reply
        .send(super::prs_ui::Checkout {
            root: root.clone(),
            number: 7,
            result: Err("Check out of #7 was stopped".into()),
            branch: crate::git::branch(&root),
            head: None,
        })
        .unwrap();
    steps(&mut h, 2);
    assert_eq!(h.state().prs.stopping_count(), 1, "the reply waits");
    h.state_mut().project = project;
    h.state_mut().indexing = None;
    assert!(h.state().checkout_busy(), "busy until the stop shows");
    steps(&mut h, 2);
    assert!(!h.state().checkout_busy());
    assert_eq!(
        h.state().prs.note,
        Some(("Check out of #7 was stopped".to_string(), true))
    );
}

/// The editor's text area on screen.
fn editor_rect(h: &Harness<'static, App>) -> egui::Rect {
    h.get_by_role(egui::accesskit::Role::MultilineTextInput)
        .rect()
}

/// The Diff checkbox is greyed out.
fn diff_disabled(h: &Harness<'static, App>) -> bool {
    use egui_kittest::kittest::NodeT;
    h.get_by_label("Diff").accesskit_node().is_disabled()
}

fn diff_view(a: &App) -> Option<std::sync::Arc<super::diff_view::Aligned>> {
    a.tabs[a.active].diff.view.clone()
}

/// The one changed word of a changed row, on the old and the new side.
fn words(r: &super::diff_view::Row) -> ((usize, usize), (usize, usize)) {
    let one = |v: &[std::ops::Range<usize>]| {
        assert_eq!(v.len(), 1, "{v:?}");
        (v[0].start, v[0].end)
    };
    (one(&r.old_words), one(&r.new_words))
}

/// (added, removed, changed) of the active tab's diff.
fn diff_counts(a: &App) -> Option<(usize, usize, usize)> {
    diff_view(a).map(|v| (v.added, v.removed, v.changed))
}

#[test]
fn e2e_diff_checkbox_splits_the_tab_and_follows_typing() {
    use super::diff_view::Kind;
    let f = Fixture::new("diffview");
    let mut h = f.harness();
    let card = f.path("app/src/main/java/com/demo/ui/UserCard.kt");
    open(&mut h, &card);
    wait(&mut h, "git status", |a| {
        a.git.changes.as_ref().is_some_and(|c| c.repo)
    });
    // line 7 changed ("count" → "clicks"), line 13 (`@Preview`) removed
    {
        let t = h.state_mut().tabs.last_mut().unwrap();
        t.text = CARD_KT
            .replace(
                "val count = mutableStateOf(0)",
                "val clicks = mutableStateOf(0)",
            )
            .replace("@Preview\n", "");
        t.reanalyse(); // shorter text: the symbols must follow before the Compose panel reads them
    }
    steps(&mut h, 2);
    let alone = editor_rect(&h);
    assert!(!diff_disabled(&h));
    assert!(
        h.query_by_label("Last commit").is_none(),
        "off: the normal editor"
    );
    h.get_by_label("Diff").click();
    wait(&mut h, "diff", |a| diff_counts(a) == Some((0, 1, 1)));
    steps(&mut h, 3);
    let v = diff_view(h.state()).unwrap();
    let changed = &v.rows[v.row_of_new[6]];
    assert_eq!(changed.kind, Kind::Changed);
    assert_eq!(words(changed), ((8, 13), (8, 14)));
    // the removed line is a spacer row above the editor's line 13
    assert_eq!(v.room, [(12, 1)]);
    // two columns: the older version on the left half, the editor on the right half
    let left = tab(&h).diff.left.expect("left column");
    let split = editor_rect(&h);
    assert!(split.left() > left.right(), "{split:?} {left:?}");
    assert!(
        left.width() > 400.0 && split.left() > alone.left() + 400.0,
        "{left:?}"
    );
    h.get_by_label("Last commit");
    h.get_by_label_contains("0 added · 1 removed · 1 changed");
    // the editor rows below the removed line moved down one row, so both sides stay aligned
    let font = egui::FontId::monospace(crate::theme::font_size());
    let row_h = h.ctx.fonts_mut(|f| f.row_height(&font));
    let gap = |h: &Harness<'static, App>| {
        let c = tab(h).cache.as_ref().unwrap();
        let g = c
            .shown
            .as_ref()
            .map_or(c.galley.clone(), |(_, g)| g.clone());
        g.rows[12].pos.y - g.rows[11].pos.y
    };
    let gap_on = gap(&h);
    if let Ok(dir) = std::env::var("DROID_LITE_SNAPSHOT_DIR") {
        h.render()
            .unwrap()
            .save(Path::new(&dir).join("e2e-diff.png"))
            .unwrap();
    }
    // typing updates the highlight: a new last line is added
    let end = tab(&h).text.len();
    h.state_mut().tabs.last_mut().unwrap().goto = Some(end);
    steps(&mut h, 3);
    type_text(&mut h, "// note");
    assert!(tab(&h).text.ends_with("}\n// note"));
    wait(&mut h, "added line", |a| diff_counts(a) == Some((1, 1, 1)));
    let v = diff_view(h.state()).unwrap();
    assert_eq!(v.rows.last().unwrap().kind, Kind::Added);
    // remembered in the session
    let session = std::fs::read_dir(f.config.join("sessions"))
        .unwrap()
        .map(|e| std::fs::read_to_string(e.unwrap().path()).unwrap())
        .collect::<String>();
    assert!(session.contains(r#""diff":"head""#), "{session}");
    // a running checkout makes the editor read-only, also with the diff on
    let root = f.root.clone();
    let reply = h.state_mut().prs.add_stopping(&root, 7);
    type_text(&mut h, "zzz");
    assert!(
        tab(&h).text.ends_with("}\n// note"),
        "no edit during a checkout"
    );
    drop(reply);
    h.state_mut().prs = Default::default();
    // off again (the shortcut): the normal editor
    key(&mut h, Modifiers::CTRL | Modifiers::SHIFT, Key::D);
    steps(&mut h, 2);
    assert!(!tab(&h).diff.on);
    assert!(tab(&h).diff.left.is_none());
    assert!((editor_rect(&h).left() - alone.left()).abs() < 1.0);
    assert!(h.query_by_label("Last commit").is_none());
    // the spacer row is gone
    let gap_off = gap(&h);
    assert!(
        (gap_on - gap_off - row_h).abs() < 0.5,
        "{gap_on} {gap_off} {row_h}"
    );
}

#[test]
fn e2e_diff_session_restores_the_checkbox() {
    let f = Fixture::new("diffsession");
    let card = f.path("app/src/main/java/com/demo/ui/UserCard.kt");
    {
        let mut h = f.harness();
        open(&mut h, &card);
        wait(&mut h, "git status", |a| {
            a.git.changes.as_ref().is_some_and(|c| c.repo)
        });
        h.get_by_label("Diff").click();
        steps(&mut h, 2);
        h.get_by_label("PR base").click();
        steps(&mut h, 2);
        assert_eq!(tab(&h).diff.against, super::diff_view::Against::Base);
    }
    let h = f.harness();
    let t = &h.state().tabs[h.state().active];
    assert_eq!(t.path, card);
    assert!(t.diff.on);
    assert_eq!(t.diff.against, super::diff_view::Against::Base);
}

#[test]
fn e2e_diff_compares_with_the_pr_base() {
    let f = Fixture::new("diffbase");
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&f.root)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };
    let main = git(&["branch", "--show-current"]);
    git(&["update-ref", &format!("refs/remotes/origin/{main}"), &main]);
    git(&["checkout", "-q", "-b", "feature"]);
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    std::fs::write(&user, USER_KT.replace("var age", "val age")).unwrap();
    git(&["commit", "-q", "-am", "feature"]);
    let mut h = f.harness();
    open(&mut h, &user);
    wait(&mut h, "PR base", |a| {
        a.git
            .changes
            .as_ref()
            .is_some_and(|c| c.repo && c.base.is_some())
    });
    h.get_by_label("Diff").click();
    // committed: nothing changed since the last commit
    wait(&mut h, "vs HEAD", |a| diff_counts(a) == Some((0, 0, 0)));
    steps(&mut h, 2);
    h.get_by_label("No changes");
    h.get_by_label("PR base").click();
    wait(&mut h, "vs the PR base", |a| {
        diff_counts(a) == Some((0, 0, 1))
    });
    steps(&mut h, 2);
    h.get_by_label_contains("0 added · 0 removed · 1 changed");
    let v = diff_view(h.state()).unwrap();
    let r = &v.rows[2];
    assert_eq!(words(r), ((34, 37), (34, 37)));
    assert_eq!(
        v.old_lines[2],
        "data class User(val name: String, var age: Int)"
    );
    // back to the last commit
    h.get_by_label("Last commit").click();
    wait(&mut h, "vs HEAD again", |a| {
        diff_counts(a) == Some((0, 0, 0))
    });
}

#[test]
fn e2e_diff_is_disabled_outside_git() {
    let f = Fixture::new("diffnogit");
    std::fs::remove_dir_all(f.root.join(".git")).unwrap();
    let mut h = f.harness();
    open(&mut h, &f.path("app/src/main/java/com/demo/data/User.kt"));
    wait(&mut h, "git status", |a| a.git.changes.is_some());
    steps(&mut h, 2);
    assert!(!h.state().git.changes.as_ref().unwrap().repo);
    assert!(diff_disabled(&h));
    h.get_by_label("Diff").click();
    steps(&mut h, 2);
    key(&mut h, Modifiers::CTRL | Modifiers::SHIFT, Key::D);
    assert!(!tab(&h).diff.on);
    assert!(tab(&h).diff.left.is_none());
    assert!(
        h.state().status.starts_with("Not a git project"),
        "{}",
        h.state().status
    );
}

/// UI-thread cost of the diff: the editor part of a frame on a 5,000-line file, diff off and on.
/// `cargo test --release e2e_diff_frame_cost -- --nocapture` prints the numbers.
#[test]
fn e2e_diff_frame_cost_on_a_5000_line_file() {
    let f = Fixture::new("diffperf");
    let big = f.path("app/src/main/java/com/demo/Big.kt");
    let text: String = (0..5000)
        .map(|i| format!("    val value{i} = compute({i}, \"label {i}\") // line {i}\n"))
        .collect();
    std::fs::write(&big, &text).unwrap();
    for args in [&["add", "."][..], &["commit", "-q", "-m", "big"]] {
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(&f.root)
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }
    let mut h = f.harness();
    open(&mut h, &big);
    wait(&mut h, "git status", |a| {
        a.git.changes.as_ref().is_some_and(|c| c.repo)
    });
    // every 10th line changed, every 50th removed
    let edited: String = text
        .lines()
        .enumerate()
        .filter(|(i, _)| i % 50 != 25)
        .map(|(i, l)| {
            if i % 10 == 3 {
                format!("{}\n", l.replace("compute", "computeAgain"))
            } else {
                format!("{l}\n")
            }
        })
        .collect();
    {
        let t = h.state_mut().tabs.last_mut().unwrap();
        t.text = edited;
        t.reanalyse();
    }
    // the middle of the file, where rows of every kind are on screen; typing makes line 2502
    // (unchanged) a changed one
    let mid = tab(&h).text.find("value2502 ").unwrap();
    h.state_mut().tabs.last_mut().unwrap().goto = Some(mid);
    steps(&mut h, 5);
    let editor_ms = |h: &mut Harness<'static, App>| {
        let mut v: Vec<f32> = (0..40)
            .map(|_| {
                h.step();
                let a = h.state();
                a.perf
                    .last
                    .iter()
                    .find(|(s, _)| *s == "editor")
                    .map_or(0.0, |s| s.1)
            })
            .collect();
        v.sort_by(f32::total_cmp);
        v[v.len() / 2]
    };
    // the slowest editor part of the frames while typing ten characters (and the compare after)
    let typing_ms = |h: &mut Harness<'static, App>| {
        let mut worst = 0.0f32;
        for c in "xyzxyzxyzx".chars() {
            h.event(Event::Text(c.to_string()));
            for _ in 0..3 {
                h.step();
                let a = h.state();
                let ms = a
                    .perf
                    .last
                    .iter()
                    .find(|(s, _)| *s == "editor")
                    .map_or(0.0, |s| s.1);
                worst = worst.max(ms);
            }
        }
        for _ in 0..30 {
            h.step();
            std::thread::sleep(Duration::from_millis(10));
            let a = h.state();
            let ms = a
                .perf
                .last
                .iter()
                .find(|(s, _)| *s == "editor")
                .map_or(0.0, |s| s.1);
            worst = worst.max(ms);
        }
        worst
    };
    let off = editor_ms(&mut h);
    let off_typing = typing_ms(&mut h);
    h.get_by_label("Diff").click();
    // the typing above (diff off) changed line 2502 too
    wait(&mut h, "diff", |a| diff_counts(a) == Some((0, 100, 501)));
    steps(&mut h, 3);
    assert!(tab(&h).diff.left.is_some());
    let on = editor_ms(&mut h);
    let on_typing = typing_ms(&mut h);
    assert!(
        tab(&h).text.contains("xyzxyzxyzxxyzxyzxyzx"),
        "typed into the editor side"
    );
    type_text(&mut h, "\nq");
    wait(&mut h, "compare after typing", |a| {
        diff_counts(a) == Some((1, 100, 501))
    });
    eprintln!(
        "diff frame cost, 5000 lines: editor part median {off:.2} ms off, {on:.2} ms on; \
         slowest while typing {off_typing:.2} ms off, {on_typing:.2} ms on"
    );
    assert!(
        on < 50.0,
        "the editor part of a frame took {on} ms with the diff on"
    );
    // a wheel over the older version scrolls both sides
    let left = tab(&h).diff.left.unwrap();
    let before = tab(&h).diff.scroll.y;
    h.event(Event::PointerMoved(left.center()));
    h.event(Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: egui::vec2(0.0, -300.0),
        phase: egui::TouchPhase::Move,
        source: egui::MouseWheelSource::Unknown,
        modifiers: Modifiers::NONE,
    });
    wait(&mut h, "scrolled from the left column", |a| {
        a.tabs[a.active].diff.scroll.y > before + 100.0
    });
}

#[test]
fn e2e_save_never_waits_for_the_disk_and_two_quick_saves_land_in_order() {
    let f = Fixture::new("saveworker");
    let mut h = f.harness();
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    open(&mut h, &user);
    h.state_mut().settings.autosave = false;
    // a slow disk: the save thread is busy until `go` is sent
    let (go, gate) = channel::<()>();
    super::saver::run(move || {
        let _ = gate.recv_timeout(Duration::from_secs(20));
    });
    let a = h.state().active;
    h.state_mut().tabs[a].text.push_str("// one\n");
    let started = std::time::Instant::now();
    h.state_mut().save();
    h.state_mut().tabs[a].text.push_str("// two\n");
    // auto-save never writes over a change on disk: the first save is not one
    h.state_mut().autosave();
    steps(&mut h, 2);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the window waited for the disk"
    );
    assert_eq!(
        std::fs::read_to_string(&user).unwrap(),
        USER_KT,
        "not written yet"
    );
    assert!(tab(&h).dirty());
    go.send(()).unwrap();
    h.state_mut().wait_saves();
    // the second save found the first one's text on disk: its own, not a change on disk
    assert!(
        std::fs::read_to_string(&user)
            .unwrap()
            .ends_with("// one\n// two\n")
    );
    assert!(
        !tab(&h).dirty() && !tab(&h).disk_changed,
        "{}",
        h.state().status
    );
    assert_eq!(h.state().status, format!("Saved {}", user.display()));
    assert_eq!(h.state().saves_pending, 0);
}

#[test]
fn e2e_a_file_a_failed_index_change_took_out_is_read_again_without_opening_it() {
    let f = Fixture::new("index-rescan");
    let mut h = f.harness();
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    h.state().index.fail_change_of(&user);
    assert_eq!(h.state().index.read().named("User").count(), 0, "taken out");
    wait(&mut h, "User.kt read again", |a| {
        a.index.read().named("User").count() == 1
    });
}

#[test]
fn e2e_a_cancelled_usage_count_is_asked_again() {
    let f = Fixture::new("count-cancelled");
    let mut h = f.harness();
    let a = h.state_mut();
    let g = a.index.read().generation;
    // a count asked, then its task cancelled by a newer hover: the reply never comes
    let (tx, rx) = channel();
    a.usages.insert("User".into(), (g, None));
    a.usages_rx.push(("User".into(), g, rx));
    drop(tx);
    a.poll_usages();
    assert!(!a.usages.contains_key("User"), "not left counting");
    assert!(a.usages_rx.is_empty());
}

#[test]
fn e2e_open_texts_share_a_tab_text_until_it_changes() {
    let f = Fixture::new("open-texts");
    let mut h = f.harness();
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    open(&mut h, &user);
    let a = h.state_mut();
    let one = a.open_texts(false);
    let two = a.open_texts(false);
    assert!(
        std::sync::Arc::ptr_eq(&one[&user], &two[&user]),
        "no copy per request"
    );
    let i = a.active;
    a.tabs[i].text.push_str("// more\n");
    let three = a.open_texts(false);
    assert!(!std::sync::Arc::ptr_eq(&two[&user], &three[&user]));
    assert!(three[&user].ends_with("// more\n"));
}

#[test]
fn e2e_a_late_search_of_the_old_project_never_fills_the_new_one() {
    let f = Fixture::new("switch-late-find");
    let mut h = f.harness();
    let (tx, rx) = channel();
    h.state_mut().find_rx = Some(rx);
    let other = f.path("app");
    let ctx = h.ctx.clone();
    h.state_mut().switch_project(&other, &ctx);
    let _ = tx.send(vec![found_line(
        &f.path("app/src/main/java/com/demo/data/User.kt"),
        USER_KT,
        0,
    )]);
    steps(&mut h, 5);
    assert!(h.state().find_rx.is_none());
    assert!(
        h.state().found.is_empty(),
        "the old project's answer is not shown"
    );
}

#[test]
fn e2e_a_refactor_waits_for_a_save_on_its_way() {
    let f = Fixture::new("refactor-waits-save");
    let mut h = f.harness();
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    open(&mut h, &user);
    place_cursor(&mut h, "class User(", 6);
    key(&mut h, Modifiers::COMMAND | Modifiers::ALT, Key::R);
    for _ in 0.."User".len() {
        key(&mut h, Modifiers::NONE, Key::Backspace);
    }
    type_text(&mut h, "Person");
    key(&mut h, Modifiers::NONE, Key::Enter);
    wait(&mut h, "rename preview", |a| {
        matches!(a.refactor, Some(refactor_ui::Refactor::Preview(_)))
    });
    // the save thread is busy: a save of User.kt waits behind it
    let (go, gate) = channel::<()>();
    saver::run(move || {
        let _ = gate.recv_timeout(Duration::from_secs(5));
    });
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        let _ = go.send(());
    });
    h.state_mut().save_tab(false);
    assert_eq!(h.state().saves_pending, 1);
    h.get_by_label("Apply").click();
    steps(&mut h, 3);
    h.state_mut().wait_saves();
    let person = f.path("app/src/main/java/com/demo/data/Person.kt");
    assert!(person.exists());
    assert!(!user.exists(), "the save landed first, not after the move");
}

#[test]
fn e2e_usages_are_not_asked_for_while_typing_only_after_the_pause() {
    let f = Fixture::new("xref-typing");
    let mut h = f.harness();
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    open(&mut h, &user);
    let a = h.state_mut();
    let i = a.active;
    a.tabs[i].text.insert_str(0, "// typed above\n");
    a.tabs[i].mark_edited();
    let asked = |a: &App| {
        let t = &a.tabs[a.active];
        a.xref.asked_for(&t.path, t.rev, a.index.read().generation)
    };
    a.count_decl_usages();
    assert!(!asked(a), "its symbols still sit at the old offsets");
    a.settle(true);
    a.count_decl_usages();
    assert!(asked(a));
}

/// Keeps the save thread busy for `ms` (a slow disk): saves sent meanwhile wait behind it.
fn slow_disk(ms: u64) {
    let (go, gate) = channel::<()>();
    saver::run(move || {
        let _ = gate.recv_timeout(Duration::from_secs(5));
    });
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(ms));
        let _ = go.send(());
    });
}

/// Opens User.kt with auto-save off, types `typed` at its end and presses ⌘S on a slow disk.
fn save_on_a_slow_disk(f: &Fixture, h: &mut Harness<'static, App>, typed: &str) -> PathBuf {
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    open(h, &user);
    let a = h.state_mut();
    a.settings.autosave = false;
    let i = a.active;
    a.tabs[i].text.push_str(typed);
    slow_disk(300);
    a.save_tab(false);
    assert_eq!(a.saves_pending, 1);
    user
}

#[test]
fn e2e_revert_right_after_a_save_keeps_what_was_saved() {
    let f = Fixture::new("revert-after-save");
    let mut h = f.harness();
    let user = save_on_a_slow_disk(&f, &mut h, "// one\n");
    h.state_mut().revert();
    h.state_mut().wait_saves();
    let disk = std::fs::read_to_string(&user).unwrap();
    assert!(disk.ends_with("// one\n"));
    assert_eq!(tab(&h).text, disk, "the tab and the disk agree");
    assert!(!tab(&h).dirty());
}

#[test]
fn e2e_a_git_revert_or_checkout_lets_a_save_on_its_way_land_first() {
    let f = Fixture::new("git-after-save");
    let mut h = f.harness();
    let root = f.root.clone();
    let user = save_on_a_slow_disk(&f, &mut h, "// one\n");
    h.state_mut().revert_file(
        &root,
        git_ui::Revert::Working {
            path: user.clone(),
            untracked: false,
        },
    );
    h.state_mut().wait_saves();
    assert_eq!(std::fs::read_to_string(&user).unwrap(), USER_KT, "reverted");
    let a = h.state_mut();
    let i = a.active;
    a.tabs[i].text.push_str("// two\n");
    slow_disk(300);
    a.save_tab(false);
    assert_eq!(a.saves_pending, 1);
    a.git_command(&["checkout", "--", "app/src/main/java/com/demo/data/User.kt"]);
    a.wait_saves();
    assert_eq!(
        std::fs::read_to_string(&user).unwrap(),
        USER_KT,
        "checked out"
    );
}

#[test]
fn e2e_a_save_that_lands_after_more_typing_keeps_the_newer_symbols_in_the_index() {
    let f = Fixture::new("save-index");
    let mut h = f.harness();
    save_on_a_slow_disk(&f, &mut h, "// one\n");
    let a = h.state_mut();
    let i = a.active;
    a.tabs[i].text.push_str("class Typed\n");
    a.tabs[i].mark_edited();
    a.settle(true);
    a.wait_saves();
    a.index.settle();
    assert_eq!(a.index.read().named("Typed").count(), 1);
}

/// Whether the index holds the very symbols the tab of `path` read (not a copy, not a second
/// read of the text).
fn index_shares_tab_symbols(a: &App, path: &Path) -> bool {
    let t = a.tabs.iter().find(|t| t.path == path).unwrap();
    let index = a.index.read();
    let (tab, indexed): (&crate::kotlin::FileSyms, &crate::kotlin::FileSyms) =
        (&t.syms, &index.files[path]);
    std::ptr::eq(tab, indexed)
}

#[test]
fn e2e_a_typing_pause_gives_the_index_the_tabs_symbols_not_a_copy() {
    let f = Fixture::new("settle-shares");
    let mut h = f.harness();
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    open(&mut h, &user);
    let a = h.state_mut();
    let i = a.active;
    a.tabs[i].text.push_str("class Typed\n");
    a.tabs[i].mark_edited();
    a.settle(true);
    a.index.settle();
    assert_eq!(a.index.read().named("Typed").count(), 1);
    assert!(index_shares_tab_symbols(a, &user));
}

#[test]
fn e2e_a_disk_event_with_the_tabs_own_text_keeps_the_tabs_symbols_in_the_index() {
    let f = Fixture::new("disk-shares");
    let mut h = f.harness();
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    open(&mut h, &user);
    // the file is written again with the tab's text (a save of it, a tool touching it)
    watcher_live(&mut h, &user);
    h.state().index.settle();
    assert!(index_shares_tab_symbols(h.state(), &user));
}

#[test]
fn e2e_a_project_index_ready_after_a_tab_opened_takes_the_tabs_symbols() {
    let f = Fixture::new("ready-shares");
    let mut h = f.harness();
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    open(&mut h, &user);
    // the project's index arrives from its worker while the tab is open
    let project = crate::project::Project::open(&f.root);
    let index = Index::build(&project.files);
    let ctx = h.ctx.clone();
    let a = h.state_mut();
    a.project_ready(project, index, None, &ctx);
    a.index.settle();
    assert!(index_shares_tab_symbols(a, &user));
}

#[test]
fn e2e_a_save_keeps_the_symbols_read_at_the_typing_pause_without_reading_again() {
    let f = Fixture::new("save-no-rescan");
    let mut h = f.harness();
    let user = save_on_a_slow_disk(&f, &mut h, "class Saved\n");
    let a = h.state_mut();
    let i = a.active;
    a.tabs[i].mark_edited();
    a.settle(true);
    a.index.settle();
    a.wait_saves();
    a.index.settle();
    assert_eq!(a.index.read().named("Saved").count(), 1);
    assert!(
        index_shares_tab_symbols(a, &user),
        "the save read the text again"
    );
}

#[test]
fn e2e_closing_a_tab_right_after_save_waits_for_the_save() {
    let f = Fixture::new("close-after-save");
    let mut h = f.harness();
    let user = save_on_a_slow_disk(&f, &mut h, "// one\n");
    let a = h.state_mut();
    a.close_tab(a.active);
    assert!(
        a.tabs.iter().any(|t| t.path == user),
        "it closes once the save landed"
    );
    a.wait_saves();
    assert!(a.tabs.iter().all(|t| t.path != user), "{}", a.status);
    assert!(
        std::fs::read_to_string(&user)
            .unwrap()
            .ends_with("// one\n")
    );
}

#[test]
fn e2e_a_close_whose_save_did_not_write_needs_a_fresh_choice() {
    let f = Fixture::new("close-save-failed");
    let mut h = f.harness();
    let user = f.path("app/src/main/java/com/demo/data/User.kt");
    open(&mut h, &user);
    let a = h.state_mut();
    a.settings.autosave = false;
    let i = a.active;
    a.tabs[i].text.push_str("// typed\n");
    // "Keep my version (save)" was picked and the save did not write
    a.close_asked = true;
    a.close_after_saves = true;
    steps(&mut h, 2);
    assert!(
        h.query_by_label("Keep a copy and close").is_some(),
        "asked again"
    );
    // the edits go away some other way (saved elsewhere, reverted)
    let a = h.state_mut();
    let i = a.active;
    a.tabs[i].saved = a.tabs[i].text.clone();
    steps(&mut h, 3);
    assert!(!h.state().close_ok, "no close without a fresh choice");
}

#[test]
fn e2e_a_translation_write_lets_a_save_of_its_file_land_first() {
    let f = Fixture::new("translation-after-save");
    let hindi = f.path("app/src/main/res/values-hi/strings.xml");
    for (dir, text) in [("values", "Hi"), ("values-hi", "Namaste")] {
        let p = f.path(&format!("app/src/main/res/{dir}/strings.xml"));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let xml = format!(
            "<resources>\n    <string name=\"hi\">{text}</string>\n    <string name=\"bye\">{text}</string>\n</resources>\n"
        );
        std::fs::write(&p, xml).unwrap();
    }
    let mut h = f.harness();
    wait(&mut h, "resources", |a| {
        a.res.get("string", "hi").len() == 2
    });
    let ctx = h.ctx.clone();
    h.state_mut().run(Action::Translations, &ctx);
    steps(&mut h, 3);
    open(&mut h, &hindi);
    let a = h.state_mut();
    a.settings.autosave = false;
    let i = a.active;
    a.tabs[i].text = a.tabs[i].text.replacen("Namaste", "Pranam", 1);
    slow_disk(300);
    a.save_tab(false);
    a.write_translation("bye".into(), 1, "Alvida");
    a.wait_saves();
    let disk = std::fs::read_to_string(&hindi).unwrap();
    assert!(disk.contains("Pranam") && disk.contains("Alvida"), "{disk}");
}

#[test]
fn a_found_line_offset_from_another_version_of_the_file_never_panics() {
    let l = found_line(Path::new("/p/A.kt"), "é\nab", 1);
    assert_eq!((l.offset, l.line, l.text.as_str()), (0, 1, "é"));
    let l = found_line(Path::new("/p/A.kt"), "ab", 9);
    assert_eq!((l.offset, l.line), (2, 1));
}
