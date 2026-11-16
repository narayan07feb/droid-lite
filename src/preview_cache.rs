//! Drawn previews kept on disk, so an unchanged file shows its pictures at once.
//!
//! The key hashes everything a picture depends on: the open file's text, the project code it
//! compiles with (paths, times, sizes), the preview specs, the Kotlin version and the libraries.
//! Any change gives a new key, so the file is drawn again. Owner (16 Nov): at most 20 pictures in
//! all (newest files first, one entry per file), none older than 30 minutes.

use egui::ColorImage;
use std::path::{Path, PathBuf};

/// Pictures kept in all; the newest entry stays even when it alone has more.
pub const MAX_IMAGES: usize = 20;
pub const TTL_SECS: u64 = 30 * 60;

pub type Cards = Vec<(String, Result<ColorImage, String>)>;

/// FNV-1a over the parts, in order.
pub fn key(parts: &[&str]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for p in parts {
        for b in p.bytes().chain([0u8]) {
            h = (h ^ b as u64).wrapping_mul(0x100000001b3);
        }
    }
    format!("{h:016x}")
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// One stored entry: `<root>/<key>/meta.json` (file, time, card names and errors) and `<i>.png`.
fn meta(root: &Path, key: &str) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(root.join(key).join("meta.json")).ok()?;
    serde_json::from_str(&text).ok()
}

/// The pictures stored under `key`, when younger than the TTL and all still readable.
pub fn lookup(root: &Path, key: &str, now: u64) -> Option<Cards> {
    let m = meta(root, key)?;
    let at = m["at"].as_u64()?;
    if now.saturating_sub(at) >= TTL_SECS {
        return None;
    }
    let dir = root.join(key);
    m["cards"]
        .as_array()?
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let name = c["name"].as_str()?.to_string();
            if let Some(e) = c["error"].as_str() {
                return Some((name, Err(e.to_string())));
            }
            let img = image::open(dir.join(format!("{i}.png"))).ok()?.to_rgba8();
            let size = [img.width() as usize, img.height() as usize];
            Some((
                name,
                Ok(ColorImage::from_rgba_unmultiplied(size, img.as_raw())),
            ))
        })
        .collect()
}

/// Stores the pictures of `file` under `key`, replaces that file's older entry, then keeps the
/// newest entries younger than the TTL up to MAX_IMAGES pictures.
pub fn store(root: &Path, key: &str, file: &Path, cards: &Cards, now: u64) {
    let dir = root.join(key);
    let _ = std::fs::remove_dir_all(&dir);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let mut list = Vec::new();
    for (i, (name, pic)) in cards.iter().enumerate() {
        match pic {
            Ok(img) => {
                let [w, h] = img.size;
                let raw: Vec<u8> = img
                    .pixels
                    .iter()
                    .flat_map(|c| c.to_srgba_unmultiplied())
                    .collect();
                let saved = image::RgbaImage::from_raw(w as u32, h as u32, raw)
                    .is_some_and(|p| p.save(dir.join(format!("{i}.png"))).is_ok());
                if !saved {
                    let _ = std::fs::remove_dir_all(&dir); // never a half entry
                    return;
                }
                list.push(serde_json::json!({ "name": name }));
            }
            Err(e) => list.push(serde_json::json!({ "name": name, "error": e })),
        }
    }
    let m = serde_json::json!({ "file": file, "at": now, "cards": list });
    if std::fs::write(dir.join("meta.json"), m.to_string()).is_err() {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    evict(root, key, file, now);
}

/// Drops entries that are too old and other entries of the same file, then the oldest entries
/// until at most MAX_IMAGES pictures are left (the newest entry always stays).
fn evict(root: &Path, keep_key: &str, file: &Path, now: u64) {
    let Ok(rd) = std::fs::read_dir(root) else {
        return;
    };
    let mut live: Vec<(u64, usize, PathBuf)> = Vec::new();
    for e in rd.flatten() {
        let k = e.file_name().to_string_lossy().into_owned();
        let m = meta(root, &k);
        let at = m.as_ref().and_then(|m| m["at"].as_u64());
        let same_file = m
            .as_ref()
            .is_some_and(|m| m["file"].as_str() == file.to_str());
        let pictures = m
            .as_ref()
            .and_then(|m| m["cards"].as_array())
            .map_or(0, |c| c.iter().filter(|c| c["error"].is_null()).count());
        match at {
            Some(at) if now.saturating_sub(at) < TTL_SECS && (k == keep_key || !same_file) => {
                live.push((at, pictures, e.path()))
            }
            _ => {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
    live.sort_by_key(|e| std::cmp::Reverse(e.0));
    let mut total = 0;
    for (i, (_, n, p)) in live.into_iter().enumerate() {
        total += n;
        if i > 0 && total > MAX_IMAGES {
            let _ = std::fs::remove_dir_all(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dl-pcache-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn cards() -> Cards {
        let img = ColorImage::new([2, 3], vec![egui::Color32::from_rgb(10, 20, 30); 6]);
        vec![
            ("Card - Light".into(), Ok(img)),
            ("Card - Dark".into(), Err("boom".into())),
        ]
    }

    fn entries(root: &Path) -> usize {
        std::fs::read_dir(root).unwrap().count()
    }

    #[test]
    fn a_stored_entry_reads_back_until_the_ttl() {
        let root = tmp("ttl");
        let k = key(&["text", "own"]);
        store(&root, &k, Path::new("/p/Card.kt"), &cards(), 1000);
        let got = lookup(&root, &k, 1000 + TTL_SECS - 1).expect("hit before the TTL");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, "Card - Light");
        let img = got[0].1.as_ref().unwrap();
        assert_eq!(img.size, [2, 3]);
        assert_eq!(img.pixels[0], egui::Color32::from_rgb(10, 20, 30));
        assert_eq!(got[1].1, Err("boom".to_string()));
        assert!(
            lookup(&root, &k, 1000 + TTL_SECS).is_none(),
            "expired at 30 minutes"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_change_gives_a_new_key_and_replaces_the_files_old_entry() {
        assert_ne!(key(&["a", "b"]), key(&["a", "c"]));
        assert_ne!(key(&["ab", "c"]), key(&["a", "bc"]), "parts are separated");
        let root = tmp("replace");
        let file = Path::new("/p/Card.kt");
        let (old, new) = (key(&["v1"]), key(&["v2"]));
        store(&root, &old, file, &cards(), 1000);
        store(&root, &new, file, &cards(), 1010);
        assert!(
            lookup(&root, &old, 1010).is_none(),
            "the file's older entry is gone"
        );
        assert!(lookup(&root, &new, 1010).is_some());
        assert_eq!(entries(&root), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn at_most_twenty_pictures_newest_first_and_no_expired_ones() {
        let root = tmp("keep");
        // one picture each (cards() has one picture and one error card)
        for i in 0..22u64 {
            let f = PathBuf::from(format!("/p/F{i}.kt"));
            store(&root, &key(&[&i.to_string()]), &f, &cards(), 1000 + i);
        }
        assert_eq!(entries(&root), MAX_IMAGES);
        assert!(
            lookup(&root, &key(&["1"]), 1030).is_none(),
            "the oldest went"
        );
        assert!(lookup(&root, &key(&["2"]), 1030).is_some());
        // the newest stays even when it alone has more than 20 pictures
        let img = ColorImage::new([1, 1], vec![egui::Color32::WHITE]);
        let big: Cards = (0..21)
            .map(|i| (format!("P{i}"), Ok(img.clone())))
            .collect();
        store(&root, &key(&["big"]), Path::new("/p/Big.kt"), &big, 1100);
        assert_eq!(entries(&root), 1);
        assert!(lookup(&root, &key(&["big"]), 1101).is_some());
        // a store after the TTL also clears the expired ones
        store(
            &root,
            &key(&["late"]),
            Path::new("/p/Late.kt"),
            &cards(),
            1100 + TTL_SECS,
        );
        assert_eq!(entries(&root), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_missing_picture_is_a_miss_not_a_blank_card() {
        let root = tmp("missing");
        let k = key(&["x"]);
        store(&root, &k, Path::new("/p/X.kt"), &cards(), 1000);
        std::fs::remove_file(root.join(&k).join("0.png")).unwrap();
        assert!(lookup(&root, &k, 1001).is_none());
        let _ = std::fs::remove_dir_all(root);
    }
}
