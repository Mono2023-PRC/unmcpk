//! Command line front end.

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;
use unmcpk::mcpk::Container;
use unmcpk::pipeline::{self, Options};

#[derive(Parser, Debug)]
#[command(
    name = "unmcpk",
    version,
    about = "Unpack NetEase MCPK containers: decode payloads, rebuild the source tree, decompile to Python 2.7",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Print container statistics without decoding anything.
    Info {
        /// Path to the .mcp container.
        container: PathBuf,
    },
    /// Decode a container into pyc files, Python sources and a manifest.
    Unpack(UnpackArgs),
}

#[derive(Args, Debug)]
struct UnpackArgs {
    /// Path to the .mcp container.
    container: PathBuf,
    /// Output directory (created if missing).
    #[arg(short, long, default_value = "unpacked")]
    output: PathBuf,
    /// Number of worker threads for decoding and decompiling.
    #[arg(short = 'j', long)]
    jobs: Option<usize>,
    /// Only unpack the first N files (useful for a quick smoke test).
    #[arg(long)]
    limit: Option<usize>,
    /// Skip writing .pyc files.
    #[arg(long)]
    no_pyc: bool,
    /// Skip writing decompiled Python sources.
    #[arg(long)]
    no_source: bool,
    /// Also dump the decoded payload bytes of every entry.
    #[arg(long)]
    raw: bool,
    /// Reproduce the legacy tool's "drop the self-update check" hack. Corrupts
    /// the module that contains it; off by default.
    #[arg(long)]
    old_compat: bool,
    /// Keep NetEase's decoy code objects instead of stubbing them
    #[arg(long)]
    keep_decoys: bool,
    /// Disable the GBK docstring repair pass.
    #[arg(long)]
    no_text_fix: bool,
    /// Decompiler backend: auto, pycdc, uncompyle6 or none.
    #[arg(long, default_value = "auto")]
    decompiler: String,
    /// Python interpreter that has uncompyle6 installed.
    #[arg(long)]
    python: Option<String>,
    /// Path of the manifest to write (defaults to <output>/manifest.json).
    #[arg(long)]
    manifest: Option<PathBuf>,
    /// Suppress progress output.
    #[arg(short, long)]
    quiet: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Info { container } => {
            let c = Container::open(&container)?;
            let info = pipeline::info(&c);
            println!("{}", serde_json::to_string_pretty(&info)?);
            Ok(())
        }
        Command::Unpack(args) => unpack(args),
    }
}

fn unpack(args: UnpackArgs) -> Result<()> {
    let container = Container::open(&args.container)?;
    if !args.quiet {
        eprintln!(
            "container: {} ({:.2} MiB, {} dirs, {} files)",
            args.container.display(),
            container.len() as f64 / (1024.0 * 1024.0),
            container.dirs.len(),
            container.files.len()
        );
    }
    let jobs = args
        .jobs
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4));
    let opts = Options {
        output: args.output.clone(),
        jobs,
        write_pyc: !args.no_pyc,
        write_source: !args.no_source,
        write_raw: args.raw,
        old_compat: args.old_compat,
        keep_decoys: args.keep_decoys,
        text_fix: !args.no_text_fix,
        limit: args.limit,
        decompiler: Some(args.decompiler.clone()),
        python: args.python.clone(),
        quiet: args.quiet,
    };

    let report = pipeline::run(&args.container, &container, &opts)?;

    let manifest = args
        .manifest
        .unwrap_or_else(|| args.output.join("manifest.json"));
    if let Some(parent) = manifest.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&manifest, serde_json::to_vec_pretty(&report)?)
        .with_context(|| format!("writing {}", manifest.display()))?;

    if !args.quiet {
        let t = &report.timings_ms;
        eprintln!(
            "\nfiles: {} ok, {} failed, {} assets ({} code objects)",
            report.files_ok, report.files_failed, report.assets, report.code_objects
        );
        if !report.layers.is_empty() {
            eprintln!("layers: {:?}", report.layers);
        }
        if !report.decompiled_ok + report.decompiled_failed > 0 {
            eprintln!(
                "decompiled: {} ok, {} failed via {}",
                report.decompiled_ok, report.decompiled_failed, report.decompiler
            );
        }
        if report.decoy_code_objects > 0 {
            eprintln!(
                "decoys: {} objects with invalid bytecode ({} bytes){}",
                report.decoy_code_objects,
                report.decoy_unknown_opcodes,
                if args.keep_decoys {
                    ", kept as-is"
                } else {
                    "; stubbed to `return None`"
                }
            );
        }
        if !report.unknown_opcodes.is_empty() {
            eprintln!("warning: unknown opcode bytes: {:?}", report.unknown_opcodes);
        }
        eprintln!(
            "timing: decode {:.0} ms, write {:.0} ms, decompile {:.0} ms, total {:.0} ms",
            t["decode"], t["write"], t["decompile"], t["total"]
        );
        eprintln!("manifest: {}", manifest.display());
        eprintln!("output:   {}", args.output.display());
    }
    Ok(())
}
