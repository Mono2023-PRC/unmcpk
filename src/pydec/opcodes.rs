//! The CPython 2.7 instruction set.
//!
//! After [`crate::nemarshal`] has rewritten the permuted opcode bytes, the
//! bytecode is exactly CPython 2.7 bytecode, so this table is the plain 2.7
//! one: 118 defined opcodes, the rest unused.

#[derive(Clone, Copy)]
pub struct OpInfo {
    pub name: &'static str,
    /// 2.7 encodes "has an argument" as `opcode >= HAVE_ARGUMENT` (90), which
    /// this flag mirrors so callers never need the arithmetic.
    pub has_arg: bool,
}

const UNUSED: OpInfo = OpInfo {
    name: "",
    has_arg: false,
};

macro_rules! ops {
    ($($byte:expr => $name:expr),* $(,)?) => {{
        let mut table = [UNUSED; 256];
        $(table[$byte] = OpInfo {
            name: $name,
            has_arg: $byte >= 90,
        };)*
        table
    }};
}

pub static TABLE: [OpInfo; 256] = ops! {
    0 => "STOP_CODE",
    1 => "POP_TOP",
    2 => "ROT_TWO",
    3 => "ROT_THREE",
    4 => "DUP_TOP",
    5 => "ROT_FOUR",
    9 => "NOP",
    10 => "UNARY_POSITIVE",
    11 => "UNARY_NEGATIVE",
    12 => "UNARY_NOT",
    13 => "UNARY_CONVERT",
    15 => "UNARY_INVERT",
    19 => "BINARY_POWER",
    20 => "BINARY_MULTIPLY",
    21 => "BINARY_DIVIDE",
    22 => "BINARY_MODULO",
    23 => "BINARY_ADD",
    24 => "BINARY_SUBTRACT",
    25 => "BINARY_SUBSCR",
    26 => "BINARY_FLOOR_DIVIDE",
    27 => "BINARY_TRUE_DIVIDE",
    28 => "INPLACE_FLOOR_DIVIDE",
    29 => "INPLACE_TRUE_DIVIDE",
    30 => "SLICE+0",
    31 => "SLICE+1",
    32 => "SLICE+2",
    33 => "SLICE+3",
    40 => "STORE_SLICE+0",
    41 => "STORE_SLICE+1",
    42 => "STORE_SLICE+2",
    43 => "STORE_SLICE+3",
    44 => "DELETE_SLICE+0",
    45 => "DELETE_SLICE+1",
    46 => "DELETE_SLICE+2",
    47 => "DELETE_SLICE+3",
    54 => "STORE_MAP",
    55 => "INPLACE_ADD",
    56 => "INPLACE_SUBTRACT",
    57 => "INPLACE_MULTIPLY",
    58 => "INPLACE_DIVIDE",
    59 => "INPLACE_MODULO",
    60 => "STORE_SUBSCR",
    61 => "DELETE_SUBSCR",
    62 => "BINARY_LSHIFT",
    63 => "BINARY_RSHIFT",
    64 => "BINARY_AND",
    65 => "BINARY_XOR",
    66 => "BINARY_OR",
    67 => "INPLACE_POWER",
    68 => "GET_ITER",
    70 => "PRINT_EXPR",
    71 => "PRINT_ITEM",
    72 => "PRINT_NEWLINE",
    73 => "PRINT_ITEM_TO",
    74 => "PRINT_NEWLINE_TO",
    75 => "INPLACE_LSHIFT",
    76 => "INPLACE_RSHIFT",
    77 => "INPLACE_AND",
    78 => "INPLACE_XOR",
    79 => "INPLACE_OR",
    80 => "BREAK_LOOP",
    81 => "WITH_CLEANUP",
    82 => "LOAD_LOCALS",
    83 => "RETURN_VALUE",
    84 => "IMPORT_STAR",
    85 => "EXEC_STMT",
    86 => "YIELD_VALUE",
    87 => "POP_BLOCK",
    88 => "END_FINALLY",
    89 => "BUILD_CLASS",
    90 => "STORE_NAME",
    91 => "DELETE_NAME",
    92 => "UNPACK_SEQUENCE",
    93 => "FOR_ITER",
    94 => "LIST_APPEND",
    95 => "STORE_ATTR",
    96 => "DELETE_ATTR",
    97 => "STORE_GLOBAL",
    98 => "DELETE_GLOBAL",
    99 => "DUP_TOPX",
    100 => "LOAD_CONST",
    101 => "LOAD_NAME",
    102 => "BUILD_TUPLE",
    103 => "BUILD_LIST",
    104 => "BUILD_SET",
    105 => "BUILD_MAP",
    106 => "LOAD_ATTR",
    107 => "COMPARE_OP",
    108 => "IMPORT_NAME",
    109 => "IMPORT_FROM",
    110 => "JUMP_FORWARD",
    111 => "JUMP_IF_FALSE_OR_POP",
    112 => "JUMP_IF_TRUE_OR_POP",
    113 => "JUMP_ABSOLUTE",
    114 => "POP_JUMP_IF_FALSE",
    115 => "POP_JUMP_IF_TRUE",
    116 => "LOAD_GLOBAL",
    119 => "CONTINUE_LOOP",
    120 => "SETUP_LOOP",
    121 => "SETUP_EXCEPT",
    122 => "SETUP_FINALLY",
    124 => "LOAD_FAST",
    125 => "STORE_FAST",
    126 => "DELETE_FAST",
    130 => "RAISE_VARARGS",
    131 => "CALL_FUNCTION",
    132 => "MAKE_FUNCTION",
    133 => "BUILD_SLICE",
    134 => "MAKE_CLOSURE",
    135 => "LOAD_CLOSURE",
    136 => "LOAD_DEREF",
    137 => "STORE_DEREF",
    140 => "CALL_FUNCTION_VAR",
    141 => "CALL_FUNCTION_KW",
    142 => "CALL_FUNCTION_VAR_KW",
    143 => "SETUP_WITH",
    145 => "EXTENDED_ARG",
    146 => "SET_ADD",
    147 => "MAP_ADD",
};

impl OpInfo {
    pub fn defined(&self) -> bool {
        !self.name.is_empty()
    }
}

pub fn info(op: u8) -> Option<&'static OpInfo> {
    let info = &TABLE[op as usize];
    info.defined().then_some(info)
}

pub fn name(op: u8) -> &'static str {
    let name = TABLE[op as usize].name;
    if name.is_empty() {
        "<invalid>"
    } else {
        name
    }
}

pub fn has_arg(op: u8) -> bool {
    TABLE[op as usize].has_arg
}

/// `COMPARE_OP`'s argument is an index into this table.
pub fn compare_op(arg: u32) -> &'static str {
    const NAMES: [&str; 12] = [
        "<", "<=", "==", "!=", ">", ">=", "in", "not in", "is", "is not", "exception match",
        "BAD",
    ];
    NAMES.get(arg as usize).copied().unwrap_or("BAD")
}

/// Binary operators, keyed by opcode.
pub fn binary_op(op: u8) -> Option<&'static str> {
    const NAMES: [(u8, &str); 13] = [
        (19, "**"),
        (20, "*"),
        (21, "/"),
        (22, "%"),
        (23, "+"),
        (24, "-"),
        (26, "//"),
        (27, "/"),
        (62, "<<"),
        (63, ">>"),
        (64, "&"),
        (65, "^"),
        (66, "|"),
    ];
    NAMES.iter().find(|(o, _)| *o == op).map(|(_, n)| *n)
}

/// In-place operators, keyed by opcode, as the operator without the `=`.
pub fn inplace_op(op: u8) -> Option<&'static str> {
    const NAMES: [(u8, &str); 13] = [
        (55, "+"),
        (56, "-"),
        (57, "*"),
        (58, "/"),
        (59, "%"),
        (67, "**"),
        (75, "<<"),
        (76, ">>"),
        (77, "&"),
        (78, "^"),
        (79, "|"),
        (28, "//"),
        (29, "/"),
    ];
    NAMES.iter().find(|(o, _)| *o == op).map(|(_, n)| *n)
}

pub fn unary_op(op: u8) -> Option<&'static str> {
    const NAMES: [(u8, &str); 5] = [
        (10, "+"),
        (11, "-"),
        (12, "not "),
        (13, "`"),
        (15, "~"),
    ];
    NAMES.iter().find(|(o, _)| *o == op).map(|(_, n)| *n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_matches_python_27() {
        // 0..=147 holds 119 defined opcodes (the 118 the game's table names,
        // plus STOP_CODE), and everything at or above HAVE_ARGUMENT takes an
        // argument.
        let defined = TABLE.iter().filter(|i| i.defined()).count();
        assert_eq!(defined, 119);
        for (byte, info) in TABLE.iter().enumerate() {
            if info.defined() {
                assert_eq!(info.has_arg, byte >= 90, "{}", info.name);
            }
        }
    }

    #[test]
    fn knows_the_common_ones() {
        assert_eq!(name(83), "RETURN_VALUE");
        assert_eq!(name(131), "CALL_FUNCTION");
        assert_eq!(name(113), "JUMP_ABSOLUTE");
        assert!(has_arg(131) && !has_arg(83));
        assert_eq!(compare_op(7), "not in");
        assert_eq!(binary_op(23), Some("+"));
        assert_eq!(inplace_op(55), Some("+"));
        assert_eq!(unary_op(12), Some("not "));
    }
}
