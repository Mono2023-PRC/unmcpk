//! Decompile one named code object from a container module, for debugging.
//!
//! `cargo run --release --example probe -- <container> <fileIdHex> <codeName> [dirIdHex]`

use std::process::ExitCode;
use unmcpk::nemarshal::Value;
use unmcpk::pipeline::{self, Options};
use unmcpk::{mcpk, pydec};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        eprintln!("usage: probe <container> <fileIdHex> <codeName> [dirIdHex]");
        return ExitCode::from(2);
    }
    let data = match std::fs::read(&args[0]) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let id = u32::from_str_radix(args[1].trim_start_matches("0x"), 16).unwrap_or(0);
    let name = args[2].clone();
    let dir = args
        .get(3)
        .and_then(|s| u32::from_str_radix(s.trim_start_matches("0x"), 16).ok());
    let container = match mcpk::Container::from_bytes(data) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let opts = Options::default();
    for entry in container.files.iter().filter(|f| f.id.0 == id && dir.map_or(true, |d| f.dir.0 == d)) {
        let decoded = pipeline::decode_entry(entry, container.body(entry), &opts);
        let Some(parsed) = decoded.parsed else {
            eprintln!("error: {}", decoded.error.unwrap_or_default());
            return ExitCode::FAILURE;
        };
        let mut codes = Vec::new();
        parsed.code.walk(&mut codes);
        for code in codes {
            if code.name_str() != name {
                continue;
            }
            println!("listing for {}", code.name_str());
            for line in pydec::disasm::listing(code) {
                println!("  {line}");
            }
            match pydec::try_decompile(code) {
                Ok(text) => println!("--- source\n{text}"),
                Err(e) => println!("--- failed: {e}"),
            }
            return ExitCode::SUCCESS;
        }
        println!("no code object named {name}");
        let _ = Value::None;
        return ExitCode::FAILURE;
    }
    ExitCode::FAILURE
}
