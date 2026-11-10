//! Measure the native decompiler over a whole container.
//!
//! `cargo run --release --example native -- <container> [limit] [outdir]`
//!
//! Prints how many modules the native decompiler can rebuild, the most common
//! reasons it falls back, and optionally writes the sources out so they can be
//! diffed against another backend.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use unmcpk::mcpk::Container;
use unmcpk::pipeline;
use unmcpk::pydec;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let container_path = match args.next() {
        Some(path) => path,
        None => {
            eprintln!("usage: native <container> [limit] [outdir]");
            std::process::exit(2);
        }
    };
    let limit: Option<usize> = args.next().and_then(|text| text.parse().ok());
    let outdir: Option<PathBuf> = args.next().map(PathBuf::from);

    let container = Container::open(Path::new(&container_path))?;
    let opts = pipeline::Options {
        quiet: true,
        write_source: false,
        write_pyc: false,
        ..Default::default()
    };

    let mut total = 0usize;
    let mut ok = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;
    let mut reasons: BTreeMap<String, usize> = BTreeMap::new();
    let mut examples: BTreeMap<String, String> = BTreeMap::new();
    let mut obj_ok = 0usize;
    let mut obj_fail = 0usize;
    let mut obj_reasons: BTreeMap<String, usize> = BTreeMap::new();
    let mut obj_examples: BTreeMap<String, String> = BTreeMap::new();
    let mut ok_by_size: Vec<usize> = Vec::new();
    let mut fail_by_size: Vec<usize> = Vec::new();
    let start = Instant::now();

    let progress = std::env::var_os("UNMCPK_PROGRESS").is_some();
    for (index, entry) in container.files.iter().enumerate() {
        if progress {
            eprintln!("[{index}] dir={:08x} file={:08x}", entry.dir.0, entry.id.0);
        }
        if let Some(limit) = limit {
            if index >= limit {
                break;
            }
        }
        let body = container.body(entry);
        let decoded = pipeline::decode_entry(entry, body, &opts);
        let Some(parsed) = decoded.parsed else {
            skipped += 1;
            continue;
        };
        total += 1;
        let size = parsed.code.count_objects();
        // Per code object: the module-level verdict hides which nested objects
        // still fall back inside an otherwise good file.
        let mut codes = Vec::new();
        parsed.code.walk(&mut codes);
        for code in codes {
            if code.decoy || synthetic(code) {
                continue;
            }
            if progress {
                eprintln!("  obj {} ({} bytes)", code.name_str(), code.bytecode.len());
            }
            let started = std::time::Instant::now();
            let verdict = pydec::try_decompile(code);
            if progress {
                eprintln!(
                    "  done {} in {} ms",
                    code.name_str(),
                    started.elapsed().as_millis()
                );
            }
            match verdict {
                Ok(_) => obj_ok += 1,
                Err(reason) => {
                    obj_fail += 1;
                    let key = normalize(&reason);
                    *obj_reasons.entry(key.clone()).or_default() += 1;
                    obj_examples
                        .entry(key)
                        .or_insert_with(|| format!("dir {:08x} file {:08x}", entry.dir.0, entry.id.0));
                }
            }
        }
        match pydec::try_decompile(&parsed.code) {
            Ok(text) => {
                ok += 1;
                ok_by_size.push(size);
                if let Some(dir) = &outdir {
                    let (path, _) = outdir_path(dir, &parsed.code, entry.dir.0, entry.id.0);
                    write_file(&path, &text)?;
                }
            }
            Err(reason) => {
                failed += 1;
                fail_by_size.push(size);
                let key = normalize(&reason);
                *reasons.entry(key.clone()).or_default() += 1;
                examples
                    .entry(key)
                    .or_insert_with(|| format!("dir {:08x} file {:08x}", entry.dir.0, entry.id.0));
            }
        }
    }

    let elapsed = start.elapsed().as_secs_f64();
    println!("modules: {total} ({ok} rebuilt, {failed} fell back, {skipped} assets skipped)");
    if total > 0 {
        println!("success: {:.1}%", 100.0 * ok as f64 / total as f64);
    }
    println!("time: {elapsed:.2}s ({:.1} modules/s)", total as f64 / elapsed.max(1e-9));
    avg("rebuilt", &ok_by_size);
    avg("fallback", &fail_by_size);
    let objects = obj_ok + obj_fail;
    if objects > 0 {
        println!(
            "code objects: {objects} ({obj_ok} rebuilt, {obj_fail} fell back, {:.2}%)",
            100.0 * obj_ok as f64 / objects as f64
        );
    }
    if !reasons.is_empty() {
        println!("\nfallback reasons:");
        let mut ranked: Vec<(&String, &usize)> = reasons.iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(a.1));
        for (reason, count) in ranked.iter().take(25) {
            let example = examples.get(*reason).map(String::as_str).unwrap_or("");
            println!("  {count:>6}  {reason}  [{example}]");
        }
    }
    if !obj_reasons.is_empty() {
        println!("
code object fallback reasons:");
        let mut ranked: Vec<(&String, &usize)> = obj_reasons.iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(a.1));
        for (reason, count) in ranked.iter().take(25) {
            let example = obj_examples
                .get(*reason)
                .map(String::as_str)
                .unwrap_or("");
            println!("  {count:>6}  {reason}  [{example}]");
        }
    }
    Ok(())
}

fn avg(label: &str, sizes: &[usize]) {
    if sizes.is_empty() {
        return;
    }
    let sum: usize = sizes.iter().sum();
    let max = sizes.iter().copied().max().unwrap_or(0);
    println!("{label}: mean {:.1} code objects, max {max}", sum as f64 / sizes.len() as f64);
}

/// Trim offsets and indices so equal failures group together.
/// Comprehension bodies are compiled into their own code objects but never
/// appear as functions in the rebuilt source, so they are not failures.
fn synthetic(code: &unmcpk::nemarshal::Code) -> bool {
    matches!(
        code.name_str().as_str(),
        "<genexpr>" | "<listcomp>" | "<setcomp>" | "<dictcomp>"
    )
}

fn normalize(reason: &str) -> String {
    let mut out = String::with_capacity(reason.len());
    let mut last_digit = false;
    for ch in reason.chars() {
        if ch.is_ascii_digit() {
            if !last_digit {
                out.push('#');
            }
            last_digit = true;
        } else {
            last_digit = false;
            out.push(ch);
        }
    }
    let out = out.trim().to_string();
    // Long Debug dumps of whole code objects carry no diagnostic value.
    match out.char_indices().nth(120) {
        Some((index, _)) => format!("{}…", &out[..index]),
        None => out,
    }
}

fn outdir_path(
    dir: &Path,
    code: &unmcpk::nemarshal::Code,
    dir_id: u32,
    file_id: u32,
) -> (PathBuf, bool) {
    let relative = code
        .filename_str()
        .and_then(unmcpk::pathmap::relative_path);
    match relative {
        Some(rel) => (dir.join(rel), true),
        None => (
            dir.join(format!("{dir_id:08x}_{file_id:08x}.py")),
            false,
        ),
    }
}

fn write_file(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, text)
}
