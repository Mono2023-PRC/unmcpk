//! Rebuild the original source tree.
//!
//! `file_id` inside the container is a lossy hash of the file name, so the name
//! cannot be recovered from the container itself. The compiled modules however
//! carry their own path in `co_filename`, which is what the game's own
//! tracebacks print — that is the only trustworthy source for the layout and it
//! is why the tool parses before it writes.

use crate::mcpk::{DirId, FileId};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Characters Windows refuses in a path component.
const ILLEGAL: &[char] = &['<', '>', ':', '"', '|', '?', '*', '\0'];

/// Reserved DOS device names, rejected even with an extension.
const RESERVED: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

fn sanitize_component(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_end_matches('.');
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return None;
    }
    let mut out: String = trimmed
        .chars()
        .map(|c| match c {
            c if ILLEGAL.contains(&c) => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    if out.len() > 100 {
        out.truncate(100);
    }
    let stem = out
        .split('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if RESERVED.contains(&stem.as_str()) {
        out.insert(0, '_');
    }
    Some(out)
}

/// Turn a `co_filename` into a safe relative path (without the extension).
///
/// Absolute build paths (`C:\build\client\foo\bar.py`, `/home/ci/x.py`) are
/// reduced to the part that is meaningful, and `..` never survives.
pub fn relative_path(filename: &str) -> Option<PathBuf> {
    if filename.trim().is_empty() {
        return None;
    }
    let unified = filename.replace('\\', "/");
    // Drop a leading drive or volume marker and any absolute root.
    let mut rest = unified.as_str();
    if let Some(idx) = rest.find(':') {
        // "C:/x" or "//server/share/x" style prefixes.
        if idx == 1 || rest.starts_with("//") {
            rest = &rest[idx + 1..];
        }
    }
    let mut parts: Vec<String> = Vec::new();
    for seg in rest.split('/') {
        if seg.is_empty() {
            continue;
        }
        match sanitize_component(seg) {
            Some(s) => parts.push(s),
            None => continue,
        }
    }
    if parts.is_empty() {
        return None;
    }
    // Normalise the extension: everything is written as .py.
    if let Some(last) = parts.last_mut() {
        let lowered = last.to_ascii_lowercase();
        for ext in [".pyc", ".pyo", ".py"] {
            if lowered.ends_with(ext) {
                let keep = last.len() - ext.len();
                last.truncate(keep);
                break;
            }
        }
        if last.is_empty() {
            parts.pop();
        }
    }
    if parts.is_empty() {
        return None;
    }
    let mut path = PathBuf::new();
    for p in parts {
        path.push(p);
    }
    path.set_extension("py");
    Some(path)
}

/// Hands out collision-free output paths, remembering what it handed out.
#[derive(Default)]
pub struct PathAllocator {
    used: HashMap<PathBuf, (DirId, FileId)>,
}

impl PathAllocator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `rel` for `(dir, id)`. Collisions with a *different* source are
    /// disambiguated with the file id, so a run is fully deterministic.
    pub fn allocate(&mut self, rel: Option<PathBuf>, dir: DirId, id: FileId) -> PathBuf {
        let fallback = || PathBuf::from(format!("{:08x}_{:08x}.py", dir.0, id.0));
        let mut candidate = rel.unwrap_or_else(fallback);
        if let Some((prev_dir, prev_id)) = self.used.get(&candidate) {
            if *prev_dir == dir && *prev_id == id {
                return candidate;
            }
            let stem = candidate
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "module".to_string());
            let parent = candidate.parent().map(Path::to_path_buf);
            let mut name = format!("{stem}__{:08x}_{:08x}.py", dir.0, id.0);
            candidate = match parent {
                Some(p) => p.join(name),
                None => PathBuf::from(&name),
            };
            // Extremely unlikely, but keep the loop honest.
            let mut n = 1;
            while self.used.contains_key(&candidate) {
                name = format!("{stem}__{:08x}_{:08x}_{n}.py", dir.0, id.0);
                n += 1;
                candidate = match candidate.parent() {
                    Some(p) => p.join(&name),
                    None => PathBuf::from(&name),
                };
            }
        }
        self.used.insert(candidate.clone(), (dir, id));
        candidate
    }

    pub fn len(&self) -> usize {
        self.used.len()
    }

    pub fn is_empty(&self) -> bool {
        self.used.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_build_prefixes() {
        assert_eq!(
            relative_path("C:\\build\\client\\game\\mod\\realms_main.py"),
            Some(PathBuf::from("build/client/game/mod/realms_main.py"))
        );
        assert_eq!(
            relative_path("ogc_api.pyc"),
            Some(PathBuf::from("ogc_api.py"))
        );
        assert_eq!(relative_path(""), None);
        assert_eq!(relative_path("../../etc/passwd"), Some(PathBuf::from("etc/passwd.py")));
    }

    #[test]
    fn rejects_reserved_names() {
        assert_eq!(relative_path("con.py"), Some(PathBuf::from("_con.py")));
    }

    #[test]
    fn deduplicates_collisions() {
        let mut a = PathAllocator::new();
        let first = a.allocate(relative_path("a/b.py"), DirId(1), FileId(1));
        let again = a.allocate(relative_path("a/b.py"), DirId(1), FileId(1));
        let other = a.allocate(relative_path("a/b.py"), DirId(1), FileId(2));
        assert_eq!(first, again);
        assert_ne!(first, other);
    }
}
