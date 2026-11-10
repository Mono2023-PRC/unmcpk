//! A Python 2.7 decompiler, so the tool has no external backend.
//!
//! The pieces: [`opcodes`] describes the instruction set after the NetEase
//! permutation has been undone, [`disasm`] turns `co_code` into instructions,
//! [`build`] rebuilds statements and expressions with a symbolic stack, and
//! [`emit`] writes them out with correct precedence and indentation.
//!
//! When a code object uses a construct [`build`] cannot model, the whole object
//! is rendered as a comment plus its instructions. That keeps the output valid
//! Python and makes the gap visible instead of silently wrong.

pub mod build;
pub mod disasm;
pub mod emit;
pub mod expr;
pub mod opcodes;

use crate::nemarshal::Code;

pub use build::{Deferred, FunctionDef, Stmt};

/// Rebuild one code object. Returns the reason when it falls back.
pub fn try_decompile(code: &Code) -> Result<String, String> {
    build::reset_budget(4096);
    let built = build::Builder::new(code).build()?;
    Ok(emit::module(&built.stmts))
}

/// Rebuild a code object, falling back to a disassembly listing.
pub fn decompile_code(code: &Code) -> String {
    let started = std::time::Instant::now();
    let result = decompile_inner(code);
    if std::env::var_os("UNMCPK_PROGRESS2").is_some() && started.elapsed().as_millis() > 150 {
        eprintln!(
            "slow {} {} ms",
            code.name_str(),
            started.elapsed().as_millis()
        );
    }
    result
}

fn decompile_inner(code: &Code) -> String {
    match try_decompile(code) {
        Ok(text) => text,
        Err(reason) => fallback(code, &reason),
    }
}

fn fallback(code: &Code, reason: &str) -> String {
    let mut out = String::new();
    out.push_str("# unmcpk: could not rebuild this code object\n");
    out.push_str(&format!("# reason: {}\n", one_line(reason)));
    out.push_str("# the original instructions follow\n");
    for line in disasm::listing(code) {
        out.push_str("#   ");
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str("pass\n");
    out
}

fn one_line(text: &str) -> String {
    text.lines().collect::<Vec<_>>().join(" ")
}

pub fn render_lambda(def: &Deferred) -> String {
    let args = emit::render_args(&def.code, &def.defaults, def.keyword_defaults.as_ref());
    let body = match try_decompile(&def.code) {
        Ok(text) => single_expression(&text).unwrap_or_else(|| {
            format!("None  # unmcpk: lambda body is {}", describe(&text))
        }),
        Err(reason) => format!("None  # unmcpk: {}", one_line(&reason)),
    };
    format!("lambda {args}: {body}")
}

/// A lambda body is a single `return <expr>`; recover the expression.
fn single_expression(text: &str) -> Option<String> {
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let line = lines.next()?;
    if lines.next().is_some() {
        return None;
    }
    line.trim()
        .strip_prefix("return ")
        .map(|rest| rest.to_string())
}

fn describe(text: &str) -> String {
    let count = text.lines().count();
    if count == 1 {
        "a statement".to_string()
    } else {
        format!("{count} statements")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nemarshal::Value;

    fn code(bytecode: Vec<u8>, consts: Vec<Value>, names: Vec<&str>, vars: Vec<&str>) -> Code {
        Code {
            bytecode,
            constants: consts,
            names: names.into_iter().map(|s| s.as_bytes().to_vec()).collect(),
            var_names: vars.into_iter().map(|s| s.as_bytes().to_vec()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn decompiles_an_assignment() {
        let code = code(
            vec![100, 0, 0, 90, 0, 0],
            vec![Value::Int(7)],
            vec!["x"],
            vec![],
        );
        assert_eq!(try_decompile(&code).unwrap(), "x = 7\n");
    }

    #[test]
    fn falls_back_with_a_visible_reason() {
        // A block opcode this pass does not model (2.7 never emits 123).
        let code = code(vec![123, 0, 0, 87], vec![], vec![], vec![]);
        let text = decompile_code(&code);
        assert!(text.starts_with("# unmcpk: could not rebuild"));
        assert!(text.contains("unsupported block opcode"));
        assert!(text.trim_end().ends_with("pass"));
    }
}
