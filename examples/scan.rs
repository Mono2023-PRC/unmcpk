//! Diagnostic: find code objects that call with keywords or star arguments.
//!
//! `cargo run --release --example scan -- <container> [count]`

use unmcpk::mcpk::Container;
use unmcpk::nemarshal::{self, Code, Value};

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
    let container = Container::open(&args[1]).unwrap();
    // "--op <byte>" finds objects containing that opcode instead of calls with
    // keyword arguments.
    let mut op_filter: Option<u8> = None;
    let mut want: usize = 3;
    let mut rest = args[2..].iter();
    while let Some(arg) = rest.next() {
        if arg == "--op" {
            op_filter = rest.next().and_then(|v| v.parse().ok());
        } else if let Ok(value) = arg.parse() {
            want = value;
        }
    }
    let mut shown = 0;
    for entry in &container.files {
        let decoded = unmcpk::pipeline::decode_entry(
            entry,
            container.body(entry),
            &unmcpk::pipeline::Options::default(),
        );
        let Some(parsed) = decoded.parsed else { continue };
        let mut codes = Vec::new();
        walk(&parsed.code, &mut codes);
        for code in codes {
            let bytes = &code.bytecode;
            let mut i = 0;
            let mut hit = false;
            while i < bytes.len() {
                let op = bytes[i];
                if Some(op) == op_filter {
                    hit = true;
                }
                if op >= 90 && i + 2 < bytes.len() {
                    let arg = (bytes[i + 2] as usize) << 8 | bytes[i + 1] as usize;
                    if op_filter.is_none()
                        && matches!(op, 131 | 140 | 141 | 142)
                        && arg >> 8 > 0
                    {
                        hit = true;
                    }
                    i += 3;
                } else {
                    i += 1;
                }
            }
            if !hit {
                continue;
            }
            println!(
                "== {:?} / {} (file {:08x})",
                code.filename_str(),
                code.name_str(),
                entry.id.0
            );
            for line in unmcpk::pydec::disasm::listing(code) {
                println!("    {line}");
            }
            shown += 1;
            if shown >= want {
                return;
            }
        }
    }
    let _ = nemarshal::Options::default();
}
