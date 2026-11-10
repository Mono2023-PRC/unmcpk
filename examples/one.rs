//! Decompile one container entry and print why it failed, with context.
//!
//! `cargo run --release --example one -- <container> <fileIdHex> [dirIdHex]`

use std::process::ExitCode;

use unmcpk::mcpk::Container;
use unmcpk::pydec;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: one <container> <fileIdHex> [dirIdHex]");
        return ExitCode::FAILURE;
    }
    let file_id = u32::from_str_radix(args[2].trim_start_matches("0x"), 16).unwrap_or(0);
    let dir_id = args
        .get(3)
        .map(|s| u32::from_str_radix(s.trim_start_matches("0x"), 16).unwrap_or(0));

    let data = match std::fs::read(&args[1]) {
        Ok(data) => data,
        Err(err) => {
            eprintln!("read {}: {err}", args[1]);
            return ExitCode::FAILURE;
        }
    };
    let container = match Container::from_bytes(data) {
        Ok(container) => container,
        Err(err) => {
            eprintln!("parse: {err}");
            return ExitCode::FAILURE;
        }
    };
    let Some(entry) = container
        .files
        .iter()
        .find(|f| f.id.0 == file_id && dir_id.map_or(true, |d| f.dir.0 == d))
    else {
        eprintln!("no such file");
        return ExitCode::FAILURE;
    };

    let body = container.body(entry);
    let decoded = unmcpk::pipeline::decode_entry(entry, body, &unmcpk::pipeline::Options::default());
    if let Some(err) = &decoded.error {
        eprintln!("decode: {err}");
        return ExitCode::FAILURE;
    }
    let Some(parsed) = decoded.parsed else {
        eprintln!("not a module");
        return ExitCode::FAILURE;
    };

    println!("filename: {:?}", parsed.code.filename_str());
    match pydec::try_decompile(&parsed.code) {
        Ok(text) => {
            println!("ok, {} bytes", text.len());
            println!("{text}");
        }
        Err(err) => {
            println!("FAILED: {err}");
            // Walk the module for the first nested object that fails.
            let mut codes = Vec::new();
            parsed.code.walk(&mut codes);
            for code in codes {
                if let Err(inner) = pydec::try_decompile(code) {
                    println!("  nested {:?}: {inner}", code.name_str());
                }
            }
        }
    }
    ExitCode::SUCCESS
}
