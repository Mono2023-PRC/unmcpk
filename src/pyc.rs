//! Emit a plain CPython 2.7 `.pyc` from a normalised [`Code`] tree.
//!
//! The output is byte-compatible with `marshal.dumps` from CPython 2.7 with no
//! interned-string table: every `R` reference has already been expanded by the
//! parser, so the emitter can stay stateless.

use crate::nemarshal::{
    Code, Value, TAG_CODE, TAG_COMPLEX_TEXT, TAG_DICT, TAG_FLOAT_TEXT, TAG_INT, TAG_LIST,
    TAG_LONG, TAG_NONE, TAG_NULL, TAG_STRING, TAG_TRUE, TAG_TUPLE, TAG_UNICODE,
};

/// CPython 2.7 magic (mtime 0) followed by the 4-byte source timestamp + size,
/// which the decompilers look at but never validate.
pub const PYC_HEADER: [u8; 8] = [
    0x03, 0xF3, 0x0D, 0x0A, // magic: 0x0DF303 (CPython 2.7), little endian
    0x00, 0x00, 0x00, 0x00, // mtime; a Python 2.7 pyc header is exactly 8 bytes
];

pub fn emit_file(code: &Code) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + code.count_objects() * 512);
    out.extend_from_slice(&PYC_HEADER);
    write_code(&mut out, code);
    out
}

fn write_i32(out: &mut Vec<u8>, v: i32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn write_str(out: &mut Vec<u8>, tag: u8, data: &[u8]) {
    out.push(tag);
    write_i32(out, data.len() as i32);
    out.extend_from_slice(data);
}

fn write_str_list(out: &mut Vec<u8>, items: &[Vec<u8>]) {
    out.push(TAG_TUPLE);
    write_i32(out, items.len() as i32);
    for item in items {
        write_str(out, TAG_STRING, item);
    }
}

fn write_code(out: &mut Vec<u8>, code: &Code) {
    out.push(TAG_CODE);
    write_i32(out, code.arg_count);
    write_i32(out, code.local_count);
    write_i32(out, code.stack_size);
    write_i32(out, code.flags);
    write_str(out, TAG_STRING, &code.bytecode);
    write_seq(out, &code.constants);
    write_str_list(out, &code.names);
    write_str_list(out, &code.var_names);
    write_str_list(out, &code.free_vars);
    write_str_list(out, &code.cell_vars);
    write_str(out, TAG_STRING, &code.filename);
    write_str(out, TAG_STRING, &code.name);
    write_i32(out, code.first_line);
    write_str(out, TAG_STRING, &code.line_table);
}

fn write_seq(out: &mut Vec<u8>, items: &[Value]) {
    out.push(TAG_TUPLE);
    write_i32(out, items.len() as i32);
    for v in items {
        write_value(out, v);
    }
}

/// Write the ASCII digits of a float without the tag (used by 'x' complex).
fn write_float_body(out: &mut Vec<u8>, v: f64) {
    let text = format!("{:?}", v);
    out.push(text.len() as u8);
    out.extend_from_slice(text.as_bytes());
}

fn write_float_text(out: &mut Vec<u8>, tag: u8, v: f64) {
    out.push(tag);
    write_float_body(out, v);
}

pub fn write_value(out: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Null => out.push(TAG_NULL),
        Value::None => out.push(TAG_NONE),
        Value::Bool(true) => out.push(TAG_TRUE),
        Value::Bool(false) => out.push(b'F'),
        Value::StopIteration => out.push(b'S'),
        Value::Ellipsis => out.push(b'.'),
        Value::Int(v) => {
            out.push(TAG_INT);
            write_i32(out, *v);
        }
        Value::Int64(v) => {
            out.push(b'I');
            out.extend_from_slice(&v.to_le_bytes());
        }
        // CPython 2.7 has no binary float tags: 'f' is a 1-byte length plus
        // ASCII digits, 'x' is two of those. 'g'/'y' only exist from 3.4 on and
        // would make a 2.7 reader (xdis, pycdc) derail.
        Value::Float(v) => write_float_text(out, TAG_FLOAT_TEXT, *v),
        Value::Complex(re, im) => {
            out.push(TAG_COMPLEX_TEXT);
            write_float_body(out, *re);
            write_float_body(out, *im);
        }
        Value::Long { negative, digits } => {
            out.push(TAG_LONG);
            // Invert the 15-digit grouping CPython uses to store the digit
            // count (see the parser for the formula). A zero-digit long is the
            // value 0L and must be written with a count of 0.
            let k = digits.len();
            let mut n = if k == 0 {
                0
            } else {
                (((k - 1) / 15) * 15 + (k - 1) % 15 + 1) as i32
            };
            if *negative {
                n = -n;
            }
            write_i32(out, n);
            for d in digits {
                out.extend_from_slice(&d.to_le_bytes());
            }
        }
        Value::Bytes(b) => write_str(out, TAG_STRING, b),
        Value::Unicode(b) => write_str(out, TAG_UNICODE, b),
        Value::Tuple(items) => write_seq(out, items),
        Value::List(items) => {
            out.push(TAG_LIST);
            write_i32(out, items.len() as i32);
            for v in items {
                write_value(out, v);
            }
        }
        // Python 2 has no set literal in the marshalled form ('<' and '>' were
        // introduced in 3.1), so a stray set is emitted as a tuple: it keeps the
        // stream readable instead of producing a pyc that no reader accepts.
        Value::Set(items) | Value::FrozenSet(items) => write_seq(out, items),
        Value::Dict(items) => {
            out.push(TAG_DICT);
            for (k, v) in items {
                write_value(out, k);
                write_value(out, v);
            }
            out.push(TAG_NULL);
        }
        Value::Code(code) => write_code(out, code),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nemarshal::{parse_module, Options};

    #[test]
    fn roundtrip_through_parser() {
        let mut code = Code::default();
        code.stack_size = 1;
        code.bytecode = vec![0x64, 0x00, 0x53];
        code.constants = vec![Value::None];
        code.filename = b"roundtrip.py".to_vec();
        code.name = b"<module>".to_vec();
        let bytes = emit_file(&code);
        assert_eq!(&bytes[..4], &PYC_HEADER[..4]);
        let parsed = parse_module(&bytes[PYC_HEADER.len()..], Options::default()).unwrap();
        assert_eq!(parsed.code.bytecode, code.bytecode);
        assert_eq!(parsed.code.filename, code.filename);
        assert_eq!(parsed.code.constants, code.constants);
    }
}
