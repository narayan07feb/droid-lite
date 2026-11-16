//! What pictures a preview function asks for, read from its annotations like Android Studio does:
//! each `@Preview(...)` is one picture with its parameters; a multipreview (`@PreviewLightDark`,
//! `@PreviewScreenSizes`, `@PreviewFontScale`, `@PreviewDynamicColors`, or a project annotation class
//! that carries previews, at any depth) is several.

use std::collections::HashMap;

/// One picture to draw.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Spec {
    /// The `@Preview` function (what the worker calls).
    pub function: String,
    /// The card title: the function, then the preview's name or what makes it differ ("Card - Dark").
    pub title: String,
    pub group: Option<String>,
    /// `uiMode` asks for night (`UI_MODE_NIGHT_YES`): dark theme, `values-night` resources.
    pub night: bool,
    pub font_scale: Option<f32>,
    pub width_dp: Option<u32>,
    pub height_dp: Option<u32>,
    pub show_background: bool,
    /// ARGB, when `backgroundColor` is given.
    pub background: Option<u32>,
    pub locale: Option<String>,
    /// `device = "spec:width=411dp,height=891dp"` or `Devices.PIXEL_5`, as written.
    pub device: Option<String>,
}

impl Spec {
    /// What the worker reads for one picture.
    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "function": self.function, "title": self.title, "night": self.night,
            "fontScale": self.font_scale, "widthDp": self.width_dp, "heightDp": self.height_dp,
            "showBackground": self.show_background, "background": self.background,
            "locale": self.locale, "device": self.device,
        })
    }
}

/// `@Name(args)` entries of an annotation list (args empty when there are no parentheses).
pub fn annotations(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'@' {
            i += 1;
            continue;
        }
        let start = i + 1;
        let mut j = start;
        while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] == b'.') {
            j += 1;
        }
        let name = text[start..j].rsplit('.').next().unwrap_or("").to_string();
        let mut k = j;
        while k < b.len() && b[k] == b' ' {
            k += 1;
        }
        let mut args = String::new();
        if k < b.len() && b[k] == b'(' {
            let mut depth = 0;
            let mut end = k;
            let mut in_str = false;
            for (n, &c) in b.iter().enumerate().skip(k) {
                match c {
                    b'"' => in_str = !in_str,
                    b'(' if !in_str => depth += 1,
                    b')' if !in_str => {
                        depth -= 1;
                        if depth == 0 {
                            end = n;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            args = text[k + 1..end].to_string();
            j = end + 1;
        }
        if !name.is_empty() {
            out.push((name, args));
        }
        i = j.max(i + 1);
    }
    out
}

/// The annotation lines right above the declaration that starts at `offset` (the line of `fun`).
pub fn annotations_before(text: &str, offset: usize) -> String {
    let start_of_line = text[..offset.min(text.len())].rfind('\n').map_or(0, |i| i + 1);
    let mut lines: Vec<&str> = Vec::new();
    let mut depth = 0i32; // inside a multi-line @Preview(...)
    for line in text[..start_of_line].lines().rev() {
        let t = line.trim();
        depth += t.matches(')').count() as i32 - t.matches('(').count() as i32;
        if t.starts_with('@') || depth > 0 || (t.is_empty() && depth > 0) || t.starts_with("//") {
            lines.push(line);
            continue;
        }
        if depth <= 0 && !t.ends_with(',') && !t.ends_with('(') {
            break;
        }
        lines.push(line);
    }
    lines.reverse();
    // the declaration line itself may carry annotations too (`@Preview @Composable fun …`)
    let decl_line = text[start_of_line..].lines().next().unwrap_or("");
    format!("{}\n{decl_line}", lines.join("\n"))
}

/// `key = value` pairs of annotation arguments.
fn args(a: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut depth = 0;
    let mut in_str = false;
    let mut part = String::new();
    let mut parts = Vec::new();
    for c in a.chars() {
        match c {
            '"' => in_str = !in_str,
            '(' if !in_str => depth += 1,
            ')' if !in_str => depth -= 1,
            ',' if !in_str && depth == 0 => {
                parts.push(std::mem::take(&mut part));
                continue;
            }
            _ => {}
        }
        part.push(c);
    }
    parts.push(part);
    for p in parts {
        if let Some((k, v)) = p.split_once('=') {
            out.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    out
}

fn string(v: &str) -> Option<String> {
    let v = v.trim();
    v.strip_prefix('"').and_then(|s| s.strip_suffix('"')).map(String::from)
}

fn number(v: &str) -> Option<u64> {
    let v = v.trim().trim_end_matches(['L', 'u', 'U']);
    match v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
        Some(h) => u64::from_str_radix(&h.replace('_', ""), 16).ok(),
        None => v.replace('_', "").parse().ok(),
    }
}

/// `uiMode` asks for night: `UI_MODE_NIGHT_YES` (0x20) in a constant expression or a number.
fn is_night(v: &str) -> bool {
    v.contains("UI_MODE_NIGHT_YES") || number(v).is_some_and(|n| n & 0x30 == 0x20)
}

/// One `@Preview(...)` as a spec (title without the function yet: `function` is filled by the caller).
fn from_preview(a: &str) -> Spec {
    let m = args(a);
    let get = |k: &str| m.get(k).map(String::as_str);
    Spec {
        title: get("name").and_then(string).unwrap_or_default(),
        group: get("group").and_then(string),
        night: get("uiMode").is_some_and(is_night),
        font_scale: get("fontScale").and_then(|v| v.trim_end_matches(['f', 'F']).parse().ok()),
        width_dp: get("widthDp").and_then(number).map(|n| n as u32),
        height_dp: get("heightDp").and_then(number).map(|n| n as u32),
        show_background: get("showBackground").is_some_and(|v| v.trim() == "true"),
        background: get("backgroundColor").and_then(number).map(|n| n as u32),
        locale: get("locale").and_then(string).filter(|s| !s.is_empty()),
        device: get("device").map(|v| string(v).unwrap_or_else(|| v.to_string())),
        ..Default::default()
    }
}

fn named(title: &str, f: impl FnOnce(&mut Spec)) -> Spec {
    let mut s = Spec { title: title.into(), ..Default::default() };
    f(&mut s);
    s
}

/// The pictures of a built-in multipreview, as Android Studio draws them.
fn builtin(name: &str) -> Option<Vec<Spec>> {
    Some(match name {
        "PreviewLightDark" => vec![named("Light", |_| {}), named("Dark", |s| s.night = true)],
        "PreviewScreenSizes" => vec![
            named("Phone", |s| s.device = Some("spec:width=411dp,height=891dp".into())),
            named("Phone - Landscape", |s| s.device = Some("spec:width=891dp,height=411dp".into())),
            named("Unfolded Foldable", |s| s.device = Some("spec:width=673dp,height=841dp".into())),
            named("Tablet", |s| s.device = Some("spec:width=1280dp,height=800dp".into())),
            named("Desktop", |s| s.device = Some("spec:width=1920dp,height=1080dp".into())),
        ],
        "PreviewFontScale" => [0.85f32, 1.0, 1.15, 1.3, 1.5, 1.8, 2.0]
            .iter()
            .map(|f| named(&format!("{}%", (f * 100.0).round() as u32), |s| s.font_scale = Some(*f)))
            .collect(),
        // the wallpaper colours cannot be faked off a phone: one picture with the default colours
        "PreviewDynamicColors" => vec![named("Dynamic colors (default wallpaper)", |_| {})],
        _ => return None,
    })
}

/// Expands one annotation into pictures: `@Preview`, a built-in multipreview, or a project annotation
/// class (from `classes`: its name -> the annotation text written above `annotation class Name`).
fn expand(name: &str, a: &str, classes: &HashMap<String, String>, depth: u32) -> Vec<Spec> {
    if name == "Preview" {
        return vec![from_preview(a)];
    }
    if let Some(v) = builtin(name) {
        return v;
    }
    if depth >= 6 {
        return vec![];
    }
    let Some(text) = classes.get(name) else { return vec![] };
    annotations(text)
        .into_iter()
        .flat_map(|(n, a)| expand(&n, &a, classes, depth + 1))
        .collect()
}

/// Every picture a preview function asks for, from its annotation text. None of its annotations
/// being a preview (e.g. a class not found) still gives one default picture: it is a @Preview function.
pub fn specs(function: &str, annotation_text: &str, classes: &HashMap<String, String>) -> Vec<Spec> {
    let mut out: Vec<Spec> = annotations(annotation_text)
        .into_iter()
        .flat_map(|(n, a)| expand(&n, &a, classes, 0))
        .collect();
    if out.is_empty() {
        out.push(Spec::default());
    }
    let many = out.len() > 1;
    for (i, s) in out.iter_mut().enumerate() {
        s.function = function.to_string();
        let what = if !s.title.is_empty() {
            s.title.clone()
        } else if many {
            // unnamed pictures of one function: say what differs, else number them
            let mut d = Vec::new();
            if s.night { d.push("Dark".to_string()); }
            if let Some(f) = s.font_scale { d.push(format!("{}%", (f * 100.0).round() as u32)); }
            if let Some(l) = &s.locale { d.push(l.clone()); }
            if let (Some(w), _) | (_, Some(w)) = (s.width_dp, s.height_dp) { d.push(format!("{w}dp")); }
            if d.is_empty() { format!("{}", i + 1) } else { d.join(", ") }
        } else {
            String::new()
        };
        s.title = if what.is_empty() { function.to_string() } else { format!("{function} - {what}") };
    }
    out
}

/// Annotation classes declared in a source text: name -> the annotation text above it.
pub fn annotation_classes(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(i) = text[at..].find("annotation class ") {
        let pos = at + i;
        let name: String = text[pos + 17..].chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
        if !name.is_empty() {
            out.push((name, annotations_before(text, pos)));
        }
        at = pos + 17;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn titles(v: &[Spec]) -> Vec<String> {
        v.iter().map(|s| s.title.clone()).collect()
    }

    #[test]
    fn light_dark_gives_two_pictures_and_dark_is_night() {
        let v = specs("Card", "@PreviewLightDark\n@Composable\nprivate fun Card()", &HashMap::new());
        assert_eq!(titles(&v), ["Card - Light", "Card - Dark"]);
        assert!(!v[0].night && v[1].night);
    }

    #[test]
    fn preview_parameters_are_read() {
        let text = "@Preview(\n    name = \"Big, dark\",\n    group = \"g\",\n    uiMode = Configuration.UI_MODE_NIGHT_YES or Configuration.UI_MODE_TYPE_NORMAL,\n    widthDp = 320, heightDp = 640,\n    fontScale = 1.5f,\n    showBackground = true, backgroundColor = 0xFF101010,\n    locale = \"fr\",\n    device = Devices.PIXEL_5,\n)\n@Composable\nfun Card()";
        let v = specs("Card", text, &HashMap::new());
        assert_eq!(v.len(), 1);
        let s = &v[0];
        assert_eq!(s.title, "Card - Big, dark");
        assert_eq!(s.group.as_deref(), Some("g"));
        assert!(s.night);
        assert_eq!((s.width_dp, s.height_dp, s.font_scale), (Some(320), Some(640), Some(1.5)));
        assert!(s.show_background);
        assert_eq!(s.background, Some(0xFF101010));
        assert_eq!(s.locale.as_deref(), Some("fr"));
        assert_eq!(s.device.as_deref(), Some("Devices.PIXEL_5"));
        let n = specs("C", "@Preview(uiMode = 0x21)", &HashMap::new());
        assert!(n[0].night, "a number with the night bits");
    }

    #[test]
    fn built_in_multipreviews_expand_like_android_studio() {
        let m = HashMap::new();
        assert_eq!(specs("C", "@PreviewScreenSizes", &m).len(), 5);
        let f = specs("C", "@PreviewFontScale", &m);
        assert_eq!(f.len(), 7);
        assert_eq!(f[3].title, "C - 130%");
        assert_eq!(f[3].font_scale, Some(1.3));
        assert_eq!(specs("C", "@PreviewDynamicColors", &m).len(), 1);
    }

    #[test]
    fn project_annotation_classes_expand_at_any_depth_and_unnamed_ones_say_what_differs() {
        let mut classes = HashMap::new();
        classes.insert("ThemePreviews".to_string(), "@Preview(name = \"Light\")\n@Preview(name = \"Dark\", uiMode = UI_MODE_NIGHT_YES)\n".to_string());
        classes.insert("AllPreviews".to_string(), "@ThemePreviews\n@PreviewFontScale\n".to_string());
        let v = specs("Screen", "@AllPreviews\n@Composable\nfun Screen()", &classes);
        assert_eq!(v.len(), 9);
        assert_eq!(&titles(&v)[..2], ["Screen - Light", "Screen - Dark"]);
        let two = specs("S", "@Preview\n@Preview(uiMode = Configuration.UI_MODE_NIGHT_YES)", &HashMap::new());
        assert_eq!(titles(&two), ["S - 1", "S - Dark"]);
        assert_eq!(titles(&specs("S", "@Preview\n@Composable", &HashMap::new())), ["S"], "a single preview is just the function");
    }

    #[test]
    fn annotations_above_a_declaration_and_annotation_classes_are_found() {
        let src = "package a\n\n@Preview(\n    name = \"x\",\n)\n@PreviewLightDark\n@Composable\nprivate fun Card() {}\n\n@Preview(name = \"L\")\n@Preview(name = \"D\", uiMode = UI_MODE_NIGHT_YES)\nannotation class ThemePreviews\n";
        let off = src.find("private fun Card").unwrap();
        let a = annotations_before(src, off);
        let names: Vec<String> = annotations(&a).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["Preview", "PreviewLightDark", "Composable"]);
        let classes = annotation_classes(src);
        assert_eq!(classes.len(), 1);
        assert_eq!(classes[0].0, "ThemePreviews");
        assert_eq!(annotations(&classes[0].1).len(), 2);
    }
}
