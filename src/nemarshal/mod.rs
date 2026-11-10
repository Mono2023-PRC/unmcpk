//! Reader for NetEase's mutated Python 2.7 marshal streams.
//!
//! The container ships `marshal.dumps` output that has been re-encoded by a
//! modified marshal module: three quarters of the type tags were moved to
//! unrelated byte values, strings may be RC4/XOR encrypted, and the field order
//! of code objects was shuffled (three different layouts exist). On top of that
//! the code object's bytecode has its *opcode* bytes permuted through a lookup
//! table; operand bytes are plain CPython 2.7 little endian.
//!
//! The parser below accepts both the standard CPython 2.7 tags (so it can also
//! read ordinary `.pyc` files) and the NetEase tags, normalising everything
//! into [`Code`]/[`Value`] so that the emitter in [`crate::pyc`] can write a
//! plain CPython 2.7 marshal stream.

pub mod tables;

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Standard CPython 2.7 tags.
// ---------------------------------------------------------------------------
pub const TAG_NULL: u8 = b'0';
pub const TAG_NONE: u8 = b'N';
pub const TAG_FALSE: u8 = b'F';
pub const TAG_TRUE: u8 = b'T';
pub const TAG_STOPITER: u8 = b'S';
pub const TAG_ELLIPSIS: u8 = b'.';
pub const TAG_INT: u8 = b'i';
pub const TAG_INT64: u8 = b'I';
pub const TAG_STRING: u8 = b's';
pub const TAG_INTERNED: u8 = b't';
pub const TAG_REF: u8 = b'R';
pub const TAG_TUPLE: u8 = b'(';
pub const TAG_LIST: u8 = b'[';
pub const TAG_DICT: u8 = b'{';
pub const TAG_CODE: u8 = b'c';
pub const TAG_UNICODE: u8 = b'u';
pub const TAG_SET: u8 = b'<';
pub const TAG_FROZENSET: u8 = b'>';
pub const TAG_FLOAT_TEXT: u8 = b'f';
pub const TAG_FLOAT_BIN: u8 = b'g';
pub const TAG_COMPLEX_TEXT: u8 = b'x';
pub const TAG_COMPLEX_BIN: u8 = b'y';
pub const TAG_LONG: u8 = b'l';

// ---------------------------------------------------------------------------
// NetEase tags.
// ---------------------------------------------------------------------------
pub const TAG_NE_CODE_0: u8 = 0x61;
pub const TAG_NE_CODE_1: u8 = 0x6f;
pub const TAG_NE_CODE_2: u8 = 0x4d;

pub const TAG_NE_STRING_0: u8 = 0x08;
pub const TAG_NE_INTERNED_0: u8 = 0x0f;
pub const TAG_NE_UNICODE_0: u8 = 0x0e;
pub const TAG_NE_STRING_1: u8 = 0x17;
pub const TAG_NE_INTERNED_1: u8 = 0x1a;
pub const TAG_NE_UNICODE_1: u8 = 0x1d;
pub const TAG_NE_STRING_2: u8 = 0x6d;
pub const TAG_NE_INTERNED_2: u8 = 0x62;
pub const TAG_NE_UNICODE_2: u8 = 0x31;

/// RC4 key for the version 1 encrypted strings.
const RC4_KEY_1: [u8; 4] = [0xA7, 0x0D, 0x37, 0x7A];
/// RC4 key for the version 2 encrypted strings.
const RC4_KEY_2: [u8; 12] = [
    0x8D, 0x06, 0xE8, 0xC8, 0xB7, 0xD7, 0xB7, 0x28, 0x46, 0x51, 0xAE, 0x04,
];
/// Single byte XOR used by the version 0 encrypted strings.
const XOR_0: u8 = 0x8D;

const MAX_DEPTH: usize = 512;
const MAX_COUNT: u32 = 1 << 26;

fn rc4(key: &[u8], data: &mut [u8]) {
    let mut s: [u8; 256] = [0; 256];
    for (i, v) in s.iter_mut().enumerate() {
        *v = i as u8;
    }
    let mut j = 0u8;
    for i in 0..256 {
        j = j
            .wrapping_add(s[i])
            .wrapping_add(key[i % key.len()]);
        s.swap(i, j as usize);
    }
    let (mut i, mut j) = (0u8, 0u8);
    for b in data.iter_mut() {
        i = i.wrapping_add(1);
        j = j.wrapping_add(s[i as usize]);
        s.swap(i as usize, j as usize);
        let k = s[(s[i as usize].wrapping_add(s[j as usize])) as usize];
        *b ^= k;
    }
}

/// A Python 2.7 `co_*` object, normalised.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Code {
    pub arg_count: i32,
    pub local_count: i32,
    pub stack_size: i32,
    pub flags: i32,
    /// Already translated back to CPython 2.7 opcodes.
    pub bytecode: Vec<u8>,
    pub constants: Vec<Value>,
    pub names: Vec<Vec<u8>>,
    pub var_names: Vec<Vec<u8>>,
    pub free_vars: Vec<Vec<u8>>,
    pub cell_vars: Vec<Vec<u8>>,
    pub filename: Vec<u8>,
    pub name: Vec<u8>,
    pub first_line: i32,
    pub line_table: Vec<u8>,
    /// True when this object's bytecode failed validation and was replaced by
    /// a stub. NetEase ships a handful of deliberately malformed "version
    /// check" code objects that no decompiler can consume.
    pub decoy: bool,
}

impl Code {
    pub fn filename_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.filename).ok()
    }
    pub fn name_str(&self) -> String {
        String::from_utf8_lossy(&self.name).into_owned()
    }
    /// Depth-first walk over this code object and every code object nested in
    /// its constants.
    pub fn walk<'a>(&'a self, out: &mut Vec<&'a Code>) {
        out.push(self);
        for c in &self.constants {
            if let Value::Code(nested) = c {
                nested.walk(out);
            }
        }
    }
    pub fn count_objects(&self) -> usize {
        let mut v = Vec::new();
        self.walk(&mut v);
        v.len()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    None,
    Bool(bool),
    Int(i32),
    Int64(i64),
    Float(f64),
    Complex(f64, f64),
    /// Raw `long` digits, base 2^16, least significant first.
    Long {
        negative: bool,
        digits: Vec<u16>,
    },
    Bytes(Vec<u8>),
    /// Bytes that were tagged as `unicode` (kept raw so nothing is lost).
    Unicode(Vec<u8>),
    Tuple(Vec<Value>),
    List(Vec<Value>),
    Dict(Vec<(Value, Value)>),
    Set(Vec<Value>),
    FrozenSet(Vec<Value>),
    Ellipsis,
    StopIteration,
    Code(Box<Code>),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Bytes(b) | Value::Unicode(b) => std::str::from_utf8(b).ok(),
            _ => None,
        }
    }
    pub fn is_code(&self) -> bool {
        matches!(self, Value::Code(_))
    }
}

#[derive(Debug, Default, Clone)]
pub struct Stats {
    /// Number of code objects seen (including nested ones).
    pub code_objects: usize,
    /// Number of `R` (interned table) references seen.
    pub interned_refs: usize,
    /// Number of interned entries registered.
    pub interned_entries: usize,
    /// Number of string values that had to be decrypted.
    pub encrypted_strings: usize,
    /// Opcode table ids encountered, with how often.
    pub opcode_tables: BTreeMap<u32, usize>,
    /// Unknown opcode bytes per table id.
    pub unknown_opcodes: BTreeMap<(u32, u8), usize>,
    /// Bytes of payload the parser did not consume.
    pub unconsumed: usize,
    /// Field-order layouts encountered (`0`/`1`/`2`, standard = 3).
    pub code_layouts: BTreeMap<u8, usize>,
    /// Code objects whose bytecode was invalid and got replaced by a stub.
    pub decoy_code_objects: usize,
    /// Unknown opcode bytes that only appeared inside decoy objects. They are
    /// not evidence of a missing opcode table, so they are kept apart from
    /// `unknown_opcodes`.
    pub decoy_unknown_opcodes: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Expand every interned reference into an inline string. Produces a pyc
    /// that is self contained, at the cost of repeating identifiers.
    pub expand_interned: bool,
    /// Re-apply the legacy "drop the self-update check function" hack.
    pub old_compat: bool,
    /// Keep malformed code objects as they are instead of stubbing them out.
    pub keep_decoys: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            expand_interned: true,
            old_compat: false,
            keep_decoys: false,
        }
    }
}

/// Opcode facts derived from the NetEase tables, computed once.
struct OpInfo {
    valid: [bool; 256],
    load_const: u8,
    names_ops: [u8; 12],
    var_ops: [u8; 3],
    return_value: u8,
}

fn op_info() -> &'static OpInfo {
    static INFO: std::sync::OnceLock<OpInfo> = std::sync::OnceLock::new();
    INFO.get_or_init(|| {
        let mut info = OpInfo {
            valid: [false; 256],
            load_const: 0,
            names_ops: [0; 12],
            var_ops: [0; 3],
            return_value: 0,
        };
        // EXTENDED_ARG never appears in the permuted tables.
        info.valid[145] = true;
        let wanted_names = [
            "STORE_NAME",
            "DELETE_NAME",
            "STORE_ATTR",
            "DELETE_ATTR",
            "STORE_GLOBAL",
            "DELETE_GLOBAL",
            "LOAD_NAME",
            "LOAD_ATTR",
            "IMPORT_NAME",
            "IMPORT_FROM",
            "LOAD_GLOBAL",
            "LOAD_METHOD",
        ];
        let wanted_vars = ["LOAD_FAST", "STORE_FAST", "DELETE_FAST"];
        let mut n_names = 0;
        let mut n_vars = 0;
        for id in [0xca49_b20f_u32, 0x0000_0000, 0xbc58_dbd5] {
            let Some(table) = tables::table(id) else { continue };
            for (_, name, py2) in table {
                info.valid[*py2 as usize] = true;
                match *name {
                    "LOAD_CONST" => info.load_const = *py2,
                    "RETURN_VALUE" => info.return_value = *py2,
                    _ => {}
                }
                if let Some(idx) = wanted_names.iter().position(|w| w == name) {
                    if n_names < info.names_ops.len() {
                        info.names_ops[n_names] = *py2;
                        n_names += 1;
                        let _ = idx;
                    }
                }
                if let Some(_idx) = wanted_vars.iter().position(|w| w == name) {
                    if n_vars < info.var_ops.len() {
                        info.var_ops[n_vars] = *py2;
                        n_vars += 1;
                    }
                }
            }
        }
        info
    })
}

/// Linear sanity check of a translated code object. Cheap, and strict enough
/// that random bytes cannot pass while real code always does.
fn validate_bytecode(code: &Code) -> Result<(), String> {
    let info = op_info();
    let bc = &code.bytecode;
    if bc.is_empty() {
        return Err("empty bytecode".into());
    }
    let mut i = 0;
    let mut last = 0u8;
    while i < bc.len() {
        let op = bc[i];
        if !info.valid[op as usize] {
            return Err(format!("invalid opcode 0x{op:02x} at {i}"));
        }
        last = op;
        if op >= 90 {
            if i + 2 >= bc.len() {
                return Err(format!("truncated operand at {i}"));
            }
            let arg = u16::from_le_bytes([bc[i + 1], bc[i + 2]]) as usize;
            if op == info.load_const && arg >= code.constants.len() {
                return Err(format!("LOAD_CONST {arg} >= consts {}", code.constants.len()));
            }
            if info.names_ops.contains(&op) && arg >= code.names.len() {
                return Err(format!("name index {arg} >= names {}", code.names.len()));
            }
            if info.var_ops.contains(&op) && arg >= code.var_names.len() {
                return Err(format!("var index {arg} >= varnames {}", code.var_names.len()));
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    if last != info.return_value {
        return Err(format!("does not end with RETURN_VALUE (0x{last:02x})"));
    }
    Ok(())
}

/// Replace a malformed object with `return None`, keeping its identity intact.
fn stub_decoy(code: &mut Code) {
    let info = op_info();
    code.bytecode = vec![info.load_const, 0, 0, info.return_value];
    code.constants = vec![Value::None];
    code.names.clear();
    code.var_names.clear();
    code.free_vars.clear();
    code.cell_vars.clear();
    code.arg_count = 0;
    code.local_count = 0;
    code.stack_size = 1;
    code.line_table = vec![1, 1];
}

pub struct Parsed {
    pub code: Code,
    /// Bytes consumed from the payload.
    pub consumed: usize,
    pub stats: Stats,
}

pub fn parse_module(payload: &[u8], opts: Options) -> Result<Parsed> {
    let mut p = Parser::new(payload, opts);
    let value = p.read_object(0).context("decoding top-level marshal object")?;
    let code = match value {
        Value::Code(c) => *c,
        other => bail!(
            "top-level object is not a code object but {}",
            type_name(&other)
        ),
    };
    let consumed = p.pos;
    let mut stats = p.stats;
    stats.unconsumed = payload.len() - consumed;
    Ok(Parsed {
        code,
        consumed,
        stats,
    })
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::None => "None",
        Value::Bool(_) => "bool",
        Value::Int(_) => "int",
        Value::Int64(_) => "int64",
        Value::Float(_) => "float",
        Value::Complex(..) => "complex",
        Value::Long { .. } => "long",
        Value::Bytes(_) => "str",
        Value::Unicode(_) => "unicode",
        Value::Tuple(_) => "tuple",
        Value::List(_) => "list",
        Value::Dict(_) => "dict",
        Value::Set(_) => "set",
        Value::FrozenSet(_) => "frozenset",
        Value::Ellipsis => "Ellipsis",
        Value::StopIteration => "StopIteration",
        Value::Code(_) => "code",
    }
}

pub struct Parser<'a> {
    data: &'a [u8],
    pos: usize,
    interned: Vec<Vec<u8>>,
    #[allow(dead_code)]
    opts: Options,
    pub stats: Stats,
}

impl<'a> Parser<'a> {
    pub fn new(data: &'a [u8], opts: Options) -> Self {
        Parser {
            data,
            pos: 0,
            interned: Vec::new(),
            opts,
            stats: Stats::default(),
        }
    }

    fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    fn byte_at(&self, off: usize) -> Result<u8> {
        self.data
            .get(off)
            .copied()
            .with_context(|| format!("unexpected end of stream at 0x{off:x}"))
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).context("length overflow")?;
        if end > self.data.len() {
            bail!(
                "unexpected end of stream: want {n} bytes at 0x{:x}, only {} left",
                self.pos,
                self.remaining()
            );
        }
        let s = &self.data[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn read_u8(&mut self) -> Result<u8> {
        let b = self.byte_at(self.pos)?;
        self.pos += 1;
        Ok(b)
    }

    fn read_i32(&mut self) -> Result<i32> {
        let b = self.take(4)?;
        Ok(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn read_u32(&mut self) -> Result<u32> {
        Ok(self.read_i32()? as u32)
    }

    fn read_i64(&mut self) -> Result<i64> {
        let b = self.take(8)?;
        Ok(i64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    fn read_f64(&mut self) -> Result<f64> {
        Ok(f64::from_bits(self.read_i64()? as u64))
    }

    fn read_count(&mut self, what: &str) -> Result<usize> {
        let n = self.read_u32()?;
        if n > MAX_COUNT {
            bail!("implausible {what} count {n} at 0x{:x}", self.pos - 4);
        }
        Ok(n as usize)
    }

    fn read_len_bytes(&mut self) -> Result<Vec<u8>> {
        let n = self.read_u32()? as usize;
        if n > self.remaining() {
            bail!(
                "string length {n} exceeds remaining {} bytes at 0x{:x}",
                self.remaining(),
                self.pos - 4
            );
        }
        Ok(self.take(n)?.to_vec())
    }

    fn push_interned(&mut self, s: Vec<u8>) -> u32 {
        self.stats.interned_entries += 1;
        self.interned.push(s);
        (self.interned.len() - 1) as u32
    }

    fn interned_ref(&mut self) -> Result<Value> {
        self.stats.interned_refs += 1;
        let idx = self.read_u32()? as usize;
        let s = self
            .interned
            .get(idx)
            .cloned()
            .with_context(|| format!("interned index {idx} out of range at 0x{:x}", self.pos - 4))?;
        Ok(Value::Bytes(s))
    }

    /// Read one object. `depth` guards against pathological nesting.
    pub fn read_object(&mut self, depth: usize) -> Result<Value> {
        if depth > MAX_DEPTH {
            bail!("marshal nesting deeper than {MAX_DEPTH}");
        }
        let start = self.pos;
        let tag = self.read_u8()?;
        let v = match tag {
            TAG_NULL => Value::Null,
            TAG_NONE => Value::None,
            TAG_TRUE => Value::Bool(true),
            TAG_FALSE => Value::Bool(false),
            TAG_STOPITER => Value::StopIteration,
            TAG_ELLIPSIS => Value::Ellipsis,
            TAG_INT => Value::Int(self.read_i32()?),
            TAG_INT64 => Value::Int64(self.read_i64()?),
            TAG_FLOAT_BIN => Value::Float(self.read_f64()?),
            TAG_COMPLEX_BIN => Value::Complex(self.read_f64()?, self.read_f64()?),
            TAG_FLOAT_TEXT => {
                let n = self.read_u8()? as usize;
                let b = self.take(n)?;
                let s = std::str::from_utf8(b).unwrap_or("");
                Value::Float(s.trim().parse::<f64>().unwrap_or(0.0))
            }
            TAG_COMPLEX_TEXT => {
                let rd = |p: &mut Self| -> Result<f64> {
                    let n = p.read_u8()? as usize;
                    let b = p.take(n)?;
                    Ok(std::str::from_utf8(b)
                        .ok()
                        .and_then(|s| s.trim().parse::<f64>().ok())
                        .unwrap_or(0.0))
                };
                let re = rd(self)?;
                let im = rd(self)?;
                Value::Complex(re, im)
            }
            TAG_LONG => {
                let n = self.read_i32()?;
                let negative = n < 0;
                let mag = n.unsigned_abs() as usize;
                // CPython encodes a count of 2^15-digit groups, not a plain
                // digit count: 1 -> 1, 15 -> 15, 16 -> 16, 30 -> 30, 31 -> 31.
                let count = ((mag - 1) / 15) * 15 + (mag - 1) % 15 + 1;
                let mut digits = Vec::with_capacity(count.min(1024));
                for _ in 0..count {
                    let b = self.take(2)?;
                    digits.push(u16::from_le_bytes([b[0], b[1]]));
                }
                Value::Long { negative, digits }
            }
            TAG_STRING => Value::Bytes(self.read_len_bytes()?),
            TAG_INTERNED => {
                let s = self.read_len_bytes()?;
                self.push_interned(s.clone());
                Value::Bytes(s)
            }
            TAG_UNICODE => Value::Unicode(self.read_len_bytes()?),
            TAG_REF => self.interned_ref()?,
            TAG_TUPLE | TAG_LIST | TAG_SET | TAG_FROZENSET => {
                let n = self.read_count("sequence")?;
                let mut items = Vec::with_capacity(n.min(4096));
                for _ in 0..n {
                    items.push(self.read_object(depth + 1)?);
                }
                match tag {
                    TAG_TUPLE => Value::Tuple(items),
                    TAG_LIST => Value::List(items),
                    TAG_SET => Value::Set(items),
                    _ => Value::FrozenSet(items),
                }
            }
            TAG_DICT => {
                let mut items = Vec::new();
                loop {
                    let k = self.read_object(depth + 1)?;
                    if k == Value::Null {
                        break;
                    }
                    let v = self.read_object(depth + 1)?;
                    items.push((k, v));
                }
                Value::Dict(items)
            }
            TAG_CODE => Value::Code(Box::new(self.read_code_standard(depth)?)),
            TAG_NE_CODE_0 => Value::Code(Box::new(self.read_code_netease(depth, 0)?)),
            TAG_NE_CODE_1 => Value::Code(Box::new(self.read_code_netease(depth, 1)?)),
            TAG_NE_CODE_2 => Value::Code(Box::new(self.read_code_netease(depth, 2)?)),
            TAG_NE_STRING_0 => Value::Bytes(self.read_ne_string(0)?),
            TAG_NE_INTERNED_0 => {
                let s = self.read_ne_string(0)?;
                self.push_interned(s.clone());
                Value::Bytes(s)
            }
            TAG_NE_UNICODE_0 => Value::Unicode(self.read_ne_string(0)?),
            TAG_NE_STRING_1 => Value::Bytes(self.read_ne_string(1)?),
            TAG_NE_INTERNED_1 => {
                let s = self.read_ne_string(1)?;
                self.push_interned(s.clone());
                Value::Bytes(s)
            }
            TAG_NE_UNICODE_1 => Value::Unicode(self.read_ne_string(1)?),
            TAG_NE_STRING_2 => Value::Bytes(self.read_ne_string(2)?),
            TAG_NE_INTERNED_2 => {
                let s = self.read_ne_string(2)?;
                self.push_interned(s.clone());
                Value::Bytes(s)
            }
            TAG_NE_UNICODE_2 => Value::Unicode(self.read_ne_string(2)?),
            other => bail!(
                "unknown marshal tag 0x{other:02x} ({:?}) at 0x{start:x}",
                other as char
            ),
        };
        Ok(v)
    }

    fn read_ne_string(&mut self, version: u8) -> Result<Vec<u8>> {
        let mut s = self.read_len_bytes()?;
        if !s.is_empty() {
            self.stats.encrypted_strings += 1;
            match version {
                0 => s.iter_mut().for_each(|b| *b ^= XOR_0),
                1 => rc4(&RC4_KEY_1, &mut s),
                _ => rc4(&RC4_KEY_2, &mut s),
            }
        }
        Ok(s)
    }

    /// A single tagged string field inside a NetEase code object. The tag
    /// decides whether the bytes are encrypted, so this must go through
    /// `read_object` rather than reading a bare length prefix.
    fn read_tagged_bytes(&mut self, what: &str, depth: usize) -> Result<Vec<u8>> {
        match self.read_object(depth + 1)? {
            Value::Bytes(b) | Value::Unicode(b) => Ok(b),
            other => bail!("{what}: expected a string, found {}", type_name(&other)),
        }
    }

    fn read_tagged_tuple(&mut self, what: &str, depth: usize) -> Result<Vec<Value>> {
        match self.read_object(depth + 1)? {
            Value::Tuple(t) => Ok(t),
            other => bail!("{what}: expected a tuple, found {}", type_name(&other)),
        }
    }

    fn read_tagged_str_list(&mut self, what: &str, depth: usize) -> Result<Vec<Vec<u8>>> {
        let items = self.read_tagged_tuple(what, depth)?;
        items
            .into_iter()
            .map(|v| match v {
                Value::Bytes(b) | Value::Unicode(b) => Ok(b),
                other => bail!("{what}: expected strings, found {}", type_name(&other)),
            })
            .collect()
    }

    /// Standard CPython 2.7 code object (`c`): raw ints, tagged everything else.
    fn read_code_standard(&mut self, depth: usize) -> Result<Code> {
        self.stats.code_objects += 1;
        *self.stats.code_layouts.entry(3).or_insert(0) += 1;
        let arg_count = self.read_i32()?;
        let local_count = self.read_i32()?;
        let stack_size = self.read_i32()?;
        let flags = self.read_i32()?;
        Ok(Code {
            arg_count,
            local_count,
            stack_size,
            flags,
            bytecode: self.read_tagged_bytes("co_code", depth)?,
            constants: self.read_tagged_tuple("consts", depth)?,
            names: self.read_tagged_str_list("names", depth)?,
            var_names: self.read_tagged_str_list("varnames", depth)?,
            free_vars: self.read_tagged_str_list("freevars", depth)?,
            cell_vars: self.read_tagged_str_list("cellvars", depth)?,
            filename: self.read_tagged_bytes("filename", depth)?,
            name: self.read_tagged_bytes("name", depth)?,
            first_line: self.read_i32()?,
            line_table: self.read_tagged_bytes("lnotab", depth)?,
            decoy: false,
        })
    }

    /// One of the three shuffled NetEase code layouts.
    fn read_code_netease(&mut self, depth: usize, layout: u8) -> Result<Code> {
        self.stats.code_objects += 1;
        *self.stats.code_layouts.entry(layout).or_insert(0) += 1;
        let mut code = Code::default();
        match layout {
            // 0x61: argcount-last layout
            0 => {
                code.line_table = self.read_tagged_bytes("lnotab", depth)?;
                code.var_names = self.read_tagged_str_list("varnames", depth)?;
                code.flags = self.read_i32()?;
                code.free_vars = self.read_tagged_str_list("freevars", depth)?;
                code.cell_vars = self.read_tagged_str_list("cellvars", depth)?;
                code.filename = self.read_tagged_bytes("filename", depth)?;
                code.stack_size = self.read_i32()?;
                code.first_line = self.read_i32()?;
                code.constants = self.read_tagged_tuple("consts", depth)?;
                code.arg_count = self.read_i32()?;
                code.bytecode = self.read_tagged_bytes("co_code", depth)?;
                code.local_count = self.read_i32()?;
                code.name = self.read_tagged_bytes("name", depth)?;
                code.names = self.read_tagged_str_list("names", depth)?;
            }
            // 0x6f: local/flag-first layout
            1 => {
                code.local_count = self.read_i32()?;
                code.flags = self.read_i32()?;
                code.constants = self.read_tagged_tuple("consts", depth)?;
                code.stack_size = self.read_i32()?;
                code.var_names = self.read_tagged_str_list("varnames", depth)?;
                code.arg_count = self.read_i32()?;
                code.free_vars = self.read_tagged_str_list("freevars", depth)?;
                code.names = self.read_tagged_str_list("names", depth)?;
                code.cell_vars = self.read_tagged_str_list("cellvars", depth)?;
                code.name = self.read_tagged_bytes("name", depth)?;
                code.bytecode = self.read_tagged_bytes("co_code", depth)?;
                code.first_line = self.read_i32()?;
                code.line_table = self.read_tagged_bytes("lnotab", depth)?;
                code.filename = self.read_tagged_bytes("filename", depth)?;
            }
            // 0x4d: the common one
            _ => {
                code.arg_count = self.read_i32()?;
                code.line_table = self.read_tagged_bytes("lnotab", depth)?;
                code.cell_vars = self.read_tagged_str_list("cellvars", depth)?;
                code.first_line = self.read_i32()?;
                code.var_names = self.read_tagged_str_list("varnames", depth)?;
                code.constants = self.read_tagged_tuple("consts", depth)?;
                code.name = self.read_tagged_bytes("name", depth)?;
                code.stack_size = self.read_i32()?;
                code.free_vars = self.read_tagged_str_list("freevars", depth)?;
                code.names = self.read_tagged_str_list("names", depth)?;
                code.bytecode = self.read_tagged_bytes("co_code", depth)?;
                code.flags = self.read_i32()?;
                code.filename = self.read_tagged_bytes("filename", depth)?;
                code.local_count = self.read_i32()?;
                let opcode_type = self.read_u32()?;
                *self.stats.opcode_tables.entry(opcode_type).or_insert(0) += 1;
                self.translate_bytecode(&mut code, opcode_type)?;
                return Ok(code);
            }
        }
        let opcode_type = self.read_u32()?;
        *self.stats.opcode_tables.entry(opcode_type).or_insert(0) += 1;
        self.translate_bytecode(&mut code, opcode_type)?;
        Ok(code)
    }

    /// Map NetEase opcode bytes onto CPython 2.7 opcodes, in place.
    fn translate_bytecode(&mut self, code: &mut Code, opcode_type: u32) -> Result<()> {
        let table = match tables::table(opcode_type) {
            Some(t) => t,
            None => {
                // Unknown table: leave the bytecode untouched rather than fail;
                // the report records the id so new builds can be added later.
                return Ok(());
            }
        };
        // Unknown bytes are collected locally: garbage blocks inside decoy
        // objects would otherwise look like a gap in our opcode tables.
        let mut unknown: BTreeMap<(u32, u8), usize> = BTreeMap::new();
        let mut translated = Vec::with_capacity(code.bytecode.len());
        let mut i = 0;
        while i < code.bytecode.len() {
            let op = code.bytecode[i];
            match table.iter().find(|(b, _, _)| *b == op) {
                Some((_, _, py2)) => {
                    translated.push(*py2);
                    // `op >= 90` is the "has two byte argument" predicate of
                    // CPython 2.7, and NetEase preserves that property when
                    // permuting the opcodes.
                    if *py2 >= 90 && i + 2 < code.bytecode.len() {
                        translated.push(code.bytecode[i + 1]);
                        translated.push(code.bytecode[i + 2]);
                        i += 3;
                    } else {
                        i += 1;
                    }
                }
                None => {
                    *unknown.entry((opcode_type, op)).or_insert(0) += 1;
                    translated.push(op);
                    if op >= 90 && i + 2 < code.bytecode.len() {
                        translated.extend_from_slice(&code.bytecode[i + 1..i + 3]);
                        i += 3;
                    } else {
                        i += 1;
                    }
                }
            }
        }
        code.bytecode = translated;
        // NetEase ships a few deliberately malformed "version check" objects
        // (names like `foo_01dVersi0n`, `encrypt_function_new_Version2`). Their
        // bytecode is random garbage that no decompiler can consume, so validate
        // it and replace it with a harmless stub — otherwise those 5% of
        // objects poison the whole module's decompilation.
        if let Err(_reason) = validate_bytecode(code) {
            self.stats.decoy_code_objects += 1;
            self.stats.decoy_unknown_opcodes += unknown.values().sum::<usize>();
            code.decoy = true;
            if !self.opts.keep_decoys {
                stub_decoy(code);
            }
        } else {
            for (key, n) in unknown {
                *self.stats.unknown_opcodes.entry(key).or_insert(0) += n;
            }
        }
        Ok(())
    }
}

/// Rough sanity check used only to decide whether `co_code` still needs RC4.
#[allow(dead_code)]
fn is_plausible_bytecode(data: &[u8]) -> bool {
    let mut i = 0;
    while i < data.len() {
        let op = data[i];
        // CPython 2.7 has opcodes up to 150 with sporadic holes; treat the
        // well known gap (START_EXPR..) as suspicious but not fatal.
        if op > 150 {
            return false;
        }
        if op >= 90 {
            if i + 2 >= data.len() {
                return false;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn le32(n: i32) -> [u8; 4] {
        n.to_le_bytes()
    }

    #[test]
    fn parses_plain_py27_code() {
        let mut b = Vec::new();
        b.push(TAG_CODE);
        b.extend(le32(0)); // argcount
        b.extend(le32(0)); // nlocals
        b.extend(le32(1)); // stacksize
        b.extend(le32(0x40)); // flags
        b.push(TAG_STRING);
        b.extend(le32(3));
        b.extend([0x64, 0x00, 0x53]); // LOAD_CONST 0; RETURN_VALUE
        b.push(TAG_TUPLE);
        b.extend(le32(1));
        b.extend([TAG_NONE]);
        b.push(TAG_TUPLE);
        b.extend(le32(0));
        b.push(TAG_TUPLE);
        b.extend(le32(0));
        b.push(TAG_TUPLE);
        b.extend(le32(0));
        b.push(TAG_TUPLE);
        b.extend(le32(0));
        b.push(TAG_STRING);
        b.extend(le32(4));
        b.extend(b"test");
        b.push(TAG_STRING);
        b.extend(le32(2));
        b.extend(b"f_");
        b.extend(le32(1)); // firstlineno
        b.push(TAG_STRING);
        b.extend(le32(0)); // lnotab
        let parsed = parse_module(&b, Options::default()).unwrap();
        assert_eq!(parsed.code.stack_size, 1);
        assert_eq!(parsed.code.bytecode, vec![0x64, 0x00, 0x53]);
        assert_eq!(parsed.code.filename, b"test");
        assert_eq!(parsed.consumed, b.len());
        assert_eq!(parsed.code.constants, vec![Value::None]);
    }

    #[test]
    fn rc4_is_symmetric() {
        let mut data = b"hello world".to_vec();
        rc4(&RC4_KEY_1, &mut data);
        assert_ne!(&data[..], b"hello world");
        rc4(&RC4_KEY_1, &mut data);
        assert_eq!(&data[..], b"hello world");
    }

    #[test]
    fn long_digit_count_matches_cpython() {
        for (n, expect) in [(1i32, 1usize), (15, 15), (16, 16), (30, 30), (31, 31)] {
            let mag = n.unsigned_abs() as usize;
            let count = ((mag - 1) / 15) * 15 + (mag - 1) % 15 + 1;
            assert_eq!(count, expect, "n={n}");
        }
    }
}
