//! Diagnostic: dump per-code-object opcode translation details for one entry.
//!
//! usage: cargo run --release --example dbg -- <container> <fileIdHex> [--max N]

use unmcpk::mcpk::Container;
use unmcpk::nemarshal::{self, Code, Value};
use unmcpk::payload::{apply_layer, LayerKind};
use unmcpk::rotor::{default_key, Rotor};

fn walk<'a>(code: &'a Code, out: &mut Vec<&'a Code>) {
    out.push(code);
    for c in &code.constants {
        if let Value::Code(inner) = c {
            walk(inner, out);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 2 && args[1] == "--scan" {
        scan(&args[2], args.get(3).and_then(|s| s.parse().ok()).unwrap_or(usize::MAX));
        return;
    }
    let container = Container::open(&args[1]).unwrap();
    let want = u32::from_str_radix(args[2].trim_start_matches("0x"), 16).unwrap();
    let entry = container
        .files
        .iter()
        .find(|e| e.id.0 == want)
        .expect("file id not in container");
    let body = container.body(entry);
    let mut rotor = Rotor::new(6, &default_key());
    let payload = apply_layer(LayerKind::RotorZlibReverse, body, &mut rotor).expect("decode");
    let opts = nemarshal::Options {
        expand_interned: true,
        old_compat: false,
        keep_decoys: false,
    };
    let parsed = nemarshal::parse_module(&payload, opts).unwrap();
    println!(
        "file {:08x} payload {} consumed {} trailer {}",
        entry.id.0,
        payload.len(),
        parsed.consumed,
        payload.len() - parsed.consumed
    );
    let mut objs = Vec::new();
    walk(&parsed.code, &mut objs);
    println!("{} code objects", objs.len());
    for (n, c) in objs.iter().enumerate() {
        let bc = &c.bytecode;
        // Re-scan the translated bytecode for bytes that are not py2.7 opcodes.
        let mut bad: Vec<usize> = Vec::new();
        let mut i = 0;
        while i < bc.len() {
            let op = bc[i];
            if op == 0x00 {
                bad.push(i);
            }
            if op >= 90 && i + 2 < bc.len() {
                i += 3;
            } else {
                i += 1;
            }
        }
        println!(
            "  [{}] {} name={} bc={} consts={} zeros={:?}",
            n,
            c.filename_str().unwrap_or("?"),
            c.name_str(),
            bc.len(),
            c.constants.len(),
            &bad.iter().take(12).collect::<Vec<_>>()
        );
        if let Some(&first) = bad.first() {
            let start = first.saturating_sub(8);
            let end = (first + 8).min(bc.len());
            println!("      ctx {:02x?}", &bc[start..end]);
            println!("      tail {:02x?}", &bc[bc.len().saturating_sub(8)..]);
        }
    }
}

fn scan(path: &str, limit: usize) {
    let container = Container::open(path).unwrap();
    let mut rotor = Rotor::new(6, &default_key());
    let mut total_objs = 0usize;
    let mut zero_objs = 0usize;
    let mut zero_bytes = 0usize;
    let mut shown = 0usize;
    for entry in container.files.iter().take(limit) {
        let body = container.body(entry);
        let Some(payload) = apply_layer(LayerKind::RotorZlibReverse, body, &mut rotor) else {
            continue;
        };
        let opts = nemarshal::Options { expand_interned: true, old_compat: false, keep_decoys: false };
        let Ok(parsed) = nemarshal::parse_module(&payload, opts) else { continue };
        let mut objs = Vec::new();
        walk(&parsed.code, &mut objs);
        for c in &objs {
            total_objs += 1;
            let bc = &c.bytecode;
            let mut i = 0;
            let mut zeros = 0usize;
            while i < bc.len() {
                let op = bc[i];
                if op == 0 {
                    zeros += 1;
                }
                if op >= 90 && i + 2 < bc.len() { i += 3 } else { i += 1 }
            }
            if zeros > 0 {
                zero_objs += 1;
                zero_bytes += zeros;
                if shown < 25 {
                    shown += 1;
                    println!(
                        "file {:08x} obj {:<34} bc={:<5} zeros={:<4} head={:02x?}",
                        entry.id.0,
                        c.name_str(),
                        bc.len(),
                        zeros,
                        &bc[..bc.len().min(18)]
                    );
                }
            }
        }
    }
    println!(
        "scan: {total_objs} code objects, {zero_objs} with zero bytes, {zero_bytes} zero bytes total"
    );
}
