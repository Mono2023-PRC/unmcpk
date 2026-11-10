//! Independent check of a finished unpack.
//!
//! The manifest is the tool's own claim, so it is not trusted: every emitted
//! `.pyc` is read back, its magic checked, and its contents re-parsed with this
//! crate's marshal reader. The re-parsed code-object count and embedded
//! filename have to match what the manifest recorded, and the number of `.pyc`
//! files on disk has to match the number of modules, so a lost or duplicated
//! file shows up too.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde_json::Value;

use crate::nemarshal::{self, Options};
use crate::pyc::PYC_HEADER;

/// What a verification run found. Problems are collected instead of printed so
/// the caller (the CLI or an example) decides how loud to be.
#[derive(Debug, Default)]
pub struct Report {
    pub modules: usize,
    pub assets: usize,
    pub objects: usize,
    pub files_on_disk: usize,
    pub layers: BTreeMap<String, usize>,
    pub trailers: BTreeMap<String, usize>,
    pub problems: Vec<String>,
}

impl Report {
    /// True when every module parsed back cleanly and the file count matches.
    pub fn is_ok(&self) -> bool {
        self.problems.is_empty() && self.files_on_disk == self.modules
    }
}

/// Re-reads every pyc under `out` and compares it against `out/manifest.json`.
pub fn verify_dir(out: &Path) -> Result<Report> {
    let text = std::fs::read_to_string(out.join("manifest.json"))
        .map_err(|err| anyhow!("cannot read {}/manifest.json: {err}", out.display()))?;
    let manifest: Value = serde_json::from_str(&text)?;
    let entries = manifest["entries"]
        .as_array()
        .ok_or_else(|| anyhow!("manifest has no entries"))?;

    let mut report = Report::default();
    for entry in entries {
        *report
            .trailers
            .entry(
                entry["trailer_marker"]
                    .as_bool()
                    .map(|marker| marker.to_string())
                    .unwrap_or_else(|| "none".into()),
            )
            .or_default() += 1;
        if entry["asset"].as_bool().unwrap_or(false) {
            report.assets += 1;
            continue;
        }
        report.modules += 1;
        if let Some(layer) = entry["layer"].as_str() {
            *report.layers.entry(layer.to_string()).or_default() += 1;
        }
        let label = format!(
            "dir {:08x} file {:08x}",
            entry["dir_id"].as_u64().unwrap_or(0),
            entry["file_id"].as_u64().unwrap_or(0)
        );
        let Some(rel) = entry["pyc"].as_str() else {
            report.problems.push(format!("{label}: no pyc recorded"));
            continue;
        };
        let path = out.join("pyc").join(rel);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) => {
                report
                    .problems
                    .push(format!("{label}: cannot read {}: {err}", path.display()));
                continue;
            }
        };
        if bytes.len() < PYC_HEADER.len() || bytes[..4] != PYC_HEADER[..4] {
            report
                .problems
                .push(format!("{label}: bad magic in {rel}"));
            continue;
        }
        match nemarshal::parse_module(&bytes[PYC_HEADER.len()..], Options::default()) {
            Ok(parsed) => {
                let count = parsed.code.count_objects();
                report.objects += count;
                let expected = entry["code_objects"].as_u64().unwrap_or(0) as usize;
                if expected != 0 && count != expected {
                    report.problems.push(format!(
                        "{label}: {count} code objects, manifest says {expected}"
                    ));
                }
                if let Some(want) = entry["co_filename"].as_str() {
                    if parsed.code.filename_str() != Some(want) {
                        report.problems.push(format!(
                            "{label}: filename {:?}, manifest says {want:?}",
                            parsed.code.filename_str()
                        ));
                    }
                }
            }
            Err(err) => report
                .problems
                .push(format!("{label}: pyc does not parse back: {err}")),
        }
    }
    report.files_on_disk = walk(out.join("pyc"))
        .iter()
        .filter(|path| path.extension().map_or(false, |ext| ext == "pyc"))
        .count();
    Ok(report)
}

fn walk(root: PathBuf) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root];
    while let Some(path) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files
}
