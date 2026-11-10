//! Dump the (already de-permuted) instructions of one container entry.
//!
//! `cargo run --release --example list -- <container> <fileIdHex> [dirIdHex]`

use std::path::Path;

use unmcpk::mcpk::Container;
use unmcpk::nemarshal::{Code, Value};
use unmcpk::pipeline;
use unmcpk::pydec::disasm::listing;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let path = args.next().unwrap_or_else(|| {
        eprintln!("usage: list <container> <fileIdHex> [dirIdHex]");
        std::process::exit(2);
    });
    let target = args.next().unwrap_or_default();
    let file_id = parse_hex(&target);
    let dir_id = args.next().map(|text| parse_hex(&text));

    let container = Container::open(Path::new(&path))?;
    let opts = pipeline::Options::default();
    if target == "scan" {
        return scan(&container, &opts);
    }
    for entry in &container.files {
        if entry.id.0 != file_id {
            continue;
        }
        if let Some(dir) = dir_id {
            if entry.dir.0 != dir {
                continue;
            }
        }
        let decoded = pipeline::decode_entry(entry, container.body(entry), &opts);
        let Some(parsed) = decoded.parsed else {
            println!("dir {:08x} file {:08x}: {}", entry.dir.0, entry.id.0, decoded.error.unwrap_or_default());
            continue;
        };
        println!(
            "dir {:08x} file {:08x}  {}",
            entry.dir.0,
            entry.id.0,
            parsed.code.filename_str().unwrap_or("<no filename>")
        );
        dump(&parsed.code, 0);
    }
    Ok(())
}

/// Print the first few modules that call MAKE_FUNCTION with defaults.
fn scan(container: &Container, opts: &pipeline::Options) -> anyhow::Result<()> {
    let mut shown = 0;
    for entry in &container.files {
        let decoded = pipeline::decode_entry(entry, container.body(entry), opts);
        let Some(parsed) = decoded.parsed else { continue };
        let mut found = false;
        let mut objects: Vec<&Code> = Vec::new();
        parsed.code.walk(&mut objects);
        for code in &objects {
            for line in listing(code) {
                if line.contains("MAKE_FUNCTION") && !line.trim_end().ends_with(" 0") {
                    found = true;
                }
            }
        }
        if found {
            println!("=== dir {:08x} file {:08x}", entry.dir.0, entry.id.0);
            dump(&parsed.code, 0);
            shown += 1;
            if shown >= 3 {
                return Ok(());
            }
        }
    }
    Ok(())
}

fn dump(code: &Code, depth: usize) {
    let pad = " ".repeat(depth * 2);
    println!(
        "{pad}code {} args={} vars={:?} names={:?} flags={:#x} decoy={}",
        code.name_str(),
        code.arg_count,
        code.var_names
            .iter()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect::<Vec<_>>(),
        code.names
            .iter()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect::<Vec<_>>(),
        code.flags,
        code.decoy
    );
    if depth < 4 {
        for line in listing(code) {
            println!("{pad}  {line}");
        }
    }
    for value in &code.constants {
        if let Value::Code(nested) = value {
            dump(nested, depth + 1);
        }
    }
}

fn parse_hex(text: &str) -> u32 {
    u32::from_str_radix(text.trim_start_matches("0x"), 16).unwrap_or(0)
}
