//! Images on review notes: kept as files in `<data>/review-images/<review id>/<name>`, never
//! inside the review's text or `notes.json`. The text names each file by its path (an agent
//! reads an image by path); `for_host` copies them to an SSH host and names the host's copies.

use crate::error::{AppError, AppResult};
use std::path::{Path, PathBuf};

/// Largest image a note takes.
pub const MAX_BYTES: usize = 10 * 1024 * 1024;
/// Most images on one note.
pub const MAX_PER_NOTE: usize = 10;

/// `<data>/review-images`.
pub fn root(data_dir: &Path) -> PathBuf {
    data_dir.join("review-images")
}

/// The image kind from the bytes themselves (whatever the name or the clipboard says).
pub fn sniff(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if b.starts_with(b"\xff\xd8\xff") {
        Some("jpg")
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some("gif")
    } else if b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some("webp")
    } else {
        None
    }
}

fn is_review_id(s: &str) -> bool {
    s.len() == 35 && s.starts_with("rv-") && s[3..].bytes().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// A name `add` made: 32 lowercase hex digits and one of the kinds `sniff` knows.
pub fn is_name(s: &str) -> bool {
    match s.split_once('.') {
        Some((stem, ext)) => {
            stem.len() == 32
                && stem.bytes().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
                && matches!(ext, "png" | "jpg" | "gif" | "webp")
        }
        None => false,
    }
}

/// Where an image of a review is (checked names only).
pub fn path(root: &Path, review: &str, name: &str) -> AppResult<PathBuf> {
    if !is_review_id(review) || !is_name(name) {
        return Err(AppError::Msg("Not an image of this review".into()));
    }
    Ok(root.join(review).join(name))
}

/// Saves an image for a review; returns its name.
pub fn add(root: &Path, review: &str, bytes: &[u8]) -> AppResult<String> {
    if bytes.len() > MAX_BYTES {
        return Err(AppError::Msg(format!("Images can be at most {} MB", MAX_BYTES / 1024 / 1024)));
    }
    let ext = sniff(bytes).ok_or_else(|| AppError::Msg("Only PNG, JPEG, GIF and WebP images can be added".into()))?;
    let name = format!("{}.{ext}", uuid::Uuid::new_v4().simple());
    let file = path(root, review, &name)?;
    std::fs::create_dir_all(file.parent().unwrap_or(root))?;
    std::fs::write(&file, bytes)?;
    Ok(name)
}

/// The kind's media type, for showing an image.
pub fn mime(name: &str) -> &'static str {
    match name.rsplit_once('.').map(|(_, e)| e) {
        Some("jpg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => "image/png",
    }
}

/// The review images `text` names: each (the text as written, the file). Only paths of the form
/// `<root>/<review id>/<name>` count, so nothing else on the computer is ever picked up.
pub fn refs(root: &Path, text: &str) -> Vec<(String, PathBuf)> {
    let prefix = root.to_string_lossy();
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    let mut from = 0;
    while let Some(at) = text[from..].find(prefix.as_ref()) {
        let start = from + at;
        let rest = &text[start + prefix.len()..];
        from = start + prefix.len();
        // `<sep>rv-…<sep>name`, either separator (Windows writes `\`)
        let mut parts = rest.splitn(3, ['/', '\\']);
        let (Some(""), Some(review), Some(tail)) = (parts.next(), parts.next(), parts.next()) else { continue };
        let name: String = tail.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '.').collect();
        let name = name.trim_end_matches('.');
        if let Ok(file) = path(root, review, name) {
            let written = format!("{}{}", prefix, &rest[..1 + review.len() + 1 + name.len()]);
            if !out.iter().any(|(w, _)| *w == written) {
                out.push((written, file));
            }
        }
    }
    out
}

/// Deletes the images no note names any more, except those of reviews still open. `bodies` are
/// every note's text.
pub fn prune(root: &Path, bodies: &[String], open: &[String]) {
    let Ok(dirs) = std::fs::read_dir(root) else { return };
    let named: std::collections::HashSet<PathBuf> = bodies.iter().flat_map(|b| refs(root, b)).map(|(_, p)| p).collect();
    for dir in dirs.flatten() {
        let review = dir.file_name().to_string_lossy().into_owned();
        if !is_review_id(&review) || open.contains(&review) {
            continue;
        }
        let Ok(files) = std::fs::read_dir(dir.path()) else { continue };
        for f in files.flatten() {
            if !named.contains(&f.path()) {
                let _ = std::fs::remove_file(f.path());
            }
        }
        let _ = std::fs::remove_dir(dir.path()); // only when empty
    }
}

/// `text` for a terminal in `cwd`: on an SSH host each review image it names is copied there
/// and named by the host's path; elsewhere the text as it is.
#[cfg(feature = "gui")]
pub async fn for_host(root: &Path, cwd: &str, text: &str) -> AppResult<String> {
    use crate::claude::remote::{self, Target};
    if !cwd.starts_with(remote::SCHEME) {
        return Ok(text.to_string());
    }
    let t = Target::parse(cwd).ok_or_else(|| AppError::Msg("Not a folder on an SSH host".into()))?;
    t.check()?;
    let mut out = text.to_string();
    for (written, file) in refs(root, text) {
        let bytes = tokio::fs::read(&file).await.map_err(|_| AppError::Msg(format!("An image of the review is gone: {}", file.display())))?;
        let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let there = remote::stage_bytes(&t, &name, &bytes).await?;
        out = out.replace(&written, &there);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nrest";
    const RV: &str = "rv-0123456789abcdef0123456789abcdef";

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("devcat-review-images-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn sniffs_by_bytes() {
        assert_eq!(sniff(PNG), Some("png"));
        assert_eq!(sniff(b"\xff\xd8\xff\xe0"), Some("jpg"));
        assert_eq!(sniff(b"GIF89a.."), Some("gif"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some("webp"));
        assert_eq!(sniff(b"<svg xmlns="), None);
        assert_eq!(sniff(b""), None);
    }

    #[test]
    fn add_checks_kind_size_and_review() {
        let root = tmp();
        let name = add(&root, RV, PNG).unwrap();
        assert!(is_name(&name) && name.ends_with(".png"));
        assert_eq!(std::fs::read(root.join(RV).join(&name)).unwrap(), PNG);
        assert!(add(&root, RV, b"<svg/>").is_err());
        assert!(add(&root, "../x", PNG).is_err());
        let big = [PNG, &vec![0u8; MAX_BYTES]].concat();
        assert!(add(&root, RV, &big).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn names_are_checked() {
        let n = "0123456789abcdef0123456789abcdef.png";
        assert!(is_name(n));
        for bad in ["../0123456789abcdef0123456789abcdef.png", "0123456789abcdef0123456789abcdef.svg",
            "0123456789ABCDEF0123456789abcdef.png", "x.png", "0123456789abcdef0123456789abcdef"] {
            assert!(!is_name(bad), "{bad}");
        }
        assert!(path(Path::new("/r"), RV, n).is_ok());
        assert!(path(Path::new("/r"), "rv-..", n).is_err());
    }

    #[test]
    fn refs_find_only_review_images() {
        let root = Path::new("/data/review-images");
        let n = "0123456789abcdef0123456789abcdef.png";
        let a = format!("/data/review-images/{RV}/{n}");
        let text = format!("1. x → y\n   (image: {a})\n2. again (image: {a}). /data/review-images/{RV}/../secret /etc/passwd /data/review-images/{RV}/x.png");
        let r = refs(root, &text);
        assert_eq!(r, vec![(a.clone(), root.join(RV).join(n))]);
        // Windows separators
        let wroot = Path::new(r"C:\Data\review-images");
        let w = format!(r"C:\Data\review-images\{RV}\{n}");
        assert_eq!(refs(wroot, &format!("(image: {w})")).len(), 1);
    }

    #[test]
    fn prune_keeps_named_and_open() {
        let root = tmp();
        let open = "rv-ffffffffffffffffffffffffffffffff";
        let kept = add(&root, RV, PNG).unwrap();
        let dropped = add(&root, RV, PNG).unwrap();
        let open_one = add(&root, open, PNG).unwrap();
        let body = format!("(image: {})", root.join(RV).join(&kept).display());
        prune(&root, &[body], &[open.to_string()]);
        assert!(root.join(RV).join(&kept).exists());
        assert!(!root.join(RV).join(&dropped).exists());
        assert!(root.join(open).join(&open_one).exists());
        prune(&root, &[], &[]);
        assert!(!root.join(RV).exists() && !root.join(open).exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
