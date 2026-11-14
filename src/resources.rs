//! Android resources: `values*/` entries (string, color, dimen, …), resource files (drawable,
//! layout, …) and `@+id/` ids, for ⌘-click on `R.string.x` / `@string/x`, hover values and colour
//! swatches.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub path: PathBuf,
    pub offset: usize,
    /// The text value for string / color / dimen / bool / integer entries.
    pub value: Option<String>,
    /// The folder qualifier, e.g. `values-night` or `drawable-xxhdpi` ("" for the default folder).
    pub qualifier: String,
    /// `translatable="false"`: never needs a translation.
    pub fixed: bool,
}

#[derive(Default)]
pub struct Resources {
    map: HashMap<(String, String), Vec<Entry>>,
    /// Manifest components by full class name, with the manifest that declares them.
    components: HashMap<String, (PathBuf, crate::android_kit::Component)>,
    /// Example deep links from the manifests' `VIEW` intent filters.
    pub links: Vec<String>,
}

const FILE_TYPES: &[&str] = &[
    "drawable",
    "mipmap",
    "layout",
    "menu",
    "raw",
    "xml",
    "anim",
    "animator",
    "font",
    "navigation",
    "color",
    "transition",
    "interpolator",
];
const VALUE_TAGS: &[&str] = &[
    "string",
    "color",
    "dimen",
    "bool",
    "integer",
    "plurals",
    "string-array",
    "integer-array",
    "array",
    "style",
    "attr",
    "declare-styleable",
    "fraction",
];

impl Resources {
    /// Walks every `res/` folder under the project root.
    pub fn scan(root: &Path) -> Resources {
        let mut r = Resources::default();
        let files = ignore::WalkBuilder::new(root)
            .hidden(true)
            .filter_entry(|e| {
                !matches!(
                    e.file_name().to_str(),
                    Some("build" | ".gradle" | ".git" | ".idea" | "node_modules")
                )
            })
            .build()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
            .map(|e| e.into_path())
            .filter(|p| {
                p.file_name().is_some_and(|n| n == "AndroidManifest.xml")
                    || p.parent()
                        .and_then(Path::parent)
                        .and_then(Path::file_name)
                        .is_some_and(|n| n == "res")
            });
        for p in files {
            if p.file_name().is_some_and(|n| n == "AndroidManifest.xml") {
                r.add_manifest(&p);
            } else {
                r.add_file(&p);
            }
        }
        r
    }

    /// Reads a module's `src/<set>/AndroidManifest.xml`; relative names start from its
    /// `package` or the module's `namespace`.
    fn add_manifest(&mut self, path: &Path) {
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        let module = path.ancestors().nth(3).unwrap_or(Path::new(""));
        let build = ["build.gradle.kts", "build.gradle"]
            .iter()
            .find_map(|f| std::fs::read_to_string(module.join(f)).ok())
            .unwrap_or_default();
        let ns = crate::android_kit::namespace(&text, &build);
        for l in crate::android_kit::deep_links(&text) {
            if !self.links.contains(&l) {
                self.links.push(l);
            }
        }
        for c in crate::android_kit::components(&text, &ns) {
            self.components
                .entry(c.class.clone())
                .or_insert_with(|| (path.to_path_buf(), c));
        }
    }

    /// Where the manifest declares this Activity / Service / Receiver / Provider / Application.
    pub fn component(&self, class: &str) -> Option<&(PathBuf, crate::android_kit::Component)> {
        self.components.get(class)
    }

    pub fn add_file(&mut self, path: &Path) {
        let Some(folder) = path
            .parent()
            .and_then(Path::file_name)
            .and_then(|n| n.to_str())
        else {
            return;
        };
        let (ty, qualifier) = folder.split_once('-').map_or((folder, ""), |(t, q)| (t, q));
        let qualifier = if qualifier.is_empty() {
            String::new()
        } else {
            folder.to_string()
        };
        let is_xml = path.extension().is_some_and(|e| e == "xml");
        if ty == "values" && is_xml {
            if let Ok(text) = std::fs::read_to_string(path) {
                for (kind, name, offset, value, fixed) in value_entries(&text) {
                    self.push(
                        kind,
                        name,
                        Entry {
                            path: path.to_path_buf(),
                            offset,
                            value,
                            qualifier: qualifier.clone(),
                            fixed,
                        },
                    );
                }
            }
            return;
        }
        if FILE_TYPES.contains(&ty) {
            let stem = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let name = stem.split('.').next().unwrap_or(stem); // icon.9.png → icon
            self.push(
                ty.to_string(),
                name.to_string(),
                Entry {
                    path: path.to_path_buf(),
                    offset: 0,
                    value: None,
                    qualifier: qualifier.clone(),
                    fixed: false,
                },
            );
            if is_xml && let Ok(text) = std::fs::read_to_string(path) {
                for (id, offset) in new_ids(&text) {
                    self.push(
                        "id".into(),
                        id,
                        Entry {
                            path: path.to_path_buf(),
                            offset,
                            value: None,
                            qualifier: qualifier.clone(),
                            fixed: false,
                        },
                    );
                }
            }
        }
    }

    fn push(&mut self, kind: String, name: String, e: Entry) {
        let v = self.map.entry((kind, name)).or_default();
        if !v.contains(&e) {
            v.push(e);
        }
    }

    /// Every string resource with its entries (default folder first), sorted by name.
    pub fn strings(&self) -> Vec<(&str, Vec<&Entry>)> {
        let mut out: Vec<(&str, Vec<&Entry>)> = self
            .map
            .iter()
            .filter(|((kind, _), _)| kind == "string")
            .map(|((_, name), v)| {
                let mut v: Vec<&Entry> = v.iter().collect();
                v.sort_by_key(|e| (!e.qualifier.is_empty(), e.qualifier.clone()));
                (name.as_str(), v)
            })
            .collect();
        out.sort_by_key(|(n, _)| *n);
        out
    }

    /// Entries for `R.<kind>.<name>`, the default folder first.
    pub fn get(&self, kind: &str, name: &str) -> Vec<&Entry> {
        let mut v: Vec<&Entry> = self
            .map
            .get(&(kind.to_string(), name.to_string()))
            .map(|v| v.iter().collect())
            .unwrap_or_default();
        v.sort_by_key(|e| (!e.qualifier.is_empty(), e.qualifier.clone()));
        v
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// The colour of `@color/x` / `R.color.x`, following one `@color/…` alias.
    pub fn color(&self, name: &str) -> Option<egui::Color32> {
        let v = self
            .get("color", name)
            .into_iter()
            .find_map(|e| e.value.clone())?;
        match v.strip_prefix("@color/") {
            Some(other) if other != name => self
                .get("color", other)
                .into_iter()
                .find_map(|e| e.value.as_deref().and_then(parse_hex)),
            _ => parse_hex(&v),
        }
    }
}

/// `<string name="x">value</string>` and friends: (kind, name, offset of the name, value).
fn value_entries(text: &str) -> Vec<(String, String, usize, Option<String>, bool)> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(i) = text[at..].find('<') {
        let start = at + i + 1;
        at = start;
        let tag: String = text[start..]
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '-')
            .collect();
        let header_end = text[start..].find('>').map_or(text.len(), |e| start + e);
        let header = &text[start..header_end];
        let (kind, item_type) = if tag == "item" {
            ("item", attr(header, "type"))
        } else {
            (tag.as_str(), None)
        };
        let kind = match (kind, item_type) {
            ("item", Some(t)) => t.to_string(),
            ("string-array" | "integer-array", _) => "array".to_string(),
            (k, _) if VALUE_TAGS.contains(&k) => k.to_string(),
            _ => continue,
        };
        let Some(name) = attr(header, "name") else {
            continue;
        };
        let name_at = start + header.find(&format!("\"{name}\"")).map_or(0, |p| p + 1);
        let value = (!header.ends_with('/')
            && matches!(
                kind.as_str(),
                "string" | "color" | "dimen" | "bool" | "integer" | "fraction"
            ))
        .then(|| {
            let body_start = header_end + 1;
            let close = format!("</{tag}>");
            text[body_start.min(text.len())..]
                .find(&close)
                .map(|e| text[body_start..body_start + e].trim().to_string())
        })
        .flatten();
        let fixed = attr(header, "translatable") == Some("false");
        out.push((kind, name.to_string(), name_at, value, fixed));
    }
    out
}

fn attr<'a>(header: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("{key}=\"");
    let i = header.find(&pat)? + pat.len();
    header[i..].split('"').next()
}

/// `@+id/name` declarations in a layout, menu or navigation file.
fn new_ids(text: &str) -> Vec<(String, usize)> {
    text.match_indices("@+id/")
        .map(|(i, m)| {
            let start = i + m.len();
            let name: String = text[start..]
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            (name, start)
        })
        .filter(|(n, _)| !n.is_empty())
        .collect()
}

/// `#RGB`, `#RRGGBB`, `#AARRGGBB` → colour.
pub fn parse_hex(s: &str) -> Option<egui::Color32> {
    let h = s.trim().strip_prefix('#')?;
    let n = u32::from_str_radix(h, 16).ok()?;
    let (a, r, g, b) = match h.len() {
        3 => (
            255,
            ((n >> 8) & 0xF) * 17,
            ((n >> 4) & 0xF) * 17,
            (n & 0xF) * 17,
        ),
        6 => (255, n >> 16, (n >> 8) & 0xFF, n & 0xFF),
        8 => (n >> 24, (n >> 16) & 0xFF, (n >> 8) & 0xFF, n & 0xFF),
        _ => return None,
    };
    Some(egui::Color32::from_rgba_unmultiplied(
        r as u8, g as u8, b as u8, a as u8,
    ))
}

/// A resource reference ending at the identifier `start..end` in `text`: `R.string.name` in code,
/// `@string/name` / `@+id/name` / `?attr/name` in XML. Returns (kind, name).
pub fn reference_at(text: &str, start: usize, end: usize) -> Option<(String, String)> {
    let name = text.get(start..end)?.to_string();
    let before = &text[..start];
    if let Some(rest) = before.strip_suffix('.') {
        // R.<kind>.<name>, also com.app.R.<kind>.<name>
        let kind: String = rest
            .chars()
            .rev()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let r = rest[..rest.len() - kind.len()].strip_suffix('.')?;
        return (r.ends_with('R') && !r.ends_with("eR") && !kind.is_empty())
            .then_some((kind, name));
    }
    let rest = before.strip_suffix('/')?;
    let kind: String = rest
        .chars()
        .rev()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let marker = &rest[..rest.len() - kind.len()];
    (marker.ends_with('@') || marker.ends_with("@+") || marker.ends_with('?'))
        .then_some((kind, name))
}

/// Colours written in code or XML on one line: `Color(0xFF6200EE)`, `#6200EE`.
pub fn literal_colors(line: &str) -> Vec<egui::Color32> {
    let mut out = Vec::new();
    for (i, _) in line.match_indices("Color(0x") {
        let hex: String = line[i + 8..]
            .chars()
            .take_while(|c| c.is_ascii_hexdigit())
            .collect();
        if let Ok(n) = u32::from_str_radix(&hex, 16) {
            let n = if hex.len() <= 6 { 0xFF00_0000 | n } else { n };
            out.push(egui::Color32::from_rgba_unmultiplied(
                (n >> 16) as u8,
                (n >> 8) as u8,
                n as u8,
                (n >> 24) as u8,
            ));
        }
    }
    for (i, _) in line.match_indices('#') {
        let hex: String = line[i + 1..]
            .chars()
            .take_while(|c| c.is_ascii_hexdigit())
            .collect();
        let next = line[i + 1 + hex.len()..].chars().next();
        // #RGB only in XML values (`>#FFF<`, `"#FFF"`); #RRGGBB / #AARRGGBB also in code and comments
        let ends = match hex.len() {
            3 => matches!(next, Some('<' | '"')),
            6 | 8 => next.is_none_or(|c| matches!(c, '<' | '"' | ' ' | '\'' | ')' | ',')),
            _ => false,
        };
        if ends {
            out.extend(parse_hex(&format!("#{hex}")));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_files_and_ids() {
        let dir = std::env::temp_dir().join(format!("droid-lite-res-{}", std::process::id()));
        let res = dir.join("app/src/main/res");
        for d in [
            "values",
            "values-night",
            "drawable",
            "drawable-xxhdpi",
            "layout",
        ] {
            std::fs::create_dir_all(res.join(d)).unwrap();
        }
        std::fs::write(res.join("values/strings.xml"), "<resources>\n  <string name=\"app_name\">Shop</string>\n  <color name=\"brand\">#6200EE</color>\n  <color name=\"accent\">@color/brand</color>\n  <dimen name=\"pad\">16dp</dimen>\n  <style name=\"Theme.Shop\" parent=\"x\"/>\n  <item type=\"id\" name=\"spare\"/>\n</resources>\n").unwrap();
        std::fs::write(
            res.join("values-night/strings.xml"),
            "<resources><color name=\"brand\">#BB86FC</color></resources>",
        )
        .unwrap();
        std::fs::write(res.join("drawable/ic_cart.xml"), "<vector/>").unwrap();
        std::fs::write(res.join("drawable-xxhdpi/logo.9.png"), "x").unwrap();
        std::fs::write(
            res.join("layout/main.xml"),
            "<Button android:id=\"@+id/buy\"/>",
        )
        .unwrap();
        let r = Resources::scan(&dir);
        assert_eq!(
            r.get("string", "app_name")[0].value.as_deref(),
            Some("Shop")
        );
        let brand = r.get("color", "brand");
        assert_eq!(brand.len(), 2);
        assert_eq!(brand[0].qualifier, "", "the default folder first");
        assert_eq!(
            r.color("accent"),
            Some(egui::Color32::from_rgb(0x62, 0x00, 0xEE)),
            "follows @color/brand"
        );
        assert_eq!(r.get("dimen", "pad")[0].value.as_deref(), Some("16dp"));
        assert_eq!(r.get("style", "Theme.Shop").len(), 1);
        assert_eq!(r.get("id", "spare").len(), 1);
        assert_eq!(r.get("drawable", "ic_cart").len(), 1);
        assert_eq!(r.get("drawable", "logo")[0].qualifier, "drawable-xxhdpi");
        let buy = &r.get("id", "buy")[0];
        assert_eq!(
            &std::fs::read_to_string(&buy.path).unwrap()[buy.offset..buy.offset + 3],
            "buy"
        );
        let s = &r.get("string", "app_name")[0];
        assert!(std::fs::read_to_string(&s.path).unwrap()[s.offset..].starts_with("app_name\""));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn references_in_code_and_xml() {
        let t = "Text(stringResource(R.string.app_name)) com.shop.R.drawable.logo @string/title @+id/buy ?attr/colorPrimary foo.bar";
        let at = |w: &str| {
            let s = t.find(w).unwrap();
            reference_at(t, s, s + w.len())
        };
        assert_eq!(at("app_name"), Some(("string".into(), "app_name".into())));
        assert_eq!(at("logo"), Some(("drawable".into(), "logo".into())));
        assert_eq!(at("title"), Some(("string".into(), "title".into())));
        assert_eq!(at("buy"), Some(("id".into(), "buy".into())));
        assert_eq!(
            at("colorPrimary"),
            Some(("attr".into(), "colorPrimary".into()))
        );
        assert_eq!(at("bar"), None);
    }

    #[test]
    fn colour_literals() {
        assert_eq!(
            literal_colors("val c = Color(0xFF6200EE)"),
            vec![egui::Color32::from_rgb(0x62, 0x00, 0xEE)]
        );
        assert_eq!(
            literal_colors("<color name=\"x\">#BB86FC</color>"),
            vec![egui::Color32::from_rgb(0xBB, 0x86, 0xFC)]
        );
        assert!(literal_colors("issue #123 fixed").is_empty());
    }
}
