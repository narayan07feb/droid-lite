//! Opens an Android / Gradle project folder: finds modules from settings.gradle(.kts) and lists source files.

use crate::lexer::{Tok, lex};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub struct Module {
    /// Gradle path, e.g. `:feature:home`. The root project is `:`.
    pub name: String,
    pub dir: PathBuf,
}

#[derive(Clone, Debug, Default)]
pub struct Project {
    pub root: PathBuf,
    pub name: String,
    pub modules: Vec<Module>,
    /// Source and build files, sorted.
    pub files: Vec<PathBuf>,
    /// Module directory → index in `modules`, so `module_of` walks up a path instead of
    /// testing every module (578 modules × 11 721 files took 651 ms; now ~10 ms).
    module_dirs: std::collections::HashMap<PathBuf, usize>,
}

const EXTENSIONS: &[&str] = &[
    "kt",
    "kts",
    "java",
    "xml",
    "gradle",
    "toml",
    "properties",
    "json",
    "md",
    "pro",
    "txt",
    "stability",
];

pub fn is_code(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("kt" | "kts" | "java")
    )
}

impl Project {
    pub fn open(root: &Path) -> Project {
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let settings = ["settings.gradle.kts", "settings.gradle"]
            .iter()
            .map(|f| root.join(f))
            .find(|p| p.exists());
        let src = settings
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default();
        let name = settings_name(&src).unwrap_or_else(|| {
            root.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        });
        let mut modules: Vec<Module> = included_modules(&src)
            .into_iter()
            .map(|m| Module {
                dir: root.join(m.trim_start_matches(':').replace(':', "/")),
                name: m,
            })
            .filter(|m| m.dir.is_dir())
            .collect();
        if ["build.gradle.kts", "build.gradle"]
            .iter()
            .any(|f| root.join(f).exists())
            || modules.is_empty()
        {
            modules.insert(
                0,
                Module {
                    name: ":".into(),
                    dir: root.clone(),
                },
            );
        }
        let mut files: Vec<PathBuf> = ignore::WalkBuilder::new(&root)
            .hidden(true)
            .filter_entry(|e| {
                !matches!(
                    e.file_name().to_str(),
                    Some("build" | ".gradle" | ".idea" | "node_modules" | ".cxx")
                )
            })
            .build()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
            .map(|e| e.into_path())
            .filter(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| EXTENSIONS.contains(&e))
            })
            .collect();
        files.sort();
        let module_dirs = modules
            .iter()
            .enumerate()
            .map(|(i, m)| (m.dir.clone(), i))
            .collect();
        Project {
            root,
            name,
            modules,
            files,
            module_dirs,
        }
    }

    /// The module that owns `path`: the one with the longest matching directory.
    pub fn module_of(&self, path: &Path) -> Option<&Module> {
        path.ancestors()
            .find_map(|a| self.module_dirs.get(a))
            .map(|&i| &self.modules[i])
    }

    /// Whether a file that appeared later belongs in the file list (same rules as `open`).
    pub fn accepts(&self, path: &Path) -> bool {
        path.strip_prefix(&self.root).is_ok_and(|rel| {
            !crate::watch::ignored(rel)
                && !rel
                    .components()
                    .any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
                && path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| EXTENSIONS.contains(&e))
        })
    }

    pub fn gradlew(&self) -> PathBuf {
        self.root.join("gradlew")
    }
}

/// Every `":module"` string passed to `include(...)` / `include "..."` in a settings file.
pub fn included_modules(settings: &str) -> Vec<String> {
    let t = lex(settings);
    let mut out = Vec::new();
    let mut inside = false;
    for (i, tok) in t.iter().enumerate() {
        let w = &settings[tok.range.clone()];
        if tok.kind == Tok::Ident {
            inside = w == "include";
            continue;
        }
        if inside && tok.kind == Tok::Str {
            let m = w.trim_matches(|c| c == '"' || c == '\'');
            if !m.is_empty() && !m.contains('$') {
                out.push(if m.starts_with(':') {
                    m.to_string()
                } else {
                    format!(":{m}")
                });
            }
        }
        if tok.kind == Tok::Keyword
            || (w == ")"
                && t.get(i + 1)
                    .is_none_or(|n| settings[n.range.clone()].ne(",")))
        {
            inside = false;
        }
    }
    out.dedup();
    out
}

fn settings_name(settings: &str) -> Option<String> {
    let line = settings
        .lines()
        .find(|l| l.trim_start().starts_with("rootProject.name"))?;
    let v = line
        .split('=')
        .nth(1)?
        .trim()
        .trim_matches(|c| c == '"' || c == '\'');
    Some(v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_kts_and_groovy_includes() {
        let kts = "rootProject.name = \"Shop\"\ninclude(\":app\")\ninclude(\n  \":core:ui\",\n  \":feature:home\",\n)\n// include(\":old\")\nincludeBuild(\"build-logic\")\n";
        assert_eq!(
            included_modules(kts),
            vec![":app", ":core:ui", ":feature:home"]
        );
        assert_eq!(settings_name(kts).as_deref(), Some("Shop"));
        assert_eq!(
            included_modules("include ':app', ':lib'\ninclude 'data'\n"),
            vec![":app", ":lib", ":data"]
        );
    }

    #[test]
    fn opens_a_project_folder() {
        let dir = std::env::temp_dir().join(format!("droid-lite-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("app/src/main/java/a")).unwrap();
        std::fs::create_dir_all(dir.join("app/build/gen")).unwrap();
        std::fs::write(
            dir.join("settings.gradle.kts"),
            "include(\":app\", \":missing\")",
        )
        .unwrap();
        std::fs::write(dir.join("build.gradle.kts"), "").unwrap();
        std::fs::write(dir.join("app/src/main/java/a/Main.kt"), "fun main() {}").unwrap();
        std::fs::write(dir.join("app/build/gen/Gen.kt"), "").unwrap();
        let p = Project::open(&dir);
        assert_eq!(
            p.modules
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>(),
            vec![":", ":app"]
        );
        let rel: Vec<_> = p
            .files
            .iter()
            .map(|f| {
                f.strip_prefix(&p.root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(
            rel,
            vec![
                "app/src/main/java/a/Main.kt",
                "build.gradle.kts",
                "settings.gradle.kts"
            ]
        );
        assert_eq!(p.module_of(&p.files[0]).unwrap().name, ":app");
        assert!(p.accepts(&p.root.join("app/src/main/java/a/New.kt")));
        assert!(!p.accepts(&p.root.join("app/build/gen/New.kt")));
        assert!(!p.accepts(&p.root.join(".idea/misc.xml")));
        assert!(!p.accepts(Path::new("/elsewhere/A.kt")));
        std::fs::remove_dir_all(&dir).ok();
    }
}
