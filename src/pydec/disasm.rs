//! Turn a code object's bytecode into a flat list of instructions.
//!
//! Offsets are byte offsets into `co_code`, which is what every jump argument
//! in 2.7 is expressed in, so callers can jump straight to `index_of(offset)`.

use super::opcodes;
use crate::nemarshal::{Code, Value};

#[derive(Clone, Debug)]
pub struct Instr {
    pub offset: usize,
    pub op: u8,
    pub arg: u32,
    /// Human-readable resolution of `arg`, for listings and for error text.
    pub arg_text: String,
    /// Annotated when the opcode is a jump, so structuring never has to know
    /// which opcodes are relative and which are absolute.
    pub target: Option<usize>,
}

impl Instr {
    pub fn name(&self) -> &'static str {
        opcodes::name(self.op)
    }

    pub fn has_arg(&self) -> bool {
        opcodes::has_arg(self.op)
    }

    /// Jump targets for absolute and relative branches, already normalised to a
    /// byte offset. `FOR_ITER`, `SETUP_*`, `JUMP_FORWARD` and friends are
    /// relative to the *next* instruction, which is why this needs the
    /// instruction length.
    fn jump_from(op: u8, offset: usize, arg: u32) -> Option<usize> {
        match op {
            // Relative (CPython 2.7 `hasjrel`): target = offset + 3 + arg.
            93 | 110 | 120 | 121 | 122 | 143 => Some(offset + 3 + arg as usize),
            // Absolute (CPython 2.7 `hasjabs`): target = arg. Note that
            // `JUMP_IF_*_OR_POP` belongs here, not with the relative jumps.
            111 | 112 | 113 | 114 | 115 | 119 => Some(arg as usize),
            _ => None,
        }
    }

    pub fn is_jump(&self) -> bool {
        self.target.is_some()
    }

    pub fn is_unconditional(&self) -> bool {
        matches!(self.op, 110 | 113 | 119)
    }
}

pub fn disassemble(code: &Code) -> Vec<Instr> {
    let bytes = &code.bytecode;
    let mut out = Vec::new();
    let mut pos = 0usize;
    let mut extended: Option<u32> = None;

    while pos < bytes.len() {
        let op = bytes[pos];
        if !opcodes::has_arg(op) {
            out.push(Instr {
                offset: pos,
                op,
                arg: 0,
                arg_text: String::new(),
                target: None,
            });
            pos += 1;
            continue;
        }
        let (raw, len) = match bytes.get(pos + 1).copied().zip(bytes.get(pos + 2).copied()) {
            Some((lo, hi)) => (((hi as u32) << 8) | lo as u32, 3usize),
            None => {
                // Truncated operand: emit what we have so the caller reports a
                // range error instead of panicking on a bad payload.
                let lo = bytes.get(pos + 1).copied().unwrap_or(0) as u32;
                (lo, bytes.len() - pos)
            }
        };
        let arg = match extended.take() {
            Some(high) => (high << 16) | raw,
            None => raw,
        };
        if op == 145 {
            extended = Some(arg);
            pos += len;
            continue;
        }
        let target = Instr::jump_from(op, pos, arg);
        out.push(Instr {
            offset: pos,
            op,
            arg,
            arg_text: resolve(code, op, arg),
            target,
        });
        pos += len;
    }
    out
}

fn resolve(code: &Code, op: u8, arg: u32) -> String {
    let index = arg as usize;
    match op {
        100 => code
            .constants
            .get(index)
            .map(repr)
            .unwrap_or_else(|| format!("<bad const {arg}>")),
        90 | 91 | 101 | 106 | 108 | 109 | 116 => code
            .names
            .get(index)
            .map(|b| text(b))
            .unwrap_or_else(|| format!("<bad name {arg}>")),
        124 | 125 | 126 => code
            .var_names
            .get(index)
            .map(|b| text(b))
            .unwrap_or_else(|| format!("<bad var {arg}>")),
        95 | 96 | 97 | 98 => code
            .names
            .get(index)
            .map(|b| text(b))
            .unwrap_or_else(|| format!("<bad name {arg}>")),
        136 | 137 => code
            .cell_vars
            .get(index)
            .or_else(|| code.free_vars.get(index))
            .map(|b| text(b))
            .unwrap_or_else(|| format!("<bad cell {arg}>")),
        135 => code
            .cell_vars
            .get(index)
            .map(|b| text(b))
            .unwrap_or_else(|| format!("<bad cell {arg}>")),
        107 => opcodes::compare_op(arg).to_string(),
        111 | 112 | 113 | 114 | 115 | 119 => format!("-> {arg}"),
        93 | 110 | 120 | 121 | 122 | 143 => format!("-> {}", arg as usize),
        _ => {
            if opcodes::has_arg(op) {
                arg.to_string()
            } else {
                String::new()
            }
        }
    }
}

pub fn repr(value: &Value) -> String {
    match value {
        Value::Null => "<null>".into(),
        Value::None => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Int(v) => v.to_string(),
        Value::Int64(v) => v.to_string(),
        Value::Float(v) => format_float(*v),
        Value::Complex(re, im) => format!("({}j)", format_float(*re + *im)),
        Value::Long {
            negative,
            digits,
        } => {
            // `digits` are base 2^15 limbs, least significant first; render the
            // decimal form by repeated division, exactly like CPython's repr.
            let mut limbs: Vec<u32> = digits.iter().map(|d| *d as u32).collect();
            let mut decimal = String::new();
            while limbs.iter().any(|d| *d != 0) {
                let mut carry = 0u32;
                for limb in limbs.iter_mut().rev() {
                    let cur = (carry << 15) | *limb;
                    *limb = cur / 10;
                    carry = cur % 10;
                }
                decimal.push(char::from(b'0' + carry as u8));
            }
            if decimal.is_empty() {
                decimal.push('0');
            }
            let digits_text: String = decimal.chars().rev().collect();
            let out = if *negative {
                format!("-{digits_text}L")
            } else {
                format!("{digits_text}L")
            };
            out
        }
        Value::Bytes(b) => escape_bytes(b),
        Value::Unicode(s) => format!("\"{}\"", escape_str(&text(s))),
        Value::Tuple(items) => format!("({})", join(items)),
        Value::List(items) => format!("[{}]", join(items)),
        Value::Dict(pairs) => {
            let inner = pairs
                .iter()
                .map(|(k, v)| format!("{}: {}", repr(k), repr(v)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{{{inner}}}")
        }
        Value::Set(items) | Value::FrozenSet(items) => format!("set([{}])", join(items)),
        Value::Ellipsis => "...".into(),
        Value::StopIteration => "StopIteration".into(),
        Value::Code(_) => "<code object>".into(),
    }
}

fn join(items: &[Value]) -> String {
    items.iter().map(repr).collect::<Vec<_>>().join(", ")
}

fn format_float(v: f64) -> String {
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 1e16 {
        format!("{v:.1}")
    } else {
        format!("{v:?}")
    }
}

/// Lossy UTF-8 view of a marshal name (names are raw bytes in the container).
pub fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

pub fn escape_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\0' => out.push_str("\\x00"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

pub fn escape_bytes(bytes: &[u8]) -> String {
    let mut out = String::from("b'");
    for &b in bytes {
        match b {
            b'\\' => out.push_str("\\\\"),
            b'\'' => out.push_str("\\'"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x20..=0x7e => out.push(b as char),
            other => out.push_str(&format!("\\x{other:02x}")),
        }
    }
    out.push('\'');
    out
}

/// Text form used by the `disasm` subcommand: `offset  name  arg`.
pub fn listing(code: &Code) -> Vec<String> {
    disassemble(code)
        .into_iter()
        .map(|i| {
            if i.has_arg() {
                format!("{:>6}  {:<22} {}", i.offset, i.name(), i.arg_text)
            } else {
                format!("{:>6}  {}", i.offset, i.name())
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(bytecode: Vec<u8>, consts: Vec<Value>, names: Vec<&str>) -> Code {
        Code {
            bytecode,
            constants: consts,
            names: names.into_iter().map(|s| s.as_bytes().to_vec()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn decodes_offsets_and_arguments() {
        // LOAD_CONST 0; RETURN_VALUE
        let c = code(vec![100, 0, 0, 83], vec![Value::None], vec![]);
        let ins = disassemble(&c);
        assert_eq!(ins.len(), 2);
        assert_eq!(ins[0].name(), "LOAD_CONST");
        assert_eq!(ins[0].offset, 0);
        assert_eq!(ins[0].arg_text, "None");
        assert_eq!(ins[1].offset, 3);
        assert_eq!(ins[1].name(), "RETURN_VALUE");
    }

    #[test]
    fn resolves_relative_and_absolute_jumps() {
        // JUMP_FORWARD 3 at offset 0 -> 0 + 3 + 3 = 6
        let c = code(vec![110, 3, 0], vec![], vec![]);
        assert_eq!(disassemble(&c)[0].target, Some(6));
        // JUMP_ABSOLUTE 7 -> absolute 7
        let c = code(vec![113, 7, 0], vec![], vec![]);
        assert_eq!(disassemble(&c)[0].target, Some(7));
    }

    #[test]
    fn handles_extended_arg() {
        // EXTENDED_ARG 1; LOAD_CONST 0 -> constant index 0x10000
        let c = code(vec![145, 1, 0, 100, 0, 0], vec![], vec![]);
        let ins = disassemble(&c);
        assert_eq!(ins.len(), 1);
        assert_eq!(ins[0].arg, 0x10000);
    }

    #[test]
    fn renders_constants_readably() {
        assert_eq!(repr(&Value::Unicode("a\"b".into())), "\"a\\\"b\"");
        assert_eq!(repr(&Value::Tuple(vec![Value::Int(1), Value::None])), "(1, None)");
        assert_eq!(
            repr(&Value::Long {
                negative: false,
                digits: vec![1, 0]
            }),
            "1L"
        );
    }
}
