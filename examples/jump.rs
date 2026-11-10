//! Diagnostic: print the raw bytes and decoded jumps of one code object.
//!
//! `cargo run --release --example jump -- <container> <fileIdHex> <name>`

use unmcpk::mcpk::Container;
use unmcpk::nemarshal::{Code, Value};
use unmcpk::pydec::disasm;

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
    let want = u32::from_str_radix(args[2].trim_start_matches("0x"), 16).unwrap();
    let needle = args.get(3).cloned().unwrap_or_default();
    let entry = container.files.iter().find(|e| e.id.0 == want).unwrap();
    let decoded = unmcpk::pipeline::decode_entry(
        entry,
        container.body(entry),
        &unmcpk::pipeline::Options::default(),
    );
    let parsed = decoded.parsed.unwrap();
    let mut codes = Vec::new();
    walk(&parsed.code, &mut codes);
    for code in codes {
        if code.name_str() != needle {
            continue;
        }
        if let Some(want) = std::env::var_os("UNMCPK_SIZE") {
            let want: usize = want.to_string_lossy().parse().unwrap();
            if code.bytecode.len() != want {
                continue;
            }
        }
        println!("== {} ({} bytes)", code.name_str(), code.bytecode.len());
        let hex: Vec<String> = code
            .bytecode
            .chunks(16)
            .map(|c| c.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" "))
            .collect();
        for line in &hex {
            println!("   {line}");
        }
        for instr in disasm::disassemble(code) {
            println!(
                "{:5}  {:20} arg={:6} target={:?}",
                instr.offset,
                instr.name(),
                instr.arg,
                instr.target
            );
        }
    }
    eprintln!("no more code objects named {needle}");
}
