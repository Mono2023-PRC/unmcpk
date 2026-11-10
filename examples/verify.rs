//! Verify a completed unpack from the command line.
//!
//! `verify <output-dir>` re-reads every emitted `.pyc`, parses it back and
//! compares it with the manifest. The same check is available from the CLI as
//! `unmcpk unpack <container> --verify`. Exit status is non-zero when anything
//! disagrees.
use std::path::Path;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: verify <output-dir>");
        std::process::exit(2);
    }
    let report = unmcpk::verify::verify_dir(Path::new(&args[1]))?;
    println!(
        "modules: {}, assets: {}, files on disk: {}",
        report.modules, report.assets, report.files_on_disk
    );
    println!("code objects re-parsed: {}", report.objects);
    println!("layers: {:?}", report.layers);
    println!("trailer marker: {:?}", report.trailers);
    for problem in &report.problems {
        println!("{problem}");
    }
    if report.is_ok() {
        println!("verify: ok");
        return Ok(());
    }
    println!("verify: FAILED");
    std::process::exit(1);
}
