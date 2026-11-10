//! Orchestration: decode every file, rebuild the tree, decompile, report.
//!
//! The pipeline is deliberately two-phase. Phase one is pure and parallel: for
//! every container entry try the layer candidates, parse the marshal stream and
//! keep the first result that looks right. Phase two is sequential and writes
//! files in container order, so two runs on the same input produce identical
//! output trees and identical manifests.

use crate::decompile::{fix_mojibake, Decompiler, Kind};
use crate::mcpk::{Container, DirId, FileId};
use crate::nemarshal::{self, Code, Options as MarshalOptions};
use crate::pathmap::{relative_path, PathAllocator};
use crate::payload::{apply_layer, string_id, LayerKind, Trailer};
use crate::pyc;
use crate::rotor::{default_key, Rotor};
use anyhow::{Context, Result};
use rayon::prelude::*;
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct Options {
    pub output: PathBuf,
    pub jobs: usize,
    pub write_pyc: bool,
    pub write_source: bool,
    pub write_raw: bool,
    pub old_compat: bool,
    /// Keep the original bytes of the decoy (version-check) code objects
    /// instead of replacing them with `return None`.
    pub keep_decoys: bool,
    pub text_fix: bool,
    pub limit: Option<usize>,
    pub decompiler: Option<String>,
    pub python: Option<String>,
    pub quiet: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            output: PathBuf::from("unpacked"),
            jobs: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4),
            write_pyc: true,
            write_source: true,
            write_raw: false,
            old_compat: false,
            keep_decoys: false,
            text_fix: true,
            limit: None,
            decompiler: None,
            python: None,
            quiet: false,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct FileReport {
    pub dir_id: u32,
    pub file_id: u32,
    pub raw_size: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload_size: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub marshal_size: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trailer: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trailer_marker: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code_objects: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decoys: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opcode_type: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub co_filename: Option<String>,
    pub asset: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pyc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decompiled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub tool: &'static str,
    pub version: &'static str,
    pub container: String,
    pub container_size: usize,
    pub directories: usize,
    pub files: usize,
    pub files_ok: usize,
    pub files_failed: usize,
    pub assets: usize,
    pub code_objects: u64,
    pub decoy_code_objects: u64,
    /// Unknown opcode bytes seen only inside decoy objects.
    pub decoy_unknown_opcodes: u64,
    pub layers: BTreeMap<String, usize>,
    pub trailers: BTreeMap<String, usize>,
    pub opcode_tables: BTreeMap<String, usize>,
    pub unknown_opcodes: BTreeMap<String, usize>,
    pub decompiler: String,
    pub decompiled_ok: usize,
    pub decompiled_failed: usize,
    pub timings_ms: BTreeMap<String, f64>,
    pub entries: Vec<FileReport>,
}

/// One container entry, decoded and parsed.
struct Decoded {
    dir: DirId,
    id: FileId,
    raw_size: usize,
    layer: Option<LayerKind>,
    payload_len: usize,
    trailer: Option<Trailer>,
    /// `None` for pure assets (no marshal payload at all).
    parsed: Option<nemarshal::Parsed>,
    error: Option<String>,
}

fn decode_entry(entry: &crate::mcpk::FileEntry, body: &[u8], opts: &Options) -> Decoded {
    let mut rotor = Rotor::new(6, &default_key());
    let mut best: Option<(u8, LayerKind, nemarshal::Parsed, Trailer)> = None;
    let mut first_error: Option<String> = None;

    for kind in LayerKind::ALL {
        let Some(payload) = apply_layer(kind, body, &mut rotor) else {
            continue;
        };
        let marshal_opts = MarshalOptions {
            expand_interned: true,
            old_compat: opts.old_compat,
            keep_decoys: opts.keep_decoys,
        };
        match nemarshal::parse_module(&payload, marshal_opts) {
            Ok(mut parsed) => {
                let trailer = Trailer::detect(&payload, parsed.consumed);
                // Prefer a candidate that consumes the payload exactly, or
                // leaves a plausible filler+marker tail.
                let score = if trailer.len == 0 {
                    3
                } else if trailer.marker && trailer.len <= 4096 {
                    2
                } else {
                    1
                };
                parsed.stats.unconsumed = trailer.len;
                let candidate = (score, kind, parsed, trailer);
                let better = match &best {
                    Some((s, _, _, _)) => score > *s,
                    None => true,
                };
                if better {
                    best = Some(candidate);
                }
                if score >= 3 {
                    break;
                }
            }
            Err(e) => {
                if first_error.is_none() {
                    first_error = Some(format!("{}: {e}", kind.label()));
                }
            }
        }
    }

    match best {
        Some((_, kind, parsed, trailer)) => {
            let payload_len = parsed.consumed + parsed.stats.unconsumed;
            Decoded {
                dir: entry.dir,
                id: entry.id,
                raw_size: body.len(),
                layer: Some(kind),
                payload_len,
                trailer: Some(trailer),
                parsed: Some(parsed),
                error: None,
            }
        }
        None => Decoded {
            dir: entry.dir,
            id: entry.id,
            raw_size: body.len(),
            layer: None,
            payload_len: 0,
            trailer: None,
            parsed: None,
            error: Some(first_error.unwrap_or_else(|| "no layer candidate decoded".into())),
        },
    }
}

/// Legacy behaviour: drop the "new version" check function from the module
/// constant table and shorten the bytecode accordingly. Kept behind a flag
/// because it corrupts the module's own code.
fn apply_old_compat(code: &mut Code) {
    let last_code = code.constants.iter().rposition(|c| c.is_code());
    let Some(idx) = last_code else { return };
    code.constants.remove(idx);
    let len = code.bytecode.len();
    if len >= 10 {
        let tail = code.bytecode[len - 1];
        code.bytecode.truncate(len - 10);
        code.bytecode.push(tail);
    }
}

pub fn run(container_path: &Path, container: &Container, opts: &Options) -> Result<Report> {
    let started = Instant::now();
    let files = match opts.limit {
        Some(n) => container.files.iter().take(n).collect::<Vec<_>>(),
        None => container.files.iter().collect::<Vec<_>>(),
    };

    let total = files.len();
    let done = AtomicUsize::new(0);
    let quiet = opts.quiet;

    // ---- phase 1: decode + parse (parallel, pure) --------------------------
    let decoded: Vec<Decoded> = files
        .par_iter()
        .map(|entry| {
            let body = container.body(entry);
            let d = decode_entry(entry, body, opts);
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            if !quiet && (n % 500 == 0 || n == total) {
                eprintln!("  decoded {n}/{total}");
            }
            d
        })
        .collect();
    let decode_ms = started.elapsed().as_secs_f64() * 1000.0;

    // ---- phase 2: write the tree (sequential, deterministic) --------------
    let write_start = Instant::now();
    let pyc_dir = opts.output.join("pyc");
    let src_dir = opts.output.join("src");
    let raw_dir = opts.output.join("raw");
    if opts.write_pyc {
        std::fs::create_dir_all(&pyc_dir)?;
    }
    if opts.write_source {
        std::fs::create_dir_all(&src_dir)?;
    }
    if opts.write_raw {
        std::fs::create_dir_all(&raw_dir)?;
    }

    let mut allocator = PathAllocator::new();
    let mut entries: Vec<FileReport> = Vec::with_capacity(decoded.len());
    let mut jobs: Vec<(PathBuf, PathBuf, usize)> = Vec::new();
    let mut layers: BTreeMap<String, usize> = BTreeMap::new();
    let mut trailers: BTreeMap<String, usize> = BTreeMap::new();
    let mut opcode_tables: BTreeMap<String, usize> = BTreeMap::new();
    let mut unknown_opcodes: BTreeMap<String, usize> = BTreeMap::new();
    let mut files_ok = 0usize;
    let files_failed = 0usize;
    let mut assets = 0usize;
    let mut code_objects = 0u64;
    let mut decoy_code_objects = 0u64;
    let mut decoy_unknown_opcodes = 0u64;

    for (i, d) in decoded.iter().enumerate() {
        let mut report = FileReport {
            dir_id: d.dir.0,
            file_id: d.id.0,
            raw_size: d.raw_size,
            layer: d.layer.map(|l| l.label().to_string()),
            payload_size: Some(d.payload_len),
            marshal_size: None,
            trailer: None,
            trailer_marker: None,
            code_objects: None,
            decoys: None,
            opcode_type: None,
            co_filename: None,
            asset: false,
            pyc: None,
            source: None,
            decompiled: None,
            error: d.error.clone(),
        };
        if let Some(layer) = d.layer {
            *layers.entry(layer.label().to_string()).or_insert(0) += 1;
        }

        let Some(parsed) = &d.parsed else {
            // Pure asset (or an entry that is not Python at all): keep the raw
            // bytes so nothing is lost.
            report.asset = true;
            assets += 1;
            if d.error.is_none() {
                report.error = None;
            }
            if opts.write_raw {
                let name = format!("{:08x}_{:08x}.bin", d.dir.0, d.id.0);
                std::fs::write(raw_dir.join(&name), container.body(files[i])).ok();
            }
            entries.push(report);
            continue;
        };

        let trailer_key = format!(
            "{}{}",
            parsed.stats.unconsumed,
            if d.trailer.map(|t| t.marker).unwrap_or(false) {
                "+fffe"
            } else {
                ""
            }
        );
        *trailers.entry(trailer_key).or_insert(0) += 1;
        for (t, n) in &parsed.stats.opcode_tables {
            *opcode_tables.entry(format!("0x{t:08x}")).or_insert(0) += n;
        }
        for ((t, op), n) in &parsed.stats.unknown_opcodes {
            *unknown_opcodes
                .entry(format!("0x{t:08x}:0x{op:02x}"))
                .or_insert(0) += n;
        }

        let mut code = parsed.code.clone();
        if opts.old_compat {
            apply_old_compat(&mut code);
        }
        let objects = code.count_objects();
        code_objects += objects as u64;
        report.marshal_size = Some(parsed.consumed);
        report.trailer = Some(parsed.stats.unconsumed);
        report.trailer_marker = d.trailer.map(|t| t.marker);
        report.code_objects = Some(objects);
        report.decoys = Some(parsed.stats.decoy_code_objects);
        decoy_code_objects += parsed.stats.decoy_code_objects as u64;
        decoy_unknown_opcodes += parsed.stats.decoy_unknown_opcodes as u64;
        let filename = code.filename_str().map(|s| s.to_string());
        report.co_filename = filename.clone();
        if let Some(t) = parsed.stats.opcode_tables.keys().next() {
            report.opcode_type = Some(*t);
        }

        let rel = allocator.allocate(filename.as_deref().and_then(relative_path), d.dir, d.id);
        let pyc_rel = rel.with_extension("pyc");
        let src_rel = rel.clone();

        if opts.write_pyc {
            let bytes = pyc::emit_file(&code);
            let path = pyc_dir.join(&pyc_rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
            report.pyc = Some(pyc_rel.to_string_lossy().into_owned());
        }
        if opts.write_source {
            report.source = Some(src_rel.to_string_lossy().into_owned());
            jobs.push((pyc_dir.join(&pyc_rel), src_dir.join(&src_rel), entries.len()));
        }
        files_ok += 1;
        entries.push(report);
    }
    let write_ms = write_start.elapsed().as_secs_f64() * 1000.0;

    // ---- phase 3: decompile ----------------------------------------------
    let decompile_start = Instant::now();
    let decompiler = if opts.write_source && !jobs.is_empty() {
        Decompiler::detect(
            opts.decompiler.as_deref(),
            opts.python.as_deref(),
            opts.jobs,
        )?
    } else {
        Decompiler::detect(Some("none"), None, 1)?
    };
    let mut decompiled_ok = 0usize;
    let mut decompiled_failed = 0usize;
    if decompiler.is_enabled() {
        decompiler.warm()?;
        let counter = AtomicUsize::new(0);
        let total_jobs = jobs.len();
        // pycdc deparses a whole batch per process; spawn cost dominates
        // otherwise (4.3k files x ~40 ms added up to ~70 s on its own).
        let batch = if decompiler.batchable() { 64 } else { 1 };
        let results: Vec<(PathBuf, usize, Result<()>)> = jobs
            .par_chunks(batch)
            .flat_map(|chunk| {
                let paths: Vec<&Path> = chunk.iter().map(|(p, _, _)| p.as_path()).collect();
                let texts = decompiler.decompile_batch(&paths);
                chunk
                    .iter()
                    .zip(texts)
                    .map(|((_, src_path, idx), text)| {
                        let result = text.and_then(|text| {
                            let text = if opts.text_fix {
                                fix_mojibake(&text)
                            } else {
                                text
                            };
                            if let Some(parent) = src_path.parent() {
                                std::fs::create_dir_all(parent).ok();
                            }
                            let mut f = std::fs::File::create(src_path)
                                .with_context(|| format!("creating {}", src_path.display()))?;
                            f.write_all(text.as_bytes())?;
                            Ok(())
                        });
                        let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
                        if !opts.quiet && (n % 200 == 0 || n == total_jobs) {
                            eprintln!("  decompiled {n}/{total_jobs}");
                        }
                        (src_path.clone(), *idx, result)
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        // The asset entry occupies a slot in `entries` but has no decompile job,
        // so job index != slot in `jobs`; the source path travels with the
        // result instead of being looked up again.
        for (src, idx, result) in results.iter() {
            let entry = &mut entries[*idx];
            match result {
                Ok(()) => {
                    decompiled_ok += 1;
                    entry.decompiled = Some(true);
                }
                Err(e) => {
                    decompiled_failed += 1;
                    entry.decompiled = Some(false);
                    entry.error = Some(format!("decompile: {e}"));
                    if opts.write_source {
                        // Leave a stub so the failure is visible in the tree.
                        std::fs::write(
                            src,
                            format!("# unmcpk: decompilation failed\n# {e}\n"),
                        )
                        .ok();
                    }
                }
            }
        }
    } else if opts.write_source {
        for (_, src, _) in &jobs {
            if let Some(parent) = src.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(src, "# unmcpk: no decompiler backend available\n").ok();
        }
    }
    let decompile_ms = decompile_start.elapsed().as_secs_f64() * 1000.0;

    let mut timings_ms = BTreeMap::new();
    timings_ms.insert("decode".into(), decode_ms);
    timings_ms.insert("write".into(), write_ms);
    timings_ms.insert("decompile".into(), decompile_ms);
    timings_ms.insert("total".into(), started.elapsed().as_secs_f64() * 1000.0);

    Ok(Report {
        tool: "unmcpk",
        version: env!("CARGO_PKG_VERSION"),
        container: container_path.display().to_string(),
        container_size: container.len(),
        directories: container.dirs.len(),
        files: total,
        files_ok,
        files_failed,
        assets,
        code_objects,
        decoy_code_objects,
        decoy_unknown_opcodes,
        layers,
        trailers,
        opcode_tables,
        unknown_opcodes,
        decompiler: match decompiler.kind() {
            Kind::None => "none".to_string(),
            Kind::Pycdc => "pycdc".to_string(),
            Kind::Uncompyle6 => "uncompyle6".to_string(),
        },
        decompiled_ok,
        decompiled_failed,
        timings_ms,
        entries,
    })
}

/// Quick container summary, no decoding.
pub fn info(container: &Container) -> serde_json::Value {
    let mut per_dir: BTreeMap<String, usize> = BTreeMap::new();
    for f in &container.files {
        *per_dir.entry(format!("{:08x}", f.dir.0)).or_insert(0) += 1;
    }
    let total: u64 = container.files.iter().map(|f| f.size as u64).sum();
    serde_json::json!({
        "container_size": container.len(),
        "dir_table_start": container.dir_table_start,
        "file_table_start": container.file_table_start,
        "data_start": container.data_start,
        "directories": container.dirs.len(),
        "files": container.files.len(),
        "file_bytes": total,
        "redirect_mcs_id": format!("{:08x}", string_id("redirect.mcs")),
        "files_per_directory": per_dir,
    })
}
