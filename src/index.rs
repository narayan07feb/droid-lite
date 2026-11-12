//! Project-wide symbol index: declarations of every Kotlin/Java file, fuzzy search, definitions, completion.

use crate::kotlin::{Decl, FileSyms, Kind, scan};
use crate::project::is_code;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct Index {
    pub files: HashMap<PathBuf, FileSyms>,
}

/// One search hit.
#[derive(Clone, Debug)]
pub struct Hit {
    pub name: String,
    pub detail: String,
    pub path: PathBuf,
    pub offset: usize,
    pub score: u32,
}

impl Index {
    /// Scans all code files on all cores.
    pub fn build(paths: &[PathBuf]) -> Index {
        let code: Vec<&PathBuf> = paths.iter().filter(|p| is_code(p)).collect();
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
        let chunk = code.len().div_ceil(threads).max(1);
        let files = std::thread::scope(|s| {
            let jobs: Vec<_> = code
                .chunks(chunk)
                .map(|c| {
                    s.spawn(move || {
                        c.iter()
                            .filter_map(|p| {
                                Some(((*p).clone(), scan(&std::fs::read_to_string(p).ok()?)))
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            jobs.into_iter()
                .flat_map(|j| j.join().unwrap_or_default())
                .collect()
        });
        Index { files }
    }

    pub fn update(&mut self, path: &Path, src: &str) {
        if is_code(path) {
            self.files.insert(path.to_path_buf(), scan(src));
        }
    }

    pub fn remove(&mut self, path: &Path) {
        self.files.remove(path);
    }

    pub fn decls(&self) -> impl Iterator<Item = (&PathBuf, &FileSyms, &Decl)> {
        self.files
            .iter()
            .flat_map(|(p, f)| f.decls.iter().map(move |d| (p, f, d)))
    }

    pub fn symbol_count(&self) -> usize {
        self.files.values().map(|f| f.decls.len()).sum()
    }

    /// Fuzzy "Go to class" (types only) or "Go to symbol" (everything).
    pub fn search(&self, query: &str, types_only: bool, limit: usize) -> Vec<Hit> {
        let items = self
            .decls()
            .filter(|(_, _, d)| !types_only || d.kind.is_type())
            .map(|(p, f, d)| Hit {
                name: d.name.clone(),
                detail: describe(f, d),
                path: p.clone(),
                offset: d.offset,
                score: 0,
            });
        rank(query, items, |h| h.name.clone(), limit)
    }

    /// Where `name` is declared. Prefers the current file, then imported and same-package declarations.
    pub fn definitions(&self, name: &str, from: &Path) -> Vec<Hit> {
        let here = self.files.get(from);
        let mut hits: Vec<Hit> = self
            .decls()
            .filter(|(_, _, d)| d.name == name)
            .map(|(p, f, d)| {
                let fq = qualified(f, d);
                let score = if p == from {
                    3
                } else if here.is_some_and(|h| {
                    h.imports.iter().any(|i| {
                        *i == fq || i.ends_with(".*") && fq.starts_with(i.trim_end_matches('*'))
                    })
                }) {
                    2
                } else if here.is_some_and(|h| h.package == f.package) {
                    1
                } else {
                    0
                };
                Hit {
                    name: d.name.clone(),
                    detail: describe(f, d),
                    path: p.clone(),
                    offset: d.offset,
                    score,
                }
            })
            .collect();
        hits.sort_by(|a, b| b.score.cmp(&a.score).then(a.path.cmp(&b.path)));
        hits
    }

    /// Completion candidates for an identifier prefix: declarations in the project, deduplicated by name.
    pub fn complete(&self, prefix: &str, limit: usize) -> Vec<Hit> {
        let mut seen = std::collections::HashSet::new();
        let items = self
            .decls()
            .filter(|(_, _, d)| seen.insert((d.name.clone(), d.kind == Kind::Fun)))
            .map(|(p, f, d)| Hit {
                name: d.name.clone(),
                detail: describe(f, d),
                path: p.clone(),
                offset: d.offset,
                score: 0,
            });
        let mut hits = rank(prefix, items, |h| h.name.clone(), limit * 4);
        // exact prefix first, then fuzzy order
        hits.sort_by_key(|h| {
            (
                !h.name.starts_with(prefix),
                !h.name.to_lowercase().starts_with(&prefix.to_lowercase()),
                u32::MAX - h.score,
            )
        });
        hits.truncate(limit);
        hits
    }

    /// Fully qualified name of a type, for adding an import.
    pub fn import_for(&self, name: &str) -> Option<String> {
        self.decls()
            .find(|(_, _, d)| d.name == name && (d.kind.is_type() || d.container.is_none()))
            .map(|(_, f, d)| qualified(f, d))
    }

    pub fn find_decl(&self, name: &str, kind: Kind) -> Option<&Decl> {
        self.decls()
            .map(|(_, _, d)| d)
            .find(|d| d.name == name && d.kind == kind)
    }
}

pub fn qualified(f: &FileSyms, d: &Decl) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if !f.package.is_empty() {
        parts.push(&f.package);
    }
    if let Some(c) = &d.container {
        parts.push(c);
    }
    parts.push(&d.name);
    parts.join(".")
}

/// Short signature shown next to a symbol: `fun UserCard(user: User, …)  com.example.ui`.
pub fn describe(f: &FileSyms, d: &Decl) -> String {
    let mut s = String::new();
    if d.is_composable() {
        s.push_str("@Composable ");
    }
    s.push_str(if d.kind == Kind::Property && d.mutable {
        "var"
    } else {
        d.kind.label()
    });
    if d.kind == Kind::Fun {
        let ps: Vec<String> = d
            .params
            .iter()
            .map(|p| format!("{}: {}", p.name, p.ty))
            .collect();
        s.push_str(&format!("({})", ps.join(", ")));
    }
    if !d.ty.is_empty() {
        s.push_str(&format!(": {}", d.ty));
    }
    let place = match &d.container {
        Some(c) if f.package.is_empty() => c.clone(),
        Some(c) => format!("{}.{}", f.package, c),
        None => f.package.clone(),
    };
    if !place.is_empty() {
        s.push_str(&format!("  {place}"));
    }
    s
}

/// Fuzzy-ranks hits by `key`. An empty query keeps the input order.
pub fn rank(
    query: &str,
    items: impl Iterator<Item = Hit>,
    key: impl Fn(&Hit) -> String,
    limit: usize,
) -> Vec<Hit> {
    if query.is_empty() {
        return items.take(limit).collect();
    }
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut buf = Vec::new();
    let q = query.to_lowercase();
    let mut scored: Vec<Hit> = items
        .filter_map(|mut h| {
            let k = key(&h);
            let s = pattern.score(Utf32Str::new(&k, &mut buf), &mut matcher)?;
            let bonus = if k.eq_ignore_ascii_case(query) {
                10_000
            } else if k.to_lowercase().starts_with(&q) {
                1_000
            } else {
                0
            };
            h.score = (s + bonus).saturating_sub(k.len() as u32);
            Some(h)
        })
        .collect();
    scored.sort_by_key(|h| std::cmp::Reverse(h.score));
    scored.truncate(limit);
    scored
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> Index {
        let mut i = Index::default();
        i.update(Path::new("/p/ui/Card.kt"), "package com.ui\nimport com.data.User\n@Composable\nfun UserCard(user: User) {}\nclass CardState\n");
        i.update(
            Path::new("/p/data/User.kt"),
            "package com.data\ndata class User(val name: String)\nclass UserRepository\n",
        );
        i.update(
            Path::new("/p/other/User.kt"),
            "package com.other\nclass User\n",
        );
        i
    }

    #[test]
    fn fuzzy_class_search_ranks_prefix_first() {
        let hits = index().search("UsRe", true, 10);
        assert_eq!(hits[0].name, "UserRepository");
        let hits = index().search("user", true, 10);
        assert_eq!(hits[0].name, "User");
        assert!(
            hits.iter().all(|h| h.name != "UserCard"),
            "funs are not types"
        );
    }

    #[test]
    fn definition_prefers_imported_declaration() {
        let defs = index().definitions("User", Path::new("/p/ui/Card.kt"));
        assert_eq!(defs[0].path, Path::new("/p/data/User.kt"));
        assert_eq!(defs.len(), 2);
    }

    #[test]
    fn completion_and_imports() {
        let i = index();
        let c = i.complete("UserC", 5);
        assert_eq!(c[0].name, "UserCard");
        assert!(c[0].detail.starts_with("@Composable fun(user: User)"));
        assert_eq!(
            i.import_for("UserRepository").as_deref(),
            Some("com.data.UserRepository")
        );
    }
}
