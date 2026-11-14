//! Which places that spell a name really use one declaration of it. A member of one class is not
//! a same-named member of another class, and a top-level function is used only where it is
//! visible. Without types this is a careful reading of each file's text (the receiver's declared
//! type, the enclosing class, imports); kotlin-lsp, when it runs, adds its exact answer.

use crate::index::{Index, type_name};
use crate::kotlin::{Decl, FileSyms};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Target {
    pub name: String,
    pub path: PathBuf,
    pub offset: usize,
    pub package: String,
    /// For a member: its class with the classes above and below it (overrides use it too).
    pub owners: Option<HashSet<String>>,
    /// A class, object or interface: used wherever its name is visible.
    pub is_type: bool,
    /// A top-level `fun Receiver.name()`, called on a value.
    pub extension: bool,
}

impl Target {
    /// `text` is the declaring file's text.
    pub fn new(index: &Index, path: &Path, f: &FileSyms, d: &Decl, text: &str) -> Target {
        let owners = d
            .container
            .as_deref()
            .filter(|_| !d.kind.is_type())
            .map(|c| hierarchy(index, c));
        let before = text.get(..d.offset).unwrap_or("").trim_end();
        Target {
            name: d.name.clone(),
            path: path.to_path_buf(),
            offset: d.offset,
            package: f.package.clone(),
            owners,
            is_type: d.kind.is_type(),
            extension: d.container.is_none() && before.ends_with('.'),
        }
    }

    /// Whether the name at `at` in this file means this declaration.
    pub fn is_use(&self, path: &Path, text: &str, syms: &FileSyms, at: usize) -> bool {
        if path == self.path && at == self.offset {
            return true;
        }
        // another declaration of the same name (an override is not a usage either)
        if syms.decls.iter().any(|d| d.offset == at) {
            return false;
        }
        let line_start = text[..at].rfind('\n').map_or(0, |i| i + 1);
        let line = text[line_start..].lines().next().unwrap_or("").trim_start();
        if line.starts_with("import ") {
            return self.package.is_empty() || line.contains(&format!("{}.", self.package));
        }
        let before = text[..at].trim_end();
        let qualified = before.ends_with('.') && !before.ends_with("..");
        let own_path = || before.trim_end_matches(['.', '?']).ends_with(&self.package);
        let Some(owners) = &self.owners else {
            // a type, or a top-level function / property
            if qualified {
                return self.extension && !self.is_type || own_path();
            }
            let shadowed = syms
                .decls
                .iter()
                .any(|d| d.name == self.name && d.container.is_none() && path != self.path);
            return !shadowed && self.visible(path, syms);
        };
        match receiver(before) {
            None | Some("this" | "super") => {
                enclosing_class(syms, at).is_some_and(|c| owners.contains(c))
            }
            Some(r) if r.starts_with(char::is_uppercase) => owners.contains(r),
            Some(r) => match receiver_type(text, r) {
                Declared::As(t) => owners.contains(&t),
                // declared here, as something we cannot read (`val db = helper.db`): not ours
                Declared::Unread => false,
                // from elsewhere (`it`, a property of a superclass): the file must name the class
                Declared::Elsewhere => owners.iter().any(|o| has_word(text, o)),
            },
        }
    }

    fn visible(&self, path: &Path, syms: &FileSyms) -> bool {
        path == self.path
            || syms.package == self.package
            || syms.imports.iter().any(|i| {
                **i == format!("{}.{}", self.package, self.name)
                    || **i == format!("{}.*", self.package)
            })
    }
}

/// A class with every class above it (its supertypes) and below it (subclasses, implementations).
fn hierarchy(index: &Index, class: &str) -> HashSet<String> {
    let mut out = HashSet::from([class.to_string()]);
    let mut todo = vec![class.to_string()];
    while let Some(c) = todo.pop() {
        if out.len() > 200 {
            break;
        }
        let up = index
            .find_type(&c)
            .map(|d| d.supers.clone())
            .unwrap_or_default();
        let down = index.implementations(&c).into_iter().map(|h| h.name);
        for n in up.into_iter().chain(down) {
            if out.insert(n.clone()) {
                todo.push(n);
            }
        }
    }
    out
}

/// The receiver right before `name`: `dao` in `dao.insert`, `""` for `f().insert`, `None` when
/// there is none.
fn receiver(before: &str) -> Option<&str> {
    let b = before
        .strip_suffix('.')?
        .trim_end_matches('?')
        .trim_end_matches('!');
    let start = b
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
        .last()
        .map_or(b.len(), |(i, _)| i);
    Some(&b[start..])
}

fn has_word(text: &str, w: &str) -> bool {
    text.match_indices(w).any(|(i, _)| {
        let ok = |c: Option<char>| !c.is_some_and(|c| c.is_alphanumeric() || c == '_');
        ok(text[..i].chars().next_back()) && ok(text[i + w.len()..].chars().next())
    })
}

/// What this file says about a value's type.
enum Declared {
    /// `dao: UserDao`, `val dao = UserDao()`, `val vm by viewModels<HomeViewModel>()`.
    As(String),
    /// `val db = helper.writableDatabase`: declared here, type not written.
    Unread,
    Elsewhere,
}

fn receiver_type(text: &str, name: &str) -> Declared {
    let mut declared = false;
    let ident = |s: &str| -> String {
        s.chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '.')
            .collect()
    };
    for (i, _) in text.match_indices(name) {
        let word_start = !text[..i]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.');
        let rest = &text[i + name.len()..];
        if !word_start || rest.starts_with(|c: char| c.is_alphanumeric() || c == '_') {
            continue;
        }
        let rest = rest.trim_start();
        let head = text[..i].trim_end();
        declared |= head.ends_with("val") || head.ends_with("var") || rest.starts_with(':');
        let ty = if let Some(t) = rest.strip_prefix(':') {
            ident(t.trim_start())
        } else if let Some(t) = rest.strip_prefix('=').filter(|t| !t.starts_with('=')) {
            let t = t.trim_start();
            let id = ident(t);
            let after = &t[id.len()..];
            if after.starts_with(['(', '<']) {
                id
            } else {
                String::new()
            }
        } else if let Some(t) = rest.strip_prefix("by ") {
            let line = t.lines().next().unwrap_or("");
            line.find('<')
                .map(|a| ident(&line[a + 1..]))
                .unwrap_or_default()
        } else {
            String::new()
        };
        let ty = type_name(&ty);
        if ty.starts_with(char::is_uppercase) {
            return Declared::As(ty);
        }
    }
    if declared {
        Declared::Unread
    } else {
        Declared::Elsewhere
    }
}

/// The class around `at`: the class of the function whose body holds it, else of the nearest
/// declaration above it.
fn enclosing_class(syms: &FileSyms, at: usize) -> Option<&str> {
    let in_fun = syms
        .decls
        .iter()
        .filter(|d| d.body.as_ref().is_some_and(|b| b.contains(&at)))
        .max_by_key(|d| d.offset);
    if let Some(d) = in_fun {
        return d.container.as_deref();
    }
    let above = syms
        .decls
        .iter()
        .filter(|d| d.offset < at)
        .max_by_key(|d| d.offset)?;
    if above.kind.is_type() {
        Some(&above.name)
    } else {
        above.container.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kotlin::scan;

    const DAO: &str = "package com.app.data\n\ninterface UserDao {\n    fun insert(u: User)\n}\n";
    const OTHER: &str = "package com.app.cache\n\nclass Cache {\n    fun insert(k: String) {}\n    fun fill() {\n        insert(\"a\")\n    }\n}\n";
    const REPO: &str = "package com.app\n\nimport com.app.data.UserDao\n\nclass Repo(private val dao: UserDao, private val cache: Cache, val db: SQLiteDatabase) {\n    fun save(u: User) {\n        dao.insert(u)\n        cache.insert(\"k\")\n        db.insert(\"t\", null, v)\n    }\n}\n";

    const REAL: &str = "package com.app.data\n\nclass RealDao : UserDao {\n    override fun insert(u: User) {}\n    fun again(u: User) {\n        insert(u)\n    }\n}\n";

    fn index() -> Index {
        let mut i = Index::default();
        for (p, t) in [
            ("/p/UserDao.kt", DAO),
            ("/p/Cache.kt", OTHER),
            ("/p/Repo.kt", REPO),
            ("/p/RealDao.kt", REAL),
        ] {
            i.insert_scanned(PathBuf::from(p), scan(t));
        }
        i
    }

    fn uses(t: &Target, path: &str, text: &str) -> Vec<usize> {
        let syms = scan(text);
        crate::refactor::occurrences(text, &t.name)
            .into_iter()
            .map(|r| r.start)
            .filter(|&o| t.is_use(Path::new(path), text, &syms, o))
            .map(|o| text[..o].matches('\n').count() + 1)
            .collect()
    }

    fn target(i: &Index, path: &str, text: &str, name: &str) -> Target {
        let f = &i.files[Path::new(path)];
        let d = f.decls.iter().find(|d| d.name == name).unwrap();
        Target::new(i, Path::new(path), f, d, text)
    }

    #[test]
    fn a_member_is_used_only_through_its_own_class() {
        let i = index();
        let dao = target(&i, "/p/UserDao.kt", DAO, "insert");
        assert_eq!(
            uses(&dao, "/p/Repo.kt", REPO),
            [7],
            "dao.insert, not cache.insert or db.insert"
        );
        assert_eq!(
            uses(&dao, "/p/Cache.kt", OTHER),
            Vec::<usize>::new(),
            "Cache's own insert"
        );
        assert_eq!(
            uses(&dao, "/p/UserDao.kt", DAO),
            [4],
            "the declaration itself"
        );
        // an override is not a usage; a call inside the implementing class is
        assert_eq!(uses(&dao, "/p/RealDao.kt", REAL), [6]);
        let cache = target(&i, "/p/Cache.kt", OTHER, "insert");
        assert_eq!(
            uses(&cache, "/p/Cache.kt", OTHER),
            [4, 6],
            "an unqualified call in its class"
        );
        assert_eq!(uses(&cache, "/p/Repo.kt", REPO), [8]);
        // the receiver's type from `val x = T()` and `by viewModels<T>()`
        let src = "class A {\n    val c = Cache()\n    val vm by viewModels<Cache>()\n    fun f() { c.insert(\"x\"); vm.insert(\"y\") }\n}\n";
        assert_eq!(uses(&cache, "/p/A.kt", src), [4, 4]);
        // unknown receiver: counts only in a file that names the class
        let src = "class B {\n    val db = helper.db\n    fun f(c: Cache) { db.insert(1) }\n}\n";
        assert!(
            uses(&cache, "/p/B.kt", src).is_empty(),
            "db is declared here, not a Cache"
        );
        let src = "fun f() { holder().insert(1) }\n";
        assert!(uses(&cache, "/p/B.kt", src).is_empty());
        let src = "fun f(c: Cache) { list.forEach { it.insert(\"k\") } }\n";
        assert_eq!(
            uses(&cache, "/p/C.kt", src),
            [1],
            "`it` in a file that names Cache"
        );
    }

    #[test]
    fn a_top_level_function_is_used_where_it_is_visible() {
        let lib = "package com.util\n\nfun format(x: Int) = x\n";
        let mut i = Index::default();
        i.insert_scanned(PathBuf::from("/p/Util.kt"), scan(lib));
        let t = target(&i, "/p/Util.kt", lib, "format");
        let user = "package com.app\n\nimport com.util.format\n\nval a = format(1)\n";
        assert_eq!(uses(&t, "/p/A.kt", user), [3, 5], "the import and the call");
        let blind = "package com.other\n\nval b = format(2)\nval c = s.format(3)\n";
        assert!(
            uses(&t, "/p/B.kt", blind).is_empty(),
            "not imported, and a member call"
        );
        let own = "package com.util\n\nfun format(s: String) = s\nval d = format(\"x\")\n";
        assert!(
            uses(&t, "/p/C.kt", own).is_empty(),
            "the file declares its own"
        );
        assert_eq!(receiver("x."), Some("x"));
        assert_eq!(receiver("x"), None);
        assert_eq!(receiver("f()."), Some(""));
        assert_eq!(receiver("vm?."), Some("vm"));
    }
}
