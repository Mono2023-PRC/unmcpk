//! Round-trip check: decode an entry, emit a .pyc, parse it back, decompile.
//!
//! `rt <container> <fileIdHex>` compares decompiling the decoded module with
//! decompiling it again after a pyc round trip, which isolates decoder bugs from
//! emitter bugs. `rt --pyc <file.pyc>` decompiles an on-disk pyc exactly the way
//! the `native` backend of the pipeline does.
use unmcpk::mcpk::{Container, FileId};
use unmcpk::nemarshal::Options;
use unmcpk::pipeline;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args[1] == "--pyc" {
        let bytes = std::fs::read(&args[2])?;
        let parsed = unmcpk::nemarshal::parse_module(
            &bytes[unmcpk::pyc::PYC_HEADER.len()..],
            Options::default(),
        )?;
        let text = unmcpk::pydec::try_decompile(&parsed.code);
        println!("pyc: {}", if text.is_ok() { "ok" } else { "FAILED" });
        if let Err(e) = text { println!("err: {e}"); }
        return Ok(());
    }
    let container = Container::open(&args[1])?;
    let file = u32::from_str_radix(args[2].trim_start_matches("0x"), 16)?;
    let entry = container
        .files
        .iter()
        .find(|e| e.id == FileId(file))
        .ok_or_else(|| anyhow::anyhow!("no such file"))?;
    let body = container.body(entry);
    let decoded = pipeline::decode_entry(entry, body, &pipeline::Options::default());
    let code = decoded.parsed.as_ref().map(|p| &p.code).ok_or_else(|| anyhow::anyhow!("{:?}", decoded.error))?;
    let direct = unmcpk::pydec::try_decompile(code);
    println!("direct: {}", if direct.is_ok() { "ok" } else { "FAILED" });
    let pyc = unmcpk::pyc::emit_file(code);
    let reparsed = unmcpk::nemarshal::parse_module(&pyc[unmcpk::pyc::PYC_HEADER.len()..], Options::default())?;
    let round = unmcpk::pydec::try_decompile(&reparsed.code);
    println!("roundtrip: {}", if round.is_ok() { "ok" } else { "FAILED" });
    if let (Err(a), _) = (&direct, ()) { println!("direct err: {a}"); }
    if let Err(e) = &round { println!("roundtrip err: {e}"); }
    Ok(())
}
