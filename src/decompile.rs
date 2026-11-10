//! Decompilation backends.
//!
//! Two are supported: the native `pycdc` binary (fast, no interpreter needed)
//! and `uncompyle6` through a small persistent Python worker pool (better
//! fidelity on Python 2.7). Both are driven through the same interface, and
//! both are optional: `--decompiler none` still produces byte-identical `.pyc`
//! files, which is what matters when only the bytecode is wanted.

use anyhow::{bail, Context, Result};
use crate::pyc::PYC_HEADER;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;

/// The worker script: one JSON request per line on stdin, one JSON response per
/// line on stdout. Keeping the interpreter alive is what makes this fast — a
/// fresh `python -m uncompyle6` per file costs ~200 ms in interpreter startup
/// alone.
const WORKER: &str = r#"
import io, json, sys, traceback
import uncompyle6.main as U

def decompile(path):
    out = io.StringIO()
    try:
        # Only pass what is needed: showasm/showast set debug options that
        # uncompyle6 3.9 then mis-reads as dicts ('bool' has no attribute 'get').
        U.decompile_file(path, outstream=out, source_encoding='utf-8')
    except Exception:
        return {'ok': False, 'error': traceback.format_exc(limit=4)}
    return {'ok': True, 'source': out.getvalue()}

sys.stdout.reconfigure(encoding='utf-8', errors='replace')
sys.stdin.reconfigure(encoding='utf-8', errors='replace')
while True:
    line = sys.stdin.readline()
    if not line:
        break
    line = line.strip()
    if not line:
        continue
    try:
        req = json.loads(line)
    except Exception:
        continue
    if req.get('cmd') == 'quit':
        break
    res = decompile(req['pyc'])
    res['id'] = req.get('id', 0)
    sys.stdout.write(json.dumps(res) + '\n')
    sys.stdout.flush()
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    None,
    /// The in-tree Python 2.7 decompiler (`crate::pydec`): no external process,
    /// no external binary, so the tool stays self-contained.
    Native,
    Pycdc,
    Uncompyle6,
}

pub struct Decompiler {
    kind: Kind,
    pycdc: Option<PathBuf>,
    python: Option<PathBuf>,
    worker_script: Option<PathBuf>,
    /// Idle workers; each holds a live interpreter.
    pool: Mutex<Vec<Worker>>,
    workers: usize,
}

struct Worker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Worker {
    fn start(python: &Path, script: &Path) -> Result<Worker> {
        let mut child = Command::new(python)
            .arg("-u")
            .arg(script)
            .env("PYTHONIOENCODING", "utf-8")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("starting python worker {}", python.display()))?;
        let stdin = child.stdin.take().context("worker stdin")?;
        let stdout = BufReader::new(child.stdout.take().context("worker stdout")?);
        Ok(Worker {
            child,
            stdin,
            stdout,
        })
    }

    fn run(&mut self, pyc: &Path, id: u32) -> Result<String> {
        let req = serde_json::json!({"id": id, "pyc": pyc.to_string_lossy()});
        writeln!(self.stdin, "{req}").context("writing request to worker")?;
        self.stdin.flush().ok();
        let mut line = String::new();
        let n = self.stdout.read_line(&mut line).context("reading worker response")?;
        if n == 0 {
            bail!("python worker exited while decompiling {}", pyc.display());
        }
        let v: serde_json::Value =
            serde_json::from_str(line.trim()).context("worker returned invalid JSON")?;
        if v.get("ok").and_then(|b| b.as_bool()).unwrap_or(false) {
            Ok(v.get("source")
                .and_then(|s| s.as_str())
                .unwrap_or_default()
                .to_string())
        } else {
            let err = v
                .get("error")
                .and_then(|s| s.as_str())
                .unwrap_or("unknown error");
            bail!("{}", last_line(err))
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = writeln!(self.stdin, "{{\"cmd\":\"quit\"}}");
        let _ = self.stdin.flush();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn last_line(s: &str) -> &str {
    s.lines()
        .filter(|l| !l.trim().is_empty())
        .next_back()
        .unwrap_or(s)
}

impl Decompiler {
    /// Pick a backend, honouring an explicit request but falling back with a
    /// clear reason when it is not available.
    pub fn detect(prefer: Option<&str>, python: Option<&str>, workers: usize) -> Result<Decompiler> {
        let pycdc = find_pycdc();
        let python = python
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("UNMCPK_PYTHON").map(PathBuf::from))
            .or_else(find_python);

        let mut d = Decompiler {
            kind: Kind::None,
            pycdc: pycdc.clone(),
            python: python.clone(),
            worker_script: None,
            pool: Mutex::new(Vec::new()),
            workers: workers.max(1),
        };

        let want = prefer.unwrap_or("auto");
        let (kind, why) = match want {
            "none" => (Kind::None, "requested".to_string()),
            "native" => (Kind::Native, "built in".to_string()),
            "pycdc" => match &d.pycdc {
                Some(p) => (Kind::Pycdc, p.display().to_string()),
                None => bail!("--decompiler pycdc requested but no pycdc binary was found"),
            },
            "uncompyle6" => match (&d.python, d.has_uncompyle6()) {
                (Some(p), true) => (Kind::Uncompyle6, p.display().to_string()),
                _ => bail!("--decompiler uncompyle6 requested but no python with uncompyle6 was found"),
            },
            _ => {
                // The built-in backend needs nothing else installed, so it is the
                // default. `pycdc` and `uncompyle6` stay available by name for
                // comparison: pycdc is far faster than uncompyle6 (0.2 s vs 97 s
                // on the largest module) and its output is closer to the original
                // source, but it has to be built and shipped separately.
                (Kind::Native, "built in".to_string())
            }
        };
        d.kind = kind;
        if kind == Kind::Uncompyle6 {
            d.worker_script = Some(write_worker_script()?);
        }
        eprintln!("decompiler: {} ({why})", match kind {
            Kind::None => "none",
            Kind::Native => "native",
            Kind::Pycdc => "pycdc",
            Kind::Uncompyle6 => "uncompyle6",
        });
        Ok(d)
    }

    /// pycdc deparses every file handed to it in a single process, so batches
    /// amortise the ~40 ms process spawn. The uncompyle6 backend keeps one
    /// interpreter per worker instead and takes one file per call.
    pub fn batchable(&self) -> bool {
        self.kind == Kind::Pycdc
    }

    /// Decompile several files at once. Falls back to one call per file when
    /// the backend cannot batch or when its output cannot be split reliably.
    pub fn decompile_batch(&self, pycs: &[&Path]) -> Vec<Result<String>> {
        if self.kind == Kind::Pycdc {
            if let Some(texts) = self.run_pycdc_batch(pycs) {
                return texts.into_iter().map(Ok).collect();
            }
        }
        pycs.iter()
            .enumerate()
            .map(|(i, p)| self.decompile_text(p, i as u32))
            .collect()
    }

    fn run_pycdc_batch(&self, pycs: &[&Path]) -> Option<Vec<String>> {
        let exe = self.pycdc.as_ref()?;
        let mut cmd = Command::new(exe);
        for p in pycs {
            cmd.arg(p);
        }
        let out = cmd.stdin(Stdio::null()).output().ok()?;
        if !out.status.success() && out.stdout.is_empty() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        split_pycdc_output(&text, pycs.len())
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    pub fn is_enabled(&self) -> bool {
        self.kind != Kind::None
    }

    fn has_uncompyle6(&self) -> bool {
        match &self.python {
            Some(py) => Command::new(py)
                .args(["-c", "import uncompyle6.main"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false),
            None => false,
        }
    }

    /// Decompile one pyc file and return the source text.
    pub fn decompile_text(&self, pyc: &Path, id: u32) -> Result<String> {
        match self.kind {
            Kind::None => bail!("no decompiler backend"),
            Kind::Native => {
                let bytes = std::fs::read(pyc).with_context(|| format!("reading {}", pyc.display()))?;
                let body = bytes
                    .get(PYC_HEADER.len()..)
                    .context("pyc shorter than its header")?;
                let parsed = crate::nemarshal::parse_module(body, crate::nemarshal::Options::default())
                    .with_context(|| format!("parsing {}", pyc.display()))?;
                // A module the builder cannot model comes back as a comment block
                // rather than source; report that as a failure so the summary and
                // the manifest stay honest.
                crate::pydec::try_decompile(&parsed.code).map_err(|reason| {
                    anyhow::anyhow!("{}", reason)
                })
            }
            Kind::Pycdc => {
                let exe = self.pycdc.as_ref().context("pycdc path")?;
                let out = Command::new(exe)
                    .arg(pyc)
                    .stdin(Stdio::null())
                    .output()
                    .with_context(|| format!("running {}", exe.display()))?;
                let text = String::from_utf8_lossy(&out.stdout).into_owned();
                if text.trim().is_empty() {
                    bail!(
                        "pycdc produced no output: {}",
                        last_line(&String::from_utf8_lossy(&out.stderr))
                    );
                }
                Ok(text)
            }
            Kind::Uncompyle6 => {
                let mut worker = match self.pool.lock().unwrap().pop() {
                    Some(w) => w,
                    None => Worker::start(
                        self.python.as_ref().context("python path")?,
                        self.worker_script.as_ref().context("worker script")?,
                    )?,
                };
                let result = worker.run(pyc, id);
                // A worker that failed on a broken pyc is still healthy; only
                // recycle it when the protocol itself broke.
                let healthy = matches!(&result, Ok(_)) || !worker_is_dead(&mut worker);
                if healthy {
                    self.pool.lock().unwrap().push(worker);
                }
                result
            }
        }
    }

    /// Ensure `workers` interpreters are warm before a parallel batch.
    pub fn warm(&self) -> Result<()> {
        if self.kind != Kind::Uncompyle6 {
            return Ok(());
        }
        let py = self.python.as_ref().context("python path")?;
        let script = self.worker_script.as_ref().context("worker script")?;
        let mut pool = self.pool.lock().unwrap();
        while pool.len() < self.workers {
            pool.push(Worker::start(py, script)?);
        }
        Ok(())
    }
}

fn worker_is_dead(_w: &mut Worker) -> bool {
    // Cheap probe: if the process already exited there is no point keeping it.
    matches!(_w.child.try_wait(), Ok(Some(_)))
}

fn write_worker_script() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join("unmcpk");
    std::fs::create_dir_all(&dir).ok();
    let path = dir.join("uncompyle6_worker.py");
    let needs_write = match std::fs::read_to_string(&path) {
        Ok(existing) => existing != WORKER,
        Err(_) => true,
    };
    if needs_write {
        std::fs::write(&path, WORKER).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(path)
}

/// Look for the pycdc binary: first next to this executable (`bin/pycdc.exe`
/// ships beside `unmcpk.exe`), then in the usual project locations, then on
/// PATH.
fn find_pycdc() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("UNMCPK_PYCDC") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("pycdc.exe"));
            candidates.push(dir.join("pycdc"));
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        // running from target/release: look in <project>/pycdc as well
        if let Some(dir) = exe.parent().and_then(|d| d.parent()) {
            candidates.push(dir.join("pycdc").join("pycdc.exe"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("pycdc").join("pycdc.exe"));
        candidates.push(cwd.join("pycdc").join("pycdc"));
        candidates.push(cwd.join("target").join("pycdc").join("pycdc.exe"));
        candidates.push(cwd.join("bin").join("pycdc.exe"));
    }
    candidates.into_iter().find(|p| p.is_file())
}

fn find_python() -> Option<PathBuf> {
    for name in ["python", "python3", "py"] {
        if let Ok(out) = Command::new(name).arg("-V").output() {
            if out.status.success() {
                let ok = Command::new(name)
                    .args(["-c", "import sys; sys.exit(0 if sys.version_info >= (3, 8) else 1)"])
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                if ok {
                    return Some(PathBuf::from(name));
                }
            }
        }
    }
    None
}

/// Repair NetEase's GBK-encoded docstrings.
///
/// The shipped modules embed byte strings in GBK, and the decompilers decode
/// them as UTF-8 — which "succeeds" for most GBK pairs and yields Greek or
/// Cyrillic lookalikes (`Ϊָ…`). Re-encoding the line and decoding it as GBK
/// recovers the Chinese text. Only lines whose non-ASCII characters are all in
/// the suspicious ranges are touched, and only when the GBK decode actually
/// produces CJK, so ordinary Latin text is never rewritten.
pub fn fix_mojibake(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 64);
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        match repair_line(line) {
            Some(fixed) => out.push_str(&fixed),
            None => out.push_str(line),
        }
    }
    out
}

fn repair_line(line: &str) -> Option<String> {
    let mut suspicious = 0usize;
    let mut non_ascii = 0usize;
    for ch in line.chars() {
        let c = ch as u32;
        if c < 0x80 {
            continue;
        }
        non_ascii += 1;
        // Latin-1 supplement, Latin extended, IPA, spacing modifiers, Greek,
        // Cyrillic - the shapes GBK pairs turn into when read as UTF-8.
        if (0xA0..=0x24F).contains(&c) || (0x2B0..=0x52F).contains(&c) {
            suspicious += 1;
        } else {
            return None;
        }
    }
    if suspicious == 0 || non_ascii == 0 {
        return None;
    }
    let (decoded, _, had_errors) = encoding_rs::GBK.decode(line.as_bytes());
    if had_errors {
        return None;
    }
    if decoded
        .chars()
        .any(|c| (0x4E00..=0x9FFF).contains(&(c as u32)) || (0x3000..=0x303F).contains(&(c as u32)))
    {
        Some(decoded.into_owned())
    } else {
        None
    }
}

/// pycdc prints a fixed three-line banner before each deparsed file:
///
/// ```text
/// # -*- coding: utf-8 -*-
/// # Source Generated with Decompyle++
/// # File: Queue.pyc (Python 2.7)
/// ```
///
/// so the banner is the only reliable delimiter. The split is rejected (and the
/// caller falls back to one process per file) unless exactly one banner shows
/// up per requested file.
fn split_pycdc_output(text: &str, expected: usize) -> Option<Vec<String>> {
    const MARK: &str = "# Source Generated with Decompyle++";
    const CODING: &str = "# -*- coding";
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let marks: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.contains(MARK))
        .map(|(i, _)| i)
        .collect();
    if marks.len() != expected || expected == 0 {
        return None;
    }
    let mut files = Vec::with_capacity(expected);
    for (k, &mark) in marks.iter().enumerate() {
        let start = if mark > 0 && lines[mark - 1].starts_with(CODING) {
            mark - 1
        } else {
            mark
        };
        let end = match marks.get(k + 1) {
            Some(&next) if next > 0 && lines[next - 1].starts_with(CODING) => next - 1,
            Some(&next) => next,
            None => lines.len(),
        };
        files.push(lines[start..end].concat());
    }
    Some(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_batched_pycdc_output() {
        let text = "# -*- coding: utf-8 -*-\n# Source Generated with Decompyle++\n# File: a.pyc (Python 2.7)\nprint 1\n# -*- coding: utf-8 -*-\n# Source Generated with Decompyle++\n# File: b.pyc (Python 2.7)\nprint 2\n";
        let parts = split_pycdc_output(text, 2).unwrap();
        assert_eq!(parts.len(), 2);
        assert!(parts[0].starts_with("# -*- coding"));
        assert!(parts[0].contains("print 1"));
        assert!(!parts[0].contains("print 2"));
        assert!(parts[1].contains("print 2"));
        assert!(!parts[1].contains("print 1"));
    }

    #[test]
    fn rejects_split_with_wrong_count() {
        let text = "# Source Generated with Decompyle++\n# File: a.pyc\n";
        assert!(split_pycdc_output(text, 2).is_none());
    }

    #[test]
    fn keeps_plain_text() {
        assert_eq!(fix_mojibake("print('hello')\n"), "print('hello')\n");
    }
}
