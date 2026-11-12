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
    /// Files that declare each name, for lookups that do not scan every declaration.
    by_name: HashMap<String, Vec<PathBuf>>,
    /// Files with a class that names each supertype, for "implemented by" lookups.
    subtypes: HashMap<String, Vec<PathBuf>>,
    /// Bumped on every change, so callers can cache results.
    pub generation: u64,
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
        let scanned: Vec<(PathBuf, FileSyms)> = std::thread::scope(|s| {
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
        let mut index = Index::default();
        for (p, f) in scanned {
            index.insert_scanned(p, f);
        }
        index
    }

    /// Adds a file whose symbols were already scanned (e.g. on a worker thread).
    pub fn insert_scanned(&mut self, path: PathBuf, syms: FileSyms) {
        self.remove(&path);
        for (map, keys) in [
            (&mut self.by_name, names_of(&syms)),
            (&mut self.subtypes, supers_of(&syms)),
        ] {
            for k in keys {
                map.entry(k).or_default().push(path.clone());
            }
        }
        self.files.insert(path, syms);
        self.generation += 1;
    }

    pub fn update(&mut self, path: &Path, src: &str) {
        if is_code(path) {
            self.insert_scanned(path.to_path_buf(), scan(src));
        }
    }

    pub fn remove(&mut self, path: &Path) {
        let Some(old) = self.files.remove(path) else {
            return;
        };
        for (map, keys) in [
            (&mut self.by_name, names_of(&old)),
            (&mut self.subtypes, supers_of(&old)),
        ] {
            for k in keys {
                if let Some(v) = map.get_mut(&k) {
                    v.retain(|p| p != path);
                    if v.is_empty() {
                        map.remove(&k);
                    }
                }
            }
        }
        self.generation += 1;
    }

    /// Drops every file under `dir` (a deleted folder).
    pub fn remove_under(&mut self, dir: &Path) {
        let gone: Vec<PathBuf> = self
            .files
            .keys()
            .filter(|p| p.starts_with(dir))
            .cloned()
            .collect();
        for p in gone {
            self.remove(&p);
        }
    }

    pub fn decls(&self) -> impl Iterator<Item = (&PathBuf, &FileSyms, &Decl)> {
        self.files
            .iter()
            .flat_map(|(p, f)| f.decls.iter().map(move |d| (p, f, d)))
    }

    /// Declarations named exactly `name`, without scanning the whole project.
    pub fn named<'a>(
        &'a self,
        name: &str,
    ) -> impl Iterator<Item = (&'a PathBuf, &'a FileSyms, &'a Decl)> + 'a {
        self.by_name
            .get_key_value(name)
            .into_iter()
            .flat_map(move |(key, paths)| {
                paths
                    .iter()
                    .filter_map(move |p| self.files.get_key_value(p))
                    .flat_map(move |(p, f)| {
                        f.decls
                            .iter()
                            .filter(move |d| d.name == *key)
                            .map(move |d| (p, f, d))
                    })
            })
    }

    /// Declarations whose container is `name` (members of a class).
    pub fn members<'a>(&'a self, container: &str) -> impl Iterator<Item = &'a Decl> + 'a {
        let key = self
            .by_name
            .get_key_value(container)
            .map(|(k, _)| k.as_str());
        self.named(container).flat_map(move |(_, f, _)| {
            f.decls
                .iter()
                .filter(move |d| d.container.as_deref().is_some_and(|c| Some(c) == key))
        })
    }

    pub fn symbol_count(&self) -> usize {
        self.files.values().map(|f| f.decls.len()).sum()
    }

    /// Fuzzy "Go to class" (types only) or "Go to symbol" (everything).
    pub fn search(&self, query: &str, types_only: bool, limit: usize) -> Vec<Hit> {
        let items = self
            .decls()
            .filter(|(_, _, d)| !types_only || d.kind.is_type());
        rank_by(query, items, |(_, _, d)| &d.name, limit)
            .into_iter()
            .map(|(score, (p, f, d))| Hit {
                name: d.name.clone(),
                detail: describe(f, d),
                path: p.clone(),
                offset: d.offset,
                score,
            })
            .collect()
    }

    /// Where `name` is declared. Prefers the current file, then imported and same-package declarations.
    pub fn definitions(&self, name: &str, from: &Path) -> Vec<Hit> {
        let here = self.files.get(from);
        let mut hits: Vec<Hit> = self
            .named(name)
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

    /// Completion candidates for an identifier prefix: one per name, exact-case prefix first.
    pub fn complete(&self, prefix: &str, limit: usize) -> Vec<Hit> {
        let names = self.by_name.keys().map(String::as_str);
        let mut top = rank_by(prefix, names, |n| n, limit * 4);
        top.sort_by_key(|(score, n)| (!n.starts_with(prefix), std::cmp::Reverse(*score)));
        top.into_iter()
            .take(limit)
            .filter_map(|(score, n)| {
                let (p, f, d) = self.named(n).next()?;
                Some(Hit {
                    name: d.name.clone(),
                    detail: describe(f, d),
                    path: p.clone(),
                    offset: d.offset,
                    score,
                })
            })
            .collect()
    }

    /// Fully qualified name of a type or top-level declaration, for adding an import.
    pub fn import_for(&self, name: &str) -> Option<String> {
        self.named(name)
            .find(|(_, _, d)| d.kind.is_type() || d.container.is_none())
            .map(|(_, f, d)| qualified(f, d))
    }

    /// Classes that extend or implement `name`, directly or through other classes (breadth first).
    pub fn implementations(&self, name: &str) -> Vec<Hit> {
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut queue = std::collections::VecDeque::from([name.to_string()]);
        while let Some(n) = queue.pop_front() {
            for p in self.subtypes.get(&n).into_iter().flatten() {
                let Some(f) = self.files.get(p) else { continue };
                for d in f
                    .decls
                    .iter()
                    .filter(|d| d.kind.is_type() && d.supers.contains(&n))
                {
                    if seen.insert((p.clone(), d.offset)) && out.len() < 500 {
                        out.push(hit(p, f, d));
                        queue.push_back(d.name.clone());
                    }
                }
            }
        }
        out
    }

    /// Members named `member` inside the type `container`, with their files.
    fn members_named<'a>(
        &'a self,
        container: &str,
        member: &'a str,
    ) -> impl Iterator<Item = (&'a PathBuf, &'a FileSyms, &'a Decl)> + 'a {
        let key = self
            .by_name
            .get_key_value(container)
            .map(|(k, _)| k.as_str());
        self.named(container).flat_map(move |(p, f, _)| {
            f.decls
                .iter()
                .filter(move |d| {
                    d.name == member && d.container.as_deref().is_some_and(|c| Some(c) == key)
                })
                .map(move |d| (p, f, d))
        })
    }

    /// The declaration an `override` member overrides: the nearest supertype of `container`
    /// that declares `member`.
    pub fn overridden(&self, container: &str, member: &str) -> Option<Hit> {
        let mut seen = std::collections::HashSet::new();
        let mut queue: std::collections::VecDeque<String> = self
            .named(container)
            .filter(|(_, _, d)| d.kind.is_type())
            .flat_map(|(_, _, d)| d.supers.clone())
            .collect();
        while let Some(s) = queue.pop_front() {
            if !seen.insert(s.clone()) {
                continue;
            }
            if let Some((p, f, d)) = self.members_named(&s, member).next() {
                return Some(hit(p, f, d));
            }
            queue.extend(
                self.named(&s)
                    .filter(|(_, _, d)| d.kind.is_type())
                    .flat_map(|(_, _, d)| d.supers.clone()),
            );
        }
        None
    }

    /// `override` members named `member` in the classes that implement `container`.
    pub fn member_implementations(&self, container: &str, member: &str) -> Vec<Hit> {
        self.implementations(container)
            .iter()
            .flat_map(|c| {
                let class = c.name.rsplit('.').next().unwrap_or(&c.name).to_string();
                self.members_named(&class, member)
                    .filter(|(_, _, d)| d.modifiers.iter().any(|m| m == "override"))
                    .map(|(p, f, d)| hit(p, f, d))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    pub fn find_decl(&self, name: &str, kind: Kind) -> Option<&Decl> {
        self.named(name).map(|(_, _, d)| d).find(|d| d.kind == kind)
    }

    pub fn find_type(&self, name: &str) -> Option<&Decl> {
        self.named(name)
            .map(|(_, _, d)| d)
            .find(|d| d.kind.is_type())
    }
}

fn names_of(f: &FileSyms) -> Vec<String> {
    let mut v: Vec<String> = f.decls.iter().map(|d| d.name.clone()).collect();
    v.sort_unstable();
    v.dedup();
    v
}

fn supers_of(f: &FileSyms) -> Vec<String> {
    let mut v: Vec<String> = f
        .decls
        .iter()
        .flat_map(|d| d.supers.iter().cloned())
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

fn hit(p: &Path, f: &FileSyms, d: &Decl) -> Hit {
    Hit {
        name: match &d.container {
            Some(c) => format!("{c}.{}", d.name),
            None => d.name.clone(),
        },
        detail: describe(f, d),
        path: p.to_path_buf(),
        offset: d.offset,
        score: 0,
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

/// Fuzzy-ranks `items` by `key` and keeps the best `limit`, without allocating per item.
/// Exact matches and prefix matches score higher; shorter names win ties. An empty query keeps the input order.
pub fn rank_by<T>(
    query: &str,
    items: impl Iterator<Item = T>,
    key: impl Fn(&T) -> &str,
    limit: usize,
) -> Vec<(u32, T)> {
    if query.is_empty() {
        return items.take(limit).map(|t| (0, t)).collect();
    }
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut buf = Vec::new();
    let mut scored: Vec<(u32, T)> = items
        .filter_map(|t| {
            let k = key(&t);
            let s = pattern.score(Utf32Str::new(k, &mut buf), &mut matcher)?;
            let prefix = k
                .get(..query.len())
                .is_some_and(|p| p.eq_ignore_ascii_case(query));
            let bonus = if prefix && k.len() == query.len() {
                10_000
            } else if prefix {
                1_000
            } else {
                0
            };
            Some(((s + bonus).saturating_sub(k.len() as u32), t))
        })
        .collect();
    if scored.len() > limit {
        scored.select_nth_unstable_by_key(limit, |(s, _)| std::cmp::Reverse(*s));
        scored.truncate(limit);
    }
    scored.sort_by_key(|(s, _)| std::cmp::Reverse(*s));
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
    fn hierarchy_implementations_and_overrides() {
        let mut i = Index::default();
        i.update(
            Path::new("/p/Api.kt"),
            "interface Api { fun load(): Int }\nabstract class Base : Api\n",
        );
        i.update(Path::new("/p/Impl.kt"), "class Impl : Base() { override fun load() = 1 }\nclass Other : Api {\n override fun load() = 2\n}\n");
        let names = |v: Vec<Hit>| v.into_iter().map(|h| h.name).collect::<Vec<_>>();
        let mut impls = names(i.implementations("Api"));
        impls.sort();
        assert_eq!(
            impls,
            vec!["Base", "Impl", "Other"],
            "direct and through Base"
        );
        assert_eq!(i.overridden("Impl", "load").unwrap().name, "Api.load");
        assert!(i.overridden("Api", "load").is_none());
        let mut m = names(i.member_implementations("Api", "load"));
        m.sort();
        assert_eq!(m, vec!["Impl.load", "Other.load"]);
        i.remove(Path::new("/p/Impl.kt"));
        assert_eq!(names(i.implementations("Api")), vec!["Base"]);
    }

    #[test]
    fn name_map_follows_updates_and_removals() {
        let mut i = index();
        assert_eq!(i.named("User").count(), 2);
        i.update(
            Path::new("/p/other/User.kt"),
            "package com.other\nclass Person\n",
        );
        assert_eq!(i.named("User").count(), 1);
        assert_eq!(i.named("Person").count(), 1);
        i.remove_under(Path::new("/p/data"));
        assert_eq!(i.named("User").count(), 0);
        assert!(i.find_type("UserRepository").is_none());
        assert_eq!(i.members("Person").count(), 0);
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
