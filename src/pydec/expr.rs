//! Expression trees and precedence-correct rendering.
//!
//! The stack machine in [`super::stack`] builds these from bytecode; rendering
//! here is the only place that knows about parentheses, so the structuring code
//! never has to.

use crate::nemarshal::Value;

use super::disasm::{escape_bytes, escape_str};

/// Which bracket a comprehension is written with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompKind {
    List,
    Set,
    Dict,
    Gen,
}

/// A comprehension or generator expression: `[e for t in it if c]`.
#[derive(Clone, Debug, PartialEq)]
pub struct Comp {
    pub kind: CompKind,
    pub element: Expr,
    pub clauses: Vec<Clause>,
}

/// One `for <target> in <iter>` of a comprehension, with its own filters.
/// `[x for a in b for c in d if e]` has two of them, in source order.
#[derive(Clone, Debug, PartialEq)]
pub struct Clause {
    pub target: Expr,
    pub iter: Expr,
    pub ifs: Vec<Expr>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Const(Value),
    /// Any name load: local, global, name, closure cell.
    Name(String),
    Attribute(Box<Expr>, String),
    Subscript(Box<Expr>, Box<Expr>),
    Slice(
        Box<Expr>,
        Option<Box<Expr>>,
        Option<Box<Expr>>,
        Option<Box<Expr>>,
    ),
    Call {
        func: Box<Expr>,
        args: Vec<Expr>,
        star: Option<Box<Expr>>,
        kwargs: Vec<(Option<String>, Expr)>,
        star_star: Option<Box<Expr>>,
    },
    BinOp(&'static str, Box<Expr>, Box<Expr>),
    Unary(&'static str, Box<Expr>),
    /// `not x` needs a space, so it is its own variant rather than `Unary`.
    Not(Box<Expr>),
    /// A chain such as `a < b < c`: operands and the operators between them.
    Compare(Vec<Expr>, Vec<&'static str>),
    /// And/or chain, flattened.
    Bool(&'static str, Vec<Expr>),
    Tuple(Vec<Expr>),
    List(Vec<Expr>),
    Set(Vec<Expr>),
    Dict(Vec<(Expr, Expr)>),
    Lambda(Box<super::build::Deferred>),
    /// An `IMPORT_NAME` result: the dotted module name plus the attributes the
    /// compiler loads to reach a submodule (`import a.b.c as x`).
    ImportModule {
        module: String,
        attrs: Vec<String>,
    },
    /// Backtick repr of 2.7.
    ReprExpr(Box<Expr>),
    Yield(Option<Box<Expr>>),
    Ternary(Box<Expr>, Box<Expr>, Box<Expr>),
    Comp(Box<Comp>),
    /// Something the stack machine could not model; renders as a placeholder
    /// comment-bearing expression so output stays valid Python.
    Unknown(String),
}

/// Binding power, lowest to highest, following Python 2.7's grammar.
mod prec {
    pub const LAMBDA: u8 = 1;
    pub const TERNARY: u8 = 2;
    pub const OR: u8 = 3;
    pub const AND: u8 = 4;
    pub const NOT: u8 = 5;
    pub const COMPARE: u8 = 6;
    pub const BOR: u8 = 7;
    pub const BXOR: u8 = 8;
    pub const BAND: u8 = 9;
    pub const SHIFT: u8 = 10;
    pub const ARITH: u8 = 11;
    pub const TERM: u8 = 12;
    pub const UNARY: u8 = 13;
    pub const POWER: u8 = 14;
    pub const PRIMARY: u8 = 15;
    pub const ATOM: u8 = 16;
    /// Comma level: tuple items, call arguments, subscripts.
    pub const TUPLE: u8 = 0;
}

impl Expr {
    pub fn precedence(&self) -> u8 {
        match self {
            Expr::Lambda(_) => prec::LAMBDA,
            Expr::Ternary(..) => prec::TERNARY,
            Expr::Bool("or", _) => prec::OR,
            Expr::Bool(..) => prec::AND,
            Expr::Not(_) => prec::NOT,
            Expr::Compare(..) => prec::COMPARE,
            Expr::BinOp(op, ..) => match *op {
                "|" => prec::BOR,
                "^" => prec::BXOR,
                "&" => prec::BAND,
                "<<" | ">>" => prec::SHIFT,
                "+" | "-" => prec::ARITH,
                "*" | "/" | "//" | "%" => prec::TERM,
                "**" => prec::POWER,
                _ => prec::TERM,
            },
            Expr::Unary(..) | Expr::ReprExpr(_) => prec::UNARY,
            Expr::Yield(_) => prec::ATOM,
            Expr::Attribute(..) | Expr::Subscript(..) | Expr::Slice(..) | Expr::Call { .. } => {
                prec::PRIMARY
            }
            Expr::ImportModule { .. } => prec::PRIMARY,
            Expr::Comp(comp) => match comp.kind {
                CompKind::Gen => prec::ATOM,
                _ => prec::ATOM,
            },
            Expr::Const(_)
            | Expr::Name(_)
            | Expr::Tuple(_)
            | Expr::List(_)
            | Expr::Set(_)
            | Expr::Dict(_)
            | Expr::Unknown(_) => prec::ATOM,
        }
    }

    /// Render, wrapping this expression in parentheses when it binds looser
    /// than `min`.
    pub fn write(&self, out: &mut String, min: u8) {
        if self.precedence() < min {
            out.push('(');
            self.write_inner(out, prec::TUPLE);
            out.push(')');
        } else {
            self.write_inner(out, min);
        }
    }

    fn write_inner(&self, out: &mut String, _min: u8) {
        match self {
            Expr::Comp(comp) => {
                let (open, close) = match comp.kind {
                    CompKind::List => ('[', ']'),
                    CompKind::Set => ('{', '}'),
                    CompKind::Dict => ('{', '}'),
                    CompKind::Gen => ('(', ')'),
                };
                out.push(open);
                match comp.kind {
                    CompKind::Dict => {
                        if let Expr::Tuple(pair) = &comp.element {
                            if pair.len() == 2 {
                                pair[0].write(out, prec::TUPLE);
                                out.push_str(": ");
                                pair[1].write(out, prec::TUPLE);
                            } else {
                                comp.element.write(out, prec::TUPLE);
                            }
                        } else {
                            comp.element.write(out, prec::TUPLE);
                        }
                    }
                    _ => comp.element.write(out, prec::TUPLE),
                }
                for clause in &comp.clauses {
                    out.push_str(" for ");
                    clause.target.write(out, prec::TUPLE);
                    out.push_str(" in ");
                    clause.iter.write(out, prec::TUPLE);
                    for condition in &clause.ifs {
                        out.push_str(" if ");
                        condition.write(out, prec::TUPLE);
                    }
                }
                out.push(close);
            }
            Expr::Const(value) => out.push_str(&const_text(value)),
            Expr::Name(name) => out.push_str(name),
            Expr::ImportModule { module, attrs } => {
                out.push_str(module);
                for attr in attrs {
                    out.push('.');
                    out.push_str(attr);
                }
            }
            Expr::Attribute(base, attr) => {
                base.write(out, prec::PRIMARY);
                out.push('.');
                out.push_str(attr);
            }
            Expr::Subscript(base, index) => {
                base.write(out, prec::PRIMARY);
                out.push('[');
                index.write(out, prec::TUPLE);
                out.push(']');
            }
            Expr::Slice(base, lo, hi, step) => {
                base.write(out, prec::PRIMARY);
                out.push('[');
                if let Some(lo) = lo {
                    lo.write(out, prec::TUPLE);
                }
                out.push(':');
                if let Some(hi) = hi {
                    hi.write(out, prec::TUPLE);
                }
                if let Some(step) = step {
                    out.push(':');
                    step.write(out, prec::TUPLE);
                }
                out.push(']');
            }
            Expr::Call {
                func,
                args,
                star,
                kwargs,
                star_star,
            } => {
                func.write(out, prec::PRIMARY);
                out.push('(');
                let mut first = true;
                let sep = |out: &mut String, first: &mut bool| {
                    if !*first {
                        out.push_str(", ");
                    }
                    *first = false;
                };
                for arg in args {
                    sep(out, &mut first);
                    arg.write(out, prec::TERNARY);
                }
                if let Some(star) = star {
                    sep(out, &mut first);
                    out.push('*');
                    star.write(out, prec::TERNARY);
                }
                for (name, value) in kwargs {
                    sep(out, &mut first);
                    match name {
                        Some(name) => {
                            out.push_str(name);
                            out.push('=');
                        }
                        None => out.push_str("**"),
                    }
                    value.write(out, prec::TERNARY);
                }
                if let Some(star_star) = star_star {
                    sep(out, &mut first);
                    out.push_str("**");
                    star_star.write(out, prec::TERNARY);
                }
                out.push(')');
            }
            Expr::BinOp(op, left, right) => {
                if *op == "**" {
                    // Right associative, and its right operand may be unary.
                    left.write(out, prec::PRIMARY);
                    out.push_str("**");
                    right.write(out, prec::UNARY);
                } else {
                    let p = self.precedence();
                    left.write(out, p);
                    out.push(' ');
                    out.push_str(op);
                    out.push(' ');
                    right.write(out, p + 1);
                }
            }
            Expr::Unary(op, operand) => {
                out.push_str(op);
                if *op == "`" {
                    operand.write(out, prec::TUPLE);
                    out.push('`');
                } else {
                    operand.write(out, prec::UNARY);
                }
            }
            Expr::Not(operand) => {
                out.push_str("not ");
                operand.write(out, prec::NOT + 1);
            }
            Expr::Compare(operands, ops) => {
                for (i, operand) in operands.iter().enumerate() {
                    if i > 0 {
                        out.push(' ');
                        out.push_str(ops[i - 1]);
                        out.push(' ');
                    }
                    operand.write(out, prec::COMPARE + 1);
                }
            }
            Expr::Bool(op, parts) => {
                let p = self.precedence();
                for (i, part) in parts.iter().enumerate() {
                    if i > 0 {
                        out.push(' ');
                        out.push_str(op);
                        out.push(' ');
                    }
                    part.write(out, p + 1);
                }
            }
            Expr::Tuple(items) => {
                out.push('(');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    item.write(out, prec::TERNARY);
                }
                if items.len() == 1 {
                    out.push(',');
                }
                out.push(')');
            }
            Expr::List(items) | Expr::Set(items) => {
                if matches!(self, Expr::Set(_)) {
                    out.push_str("set([");
                } else {
                    out.push('[');
                }
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    item.write(out, prec::TERNARY);
                }
                if matches!(self, Expr::Set(_)) {
                    out.push_str("])");
                } else {
                    out.push(']');
                }
            }
            Expr::Dict(pairs) => {
                out.push('{');
                for (i, (key, value)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    key.write(out, prec::TERNARY);
                    out.push_str(": ");
                    value.write(out, prec::TERNARY);
                }
                out.push('}');
            }
            Expr::Lambda(deferred) => deferred.write_lambda(out),
            Expr::ReprExpr(inner) => {
                out.push('`');
                inner.write(out, prec::TUPLE);
                out.push('`');
            }
            Expr::Yield(value) => {
                out.push_str("yield");
                if let Some(value) = value {
                    out.push(' ');
                    value.write(out, prec::TERNARY);
                }
            }
            Expr::Ternary(then, cond, otherwise) => {
                then.write(out, prec::OR);
                out.push_str(" if ");
                cond.write(out, prec::OR);
                out.push_str(" else ");
                otherwise.write(out, prec::TERNARY);
            }
            Expr::Unknown(text) => out.push_str(text),
        }
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, prec::TUPLE);
        out
    }

    /// A tuple of the given items, or a single item unwrapped when it is the
    /// only one (used for call arguments built from `BUILD_TUPLE`).
    pub fn tuple_or_single(items: Vec<Expr>) -> Expr {
        Expr::Tuple(items)
    }

    pub fn as_const(&self) -> Option<&Value> {
        match self {
            Expr::Const(value) => Some(value),
            _ => None,
        }
    }
}

pub fn const_text(value: &Value) -> String {
    match value {
        Value::Null | Value::None => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Int(v) => v.to_string(),
        Value::Int64(v) => v.to_string(),
        Value::Float(v) => {
            if v.is_finite() && v.fract() == 0.0 && v.abs() < 1e16 {
                format!("{v:.1}")
            } else {
                format!("{v:?}")
            }
        }
        Value::Complex(re, im) => {
            let re_text = if *re == 0.0 {
                String::new()
            } else if *re == re.trunc() {
                format!("{re:.1}")
            } else {
                format!("{re:?}")
            };
            let im_text = if *im == 0.0 && !re_text.is_empty() {
                String::new()
            } else if *im == im.trunc() {
                format!("{im:.1}")
            } else {
                format!("{im:?}")
            };
            if im_text.is_empty() {
                format!("({re_text}+0j)")
            } else {
                format!("({re_text}+{im_text}j)")
            }
        }
        Value::Long {
            negative,
            digits,
        } => {
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
            if *negative {
                format!("-{digits_text}L")
            } else {
                format!("{digits_text}L")
            }
        }
        Value::Bytes(bytes) => escape_bytes(bytes),
        Value::Unicode(bytes) => format!("u\"{}\"", escape_str(&super::disasm::text(bytes))),
        Value::Tuple(items) => {
            let inner: Vec<String> = items.iter().map(const_text).collect();
            if inner.len() == 1 {
                format!("({},)", inner[0])
            } else {
                format!("({})", inner.join(", "))
            }
        }
        Value::List(items) => {
            let inner: Vec<String> = items.iter().map(const_text).collect();
            format!("[{}]", inner.join(", "))
        }
        Value::Dict(pairs) => {
            let inner: Vec<String> = pairs
                .iter()
                .map(|(k, v)| format!("{}: {}", const_text(k), const_text(v)))
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
        Value::Set(items) | Value::FrozenSet(items) => {
            let inner: Vec<String> = items.iter().map(const_text).collect();
            format!("set([{}])", inner.join(", "))
        }
        Value::Ellipsis => "...".to_string(),
        Value::StopIteration => "StopIteration".to_string(),
        Value::Code(_) => "<code>".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> Expr {
        Expr::Name(s.into())
    }

    fn bin(op: &'static str, l: Expr, r: Expr) -> Expr {
        Expr::BinOp(op, Box::new(l), Box::new(r))
    }

    #[test]
    fn respects_precedence() {
        // (a + b) * c needs parentheses, a + b * c does not.
        let tree = bin("*", bin("+", name("a"), name("b")), name("c"));
        assert_eq!(tree.render(), "(a + b) * c");
        let tree = bin("+", name("a"), bin("*", name("b"), name("c")));
        assert_eq!(tree.render(), "a + b * c");
    }

    #[test]
    fn power_is_right_associative() {
        let tree = bin("**", name("a"), bin("**", name("b"), name("c")));
        assert_eq!(tree.render(), "a**b**c");
        let tree = bin("**", bin("**", name("a"), name("b")), name("c"));
        assert_eq!(tree.render(), "(a**b)**c");
        let tree = Expr::Unary("-", Box::new(bin("**", name("x"), Expr::Const(Value::Int(2)))));
        assert_eq!(tree.render(), "-x**2");
    }

    #[test]
    fn renders_calls_and_attributes() {
        let call = Expr::Call {
            func: Box::new(Expr::Attribute(Box::new(name("os")), "path".into())),
            args: vec![name("a"), Expr::Const(Value::Int(1))],
            star: None,
            kwargs: vec![(Some("k".into()), name("v"))],
            star_star: None,
        };
        assert_eq!(call.render(), "os.path(a, 1, k=v)");
    }

    #[test]
    fn renders_ternary_and_bool_chains() {
        let ternary = Expr::Ternary(
            Box::new(name("a")),
            Box::new(Expr::Bool("and", vec![name("b"), name("c")])),
            Box::new(name("d")),
        );
        assert_eq!(ternary.render(), "a if b and c else d");
        let or_of_and = Expr::Bool(
            "or",
            vec![Expr::Bool("and", vec![name("a"), name("b")]), name("c")],
        );
        assert_eq!(or_of_and.render(), "a and b or c");
    }

    #[test]
    fn renders_compare_chain() {
        let chain = Expr::Compare(vec![name("a"), name("b"), name("c")], vec!["<", "<="]);
        assert_eq!(chain.render(), "a < b <= c");
    }

    #[test]
    fn renders_singleton_tuple_and_slice() {
        assert_eq!(Expr::Tuple(vec![name("a")]).render(), "(a,)");
        let slice = Expr::Slice(Box::new(name("s")), Some(Box::new(name("a"))), None, None);
        assert_eq!(slice.render(), "s[a:]");
    }

    #[test]
    fn renders_longs_as_decimal() {
        // Limbs are base 2^15: [0, 1] is 32768, [1, 0] is 1.
        let value = Value::Long {
            negative: false,
            digits: vec![0, 1],
        };
        assert_eq!(const_text(&value), "32768L");
        let value = Value::Long {
            negative: true,
            digits: vec![1, 0],
        };
        assert_eq!(const_text(&value), "-1L");
    }
}
