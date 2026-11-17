//! Accuracy of usages, go-to-declaration and Hilt links on a real project, against the expected
//! lists in `tests/fixtures/accuracy/*.json` (written by `scripts/accuracy_expected.py`).
//!
//!     scripts/accuracy_expected.py snapshot <BranchAndroid checkout> /tmp/accuracy-project
//!     DROID_LITE_ACCURACY_PROJECT=/tmp/accuracy-project \
//!         cargo test --release accuracy_on_a_real_project -- --ignored --nocapture

use crate::index::{Hit, Index};
use crate::project::is_code;
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> Value {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/accuracy")
        .join(name);
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

fn line_of(text: &str, o: usize) -> usize {
    text[..o].matches('\n').count() + 1
}

/// `rel:line:n`: the n-th place of the name on its line, like the expected lists.
fn place(root: &Path, path: &Path, text: &str, name: &str, o: usize) -> String {
    let line = line_of(text, o);
    let n = crate::refactor::occurrences(text, name)
        .iter()
        .filter(|r| r.start <= o && line_of(text, r.start) == line)
        .count();
    format!(
        "{}:{line}:{n}",
        path.strip_prefix(root).unwrap_or(path).display()
    )
}

struct Project {
    root: PathBuf,
    index: Index,
    files: Vec<PathBuf>,
    texts: HashMap<PathBuf, String>,
}

impl Project {
    fn text(&self, p: &Path) -> &str {
        self.texts.get(p).map_or("", String::as_str)
    }
}

fn open(root: &Path) -> Project {
    let p = crate::project::Project::open(root);
    let files: Vec<PathBuf> = p.files.iter().filter(|f| is_code(f)).cloned().collect();
    let index = Index::build(&files);
    let texts = files
        .iter()
        .filter_map(|f| Some((f.clone(), std::fs::read_to_string(f).ok()?)))
        .collect();
    Project {
        root: root.to_path_buf(),
        index,
        files,
        texts,
    }
}

/// The declaration named `name` on `line` of `rel`.
fn decl_at(p: &Project, rel: &str, name: &str, line: usize) -> Option<(PathBuf, usize)> {
    let path = p.root.join(rel);
    let f = p.index.files.get(&path)?;
    let text = p.text(&path);
    f.decls
        .iter()
        .filter(|d| d.name == name)
        .min_by_key(|d| line_of(text, d.offset).abs_diff(line))
        .map(|d| (path.clone(), d.offset))
}

#[derive(Default)]
struct Score {
    tp: usize,
    fp: usize,
    fn_: usize,
}

impl Score {
    fn p(&self) -> f64 {
        self.tp as f64 / (self.tp + self.fp).max(1) as f64
    }
    fn r(&self) -> f64 {
        self.tp as f64 / (self.tp + self.fn_).max(1) as f64
    }
}

#[test]
#[ignore = "needs a real project: DROID_LITE_ACCURACY_PROJECT"]
fn accuracy_on_a_real_project() {
    let Ok(root) = std::env::var("DROID_LITE_ACCURACY_PROJECT") else {
        eprintln!("DROID_LITE_ACCURACY_PROJECT is not set");
        return;
    };
    let t = std::time::Instant::now();
    let p = open(&std::fs::canonicalize(&root).unwrap());
    eprintln!("indexed {} files in {:?}", p.files.len(), t.elapsed());
    let (usages, defs) = usages_and_definitions(&p);
    let hilt = hilt(&p);
    eprintln!(
        "usages: precision {:.1}% recall {:.1}% · definition {:.1}% · hilt {:.1}%",
        usages.p() * 100.0,
        usages.r() * 100.0,
        defs * 100.0,
        hilt * 100.0
    );
    let min = std::env::var("DROID_LITE_ACCURACY_MIN").map_or(0.95, |v| v.parse().unwrap());
    assert!(usages.p() >= min && usages.r() >= min && defs >= min && hilt >= min);
}

fn usages_and_definitions(p: &Project) -> (Score, f64) {
    let expected = fixture("usages.json");
    let mut score = Score::default();
    let (mut hit, mut tried) = (0, 0);
    for s in expected["symbols"].as_array().unwrap() {
        let name = s["name"].as_str().unwrap();
        let line = s["line"].as_u64().unwrap() as usize;
        let rel = s["file"].as_str().unwrap();
        let want: BTreeSet<String> = s["usages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|u| u.as_str().unwrap().to_string())
            .collect();
        let target = decl_at(p, rel, name, line);
        let got: BTreeSet<String> = match &target {
            Some((path, offset)) => engine::usages(p, path, *offset)
                .into_iter()
                .map(|(f, o)| place(&p.root, &f, p.text(&f), name, o))
                .collect(),
            None => BTreeSet::new(),
        };
        let (tp, fp, fn_) = (
            got.intersection(&want).count(),
            got.difference(&want).count(),
            want.difference(&got).count(),
        );
        score.tp += tp;
        score.fp += fp;
        score.fn_ += fn_;
        if fp + fn_ > 0 || target.is_none() {
            eprintln!(
                "usages {name} ({rel}:{line}){}: +{fp} -{fn_}  extra {:?}  missed {:?}",
                if target.is_none() { " NOT INDEXED" } else { "" },
                got.difference(&want).take(4).collect::<Vec<_>>(),
                want.difference(&got).take(4).collect::<Vec<_>>()
            );
        }
        // go to declaration from each usage
        let jumps = s["jump"].as_object().unwrap();
        for u in &want {
            let mut parts = u.rsplitn(3, ':');
            let (nth, l, f) = (
                parts.next().unwrap(),
                parts.next().unwrap(),
                parts.next().unwrap(),
            );
            let path = p.root.join(f);
            let text = p.text(&path);
            let at = crate::refactor::occurrences(text, name)
                .into_iter()
                .filter(|r| line_of(text, r.start) == l.parse::<usize>().unwrap())
                .nth(nth.parse::<usize>().unwrap() - 1)
                .map(|r| r.start);
            let expect = match jumps.get(u.as_str()) {
                Some(to) => {
                    let (tf, tl) = to.as_str().unwrap().rsplit_once(':').unwrap();
                    decl_at(p, tf, name, tl.parse().unwrap())
                }
                None => target.clone(),
            };
            tried += 1;
            let got = at.and_then(|at| engine::definition(p, &path, at));
            if got.is_some() && got == expect {
                hit += 1;
            } else if std::env::var("DROID_LITE_ACCURACY_VERBOSE").is_ok() {
                eprintln!("definition {name} at {u}: got {got:?}");
            }
        }
    }
    eprintln!("definition: {hit}/{tried}");
    (score, hit as f64 / tried.max(1) as f64)
}

/// A provider is right when its "injected in" places are the graph's and each of those places
/// links back to exactly this provider.
fn hilt(p: &Project) -> f64 {
    let texts = crate::resolve::Texts::default();
    let r = crate::resolve::Resolver::new(&p.index, &texts);
    let mut graph = crate::resolve::HiltGraph::default();
    let t = std::time::Instant::now();
    while !graph.refresh(&r, &|| false) {}
    eprintln!(
        "hilt graph: {} files in {:?}",
        graph.files.len(),
        t.elapsed()
    );
    let g = &graph;
    let expected = fixture("hilt.json");
    let sample = expected["sample"].as_array().unwrap();
    // classes and functions in the debug graph: an injection site elsewhere (unused by the app,
    // or another variant) is not in the truth, so it is reported but not counted
    let in_graph: BTreeSet<(String, usize)> = expected["graph_owners"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| {
            (
                o[0].as_str().unwrap().to_string(),
                o[1].as_u64().unwrap() as usize,
            )
        })
        .collect();
    let mut outside = 0;
    let mut right = 0;
    for e in sample {
        let rel = e["file"].as_str().unwrap();
        let name = e["name"].as_str().unwrap();
        let line = e["line"].as_u64().unwrap() as usize;
        let want: BTreeSet<(String, usize)> = e["sites"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| {
                (
                    s["file"].as_str().unwrap().to_string(),
                    s["line"].as_u64().unwrap_or(0) as usize,
                )
            })
            .collect();
        let Some((path, offset)) = decl_at(p, rel, name, line) else {
            eprintln!("hilt {name} ({rel}:{line}): NOT INDEXED");
            continue;
        };
        let owner = |h: &Hit| {
            let rel = h
                .path
                .strip_prefix(&p.root)
                .unwrap_or(&h.path)
                .display()
                .to_string();
            (rel, engine::owner_line(p, &h.path, h.offset))
        };
        let all: BTreeSet<(String, usize)> = engine::injected_in(g, &path, offset)
            .iter()
            .map(owner)
            .collect();
        let got: BTreeSet<(String, usize)> = all
            .iter()
            .filter(|o| in_graph.contains(*o))
            .cloned()
            .collect();
        outside += all.len() - got.len();
        let mut back_wrong = Vec::new();
        for s in e["sites"].as_array().unwrap() {
            let sp = p.root.join(s["file"].as_str().unwrap());
            let sl = s["line"].as_u64().unwrap_or(0) as usize;
            let owner_name = s["owner"].as_str().unwrap();
            let param = s["param"].as_u64().map(|x| x as usize);
            let providers: BTreeSet<(String, usize)> =
                match decl_at(p, s["file"].as_str().unwrap(), owner_name, sl) {
                    Some((sp2, so)) => engine::providers_of(g, &sp2, so, param)
                        .iter()
                        .map(owner)
                        .collect(),
                    None => BTreeSet::new(),
                };
            let _ = sp;
            let want: BTreeSet<(String, usize)> = s["providers"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| {
                    (
                        x[0].as_str().unwrap().to_string(),
                        x[1].as_u64().unwrap() as usize,
                    )
                })
                .collect();
            if providers != want {
                back_wrong.push((owner_name.to_string(), param, providers));
            }
        }
        if got == want && back_wrong.is_empty() {
            right += 1;
        } else {
            eprintln!(
                "hilt {name} ({rel}:{line}): injected-in extra {:?} missed {:?}; wrong provider links {}: {:?}",
                got.difference(&want).take(3).collect::<Vec<_>>(),
                want.difference(&got).take(3).collect::<Vec<_>>(),
                back_wrong.len(),
                back_wrong.iter().take(2).collect::<Vec<_>>()
            );
        }
    }
    eprintln!(
        "hilt: {right}/{} ({outside} injection sites outside the debug graph not counted)",
        sample.len()
    );
    right as f64 / sample.len() as f64
}

/// The engine under test.
mod engine {
    use super::*;

    use crate::resolve::{Res, Resolver, Texts};

    fn texts() -> &'static Texts {
        static T: std::sync::OnceLock<Texts> = std::sync::OnceLock::new();
        T.get_or_init(Texts::default)
    }

    /// Places that use the declaration at `offset` of `path` (not the declaration itself).
    pub fn usages(p: &Project, path: &Path, offset: usize) -> Vec<(PathBuf, usize)> {
        let r = Resolver::new(&p.index, texts());
        r.usages(&(path.to_path_buf(), offset), &p.files, &|| false)
    }

    /// Where ⌘-click on the name at `at` goes when it opens one declaration.
    pub fn definition(p: &Project, path: &Path, at: usize) -> Option<(PathBuf, usize)> {
        let r = Resolver::new(&p.index, texts());
        let f = r.file(path)?;
        match r.resolve(&f, at) {
            Res::Decls(v) if v.len() == 1 => Some(v[0].clone()),
            _ => None,
        }
    }

    /// The line of the declaration a site hit stands for (a class, a function or a field).
    pub fn owner_line(p: &Project, path: &Path, offset: usize) -> usize {
        line_of(p.text(path), offset)
    }

    fn hit((path, offset): &(PathBuf, usize)) -> Hit {
        Hit {
            name: String::new(),
            detail: String::new(),
            path: path.clone(),
            offset: *offset,
            score: 0,
        }
    }

    pub fn injected_in(g: &crate::resolve::HiltGraph, path: &Path, offset: usize) -> Vec<Hit> {
        let Some(b) = g.binding(&(path.to_path_buf(), offset)) else {
            return Vec::new();
        };
        g.injected_in(b).iter().map(|s| hit(&s.owner)).collect()
    }

    pub fn providers_of(
        g: &crate::resolve::HiltGraph,
        path: &Path,
        offset: usize,
        param: Option<usize>,
    ) -> Vec<Hit> {
        let owner = (path.to_path_buf(), offset);
        g.sites_of(&owner)
            .into_iter()
            .filter(|s| s.param == param)
            .flat_map(|s| g.providers_of(s))
            .map(|b| hit(&b.decl))
            .collect()
    }
}

/// `DROID_LITE_DEBUG=rel/File.kt:line:name` prints what the name there resolves to.
#[test]
#[ignore = "debugging aid"]
fn debug_one_place() {
    let (Ok(root), Ok(what)) = (
        std::env::var("DROID_LITE_ACCURACY_PROJECT"),
        std::env::var("DROID_LITE_DEBUG"),
    ) else {
        return;
    };
    let p = open(&std::fs::canonicalize(&root).unwrap());
    let texts = crate::resolve::Texts::default();
    let r = crate::resolve::Resolver::new(&p.index, &texts);
    for w in what.split(',') {
        let mut it = w.rsplitn(3, ':');
        let (name, line, rel) = (it.next().unwrap(), it.next().unwrap(), it.next().unwrap());
        let path = p.root.join(rel);
        let f = r.file(&path).unwrap();
        for o in crate::refactor::occurrences(&f.text, name) {
            if line_of(&f.text, o.start) == line.parse::<usize>().unwrap() {
                let res = r.resolve(&f, o.start);
                let res = match res {
                    crate::resolve::Res::Decls(v) => v
                        .iter()
                        .map(|(p2, off)| {
                            format!(
                                "{}:{}",
                                p2.file_name().unwrap().to_string_lossy(),
                                line_of(p.text(p2), *off)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(", "),
                    other => format!("{other:?}"),
                };
                eprintln!(
                    "{w} @{}: {res}  [{}]",
                    o.start,
                    r.debug_receiver(&f, o.start)
                );
            }
        }
    }
}

/// `DROID_LITE_DEBUG_HILT=rel/File.kt,...` prints the Hilt bindings and sites each file adds.
#[test]
#[ignore = "debugging aid"]
fn debug_hilt_file() {
    let (Ok(root), Ok(what)) = (
        std::env::var("DROID_LITE_ACCURACY_PROJECT"),
        std::env::var("DROID_LITE_DEBUG_HILT"),
    ) else {
        return;
    };
    let p = open(&std::fs::canonicalize(&root).unwrap());
    let texts = crate::resolve::Texts::default();
    let r = crate::resolve::Resolver::new(&p.index, &texts);
    for rel in what.split(',') {
        let h = r.hilt_file(&p.root.join(rel));
        for b in &h.providers {
            eprintln!(
                "{rel}: provides {} at line {}",
                b.key,
                line_of(p.text(&b.decl.0), b.decl.1)
            );
        }
        for s in &h.sites {
            eprintln!(
                "{rel}: site {:?} {} at line {}",
                s.param,
                s.key,
                line_of(p.text(&s.owner.0), s.at)
            );
        }
    }
}
