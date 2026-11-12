//! A minimal JVM class-file reader: the class name, its kind, and its public static methods with
//! whether they carry `@Composable`. Enough to index library classes and Kotlin top-level functions
//! (`TextKt.Text`) from jars and aars that ship without sources.

pub struct ClassInfo {
    /// Internal name, e.g. `androidx/compose/material3/TextKt`.
    pub name: String,
    pub public: bool,
    pub kind: ClassKind,
    /// Public static methods: (name, is @Composable).
    pub statics: Vec<(String, bool)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClassKind {
    Class,
    Interface,
    Enum,
    Annotation,
}

struct R<'a> {
    b: &'a [u8],
    at: usize,
}

impl R<'_> {
    fn u1(&mut self) -> Option<u8> {
        let v = *self.b.get(self.at)?;
        self.at += 1;
        Some(v)
    }
    fn u2(&mut self) -> Option<u16> {
        Some(u16::from(self.u1()?) << 8 | u16::from(self.u1()?))
    }
    fn u4(&mut self) -> Option<u32> {
        Some(u32::from(self.u2()?) << 16 | u32::from(self.u2()?))
    }
    fn skip(&mut self, n: usize) -> Option<()> {
        (self.at + n <= self.b.len()).then(|| self.at += n)
    }
}

const COMPOSABLE: &str = "Landroidx/compose/runtime/Composable;";

pub fn parse(bytes: &[u8]) -> Option<ClassInfo> {
    let mut r = R { b: bytes, at: 0 };
    if r.u4()? != 0xCAFE_BABE {
        return None;
    }
    r.skip(4)?;
    let count = r.u2()? as usize;
    let mut utf8: Vec<Option<String>> = vec![None; count];
    let mut class_name_idx: Vec<u16> = vec![0; count];
    let mut i = 1;
    while i < count {
        match r.u1()? {
            1 => {
                let n = r.u2()? as usize;
                let s = bytes.get(r.at..r.at + n)?;
                utf8[i] = Some(String::from_utf8_lossy(s).into_owned());
                r.skip(n)?;
            }
            7 => class_name_idx[i] = r.u2()?,
            8 | 16 | 19 | 20 => {
                r.skip(2)?;
            }
            3 | 4 | 9 | 10 | 11 | 12 | 17 | 18 => {
                r.skip(4)?;
            }
            5 | 6 => {
                r.skip(8)?;
                i += 1; // longs and doubles take two slots
            }
            15 => {
                r.skip(3)?;
            }
            _ => return None,
        }
        i += 1;
    }
    let text = |idx: u16| utf8.get(idx as usize).cloned().flatten();
    let access = r.u2()?;
    let this = r.u2()?;
    let name = text(*class_name_idx.get(this as usize)?)?;
    r.skip(2)?; // super class
    let n = r.u2()? as usize;
    r.skip(2 * n)?; // interfaces
    let fields = r.u2()?;
    for _ in 0..fields {
        r.skip(6)?;
        skip_attributes(&mut r)?;
    }
    let mut statics = Vec::new();
    for _ in 0..r.u2()? {
        let m_access = r.u2()?;
        let m_name = text(r.u2()?).unwrap_or_default();
        r.skip(2)?; // descriptor
        let mut composable = false;
        for _ in 0..r.u2()? {
            let a_name = text(r.u2()?).unwrap_or_default();
            let len = r.u4()? as usize;
            let end = r.at + len;
            if a_name.ends_with("Annotations") && !a_name.contains("Parameter") {
                let mut a = R {
                    b: bytes.get(..end)?,
                    at: r.at,
                };
                for _ in 0..a.u2()? {
                    let (ty, ok) = annotation(&mut a, &text)?;
                    composable |= ok && ty == COMPOSABLE;
                }
            }
            r.at = end;
        }
        // public (0x1) static (0x8), not synthetic (0x1000)
        if m_access & 0x1009 == 0x0009 && !m_name.starts_with('<') && !m_name.contains('$') {
            statics.push((m_name, composable));
        }
    }
    let kind = if access & 0x2000 != 0 {
        ClassKind::Annotation
    } else if access & 0x0200 != 0 {
        ClassKind::Interface
    } else if access & 0x4000 != 0 {
        ClassKind::Enum
    } else {
        ClassKind::Class
    };
    Some(ClassInfo {
        name,
        public: access & 0x0001 != 0 && access & 0x1000 == 0,
        kind,
        statics,
    })
}

fn skip_attributes(r: &mut R) -> Option<()> {
    for _ in 0..r.u2()? {
        r.skip(2)?;
        let len = r.u4()? as usize;
        r.skip(len)?;
    }
    Some(())
}

/// Reads one annotation; returns its type descriptor.
fn annotation(r: &mut R, text: &dyn Fn(u16) -> Option<String>) -> Option<(String, bool)> {
    let ty = text(r.u2()?).unwrap_or_default();
    for _ in 0..r.u2()? {
        r.skip(2)?;
        element_value(r, text)?;
    }
    Some((ty, true))
}

fn element_value(r: &mut R, text: &dyn Fn(u16) -> Option<String>) -> Option<()> {
    match r.u1()? {
        b'B' | b'C' | b'D' | b'F' | b'I' | b'J' | b'S' | b'Z' | b's' | b'c' => r.skip(2),
        b'e' => r.skip(4),
        b'@' => annotation(r, text).map(|_| ()),
        b'[' => {
            for _ in 0..r.u2()? {
                element_value(r, text)?;
            }
            Some(())
        }
        _ => None,
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// Compiles Java sources with javac into `dir`; returns the class files, or None without a JDK.
    pub fn javac(dir: &Path, sources: &[(&str, &str)]) -> Option<Vec<(String, Vec<u8>)>> {
        let src = dir.join("src");
        let out = dir.join("classes");
        let mut files: Vec<PathBuf> = Vec::new();
        for (rel, code) in sources {
            let p = src.join(rel);
            std::fs::create_dir_all(p.parent()?).ok()?;
            std::fs::write(&p, code).ok()?;
            files.push(p);
        }
        let ok = std::process::Command::new("javac")
            .arg("-d")
            .arg(&out)
            .args(&files)
            .status()
            .ok()?
            .success();
        if !ok {
            return None;
        }
        let mut v = Vec::new();
        for e in ignore::WalkBuilder::new(&out)
            .build()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "class"))
        {
            v.push((
                e.path()
                    .strip_prefix(&out)
                    .ok()?
                    .to_string_lossy()
                    .into_owned(),
                std::fs::read(e.path()).ok()?,
            ));
        }
        Some(v)
    }

    pub const COMPOSABLE_SRC: (&str, &str) = (
        "androidx/compose/runtime/Composable.java",
        "package androidx.compose.runtime;\nimport java.lang.annotation.*;\n@Retention(RetentionPolicy.CLASS)\npublic @interface Composable {}\n",
    );
    pub const TEXT_SRC: (&str, &str) = ("demo/ui/TextKt.java", "package demo.ui;\npublic final class TextKt {\n  @androidx.compose.runtime.Composable @Deprecated public static void Text(String s) {}
  @androidx.compose.runtime.Composable public static void Text(int resId) {}\n  public static int helper(long a, double b) { return 1; }\n  private static void hidden() {}\n}\n");

    #[test]
    fn reads_names_kinds_and_composable_statics() {
        let dir = std::env::temp_dir().join(format!("droid-lite-classfile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let Some(classes) = javac(
            &dir,
            &[
                COMPOSABLE_SRC,
                TEXT_SRC,
                (
                    "demo/ui/Mode.java",
                    "package demo.ui; public enum Mode { A, B; public static final long BIG = 1L << 40; }",
                ),
            ],
        ) else {
            eprintln!("no javac: skipped");
            return;
        };
        let get =
            |n: &str| parse(&classes.iter().find(|(p, _)| p.ends_with(n)).unwrap().1).unwrap();
        let text = get("TextKt.class");
        assert_eq!(text.name, "demo/ui/TextKt");
        assert!(text.public);
        assert_eq!(
            text.statics,
            vec![
                ("Text".to_string(), true),
                ("Text".to_string(), true),
                ("helper".to_string(), false)
            ]
        );
        assert_eq!(get("Composable.class").kind, ClassKind::Annotation);
        assert_eq!(get("Mode.class").kind, ClassKind::Enum);
        assert!(parse(b"not a class").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
