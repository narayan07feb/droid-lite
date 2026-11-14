//! The Project tree, in two views like Android Studio:
//! - **Android**: per module `manifests`, `kotlin` (packages as dotted names) and `res`, one group per
//!   source set (`kotlin (test)`, `kotlin (commonMain)`), then the module's own files.
//! - **Project**: the real folders, with `src/` left out (source sets sit under the module) and
//!   single-child folder chains joined (`kotlin · co.app.feature`).
//!
//! Both nest modules by their Gradle path (`:feature:lending:data` → feature › lending › data), and
//! colour source sets: tests green, platform sets (`androidMain`, `iosMain`…) blue.

use crate::icons as ic;
use crate::project::Project;
use egui::{Color32, RichText, Ui};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// A module's content: its folders / groups and its own files.
type Content = (Vec<Node>, Vec<PathBuf>);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(super) enum View {
    #[default]
    Android,
    Project,
}

impl View {
    pub fn name(self) -> &'static str {
        match self {
            View::Android => "Android",
            View::Project => "Project",
        }
    }
}

#[derive(Default, Debug)]
pub(super) struct Node {
    pub label: String,
    /// Stable id for the open / closed state.
    key: String,
    icon: &'static str,
    tint: Option<Color32>,
    /// A Gradle module (shown bold).
    module: bool,
    pub dirs: Vec<Node>,
    pub files: Vec<PathBuf>,
}

impl Node {
    pub fn contains(&self, f: &Path) -> bool {
        self.files.iter().any(|p| p == f) || self.dirs.iter().any(|d| d.contains(f))
    }
}

const TEST: Color32 = Color32::from_rgb(0x5F, 0xB8, 0x65);
const PLATFORM: Color32 = Color32::from_rgb(0x58, 0x9D, 0xF6);

/// Tests green, platform source sets blue, `main` / `commonMain` plain.
fn set_tint(set: &str) -> Option<Color32> {
    if set.to_ascii_lowercase().contains("test") {
        Some(TEST)
    } else if set == "main" || set == "commonMain" {
        None
    } else {
        Some(PLATFORM)
    }
}

/// Order of source sets inside a module: main code first, platform sets, tests last.
fn set_rank(set: &str) -> (u8, String) {
    let r = match set {
        "main" | "commonMain" => 0,
        s if s.to_ascii_lowercase().contains("test") => 2,
        _ => 1,
    };
    (r, set.to_string())
}

/// A folder tree under construction.
#[derive(Default)]
struct Dir {
    dirs: BTreeMap<String, Dir>,
    files: Vec<PathBuf>,
}

impl Dir {
    fn insert(&mut self, comps: &[String], f: &Path) {
        match comps.split_first() {
            Some((c, rest)) if !rest.is_empty() => {
                self.dirs.entry(c.clone()).or_default().insert(rest, f)
            }
            _ => self.files.push(f.to_path_buf()),
        }
    }

    /// Converts to nodes, joining single-child chains; after `kotlin` / `java` the join is a dot.
    fn into_nodes(self, key: &str) -> Content {
        let dirs = self
            .dirs
            .into_iter()
            .map(|(name, mut d)| {
                let mut label = name.clone();
                let mut pkg = name == "kotlin" || name == "java";
                while d.files.is_empty() && d.dirs.len() == 1 {
                    let (n, next) = d.dirs.pop_first().expect("one child");
                    label = if pkg && label.contains(" · ") {
                        format!("{label}.{n}")
                    } else if pkg {
                        format!("{label} · {n}")
                    } else {
                        format!("{label}/{n}")
                    };
                    pkg |= n == "kotlin" || n == "java";
                    d = next;
                }
                let key = format!("{key}/{label}");
                let (dirs, files) = d.into_nodes(&key);
                Node {
                    label,
                    key,
                    icon: ic::FOLDER_SIMPLE,
                    dirs,
                    files,
                    ..Default::default()
                }
            })
            .collect();
        let mut files = self.files;
        files.sort();
        (dirs, files)
    }
}

fn comps(rel: &Path) -> Vec<String> {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect()
}

/// A labelled group node (`manifests`, `kotlin (test)`, `res`).
fn group(label: String, key: &str, icon: &'static str, tint: Option<Color32>, d: Dir) -> Node {
    let key = format!("{key}/{label}");
    let (dirs, files) = d.into_nodes(&key);
    Node {
        label,
        key,
        icon,
        tint,
        dirs,
        files,
        ..Default::default()
    }
}

/// The content of one module in the Android view.
fn android_module(files: &[(Vec<String>, &Path)], key: &str) -> Content {
    // (order, source set, kind) → folder tree
    let mut groups: BTreeMap<(u8, (u8, String), String), Dir> = BTreeMap::new();
    let mut packages: BTreeMap<(u8, String), BTreeMap<String, Vec<PathBuf>>> = BTreeMap::new();
    let mut rest = Dir::default();
    for (c, f) in files {
        match c.as_slice() {
            [src, set, manifest] if src == "src" && manifest == "AndroidManifest.xml" => {
                groups
                    .entry((0, set_rank(set), "manifests".into()))
                    .or_default()
                    .files
                    .push(f.to_path_buf());
            }
            [src, set, kind, path @ ..] if src == "src" && (kind == "kotlin" || kind == "java") => {
                let pkg = path[..path.len().saturating_sub(1)].join(".");
                packages
                    .entry(set_rank(set))
                    .or_default()
                    .entry(pkg)
                    .or_default()
                    .push(f.to_path_buf());
            }
            [src, set, kind, path @ ..] if src == "src" && !path.is_empty() => {
                let order = if kind == "res" { 2 } else { 3 };
                groups
                    .entry((order, set_rank(set), kind.clone()))
                    .or_default()
                    .insert(path, f);
            }
            _ => rest.insert(c, f),
        }
    }
    let suffix = |set: &str| {
        if set == "main" {
            String::new()
        } else {
            format!(" ({set})")
        }
    };
    let mut out = Vec::new();
    // manifests first (one group, sets as files), then code, res and the rest per source set
    let mut manifests = Dir::default();
    let mut others = Vec::new();
    for ((order, (_, set), kind), d) in groups {
        if order == 0 {
            for f in d.files {
                manifests.dirs.entry(set.clone()).or_default().files.push(f);
            }
        } else {
            others.push(group(
                format!("{kind}{}", suffix(&set)),
                key,
                ic::FOLDER_SIMPLE,
                set_tint(&set),
                d,
            ));
        }
    }
    if !manifests.dirs.is_empty() {
        // a single `main` manifest sits directly in the group, like Android Studio
        if manifests.dirs.len() == 1 && manifests.dirs.contains_key("main") {
            manifests = manifests.dirs.remove("main").unwrap_or_default();
        }
        out.push(group(
            "manifests".into(),
            key,
            ic::FOLDER_SIMPLE,
            None,
            manifests,
        ));
    }
    for ((_, set), pkgs) in packages {
        let gkey = format!("{key}/kotlin{}", suffix(&set));
        let mut root_files = Vec::new();
        let dirs = pkgs
            .into_iter()
            .filter_map(|(pkg, mut files)| {
                files.sort();
                if pkg.is_empty() {
                    root_files = files;
                    return None;
                }
                Some(Node {
                    key: format!("{gkey}/{pkg}"),
                    label: pkg,
                    icon: ic::PACKAGE_SMALL,
                    files,
                    ..Default::default()
                })
            })
            .collect();
        out.push(Node {
            label: format!("kotlin{}", suffix(&set)),
            key: gkey,
            icon: ic::FOLDER_SIMPLE,
            tint: set_tint(&set),
            dirs,
            files: root_files,
            ..Default::default()
        });
    }
    // code before res: `others` holds res (order 2) and the rest (order 3), already sorted
    out.extend(others);
    let (dirs, files) = rest.into_nodes(key);
    out.extend(dirs);
    (out, files)
}

/// The content of one module in the Project view: real folders, `src/` left out.
fn project_module(files: &[(Vec<String>, &Path)], key: &str) -> Content {
    let mut sets: BTreeMap<(u8, String), Dir> = BTreeMap::new();
    let mut rest = Dir::default();
    for (c, f) in files {
        match c.as_slice() {
            [src, set, path @ ..] if src == "src" && !path.is_empty() => {
                sets.entry(set_rank(set)).or_default().insert(path, f);
            }
            _ => rest.insert(c, f),
        }
    }
    let mut out: Vec<Node> = sets
        .into_iter()
        .map(|((_, set), d)| group(set.clone(), key, ic::FOLDER_SIMPLE, set_tint(&set), d))
        .collect();
    let (dirs, files) = rest.into_nodes(key);
    out.extend(dirs);
    (out, files)
}

/// Builds the tree: modules nested by Gradle path, the root project first.
pub(super) fn build(p: &Project, view: View) -> Vec<Node> {
    let mut by_module: HashMap<&str, Vec<(Vec<String>, &Path)>> = HashMap::new();
    for f in &p.files {
        let Some(m) = p.module_of(f) else { continue };
        by_module
            .entry(m.name.as_str())
            .or_default()
            .push((comps(f.strip_prefix(&m.dir).unwrap_or(f)), f.as_path()));
    }
    let content = |name: &str| {
        let files = by_module.get(name).map_or(&[][..], Vec::as_slice);
        match view {
            View::Android => android_module(files, name),
            View::Project => project_module(files, name),
        }
    };
    // modules as a tree of Gradle path segments
    #[derive(Default)]
    struct Seg {
        children: BTreeMap<String, Seg>,
        module: Option<String>,
    }
    let mut root = Seg::default();
    for m in p.modules.iter().filter(|m| m.name != ":") {
        let mut s = &mut root;
        for part in m.name.trim_start_matches(':').split(':') {
            s = s.children.entry(part.to_string()).or_default();
        }
        s.module = Some(m.name.clone());
    }
    fn to_nodes(s: Seg, path: &str, content: &dyn Fn(&str) -> Content) -> Vec<Node> {
        s.children
            .into_iter()
            .map(|(name, child)| {
                let key = format!("{path}:{name}");
                let module = child.module.clone();
                let (mut dirs, files) = module.as_deref().map(content).unwrap_or_default();
                dirs.extend(to_nodes(child, &key, content));
                Node {
                    label: name,
                    icon: if module.is_some() {
                        ic::PACKAGE
                    } else {
                        ic::FOLDER_SIMPLE
                    },
                    module: module.is_some(),
                    key,
                    dirs,
                    files,
                    ..Default::default()
                }
            })
            .collect()
    }
    let mut out = Vec::new();
    let (dirs, files) = content(":");
    if !dirs.is_empty() || !files.is_empty() {
        out.push(Node {
            label: format!("{} (root)", p.name),
            key: ":".into(),
            icon: ic::PACKAGE,
            module: true,
            dirs,
            files,
            ..Default::default()
        });
    }
    out.extend(to_nodes(root, "", &content));
    out
}

/// Git status letter by file, for colouring the tree (M modified, A / ? new).
pub(super) type StatusMap = HashMap<PathBuf, char>;

/// The tree's selected file and the file being located, if any.
#[derive(Clone, Copy)]
pub(super) struct Sel<'a> {
    pub active: Option<&'a Path>,
    pub locate: Option<&'a Path>,
    pub status: &'a StatusMap,
    /// Declarations by file, for the Activity / ViewModel / Composable icons.
    pub syms: &'a HashMap<PathBuf, crate::kotlin::FileSyms>,
    /// Right-click → New file in this folder.
    pub new_in: &'a std::cell::Cell<Option<PathBuf>>,
}

/// Draws nodes; a clicked file goes into `open`.
pub(super) fn show(ui: &mut Ui, nodes: &[Node], sel: Sel, open: &mut Option<PathBuf>) {
    for n in nodes {
        let mut text = RichText::new(format!("{}  {}", n.icon, n.label));
        if n.module {
            text = text.strong();
        }
        if let Some(t) = n.tint {
            text = text.color(t);
        }
        egui::CollapsingHeader::new(text)
            .id_salt(&n.key)
            .open(sel.locate.filter(|f| n.contains(f)).map(|_| true))
            .show(ui, |ui| {
                show(ui, &n.dirs, sel, open);
                for f in &n.files {
                    file_row(ui, f, sel, open);
                }
            });
    }
}

fn file_row(ui: &mut Ui, f: &Path, sel: Sel, open: &mut Option<PathBuf>) {
    let name = f
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (icon, color) = super::assist::kind_icon(f, sel.syms.get(f));
    let mut job = egui::text::LayoutJob::default();
    let fmt = |color| egui::TextFormat {
        font_id: egui::FontId::proportional(13.0),
        color,
        ..Default::default()
    };
    job.append(icon, 0.0, fmt(color));
    // like Android Studio: modified files blue, new files green
    let name_color = match sel.status.get(f) {
        Some('M' | 'R') => Color32::from_rgb(0x58, 0x9D, 0xF6),
        Some('A' | '?') => Color32::from_rgb(0x5F, 0xB8, 0x65),
        _ => crate::theme::t().text,
    };
    job.append(&format!("  {name}"), 0.0, fmt(name_color));
    let r = ui.selectable_label(sel.active == Some(f), job);
    if sel.locate == Some(f) {
        r.scroll_to_me(Some(egui::Align::Center));
    }
    if r.clicked() {
        *open = Some(f.to_path_buf());
    }
    r.context_menu(|ui| {
        if ui
            .button(format!(
                "{}  New Kotlin file here…",
                crate::icons::FILE_PLUS
            ))
            .clicked()
        {
            sel.new_in.set(f.parent().map(Path::to_path_buf));
            ui.close();
        }
    });
}

/// Files whose name contains `query` (any case), best first: name starts with it, then shorter.
pub(super) fn filter<'a>(files: &'a [PathBuf], query: &str, limit: usize) -> Vec<&'a PathBuf> {
    let q = query.to_lowercase();
    let mut hits: Vec<(usize, usize, &PathBuf)> = files
        .iter()
        .filter_map(|f| {
            let name = f.file_name()?.to_string_lossy().to_lowercase();
            let at = name.find(&q)?;
            Some((usize::from(at != 0), name.len(), f))
        })
        .collect();
    hits.sort();
    hits.into_iter().take(limit).map(|(_, _, f)| f).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(nodes: &[Node]) -> Vec<&str> {
        nodes.iter().map(|n| n.label.as_str()).collect()
    }

    fn find<'a>(nodes: &'a [Node], label: &str) -> &'a Node {
        nodes
            .iter()
            .find(|n| n.label == label)
            .unwrap_or_else(|| panic!("{label} in {:?}", labels(nodes)))
    }

    fn rel(c: &str) -> Vec<String> {
        c.split('/').map(String::from).collect()
    }

    #[test]
    fn android_view_groups_manifests_packages_res_and_source_sets() {
        let paths = [
            "src/main/AndroidManifest.xml",
            "src/main/kotlin/co/app/card/data/Repo.kt",
            "src/main/kotlin/co/app/card/data/Api.kt",
            "src/main/kotlin/co/app/card/ui/Screen.kt",
            "src/test/kotlin/co/app/card/data/RepoTest.kt",
            "src/main/res/values/strings.xml",
            "build.gradle.kts",
        ];
        let files: Vec<(Vec<String>, PathBuf)> =
            paths.iter().map(|p| (rel(p), PathBuf::from(p))).collect();
        let files: Vec<(Vec<String>, &Path)> = files
            .iter()
            .map(|(c, p)| (c.clone(), p.as_path()))
            .collect();
        let (dirs, root_files) = android_module(&files, ":card");
        assert_eq!(
            labels(&dirs),
            ["manifests", "kotlin", "kotlin (test)", "res"]
        );
        assert_eq!(find(&dirs, "manifests").files.len(), 1);
        let kotlin = find(&dirs, "kotlin");
        assert_eq!(labels(&kotlin.dirs), ["co.app.card.data", "co.app.card.ui"]);
        assert_eq!(find(&kotlin.dirs, "co.app.card.data").files.len(), 2);
        assert_eq!(find(&dirs, "kotlin (test)").tint, Some(TEST));
        assert_eq!(root_files, [PathBuf::from("build.gradle.kts")]);
    }

    #[test]
    fn kmp_source_sets_are_labelled_and_ordered() {
        let paths = [
            "src/iosMain/kotlin/a/Platform.ios.kt",
            "src/commonTest/kotlin/a/T.kt",
            "src/commonMain/kotlin/a/Platform.kt",
            "src/androidMain/kotlin/a/Platform.android.kt",
        ];
        let files: Vec<(Vec<String>, PathBuf)> =
            paths.iter().map(|p| (rel(p), PathBuf::from(p))).collect();
        let files: Vec<(Vec<String>, &Path)> = files
            .iter()
            .map(|(c, p)| (c.clone(), p.as_path()))
            .collect();
        let (dirs, _) = android_module(&files, ":shared");
        assert_eq!(
            labels(&dirs),
            [
                "kotlin (commonMain)",
                "kotlin (androidMain)",
                "kotlin (iosMain)",
                "kotlin (commonTest)"
            ]
        );
        assert_eq!(find(&dirs, "kotlin (iosMain)").tint, Some(PLATFORM));
        assert_eq!(find(&dirs, "kotlin (commonMain)").tint, None);
        // the Project view leaves out src/ and joins the package chain
        let (dirs, _) = project_module(&files, ":shared");
        assert_eq!(
            labels(&dirs),
            ["commonMain", "androidMain", "iosMain", "commonTest"]
        );
        assert_eq!(labels(&find(&dirs, "commonMain").dirs), ["kotlin · a"]);
    }

    #[test]
    fn filter_ranks_name_prefix_first() {
        let files: Vec<PathBuf> = [
            "a/LoginScreen.kt",
            "b/UserLogin.kt",
            "c/Login.kt",
            "d/Other.kt",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        let hits: Vec<&str> = filter(&files, "login", 10)
            .iter()
            .map(|p| p.to_str().unwrap())
            .collect();
        assert_eq!(hits, ["c/Login.kt", "a/LoginScreen.kt", "b/UserLogin.kt"]);
    }
}
