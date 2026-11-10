//! Rebuild statements and expressions from a flat instruction stream.
//!
//! The approach is a linear pass with a symbolic stack, plus lookahead when a
//! jump introduces structure. Anything the pass cannot model makes the whole
//! code object fall back to a disassembly listing, so the output is always valid
//! Python — never half-parsed.

use super::disasm::{disassemble, text as disasm_text, Instr};
use super::expr::{Clause, Comp, CompKind, Expr};
use crate::nemarshal::{Code, Value};

#[derive(Clone, Debug, PartialEq)]
pub struct Deferred {
    pub code: Box<Code>,
    pub defaults: Vec<Expr>,
    pub keyword_defaults: Option<Expr>,
}

impl Deferred {
    pub fn write_lambda(&self, out: &mut String) {
        out.push_str(&super::render_lambda(self));
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Stmt {
    Expr(Expr),
    Assign {
        targets: Vec<Expr>,
        value: Expr,
    },
    AugAssign {
        target: Expr,
        op: &'static str,
        value: Expr,
    },
    Print {
        dest: Option<Expr>,
        items: Vec<Expr>,
        newline: bool,
    },
    Return(Option<Expr>),
    If {
        test: Expr,
        body: Vec<Stmt>,
        orelse: Vec<Stmt>,
    },
    While {
        test: Expr,
        body: Vec<Stmt>,
        orelse: Vec<Stmt>,
    },
    For {
        target: Expr,
        iter: Expr,
        body: Vec<Stmt>,
        orelse: Vec<Stmt>,
    },
    With {
        ctx: Expr,
        target: Option<Expr>,
        body: Vec<Stmt>,
    },
    Try {
        body: Vec<Stmt>,
        handlers: Vec<Handler>,
        orelse: Vec<Stmt>,
        finally: Vec<Stmt>,
    },
    Break,
    Continue,
    Pass,
    Import(Vec<(String, Option<String>)>),
    FromImport {
        module: String,
        level: usize,
        names: Vec<(String, Option<String>)>,
        star: bool,
    },
    FunctionDef(Box<FunctionDef>),
    ClassDef(Box<ClassDef>),
    Raise {
        kind: Option<Expr>,
        value: Option<Expr>,
        traceback: Option<Expr>,
    },
    Assert {
        test: Expr,
        message: Option<Expr>,
    },
    Del(Vec<Expr>),
    Global(Vec<String>),
    Yield(Option<Expr>),
    Comment(String),
    /// A construct this pass does not model yet; emitted as a comment plus the
    /// instructions it covers.
    Raw(Vec<String>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClassDef {
    pub name: String,
    pub bases: Vec<Expr>,
    /// Expressions applied to the class after its body ran (`@deco`).
    pub decorators: Vec<Expr>,
    /// The class body; its statements run in a fresh namespace.
    pub code: Box<Code>,
}

/// One `except` clause: the exception type it matches (None for a bare
/// `except:`), the name it binds, and the statements it runs.
#[derive(Clone, Debug, PartialEq)]
pub struct Handler {
    pub kind: Option<Expr>,
    pub name: Option<Expr>,
    pub body: Vec<Stmt>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FunctionDef {
    /// Set when the definition was recovered from `name = lambda ...`; the code
    /// object's own name wins at render time otherwise.
    pub name: Option<String>,
    pub code: Box<Code>,
    pub defaults: Vec<Expr>,
    pub keyword_defaults: Option<Expr>,
    pub decorators: Vec<Expr>,
}

impl FunctionDef {
    pub fn name(&self) -> String {
        match &self.name {
            Some(name) if name != "<lambda>" => name.clone(),
            _ => self.code.name_str(),
        }
    }
}

/// An import in progress: `IMPORT_NAME` starts one, `IMPORT_FROM` adds names,
/// and the statement finishes at the last store or at a `POP_TOP`.
#[derive(Clone, Debug)]
struct PendingImport {
    #[allow(dead_code)] // kept for symmetry with the bytecode layout
    level: i32,
    names: Vec<(String, Option<String>)>,
    star: bool,
}

#[derive(Clone, Debug)]
enum Item {
    Expr(Expr),
    /// A code object that `LOAD_CONST` pushed and `MAKE_FUNCTION` consumes.
    Code(Box<Code>),
    /// `from m import ...`, with the module left on the stack.
    FromModule {
        module: String,
        level: i32,
    },
    /// The value `IMPORT_FROM` just pushed, waiting for its store.
    FromAttr(String),
    /// A finished `BUILD_CLASS` waiting for its store.
    Class {
        name: String,
        bases: Vec<Expr>,
        decorators: Vec<Expr>,
        code: Box<Code>,
    },
    /// `UNPACK_SEQUENCE` has popped the value and is waiting for its targets.
    Unpack {
        value: Expr,
        targets: Vec<Expr>,
        remaining: usize,
    },
    /// The iterable `GET_ITER` converted, waiting for `FOR_ITER`.
    Iter(Box<Expr>),
    /// Result of `BUILD_SLICE`, waiting for the `BINARY_SUBSCR` that consumes it.
    Slice {
        lo: Option<Expr>,
        hi: Option<Expr>,
        step: Option<Expr>,
    },
}

pub struct Builder<'a> {
    code: &'a Code,
    ins: Vec<Instr>,
    pos: usize,
    stack: Vec<Item>,
    /// Number of duplicated values still to be consumed by stores.
    dup_pending: usize,
    pending_targets: Option<(Vec<Expr>, Expr)>,
    /// Set by an `INPLACE_*` opcode, consumed by the following store.
    pending_aug: Option<(Expr, &'static str, Expr)>,
    printed: Vec<(Option<Expr>, Expr)>,
    /// Destination of the print, when only `PRINT_NEWLINE_TO` carried it.
    print_dest: Option<Expr>,
    print_newline: bool,
    pending_import: Option<PendingImport>,
    /// Offset the builder must not step past (used by sub-builders that only
    /// evaluate an expression range).
    limit: Option<usize>,
    /// Offset of the innermost loop head (FOR_ITER or the while condition).
    loop_head: Option<usize>,
    /// Set when a jump ends the current block early (a `break`, or the jump
    /// that skips the rest of an enclosing `if`).
    terminated: bool,
    exit_target: Option<usize>,
    /// Offset that ends the suite currently being parsed.
    block_stop: usize,
    /// Instructions consumed so far; guards against a structuring loop that
    /// fails to advance.
    steps: usize,
    /// Whether this builder may probe for a conditional expression. Only the
    /// outermost builder does; nesting those probes is exponential.
    probe: bool,
    /// The shared per-object builder allowance ran out.
    over_budget: bool,
    id: usize,
}

thread_local! {
    /// Builders still allowed for the object being decompiled.
    static BUDGET: std::cell::Cell<usize> = const { std::cell::Cell::new(usize::MAX) };
}

/// Start a fresh allowance for one top-level object.
pub fn reset_budget(limit: usize) {
    BUDGET.with(|cell| cell.set(limit));
}

/// Take one builder from the allowance.
fn take_budget() -> Option<()> {
    BUDGET.with(|cell| {
        let left = cell.get();
        if left == 0 {
            None
        } else {
            cell.set(left - 1);
            Some(())
        }
    })
}

fn next_builder_id() -> usize {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

thread_local! {
    /// Nested expression evaluations in flight on this thread. Every guard also
    /// builds a sub-builder, so this bounds the native stack as well.
    static RECURSION: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

const MAX_RECURSION: usize = 48;

/// Bounds expression re-entry: without it a construct the builder cannot model
/// can nest without limit and blow the stack instead of falling back.
struct Recursion;

impl Recursion {
    fn enter() -> Result<Recursion, String> {
        RECURSION.with(|cell| {
            let depth = cell.get();
            if depth >= MAX_RECURSION {
                return Err(String::from("expression nested too deeply"));
            }
            cell.set(depth + 1);
            Ok(Recursion)
        })
    }
}

impl Drop for Recursion {
    fn drop(&mut self) {
        RECURSION.with(|cell| cell.set(cell.get().saturating_sub(1)));
    }
}

/// Statements plus the bytecode span they came from, for diagnostics.
pub struct Built {
    pub stmts: Vec<Stmt>,
    pub consumed: usize,
}

const MAX_DEPTH: usize = 64;

/// Negate a test expression. Comparisons become their opposite operator, which
/// is what the source said; everything else gets a `not`.
fn invert_test(test: Expr) -> Expr {
    // `not not x` collapses to `x`
    if let Expr::Not(inner) = &test {
        return (**inner).clone();
    }
    if let Expr::Compare(operands, ops) = &test {
        if ops.len() == 1 {
            let flipped = match ops[0] {
                "in" => "not in",
                "not in" => "in",
                "is" => "is not",
                "is not" => "is",
                "==" => "!=",
                "!=" => "==",
                "<" => ">=",
                ">=" => "<",
                ">" => "<=",
                "<=" => ">",
                other => other,
            };
            if flipped != ops[0] {
                return Expr::Compare(operands.clone(), vec![flipped]);
            }
        }
    }
    Expr::Not(Box::new(test))
}

/// The text of a string constant, for names the compiler pushes as constants
/// (class names, for example).
fn const_name(value: &Value) -> Option<String> {
    match value {
        Value::Bytes(bytes) | Value::Unicode(bytes) => Some(super::disasm::text(bytes)),
        _ => None,
    }
}

/// Turn the item under a subscript into an index expression: a plain value, a
/// tuple, or the `lo:hi:step` triple `BUILD_SLICE` leaves on the stack.
fn subscr(base: Expr, index: Item) -> Result<Expr, String> {
    match index {
        Item::Expr(Expr::Tuple(items)) => Ok(Expr::Subscript(
            Box::new(base),
            Box::new(Expr::Tuple(items)),
        )),
        Item::Expr(other) => Ok(Expr::Subscript(Box::new(base), Box::new(other))),
        Item::Slice { lo, hi, step } => Ok(Expr::Slice(
            Box::new(base),
            lo.map(Box::new),
            hi.map(Box::new),
            step.map(Box::new),
        )),
        other => Err(format!("expected an index, found {other:?}")),
    }
}

impl<'a> Builder<'a> {
    pub fn new(code: &'a Code) -> Self {
        Self {
            code,
            ins: disassemble(code),
            pos: 0,
            stack: Vec::new(),
            dup_pending: 0,
            pending_targets: None,
            pending_aug: None,
            printed: Vec::new(),
            print_dest: None,
            print_newline: false,
            pending_import: None,
            limit: None,
            loop_head: None,
            terminated: false,
            exit_target: None,
            block_stop: usize::MAX,
            steps: 0,
            probe: true,
            over_budget: take_budget().is_none(),
            id: next_builder_id(),
        }
    }

    pub fn build(mut self) -> Result<Built, String> {
        let stmts = self.parse_suite(None, 0)?;
        if let Some(item) = self.stack.pop() {
            return Err(format!(
                "leftover on stack: {item:?} after {} statements (last: {:?})",
                stmts.len(),
                stmts.last()
            ));
        }
        Ok(Built {
            stmts,
            consumed: self.ins.len(),
        })
    }

    fn offset(&self) -> usize {
        self.ins.get(self.pos).map(|i| i.offset).unwrap_or(usize::MAX)
    }

    fn index_of(&self, offset: usize) -> Option<usize> {
        self.ins.iter().position(|i| i.offset == offset)
    }

    fn cur(&self) -> Option<&Instr> {
        self.ins.get(self.pos)
    }

    fn push(&mut self, expr: Expr) {
        self.stack.push(Item::Expr(expr));
    }

    /// Pop the raw stack item, accepting shapes `pop` rejects (a class under
    /// construction, an iterator, ...).
    fn pop_item(&mut self) -> Result<Item, String> {
        self.stack.pop().ok_or_else(|| {
            format!(
                "stack underflow at {}",
                self.cur().map_or(usize::MAX, |instr| instr.offset)
            )
        })
    }

    fn pop(&mut self) -> Result<Expr, String> {
        match self.stack.pop() {
            Some(Item::Expr(expr)) => Ok(expr),
            Some(Item::Iter(_)) => Err(String::from("iteration value used as an expression")),
            Some(other) => Err(format!("expected a value, found {other:?}")),
            None => Err(format!(
                "stack underflow at {}",
                self.cur().map_or(usize::MAX, |instr| instr.offset)
            )),
        }
    }

    fn pop_opt(&mut self) -> Option<Expr> {
        match self.stack.pop() {
            Some(Item::Expr(expr)) => Some(expr),
            Some(Item::Iter(_)) => None,
            Some(other) => {
                self.stack.push(other);
                None
            }
            None => None,
        }
    }

    // ---------------------------------------------------------------- suites

    fn parse_suite(&mut self, stop: Option<usize>, depth: usize) -> Result<Vec<Stmt>, String> {
        let saved_stop = self.block_stop;
        self.block_stop = stop.unwrap_or(saved_stop);
        let result = self.parse_suite_inner(stop, depth);
        self.block_stop = saved_stop;
        result
    }

    fn parse_suite_inner(
        &mut self,
        stop: Option<usize>,
        depth: usize,
    ) -> Result<Vec<Stmt>, String> {
        if depth > MAX_DEPTH {
            return Err("nesting too deep".into());
        }
        let mut stmts = Vec::new();
        loop {
            let Some(instr) = self.cur().cloned() else { break };
            // Offsets only grow inside a code object, so overshooting the stop
            // offset means the statement just parsed already left the range.
            if stop.map_or(false, |stop| instr.offset >= stop) {
                break;
            }
            if self.over_budget {
                return Err(String::from("decompiler budget exhausted"));
            }
            // After a statement that already ended the flow the compiler may
            // still leave the jump of the branch it optimised away; it is dead
            // code, not a statement.
            if matches!(
                stmts.last(),
                Some(Stmt::Continue | Stmt::Break | Stmt::Return(_))
            ) && instr.is_unconditional()
            {
                self.pos += 1;
                continue;
            }
            self.steps += 1;
            if self.steps > self.ins.len() * 8 + 256 {
                return Err(format!(
                    "instruction budget exhausted at offset {}",
                    instr.offset
                ));
            }
            if std::env::var_os("UNMCPK_TRACE").is_some() {
                eprintln!(
                    "  b{:02} suite dispatch {} {} stack[{}]",
                    self.id,
                    instr.offset,
                    instr.name(),
                    self.stack.len()
                );
            }
            match instr.op {
                // 114 POP_JUMP_IF_FALSE, 115 POP_JUMP_IF_TRUE
                114 | 115 => {
                    // A sub-builder evaluates a range of an expression; a
                    // conditional jump there means the range is not an
                    // expression after all, so let the caller decide.
                    if !self.probe {
                        // The range is not a single expression; stop it here so
                        // the caller can fall back to a statement.
                        break;
                    }
                    if std::env::var_os("UNMCPK_TRACE").is_some() {
                        eprintln!("  b{:02} suite if at {} stack[{}]", self.id, instr.offset, self.stack.len());
                    }
                    // Each of the shapes below pops the test and pushes exactly
                    // one value; if the arms do not balance the stack the shape
                    // was not an expression after all, and the jump belongs to a
                    // statement. Checking the depth keeps one bad guess from
                    // corrupting everything that follows.
                    let stack_depth = self.stack.len();
                    let mark = self.stack.clone();
                    let mark_pos = self.pos;
                    if let Some(stmt) = self.try_ternary_expression()? {
                        if self.stack.len() + 1 == stack_depth {
                            stmts.push(Stmt::Expr(stmt));
                            continue;
                        }
                        self.stack = mark.clone();
                        self.pos = mark_pos;
                    }
                    if !self.stack.is_empty() {
                        if let Some(value) = self.try_shared_tail(&instr)? {
                            if self.stack.len() + 1 == stack_depth {
                                self.stack.push(Item::Expr(value));
                                continue;
                            }
                            self.stack = mark.clone();
                            self.pos = mark_pos;
                        }
                    }
                    if !self.stack.is_empty() {
                        let saved_pos = self.pos;
                        let saved_stack = self.stack.clone();
                        let target_index = match instr.target.and_then(|t| self.index_of(t)) {
                            Some(index) => index,
                            None => {
                                self.pos = saved_pos;
                                self.stack = saved_stack;
                                let stmt = self.parse_if(stop, depth)?;
                                stmts.push(stmt);
                                continue;
                            }
                        };
                        let raw = self.pop()?;
                        let test = if instr.op == 115 { invert_test(raw) } else { raw };
                        if let Some(value) = self.try_branch_value(&test, target_index)? {
                            if self.stack.len() + 1 == stack_depth {
                                self.stack.push(Item::Expr(value));
                                continue;
                            }
                            self.stack = saved_stack.clone();
                            self.pos = saved_pos;
                            let stmt = self.parse_if(stop, depth)?;
                            stmts.push(stmt);
                            continue;
                        }
                        self.pos = saved_pos;
                        self.stack = saved_stack;
                    }
                    let stmt = self.parse_if(stop, depth)?;
                    stmts.push(stmt);
                }
                // 80 BREAK_LOOP ends the suite: nothing after it can run.
                80 => {
                    self.pos += 1;
                    stmts.push(Stmt::Break);
                    break;
                }
                // 119 CONTINUE_LOOP
                119 => {
                    self.pos += 1;
                    stmts.push(Stmt::Continue);
                }
                // 120 SETUP_LOOP
                120 => {
                    let stmt = self.parse_loop(&instr, depth)?;
                    stmts.push(stmt);
                }
                121 => {
                    let stmt = match self.parse_try_except(depth) {
                        Ok(stmt) => stmt,
                        Err(err) => {
                            if std::env::var_os("UNMCPK_TRACE").is_some() {
                                eprintln!("EXC setup at {} failed: {err}", instr.offset);
                            }
                            return Err(err);
                        }
                    };
                    stmts.push(stmt);
                }
                // 122 SETUP_FINALLY
                122 => {
                    let stmt = self.parse_try_finally(depth)?;
                    stmts.push(stmt);
                }
                143 => {
                    let stmt = self.parse_with(depth)?;
                    stmts.push(stmt);
                }
                123 => return Err(format!("unsupported block opcode {}", instr.name())),
                // The compiler merges the two arms of a conditional expression
                // into one store: `x = A if c else B` then arrives here as a
                // statement-level `if` with bare values, followed by the store
                // that has nothing left to consume.
                90 | 97 | 125 | 137 if self.stack.is_empty() => {
                    if let Some(Stmt::If { test, body, orelse }) = stmts.last().cloned() {
                        let arms = match (body.as_slice(), orelse.as_slice()) {
                            ([Stmt::Expr(then)], [Stmt::Expr(other)]) => {
                                Some((then.clone(), other.clone()))
                            }
                            _ => None,
                        };
                        if let Some((then, other)) = arms {
                            let name = match instr.op {
                                125 => self.var_at(instr.arg)?,
                                137 => self.cell_at(instr.arg)?,
                                _ => self.name_at(instr.arg)?,
                            };
                            stmts.pop();
                            stmts.push(Stmt::Assign {
                                targets: vec![Expr::Name(name)],
                                value: Expr::Ternary(
                                    Box::new(then),
                                    Box::new(test),
                                    Box::new(other),
                                ),
                            });
                            self.pos += 1;
                            continue;
                        }
                    }
                    return Err(format!(
                        "store without a value (in {} at {})",
                        instr.name(),
                        instr.offset
                    ));
                }
                _ => {
                    let before = self.pos;
                    let depth_before = self.stack.len();
                    // `terminated` belongs to the block that just ended; a new
                    // statement must not inherit it, or the suite stops early.
                    self.terminated = false;
                    self.exit_target = None;
                    if std::env::var_os("UNMCPK_TRACE").is_some() {
                        eprintln!("  b{:02} stmt? depth={depth_before} at {}", self.id, instr.offset);
                    }
                    let pos_before = self.pos;
                    let produced = self.parse_statement(depth)?;
                    if self.pos < pos_before
                        && std::env::var_os("UNMCPK_TRACE").is_some()
                    {
                        eprintln!(
                            "  b{:02} POS MOVED BACK {} -> {} from {}",
                            self.id, pos_before, self.pos, instr.offset
                        );
                    }
                    if std::env::var_os("UNMCPK_TRACE").is_some() {
                        eprintln!(
                            "  b{:02} after stmt? pos={} cur={:?} produced={} terminated={} limit={:?}",
                            self.id,
                            self.pos,
                            self.cur().map(|i| (i.offset, i.name())),
                            produced.is_some(),
                            self.terminated,
                            self.limit
                        );
                    }
                    if let Some(stmt) = produced {
                        stmts.push(stmt);
                    } else if self.terminated {
                        break;
                    } else if self.pos == before {
                        return Err(format!(
                            "cannot structure {} at {}",
                            instr.name(),
                            instr.offset
                        ));
                    }
                }
            }
        }
        Ok(stmts)
    }

    /// `a if c else b` shows up as a conditional jump inside an expression.
    /// Only accept it when both arms evaluate to exactly one value.
    /// Whether the current conditional jump can be a conditional expression at
    /// all: both arms are short and the then-arm ends with an unconditional
    /// jump. Without this pre-check every statement-level `if` inside a large
    /// function would run an evaluation pass over its whole body, and those
    /// passes nest, which is exponential.
    fn ternary_shape(&self) -> bool {
        const MAX_ARM: usize = 40;
        let Some(cur) = self.cur() else { return false };
        let Some(target_index) = cur.target.and_then(|t| self.index_of(t)) else {
            return false;
        };
        if target_index < self.pos + 3 {
            return false;
        }
        let arm_end = target_index - 1;
        let then_len = arm_end.saturating_sub(self.pos + 1);
        let else_len = target_index.saturating_sub(arm_end + 1);
        self.ins[arm_end].is_unconditional() && then_len <= MAX_ARM && else_len <= MAX_ARM
    }

    fn try_ternary_expression(&mut self) -> Result<Option<Expr>, String> {
        let test = match self.stack.last() {
            Some(Item::Expr(_)) => self.pop()?,
            _ => return Ok(None),
        };
        let jump = self.cur().cloned().unwrap();
        let Some(target) = jump.target else {
            self.stack.push(Item::Expr(test));
            return Ok(None);
        };
        let Some(target_index) = self.index_of(target) else {
            self.stack.push(Item::Expr(test));
            return Ok(None);
        };
        // The then-arm ends right before the target with a forward jump.
        let Some(prev) = target_index.checked_sub(1).and_then(|i| self.ins.get(i)) else {
            self.stack.push(Item::Expr(test));
            return Ok(None);
        };
        if !matches!(prev.op, 110 | 113) || prev.target.is_none() {
            self.stack.push(Item::Expr(test));
            return Ok(None);
        }
        let arm_end = target_index - 1;
        let else_start = target_index;
        let Some(join_index) = prev.target.and_then(|t| self.index_of(t)) else {
            self.stack.push(Item::Expr(test));
            return Ok(None);
        };
        // An arm that fails to evaluate as a single expression means this is a
        // statement-level `if`/`while`, not a conditional expression.
        let then_arm = match self.eval_range(self.pos + 1, arm_end) {
            Ok(Some(value)) => Some(value),
            // An arm that needs structuring itself (a merged conditional) still
            // yields one value when the range is walked with conditionals on.
            Ok(None) | Err(_) if self.arm_needs_structuring(self.pos + 1, arm_end) => {
                match self.eval_value_structured(self.pos + 1, arm_end) {
                    Ok(value) => value,
                    Err(_) => {
                        self.stack.push(Item::Expr(test));
                        return Ok(None);
                    }
                }
            }
            Ok(_) | Err(_) => None,
        };
        let else_arm = match self.eval_range(else_start, join_index) {
            Ok(Some(value)) => Some(value),
            Ok(None) | Err(_) if self.arm_needs_structuring(else_start, join_index) => {
                match self.eval_value_structured(else_start, join_index) {
                    Ok(value) => value,
                    Err(_) => {
                        self.stack.push(Item::Expr(test));
                        return Ok(None);
                    }
                }
            }
            Ok(_) | Err(_) => None,
        };
        let (Some(then_val), Some(else_val)) = (then_arm, else_arm) else {
            // The arms share instructions with the surrounding statement (one
            // store for both branches): rebuild it as a single conditional.
            match self.try_merged_conditional(&test, target_index)? {
                Some(expr) => {
                    self.pos = join_index;
                    return Ok(Some(expr));
                }
                None => {
                    self.stack.push(Item::Expr(test));
                    return Ok(None);
                }
            }
        };
        self.pos = join_index;
        Ok(Some(Expr::Ternary(
            Box::new(then_val),
            Box::new(test),
            Box::new(else_val),
        )))
    }

    /// True when an arm contains a conditional jump of its own, i.e. when it
    /// might only evaluate as an expression once conditionals are structured.
    fn arm_needs_structuring(&self, start: usize, end: usize) -> bool {
        // Structured retries are quadratic in the worst case, so the shape the
        // compiler emits for merged branches (a handful of instructions) is the
        // only thing worth paying for.
        const MAX_MERGED_ARM: usize = 14;
        let end = end.min(self.ins.len());
        // 111/112 matter as well: a chain like `a == b == c == d` nests the
        // remaining comparisons under one shared join.
        end - start <= MAX_MERGED_ARM
            && (start..end).any(|i| matches!(self.ins[i].op, 111 | 112 | 114 | 115))
    }

    /// Rebuild the shape the compiler produces when both branches of an `if`
    /// store into the same place and one of them is itself a conditional:
    ///
    /// ```text
    /// <test>; POP_JUMP_IF_FALSE else; <continuation>; {POP_JUMP_IF_*}
    ///   else: <then-value>; JUMP_FORWARD join; else: <else-value>; join: STORE
    /// ```
    ///
    /// which is `<then-value> if <test> and <continuation> else <else-value>`.
    /// `x = a if cond else b` where the compiler merged the two assignments:
    /// both arms leave exactly one value on the stack, and a single STORE at
    /// the join consumes it. Without a store at the join this is an ordinary
    /// statement-level `if`, which `parse_if` handles.
    /// Code where both limbs of a test jump into one shared tail:
    ///
    /// ```text
    ///   <A>; POP_JUMP_IF_TRUE T; <B>; JUMP_IF_FALSE_OR_POP J
    /// T: <T>; J: ...
    /// ```
    ///
    /// which is `(A or B) and T`; the mirror image with `POP_JUMP_IF_FALSE` and
    /// `JUMP_IF_TRUE_OR_POP` is `(A and B) or T`. Both limbs must be pure values.
    fn try_shared_tail(&mut self, jump: &Instr) -> Result<Option<Expr>, String> {
        let Some(target) = jump.target else {
            return Ok(None);
        };
        let Some(target_index) = self.index_of(target) else {
            return Ok(None);
        };
        // The limb the jump skips over has to end in the opposite test.
        let want = if jump.op == 115 { 111 } else { 112 };
        let Some(limb_end) = (self.pos + 1..target_index).find(|&i| self.ins[i].op == want) else {
            return Ok(None);
        };
        let Some(join) = self.ins[limb_end].target else {
            return Ok(None);
        };
        let Some(join_index) = self.index_of(join).filter(|&i| i > target_index) else {
            return Ok(None);
        };
        let Ok(first) = self.pop() else {
            return Ok(None);
        };
        let Some(limb) = self.eval_value_structured(self.pos + 1, limb_end)? else {
            self.stack.push(Item::Expr(first));
            return Ok(None);
        };
        let Some(tail) = self.eval_value_structured(target_index, join_index)? else {
            self.stack.push(Item::Expr(first));
            return Ok(None);
        };
        let (inner_op, outer_op) = if jump.op == 115 {
            ("or", "and")
        } else {
            ("and", "or")
        };
        self.pos = join_index;
        Ok(Some(Expr::Bool(
            outer_op,
            vec![Expr::Bool(inner_op, vec![first, limb]), tail],
        )))
    }

    fn try_branch_value(
        &mut self,
        test: &Expr,
        target_index: usize,
    ) -> Result<Option<Expr>, String> {
        let Some(prev) = target_index.checked_sub(1).and_then(|i| self.ins.get(i)) else {
            return Ok(None);
        };
        if !prev.is_unconditional() {
            return Ok(None);
        }
        let Some(join) = prev.target.and_then(|target| self.index_of(target)) else {
            return Ok(None);
        };
        if join <= target_index {
            return Ok(None);
        }
        let Some(store) = self.ins.get(join) else {
            return Ok(None);
        };
        // A store, or the RETURN_VALUE that turns the value into the result
        // (`return a if c else b` compiles the same way).
        if !matches!(store.op, 90 | 97 | 125 | 137 | 83) {
            return Ok(None);
        }
        let Some(then_value) = self.eval_value_structured(self.pos + 1, target_index - 1)? else {
            return Ok(None);
        };
        let Some(else_value) = self.eval_value_structured(target_index, join)? else {
            return Ok(None);
        };
        self.pos = join;
        Ok(Some(Expr::Ternary(
            Box::new(then_value),
            Box::new(test.clone()),
            Box::new(else_value),
        )))
    }

    fn try_merged_conditional(
        &mut self,
        test: &Expr,
        target_index: usize,
    ) -> Result<Option<Expr>, String> {
        let Some(prev) = target_index.checked_sub(1).and_then(|i| self.ins.get(i)) else {
            return Ok(None);
        };
        if !matches!(prev.op, 110 | 113) {
            return Ok(None);
        }
        let Some(join) = prev.target.and_then(|t| self.index_of(t)) else {
            return Ok(None);
        };
        if join <= target_index {
            return Ok(None);
        }
        // Exactly one inner conditional, and it must jump to the same else.
        let mut inner = None;
        for index in self.pos + 1..target_index {
            if matches!(self.ins[index].op, 114 | 115) {
                if inner.is_some() || self.ins[index].target != Some(self.ins[target_index].offset) {
                    return Ok(None);
                }
                inner = Some(index);
            }
        }
        let Some(inner) = inner else {
            return Ok(None);
        };
        let Some(continuation) = self.eval_range(self.pos + 1, inner)? else {
            return Ok(None);
        };
        let combined = match self.ins[inner].op {
            114 => Expr::Bool("and", vec![test.clone(), continuation]),
            _ => Expr::Bool(
                "and",
                vec![test.clone(), invert_test(continuation)],
            ),
        };
        let Some(then_value) = self.eval_range(inner + 1, target_index - 1)? else {
            return Ok(None);
        };
        let Some(else_value) = self.eval_range(target_index, join)? else {
            return Ok(None);
        };
        Ok(Some(Expr::Ternary(
            Box::new(then_value),
            Box::new(combined),
            Box::new(else_value),
        )))
    }

    /// Evaluate `[start, end)` as one value, allowing conditionals inside the
    /// range. Used as the fallback for a conditional-expression arm whose own
    /// arms were merged with the surrounding statement by the compiler (the
    /// `if logLevel: x = A if c else y  /  else: x = y` shape, where both
    /// branches share one store).
    /// Same as [`Self::eval_value_structured`], but the probe starts with a
    /// value already on its stack. A nested chain (`a == b == c == d`) needs the
    /// operand the compiler duplicated in front of it.
    fn eval_value_structured_seeded(
        &self,
        start: usize,
        end: usize,
        seed: Option<Item>,
    ) -> Result<Option<Expr>, String> {
        let _recursion = Recursion::enter()?;
        let stop_offset = self.ins.get(end).map(|i| i.offset).unwrap_or(usize::MAX);
        let mut sub = Builder {
            pending_import: None,
            code: self.code,
            ins: self.ins.clone(),
            pos: start,
            stack: seed.into_iter().collect(),
            dup_pending: 0,
            pending_targets: None,
            pending_aug: None,
            printed: Vec::new(),
            print_dest: None,
            print_newline: false,
            limit: Some(stop_offset),
            block_stop: stop_offset,
            loop_head: None,
            terminated: false,
            exit_target: None,
            steps: 0,
            probe: true,
            over_budget: take_budget().is_none(),
            id: next_builder_id(),
        };
        let stmts = match sub.parse_suite(Some(stop_offset), 0) {
            Ok(stmts) => stmts,
            Err(_) => return Ok(None),
        };
        let reached = sub.pos >= end || (sub.terminated && sub.pos + 1 >= end);
        if !stmts.is_empty() || !reached {
            return Ok(None);
        }
        match sub.stack.pop() {
            Some(Item::Expr(expr)) => Ok(Some(expr)),
            Some(Item::Iter(expr)) => Ok(Some(*expr)),
            _ => Ok(None),
        }
    }

    fn eval_value_structured(&self, start: usize, end: usize) -> Result<Option<Expr>, String> {
        let _recursion = Recursion::enter()?;
        let stop_offset = self.ins.get(end).map(|i| i.offset).unwrap_or(usize::MAX);
        let mut sub = Builder {
            pending_import: None,
            code: self.code,
            ins: self.ins.clone(),
            pos: start,
            stack: Vec::new(),
            dup_pending: 0,
            pending_targets: None,
            pending_aug: None,
            printed: Vec::new(),
            print_dest: None,
            print_newline: false,
            limit: Some(stop_offset),
            block_stop: stop_offset,
            loop_head: None,
            terminated: false,
            exit_target: None,
            steps: 0,
            probe: true,
            over_budget: take_budget().is_none(),
            id: next_builder_id(),
        };
        let stmts = match sub.parse_suite(Some(stop_offset), 0) {
            Ok(stmts) => stmts,
            // A probe that cannot structure the range means "not this shape";
            // the failure belongs to the probe, not to the enclosing object.
            Err(_) => return Ok(None),
        };
        // A range can stop past its requested end when a shared join pulls the
        // last operand along (`a and b` merging into a single store); one value
        // and no statements still make it a value.
        let reached = sub.pos >= end || (sub.terminated && sub.pos + 1 >= end);
        if !stmts.is_empty() || !reached || sub.stack.len() != 1 {
            return Ok(None);
        }
        match sub.stack.pop() {
            Some(Item::Expr(expr)) => Ok(Some(expr)),
            // `... if c else ...` feeding a `for` ends in `GET_ITER`, so the
            // value arrives wrapped as an iterator.
            Some(Item::Iter(expr)) => Ok(Some(*expr)),
            _ => Ok(None),
        }
    }

    /// Evaluate `[start, end)` expecting the iterable of a `for` loop, which
    /// ends in `GET_ITER` and therefore leaves an iterator item.
    fn eval_iterable(&self, start: usize, end: usize) -> Result<Option<Expr>, String> {
        let _recursion = Recursion::enter()?;
        let stop_offset = self
            .ins
            .get(end)
            .map(|i| i.offset)
            .unwrap_or(usize::MAX);
        let mut sub = Builder {
            pending_import: None,
            code: self.code,
            ins: self.ins.clone(),
            pos: start,
            stack: Vec::new(),
            dup_pending: 0,
            pending_targets: None,
            pending_aug: None,
            printed: Vec::new(),
            print_dest: None,
            print_newline: false,
            limit: Some(stop_offset),
            block_stop: stop_offset,
            loop_head: None,
            terminated: false,
            exit_target: None,
            steps: 0,
            probe: false,
            over_budget: take_budget().is_none(),
            id: next_builder_id(),
        };
        let stmts = match sub.parse_suite(Some(stop_offset), 0) {
            Ok(stmts) => stmts,
            // A probe that cannot structure the range means "not this shape";
            // the failure belongs to the probe, not to the enclosing object.
            Err(_) => return Ok(None),
        };
        if !stmts.is_empty() || sub.pos != end {
            return Ok(None);
        }
        match sub.stack.pop() {
            Some(Item::Iter(expr)) => Ok(Some(*expr)),
            Some(Item::Expr(expr)) => Ok(Some(expr)),
            _ => Ok(None),
        }
    }

    /// Like [`Self::eval_range`], but starts with one value already on the
    /// stack. Chained comparisons (`a < b < c`) leave the middle operand there.
    fn eval_range_seeded(
        &self,
        start: usize,
        end: usize,
        seed: Option<Item>,
    ) -> Result<Option<Expr>, String> {
        let Some(seed) = seed else {
            return Ok(None);
        };
        let _recursion = Recursion::enter()?;
        let stop_offset = self
            .ins
            .get(end)
            .map(|i| i.offset)
            .unwrap_or(usize::MAX);
        let mut sub = Builder {
            pending_import: None,
            code: self.code,
            ins: self.ins.clone(),
            pos: start,
            stack: vec![seed],
            dup_pending: 0,
            pending_targets: None,
            pending_aug: None,
            printed: Vec::new(),
            print_dest: None,
            print_newline: false,
            limit: Some(stop_offset),
            block_stop: stop_offset,
            loop_head: None,
            terminated: false,
            exit_target: None,
            steps: 0,
            probe: false,
            over_budget: take_budget().is_none(),
            id: next_builder_id(),
        };
        let stmts = match sub.parse_suite(Some(stop_offset), 0) {
            Ok(stmts) => stmts,
            // A probe that cannot structure the range means "not this shape";
            // the failure belongs to the probe, not to the enclosing object.
            Err(_) => return Ok(None),
        };
        // A range may end with the jump that leaves it (the interpreter's own
        // `JUMP_FORWARD` past a chained comparison), which the suite stops on
        // instead of consuming.
        let reached = sub.pos == end || (sub.terminated && sub.pos + 1 == end);
        if !stmts.is_empty() || !reached || sub.stack.len() != 1 {
            return Ok(None);
        }
        match sub.stack.pop() {
            Some(Item::Expr(expr)) => Ok(Some(expr)),
            _ => Ok(None),
        }
    }

    /// Evaluate `[start, end)` expecting exactly one value and no side effects.
    /// Used to classify a region as an expression arm.
    fn eval_range(&self, start: usize, end: usize) -> Result<Option<Expr>, String> {
        let _recursion = Recursion::enter()?;
        let stop_offset = self
            .ins
            .get(end)
            .map(|i| i.offset)
            .unwrap_or(usize::MAX);
        let mut sub = Builder {
            pending_import: None,
            code: self.code,
            ins: self.ins.clone(),
            pos: start,
            stack: Vec::new(),
            dup_pending: 0,
            pending_targets: None,
            pending_aug: None,
            printed: Vec::new(),
            print_dest: None,
            print_newline: false,
            limit: Some(stop_offset),
            block_stop: stop_offset,
            loop_head: None,
            terminated: false,
            exit_target: None,
            steps: 0,
            probe: false,
            over_budget: take_budget().is_none(),
            id: next_builder_id(),
        };
        if std::env::var_os("UNMCPK_TRACE").is_some() {
            eprintln!("  b{:02} eval_range {start}..{end} stop={stop_offset}", sub.id);
        }
        let stmts = match sub.parse_suite(Some(stop_offset), 0) {
            Ok(stmts) => stmts,
            // A probe that cannot structure the range means "not this shape";
            // the failure belongs to the probe, not to the enclosing object.
            Err(_) => return Ok(None),
        };
        if !stmts.is_empty() {
            return Ok(None);
        }
        if sub.pos != end {
            return Ok(None);
        }
        match sub.stack.len() {
            1 => match sub.stack.pop() {
                Some(Item::Expr(expr)) => Ok(Some(expr)),
                _ => Ok(None),
            },
            _ => Ok(None),
        }
    }

    /// Like `eval_range`, but keeps every value the range leaves behind and
    /// does not insist on exactly one. Comprehension elements may push a key
    /// and a value (`MAP_ADD`) or a tuple plus its length.
    fn eval_values_relaxed(&self, start: usize, end: usize) -> Result<Vec<Expr>, String> {
        let _recursion = Recursion::enter()?;
        let stop_offset = self
            .ins
            .get(end)
            .map(|i| i.offset)
            .unwrap_or(usize::MAX);
        let mut sub = Builder {
            pending_import: None,
            code: self.code,
            ins: self.ins.clone(),
            pos: start,
            stack: Vec::new(),
            dup_pending: 0,
            pending_targets: None,
            pending_aug: None,
            printed: Vec::new(),
            print_dest: None,
            print_newline: false,
            limit: Some(stop_offset),
            block_stop: stop_offset,
            loop_head: None,
            terminated: false,
            exit_target: None,
            steps: 0,
            probe: false,
            over_budget: take_budget().is_none(),
            id: next_builder_id(),
        };
        let stmts = match sub.parse_suite(Some(stop_offset), 0) {
            Ok(stmts) => stmts,
            // A probe that cannot structure the range means "not this shape";
            // the failure belongs to the probe, not to the enclosing object.
            Err(_) => return Ok(Vec::new()),
        };
        if !stmts.is_empty() || sub.pos != end {
            return Ok(Vec::new());
        }
        let mut values = Vec::new();
        for item in sub.stack {
            if let Item::Expr(expr) = item {
                values.push(expr);
            }
        }
        Ok(values)
    }

    /// `try:` ... `except` clauses. The 2.7 layout is
    /// `SETUP_EXCEPT handler; <body>; POP_BLOCK; JUMP_FORWARD end; handler:
    /// <clauses>; END_FINALLY; end:` and each clause is either three `POP_TOP`s
    /// (bare) or `DUP_TOP <type> COMPARE_OP exception match POP_JUMP_IF_FALSE
    /// next; POP_TOP [STORE name]; <body>; JUMP_FORWARD end`.
    /// `with ctx as name:` is `SETUP_WITH cleanup; [STORE]; body; POP_BLOCK;
    /// LOAD_CONST None; WITH_CLEANUP; END_FINALLY`.
    fn parse_with(&mut self, depth: usize) -> Result<Stmt, String> {
        let setup = self.cur().cloned().ok_or_else(|| String::from("with at the end"))?;
        let limit = setup
            .target
            .and_then(|offset| self.index_of(offset))
            .unwrap_or(self.ins.len());
        let ctx = self.pop()?;
        self.pos += 1;
        let mut target = None;
        // `with cm:` discards the value of `__enter__` with a POP_TOP.
        if self.cur().map(|i| i.op) == Some(1) {
            self.pos += 1;
        }
        if let Some(store) = self.cur().cloned() {
            if matches!(store.op, 90 | 97 | 125 | 126 | 136 | 137) {
                target = Some(self.simple_target(store.op, &store)?);
                self.pos += 1;
            }
        }
        // The body can hold blocks of its own (`try` inside `with`), so the
        // POP_BLOCK that closes it is the last one before the cleanup.
        let Some(block_index) = (self.pos..limit).rev().find(|&i| self.ins[i].op == 87) else {
            return Err(String::from("with body without POP_BLOCK"));
        };
        let body = self.parse_suite(Some(self.ins[block_index].offset), depth + 1)?;
        // Skip POP_BLOCK, LOAD_CONST None, WITH_CLEANUP and END_FINALLY.
        let mut index = block_index;
        while index < self.ins.len() && self.ins[index].op != 88 {
            index += 1;
        }
        self.pos = index + 1;
        Ok(Stmt::With { ctx, target, body })
    }

    fn parse_try_except(&mut self, depth: usize) -> Result<Stmt, String> {
        let setup = self.cur().cloned().unwrap();
        let Some(handler_offset) = setup.target else {
            return Err(String::from("SETUP_EXCEPT without target"));
        };
        let Some(handler_index) = self.index_of(handler_offset) else {
            return Err(format!("SETUP_EXCEPT to unknown offset {handler_offset}"));
        };
        self.pos += 1;
        // Nested `with`/loop blocks contribute their own POP_BLOCK; the try
        // body's one is followed by the JUMP_FORWARD that skips the handlers.
        let Some(block_index) = (self.pos..handler_index)
            .rev()
            .find(|&i| self.ins[i].op == 87 && self.ins.get(i + 1).map(|n| n.op) == Some(110))
            .or_else(|| (self.pos..handler_index).rev().find(|&i| self.ins[i].op == 87))
        else {
            return Err(String::from("try body without POP_BLOCK"));
        };
        let body = self.parse_suite(Some(self.ins[block_index].offset), depth + 1)?;
        // Whatever is left before the block's own POP_BLOCK can only be the
        // leftovers of a branch that already ended the flow.
        while self.pos < block_index
            && self
                .cur()
                .map(|i| matches!(i.op, 1 | 87 | 88) || i.is_unconditional())
                .unwrap_or(false)
        {
            self.pos += 1;
        }
        if self.pos < block_index {
            return Err(format!(
                "try body stopped early at {}",
                self.ins[self.pos].offset
            ));
        }
        self.pos = block_index + 1;
        let Some(end_offset) = self
            .ins
            .get(self.pos)
            // JUMP_FORWARD normally, but a try at the very end of a block is
            // compiled to JUMP_ABSOLUTE.
            .filter(|i| matches!(i.op, 110 | 113))
            .and_then(|i| i.target)
        else {
            return Err(String::from("try without a jump past its handlers"));
        };
        let Some(mut finally_index) = (handler_index..self.ins.len()).find(|&i| self.ins[i].op == 88)
        else {
            return Err(String::from("try without END_FINALLY"));
        };
        // A `try` inside a loop body has no "jump past its handlers": the
        // instruction after its `POP_BLOCK` is the loop's own flow, pointing
        // back inside the body. Only a target past the handlers (or, at the tail
        // of a loop, back before the `try`) is that jump.
        let has_forward_exit = end_offset > handler_offset;
        let Some(end_index) = (if has_forward_exit {
            self.index_of(end_offset)
        } else {
            Some(finally_index + 1)
        }) else {
            return Err(format!("try jumps to unknown offset {end_offset}"));
        };
        let trace = std::env::var_os("UNMCPK_TRACE").is_some();
        if trace {
            eprintln!(
                "EXC enter setup={} handler={} block={} end={} finally={}",
                self.ins[self.pos].offset, handler_offset,
                self.ins[block_index].offset, end_offset, self.ins[finally_index].offset
            );
        }
        self.pos = handler_index;
        let mut handlers = Vec::new();
        loop {
            if trace {
                eprintln!("EXC clause loop pos={} finally_index={}", self.ins.get(self.pos).map_or(usize::MAX, |i| i.offset), self.ins[finally_index].offset);
            }
            // The first END_FINALLY after the handler region can belong to a
            // `try` nested inside a clause body, so the chain continues past it
            // when another clause starts after it.
            if self.pos >= finally_index {
                let starts_clause = matches!(self.cur().map(|i| i.op), Some(4) | Some(1));
                let next_fin = (self.pos..self.ins.len()).find(|&i| self.ins[i].op == 88);
                match (starts_clause, next_fin) {
                    (true, Some(next_fin)) if next_fin > finally_index => {
                        finally_index = next_fin;
                    }
                    _ => break,
                }
            }
            let Some(instr) = self.cur().cloned() else {
                break;
            };
            let (kind, name, stop_index) = match instr.op {
                // DUP_TOP: a typed clause.
                4 => {
                    self.pos += 1;
                    let Some(compare_index) = (self.pos..finally_index)
                        .find(|&i| self.ins[i].op == 107)
                    else {
                        return Err(String::from("except clause without a type match"));
                    };
                    let kind = self
                        .eval_range(self.pos, compare_index)?
                        .ok_or_else(|| String::from("except clause without a type"))?;
                    self.pos = compare_index + 1;
                    let Some(jump) = self.cur().cloned() else {
                        return Err(String::from("except clause without a test"));
                    };
                    if jump.op != 114 {
                        return Err(format!(
                            "except clause does not test its type: {}",
                            jump.name()
                        ));
                    }
                    let Some(next_offset) = jump.target else {
                        return Err(String::from("except clause without a next clause"));
                    };
                    let Some(next_index) = self.index_of(next_offset) else {
                        return Err(format!("except clause to unknown offset {next_offset}"));
                    };
                    // The clause prologue drops the matched value, then either
                    // binds a name or drops the remaining two values.
                    self.pos += 1; // past the conditional jump
                    if self.cur().map(|i| i.op) == Some(1) {
                        self.pos += 1; // the matched value
                    }
                    let mut name = None;
                    if self.cur().map(|i| i.op) == Some(1) {
                        // `except Foo:` - value and traceback follow.
                        self.pos += 1;
                        if self.cur().map(|i| i.op) == Some(1) {
                            self.pos += 1;
                        }
                    } else if self.cur().map(|i| i.op) == Some(92) {
                        // `except Foo, (a, b):` unpacks the matched value
                        // before binding: UNPACK_SEQUENCE n; store x n; POP_TOP.
                        let count = self.cur().map(|i| i.arg as usize).unwrap_or(0);
                        self.pos += 1;
                        let mut targets = Vec::with_capacity(count);
                        for _ in 0..count {
                            let Some(store) = self.cur().cloned() else {
                                break;
                            };
                            if !matches!(store.op, 90 | 97 | 125 | 126 | 136 | 137) {
                                break;
                            }
                            targets.push(self.simple_target(store.op, &store)?);
                            self.pos += 1;
                        }
                        if !targets.is_empty() {
                            name = Some(Expr::Tuple(targets));
                        }
                        if self.cur().map(|i| i.op) == Some(1) {
                            self.pos += 1;
                        }
                    } else if let Some(store) = self.cur().cloned() {
                        if matches!(store.op, 90 | 97 | 125 | 126 | 136 | 137) {
                            name = Some(self.simple_target(store.op, &store)?);
                            self.pos += 1;
                            // The traceback is dropped right after the binding.
                            if self.cur().map(|i| i.op) == Some(1) {
                                self.pos += 1;
                            }
                        }
                    }
                    (Some(kind), name, next_index)
                }
                // Bare `except:` - the interpreter's triple is dropped.
                1 => {
                    while self.pos < finally_index && self.cur().map(|i| i.op) == Some(1) {
                        self.pos += 1;
                    }
                    (None, None, finally_index)
                }
                other => {
                    return Err(format!("cannot start an except clause with byte {other}"));
                }
            };
            // The clause body ends right before the jump to the common exit.
            let mut stop = stop_index;
            if stop > self.pos && matches!(self.ins[stop - 1].op, 110 | 113) {
                stop -= 1;
            }
            if trace {
                eprintln!(
                    "EXC body {}..{} from {}",
                    self.ins[self.pos].offset,
                    self.ins[stop].offset,
                    self.ins[stop_index].offset
                );
            }
            let body = match self.parse_suite(Some(self.ins[stop].offset), depth + 1) {
                Ok(body) => body,
                Err(err) => {
                    if trace {
                        eprintln!("EXC body failed at {}: {err}", self.ins[self.pos.min(self.ins.len() - 1)].offset);
                    }
                    return Err(err);
                }
            };
            if self.pos < stop {
                return Err(format!(
                    "except body stopped early at {}",
                    self.ins[self.pos].offset
                ));
            }
            self.pos = stop_index;
            handlers.push(Handler { kind, name, body });
        }
        // A try at the tail of a loop body has no forward exit: its "jump
        // past the handlers" is the back edge, so resume after the handlers
        // and let the enclosing loop see its own back edge.
        if trace {
            eprintln!("EXC done handlers={} end_offset={}", handlers.len(), end_offset);
        }
        self.pos = if has_forward_exit { end_index } else { finally_index + 1 };
        Ok(Stmt::Try {
            body,
            handlers,
            orelse: Vec::new(),
            finally: Vec::new(),
        })
    }

    /// `try:` ... `finally:`: the finally body is reached both by falling
    /// through (after `POP_BLOCK; LOAD_CONST None`) and by the unwinder.
    fn parse_try_finally(&mut self, depth: usize) -> Result<Stmt, String> {
        let setup = self.cur().cloned().unwrap();
        let Some(handler_offset) = setup.target else {
            return Err(String::from("SETUP_FINALLY without target"));
        };
        let Some(finally_index) = self.index_of(handler_offset) else {
            return Err(format!("SETUP_FINALLY to unknown offset {handler_offset}"));
        };
        self.pos += 1;
        let Some(block_index) = (self.pos..finally_index)
            .rev()
            .find(|&i| self.ins[i].op == 87 && self.ins.get(i + 1).map(|n| n.op) == Some(110))
            .or_else(|| (self.pos..finally_index).rev().find(|&i| self.ins[i].op == 87))
        else {
            return Err(String::from("try body without POP_BLOCK"));
        };
        let body = self.parse_suite(Some(self.ins[block_index].offset), depth + 1)?;
        while self.pos < block_index
            && self
                .cur()
                .map(|i| matches!(i.op, 1 | 87 | 88) || i.is_unconditional())
                .unwrap_or(false)
        {
            self.pos += 1;
        }
        if self.pos < block_index {
            return Err(format!(
                "try body stopped early at {}",
                self.ins[self.pos].offset
            ));
        }
        let Some(end_index) = (finally_index..self.ins.len()).find(|&i| self.ins[i].op == 88) else {
            return Err(String::from("try/finally without END_FINALLY"));
        };
        self.pos = finally_index;
        let finally = self.parse_suite(Some(self.ins[end_index].offset), depth + 1)?;
        self.pos = end_index + 1;
        Ok(Stmt::Try {
            body,
            handlers: Vec::new(),
            orelse: Vec::new(),
            finally,
        })
    }

    /// Rebuild a short-circuit `and`/`or` chain: the value stays on the stack
    /// and `JUMP_IF_FALSE_OR_POP`/`JUMP_IF_TRUE_OR_POP` skips the rest of the
    /// chain once the result is decided.
    fn parse_bool_chain(&mut self) -> Result<(), String> {
        loop {
            let Some(instr) = self.cur().cloned() else {
                break;
            };
            let op_text = match instr.op {
                111 => "and",
                112 => "or",
                _ => break,
            };
            let Some(target) = instr.target else {
                return Err(format!("{} without target", instr.name()));
            };
            let Some(target_index) = self.index_of(target) else {
                return Err(format!("{} to unknown offset {target}", instr.name()));
            };
            let left = self.pop()?;
            self.pos += 1;
            // A chained comparison leaves the shared operand on the stack
            // (`a < b < c` duplicates `b`), so the right-hand range needs it as
            // the left operand of its own COMPARE_OP.
            // Only a chained comparison leaves an operand for the right-hand
            // range to reuse; anything else on the stack belongs to the
            // surrounding statement and must stay there.
            const COMPARE_OP: u8 = 107;
            // The chain's join can sit on a terminator (`return a < b < c`
            // jumps past the comparison to the RETURN_VALUE); the right-hand
            // operand ends where that terminator starts.
            let mut operand_end = target_index;
            while operand_end > self.pos
                && matches!(self.ins[operand_end - 1].op, 83 | 110 | 113)
            {
                operand_end -= 1;
            }
            let chain_like = (self.pos..target_index).any(|i| self.ins[i].op == COMPARE_OP);
            let carried = match self.stack.last() {
                Some(Item::Expr(_)) if chain_like => self.stack.pop(),
                _ => None,
            };
            let seeded = self.eval_range_seeded(self.pos, operand_end, carried.clone())?;
            let used_seed = seeded.is_some();
            let right = match seeded {
                Some(value) => Some(value),
                None => {
                    // Not a seeded range after all: restore and evaluate plain.
                    if let Some(item) = carried {
                        self.stack.push(item);
                    }
                    // A conditional expression can share the join of this
                    // chain, leaving its arm jump inside the operand range:
                    // the operand then ends where that jump is.
                    let straight = self.eval_range(self.pos, operand_end)?;
                    match straight {
                        Some(value) => Some(value),
                        None => {
                            let cut = (self.pos..operand_end)
                                .find(|&i| self.ins[i].is_unconditional());
                            let trimmed = match cut {
                                Some(cut) if cut > self.pos => {
                                    self.eval_range(self.pos, cut)?
                                }
                                _ => None,
                            };
                            match trimmed {
                                Some(value) => Some(value),
                                // The operand can hold a conditional expression
                                // of its own (`a and b if c else d`).
                                None if self.arm_needs_structuring(self.pos, target_index) => {
                                    let seed = self.stack.last().cloned();
                                    match self.eval_value_structured_seeded(
                                        self.pos,
                                        target_index,
                                        seed,
                                    )? {
                                        Some(value) => Some(value),
                                        None => {
                                            self.eval_value_structured(self.pos, target_index)?
                                        }
                                    }
                                }
                                None => None,
                            }
                        }
                    }
                }
            };
            let Some(right) = right else {
                return Err(format!("{} without a right operand", instr.name()));
            };

            self.pos = target_index;
            if used_seed {
                // The false arm of a chained comparison drops the operand the
                // comparison duplicated: `ROT_TWO; POP_TOP` (or ROT_THREE).
                while matches!(self.cur().map(|i| i.op), Some(2 | 3))
                    && self.ins.get(self.pos + 1).map(|i| i.op) == Some(1)
                {
                    self.pos += 2;
                }
            }
            let combined = match (left, right) {
                (Expr::Bool(name, mut items), Expr::Bool(other, more))
                    if name == op_text && other == op_text =>
                {
                    items.extend(more);
                    Expr::Bool(op_text, items)
                }
                (Expr::Bool(name, mut items), other) if name == op_text => {
                    items.push(other);
                    Expr::Bool(op_text, items)
                }
                (other, Expr::Bool(name, more)) if name == op_text => {
                    let mut items = vec![other];
                    items.extend(more);
                    Expr::Bool(op_text, items)
                }
                (left, right) => Expr::Bool(op_text, vec![left, right]),
            };
            self.push(combined);
        }
        Ok(())
    }

    /// The target of a store instruction that needs nothing from the stack.
    /// A `[e for x in it if c]` loop, recognised by a `FOR_ITER` that no
    /// `SETUP_LOOP` introduced; the container was pushed by `BUILD_LIST`/`SET`.
    /// A `for` loop that reached us without its `SETUP_LOOP` (compiled inside a
    /// generator, or after the enclosing block was consumed): same shape as a
    /// comprehension but with no container and no append.
    fn parse_bare_for(
        &mut self,
        for_instr: Instr,
        end_index: usize,
        depth: usize,
    ) -> Result<Option<Stmt>, String> {
        let Some(item) = self.stack.pop() else {
            return Err(format!(
                "comprehension without a container append at {}",
                for_instr.offset
            ));
        };
        let iter = match item {
            Item::Iter(expr) => *expr,
            Item::Expr(expr) => expr,
            other => {
                self.stack.push(other);
                return Err(format!(
                    "comprehension without a container append at {}",
                    for_instr.offset
                ));
            }
        };
        self.pos += 1;
        let Some(store) = self.cur().cloned() else {
            return Err(format!("for loop without a target at {}", for_instr.offset));
        };
        if !matches!(store.op, 90 | 97 | 125 | 126 | 136 | 137) {
            return Err(format!("for loop without a target at {}", store.offset));
        }
        let target = self.simple_target(store.op, &store)?;
        self.pos += 1;
        let body_end = (self.pos..end_index)
            .rev()
            .find(|&i| self.ins[i].op == 113 && self.ins[i].target == Some(for_instr.offset))
            .unwrap_or(end_index);
        let saved_head = self.loop_head;
        self.loop_head = Some(for_instr.offset);
        let body = self.parse_suite(Some(self.ins[body_end].offset), depth + 1)?;
        self.loop_head = saved_head;
        self.pos = end_index;
        Ok(Some(Stmt::For {
            target,
            iter,
            body,
            orelse: Vec::new(),
        }))
    }

    fn parse_comp_loop(&mut self, depth: usize) -> Result<Option<Stmt>, String> {
        let for_instr = self.cur().cloned().ok_or("comprehension without FOR_ITER")?;
        let end_offset = for_instr
            .target
            .ok_or("comprehension FOR_ITER without a target")?;
        let end_index = self
            .index_of(end_offset)
            .ok_or_else(|| format!("comprehension loop to unknown offset {end_offset}"))?;
        // A generator expression's code object receives its iterator as
        // argument `.0` and starts looping straight away, with no container and
        // no `GET_ITER` of its own.
        let mut is_gen = false;
        let iterable = match self.stack.pop() {
            Some(Item::Iter(expr)) => *expr,
            Some(Item::Expr(expr)) => {
                is_gen = true;
                expr
            }
            other => {
                return Err(format!(
                    "comprehension without an iterator at {} (stack top {})",
                    self.cur().map_or(usize::MAX, |i| i.offset),
                    match other {
                        Some(Item::Expr(_)) => "expression",
                        Some(Item::Code(_)) => "code",
                        Some(Item::Class { .. }) => "class",
                        Some(Item::FromModule { .. }) => "module",
                        Some(Item::FromAttr(_)) => "attribute",
                        Some(Item::Iter(_)) => "iterator",
                        Some(Item::Unpack { .. }) => "unpack",
                        Some(Item::Slice { .. }) => "slice",
                        None => "empty",
                    }
                ))
            }
        };
        self.pos += 1;
        let store = self.cur().cloned().ok_or("comprehension without a target")?;
        let target = self.simple_target(store.op, &store)?;
        self.pos += 1;
        let terminator = |op: u8| {
            if is_gen {
                op == 86
            } else {
                // STORE_MAP is how a dict comprehension adds its pair.
                matches!(op, 94 | 146 | 147 | 54)
            }
        };
        // `FOR_ITER`'s exit target normally sits right after the loop, but a
        // compiled comprehension can point it inside the element expression;
        // the append is still ahead of us, so widen the search.
        let append_index = match (self.pos..end_index).find(|&i| terminator(self.ins[i].op)) {
            Some(index) => index,
            None => match (self.pos..self.ins.len()).find(|&i| terminator(self.ins[i].op)) {
                Some(index) => index,
                // No container and no yield: this is an ordinary `for` loop
                // whose SETUP_LOOP was already consumed (or which sits inside a
                // generator), not a comprehension.
                None => return self.parse_bare_for(for_instr.clone(), end_index, depth),
            },
        };
        // `[x for a in b for c in d]` nests another `for` before the append:
        // each one is its own clause, in source order.
        let mut nested: Vec<Clause> = Vec::new();
        loop {
            let Some(inner_for) = (self.pos..append_index).find(|&i| self.ins[i].op == 93) else {
                break;
            };
            let Some(iter_expr) = self.eval_iterable(self.pos, inner_for)? else {
                break;
            };
            self.pos = inner_for + 1;
            let Some(store) = self.cur().cloned() else {
                break;
            };
            if !matches!(store.op, 90 | 97 | 125 | 126 | 136 | 137) {
                return Err(format!(
                    "comprehension clause without a target at {}",
                    store.offset
                ));
            }
            let target = self.simple_target(store.op, &store)?;
            self.pos += 1;
            nested.push(Clause {
                target,
                iter: iter_expr,
                ifs: Vec::new(),
            });
        }
        // Every conditional jump before the append guards the element: it
        // skips to the end of the loop body for items the filter rejects.
        let body_start = self.pos;
        let mut cursor = self.pos;
        let mut ifs = Vec::new();
        while let Some(jump) = (cursor..append_index)
            .find(|&i| matches!(self.ins[i].op, 114 | 115))
        {
            let condition = self
                .eval_range(cursor, jump)?
                .ok_or_else(|| format!(
                    "comprehension condition without a value at {}..{}",
                    self.ins[cursor].offset, self.ins[jump].offset
                ))?;
            ifs.push(condition);
            cursor = jump + 1;
        }
        let element_start = cursor;
        let mut values = self.eval_values_relaxed(element_start, append_index)?;
        if values.is_empty() {
            // The element can itself be a conditional expression (`f(x) if c
            // else x`): its `if` looks like a filter, and taking it as one
            // leaves an empty arm. Rebuild the whole body structurally with the
            // filters still in place.
            if let Some(value) = self.eval_value_structured(element_start, append_index)? {
                values.push(value);
            } else if element_start != body_start {
                if let Some(value) = self.eval_value_structured(body_start, append_index)? {
                    values.push(value);
                    ifs.clear();
                }
            }
        }
        let append_op = self.ins[append_index].op;
        let element = match (append_op, values.as_slice()) {
            (147 | 54, [key, value]) => Expr::Tuple(vec![key.clone(), value.clone()]),
            (_, []) => return Err(format!(
                "comprehension element without a value at {}..{}",
                self.ins[element_start].offset,
                self.ins[append_index].offset
            )),
            (_, many) => many[many.len() - 1].clone(),
        };
        let kind = match append_op {
            94 => CompKind::List,
            146 => CompKind::Set,
            147 | 54 => CompKind::Dict,
            86 => CompKind::Gen,
            other => return Err(format!("cannot append into a container with byte {other}")),
        };
        // A generator expression is a function whose body yields the element;
        // rendered on its own it is that element, and the call site rebuilds the
        // comprehension from it.
        if is_gen {
            self.pos = end_index;
            return Ok(Some(Stmt::Return(Some(element))));
        }
        // The container literal itself is the result, so it leaves the stack.
        self.stack.pop();
        // Continue after the loop: past `FOR_ITER`'s target, and past the
        // backwards jump that closes the body.
        // A nested comprehension closes several loops, so more than one
        // backwards jump can follow the append.
        let mut exit = append_index + 1;
        while let Some(instr) = self.ins.get(exit) {
            if instr.is_unconditional() && instr.target.map_or(false, |t| t < instr.offset) {
                exit += 1;
            } else {
                break;
            }
        }
        let back_edge = Some(exit - 1);
        let end_index = match back_edge {
            Some(index) => end_index.max(index + 1),
            None => end_index,
        };
        let mut clauses = vec![Clause {
            target,
            iter: iterable,
            ifs: Vec::new(),
        }];
        clauses.extend(nested);
        if let Some(last) = clauses.last_mut() {
            last.ifs = ifs;
        }
        self.stack.push(Item::Expr(Expr::Comp(Box::new(Comp {
            kind,
            element,
            clauses,
        }))));
        self.pos = end_index;
        Ok(None)
    }

    fn simple_target(&mut self, op: u8, instr: &Instr) -> Result<Expr, String> {
        match op {
            90 | 97 => Ok(Expr::Name(self.name_at(instr.arg)?)),
            125 | 126 => Ok(Expr::Name(self.var_at(instr.arg)?)),
            136 | 137 => Ok(Expr::Name(self.cell_at(instr.arg)?)),
            // UNPACK_SEQUENCE: `for k, v in ...`
            92 => {
                let count = instr.arg;
                let mut items = Vec::new();
                for _ in 0..count {
                    self.pos += 1;
                    let Some(inner) = self.cur().cloned() else {
                        return Err(String::from("truncated UNPACK_SEQUENCE"));
                    };
                    items.push(self.simple_target(inner.op, &inner)?);
                }
                Ok(Expr::Tuple(items))
            }
            _ => Err(format!("cannot use {} as a loop target", instr.name())),
        }
    }

    fn parse_loop(&mut self, instr: &Instr, depth: usize) -> Result<Stmt, String> {
        let Some(target) = instr.target else {
            return Err(String::from("SETUP_LOOP without target"));
        };
        let Some(end_index) = self.index_of(target) else {
            return Err(format!("SETUP_LOOP to unknown offset {target}"));
        };
        self.pos += 1;
        // The iterable of a `for` can span several instructions, so look for
        // whichever comes first: FOR_ITER or a condition jump.
        // A nested block (another SETUP_*) before any FOR_ITER or condition
        // means the loop we are looking at has no test of its own: it is a
        // `while 1:` whose body starts with that block.
        let block_end = (self.pos..end_index)
            .find(|&i| matches!(self.ins[i].op, 120 | 121 | 122 | 123 | 143))
            .unwrap_or(end_index);
        let head = (self.pos..block_end)
            .find(|&i| matches!(self.ins[i].op, 93 | 114 | 115))
            .unwrap_or(block_end);
        if std::env::var_os("UNMCPK_TRACE").is_some() {
            eprintln!("  b{:02} loop head={} end={} pos={}", self.id, head, end_index, self.pos);
        }
        // A `for` whose iterable is a conditional expression has a 114/115
        // before its FOR_ITER. Try the `for` reading first and fall back to
        // `while` if it does not fit: the other order used to misplace the head.
        let iterable_guard = self.ins.get(head).map(|i| matches!(i.op, 114 | 115)).unwrap_or(false)
            && (head..end_index)
                .find(|&i| self.ins[i].op == 93)
                .map_or(false, |for_index| {
                    self.ins[head]
                        .target
                        .map_or(false, |t| t < self.ins[for_index].offset)
                });
        if self.ins.get(head).map(|i| i.op) == Some(93) || iterable_guard {
            let for_head = if self.ins.get(head).map(|i| i.op) == Some(93) {
                head
            } else {
                (head..end_index)
                    .find(|&i| self.ins[i].op == 93)
                    .unwrap_or(head)
            };
            let saved_pos = self.pos;
            let saved_stack = self.stack.clone();
            match self.parse_for(for_head, end_index, target, depth) {
                Ok(stmt) => return Ok(stmt),
                Err(_) if self.ins.get(head).map(|i| i.op) != Some(93) => {
                    self.pos = saved_pos;
                    self.stack = saved_stack;
                }
                Err(err) => return Err(err),
            }
        }
        {
            self.parse_while(end_index, target, depth, Some(block_end))
        }
    }

    fn parse_for(
        &mut self,
        for_index: usize,
        end_index: usize,
        end_offset: usize,
        depth: usize,
    ) -> Result<Stmt, String> {
        let iterable_start = self.pos;
        let iter = match self.eval_iterable(iterable_start, for_index)? {
            Some(iter) => iter,
            // The iterable can hold a conditional expression (`for x in (a if c
            // else b)`), which only the structural pass can rebuild.
            None => match self.eval_value_structured(iterable_start, for_index)? {
                Some(iter) => iter,
                None => return Err(String::from("for loop without an iterable")),
            },
        };
        self.pos = for_index;
        let Some(for_iter) = self.cur().cloned() else {
            return Err(String::from("for loop without FOR_ITER"));
        };
        if for_iter.op != 93 {
            return Err(format!("expected FOR_ITER, found {}", for_iter.name()));
        }
        let Some(body_offset) = for_iter.target else {
            return Err(String::from("FOR_ITER without target"));
        };
        let Some(tail_index) = self.index_of(body_offset) else {
            return Err(format!("FOR_ITER to unknown offset {body_offset}"));
        };
        // `GET_ITER` may have left an iterator item behind; the evaluated
        // range covers it, so drop it if present.
        if let Some(Item::Iter(_)) = self.stack.last() {
            self.stack.pop();
        }
        self.pos += 1;
        let Some(store) = self.cur().cloned() else {
            return Err(String::from("for loop without a target"));
        };
        let target = self.simple_target(store.op, &store)?;
        self.pos += 1;
        let saved_head = self.loop_head;
        self.loop_head = Some(self.ins[for_index].offset);
        // The body ends with the back edge to FOR_ITER.
        let back_edge = tail_index.saturating_sub(1).max(self.pos);
        let body = self.parse_suite(Some(self.ins[back_edge].offset), depth + 1)?;
        self.loop_head = saved_head;
        let mut body = body;
        if self.terminated {
            self.terminated = false;
            if self.exit_target.take() == Some(end_offset) {
                body.push(Stmt::Break);
                self.pos += 1;
            }
            // Skip the structural jumps the compiler pads the block with.
            while self.pos < back_edge && self.cur().map(|i| i.is_unconditional()).unwrap_or(false) {
                self.pos += 1;
            }
        }
        // Structural padding can sit between the body and the back edge.
        while self.pos < back_edge
            && self
                .cur()
                .map(|i| i.op == 87 || i.is_unconditional())
                .unwrap_or(false)
        {
            self.pos += 1;
        }
        if self.pos < back_edge {
            return Err(format!(
                "for loop body stopped early at {}",
                self.ins[self.pos].offset
            ));
        }
        if matches!(body.last(), Some(Stmt::Continue)) {
            body.pop();
        }
        let head_offset = self.ins[for_index].offset;
        while self.pos < end_index
            && self
                .cur()
                .map(|i| {
                    i.is_unconditional()
                        && i.target.map_or(false, |t| t <= head_offset)
                })
                .unwrap_or(false)
        {
            self.pos += 1;
        }
        let orelse = self.parse_else_after(tail_index, end_offset, depth)?;
        self.pos = end_index;
        Ok(Stmt::For {
            target,
            iter,
            body,
            orelse,
        })
    }

    fn parse_while(
        &mut self,
        end_index: usize,
        end_offset: usize,
        depth: usize,
        test_limit: Option<usize>,
    ) -> Result<Stmt, String> {
        let test_start = self.pos;
        let saved_head = self.loop_head;
        self.loop_head = Some(self.ins[test_start].offset);
        let start_offset = self.ins[test_start].offset;
        // The loop's back edge: an absolute jump to the head or to the test.
        // The loop's back edge is the last jump in the block that returns to
        // the head: an `if` inside the body can jump back too, but the loop's
        // own one comes after everything else.
        let loop_back = self
            .ins
            .iter()
            .enumerate()
            .skip(test_start)
            .take(end_index.saturating_sub(test_start))
            .rev()
            .find(|(_, i)| i.op == 113 && i.target.map_or(false, |t| t <= start_offset))
            .map(|(index, _)| index);
        // A test is only a test when it evaluates to a single value before the
        // body starts; `while 1:` has none, and a conditional jump inside the
        // body belongs to an `if` instead.
        let candidate = loop_back.and_then(|back_edge| {
            let scan_end = test_limit.map_or(back_edge, |limit| limit.min(back_edge));
            (test_start..scan_end)
                .find(|&index| matches!(self.ins[index].op, 114 | 115))
                .map(|index| (index, back_edge))
        });
        let (test, back_edge, body_start) = match candidate {
            Some((jump_index, back_edge)) => {
                match self.eval_range(test_start, jump_index)? {
                    Some(test) => {
                        if std::env::var_os("UNMCPK_TRACE").is_some() {
                            eprintln!(
                                "  b{:02} while test_start={} jump={} start_offset={}",
                                self.id, test_start, jump_index, start_offset
                            );
                        }
                        let inverted = self.ins[jump_index].op == 115;
                        let test = if inverted { invert_test(test) } else { test };
                        (test, back_edge, jump_index + 1)
                    }
                    // No test: the jump we found is part of the body, so this
                    // is a `while 1:` loop that only leaves through `break`.
                    None => (
                        Expr::Const(crate::nemarshal::Value::Int(1)),
                        back_edge,
                        test_start,
                    ),
                }
            }
            // No conditional jump and no back edge either: the body leaves
            // only through `break` (or through an exception), so the loop has
            // no test and runs to the end of its block.
            None => (
                Expr::Const(crate::nemarshal::Value::Int(1)),
                loop_back.unwrap_or(end_index),
                test_start,
            ),
        };
        self.pos = body_start;
        if std::env::var_os("UNMCPK_TRACE").is_some() {
            eprintln!("  b{:02} while back_edge={} offset={}", self.id, back_edge, self.ins[back_edge].offset);
        }
        let body = self.parse_suite(Some(self.ins[back_edge].offset), depth + 1)?;
        self.loop_head = saved_head;
        let mut body = body;
        if self.terminated {
            self.terminated = false;
            if self.exit_target.take() == Some(end_offset) {
                body.push(Stmt::Break);
                self.pos += 1;
            }
            while self.pos < back_edge && self.cur().map(|i| i.is_unconditional()).unwrap_or(false) {
                self.pos += 1;
            }
        }
        // A body whose last `if` jumped straight back to the head leaves that
        // jump behind, in front of the loop's own back edge.
        while self.pos < back_edge
            && self
                .cur()
                .map(|i| {
                    i.is_unconditional() && i.target.map_or(false, |t| t <= start_offset)
                })
                .unwrap_or(false)
        {
            self.pos += 1;
        }
        if self.pos < back_edge {
            return Err(format!(
                "while loop body stopped early at {}",
                self.ins[self.pos].offset
            ));
        }
        if matches!(body.last(), Some(Stmt::Continue)) {
            body.pop();
        }

        // A `continue` compiled at the end of the body leaves a jump back to
        // the head behind the loop's own back edge.
        while self.pos < end_index
            && self
                .cur()
                .map(|i| {
                    i.is_unconditional()
                        && i.target
                            .map_or(false, |t| t <= self.ins[body_start].offset)
                })
                .unwrap_or(false)
        {
            self.pos += 1;
        }
        // A body that already broke out of the loop leaves nothing before the
        // loop's end, so there is no else clause to look for. A BREAK_LOOP that
        // follows the body belongs to the loop as well.
        let tail_offset = self
            .ins
            .get(back_edge)
            .map(|i| i.offset)
            .unwrap_or(usize::MAX);
        let mut probe = back_edge;
        while let Some(instr) = self.ins.get(probe) {
            let structural = instr.op == 87
                || (instr.is_unconditional()
                    && instr
                        .target
                        .map_or(false, |t| t == end_offset || t <= tail_offset));
            if !structural {
                break;
            }
            probe += 1;
        }
        let trailing_break = self.ins.get(probe).map_or(false, |i| i.op == 80);
        let orelse = if matches!(body.last(), Some(Stmt::Break)) {
            Vec::new()
        } else if trailing_break {
            body.push(Stmt::Break);
            self.pos = probe + 1;
            Vec::new()
        } else {
            self.parse_else_after(back_edge, end_offset, depth)?
        };
        self.pos = end_index;
        Ok(Stmt::While {
            test,
            body,
            orelse,
        })
    }

    /// A loop body is followed by POP_BLOCK and, when the source had an `else`
    /// clause, by that body before the loop's end offset.
    fn parse_else_after(
        &mut self,
        tail_index: usize,
        end_offset: usize,
        depth: usize,
    ) -> Result<Vec<Stmt>, String> {
        // Between a loop body and the code that follows, the compiler pads the
        // block with structural instructions: the loop's POP_BLOCK, a jump past
        // the `else` clause, and (for `while True:`) the back edge itself.
        let tail_offset = self.ins.get(tail_index).map(|i| i.offset).unwrap_or(usize::MAX);
        let mut pos = tail_index;
        loop {
            let Some(instr) = self.ins.get(pos) else { break };
            let structural = instr.op == 87
                || (instr.is_unconditional()
                    && instr
                        .target
                        .map_or(false, |t| t == end_offset || t <= tail_offset));
            if !structural {
                break;
            }
            pos += 1;
        }
        self.pos = pos;
        self.parse_suite(Some(end_offset), depth + 1)
    }

    fn parse_or_chain(
        &mut self,
        test: &Expr,
        target: usize,
        target_index: usize,
        depth: usize,
    ) -> Result<Option<Stmt>, String> {
        let mut parts = vec![test.clone()];
        let mut index = self.pos + 1;
        let mut else_offset = None;
        while index < target_index {
            let Some(link) = (index..target_index)
                .find(|&i| matches!(self.ins[i].op, 114 | 115))
            else {
                break;
            };
            let Ok(Some(value)) = self.eval_range(index, link) else {
                return Ok(None);
            };
            parts.push(value);
            if self.ins[link].op == 115 {
                index = link + 1;
            } else {
                else_offset = self.ins[link].target;
                break;
            }
        }
        let Some(else_offset) = else_offset else {
            return Ok(None);
        };
        let Some(else_index) = self.index_of(else_offset) else {
            return Ok(None);
        };
        // The jump that ends the body has to go forward, past the body: a
        // backwards jump is the loop of an inlined comprehension, not the end
        // of the arm.
        // The arm ends with the jump that skips past the else clause: it is
        // the last forward jump in the arm that lands beyond the else start.
        // Jumps inside the arm (an inlined conditional, a comprehension) land
        // before that and must not be mistaken for it.
        let body_end = (target_index..else_index).rfind(|&i| {
            self.ins[i].is_unconditional()
                && self
                    .ins[i]
                    .target
                    .map_or(false, |t| t > else_offset)
        });
        self.pos = target_index;
        if let Some(body_end) = body_end {
            let body = self.parse_suite(Some(self.ins[body_end].offset), depth + 1)?;
            let join_offset = self.ins[body_end].target.unwrap_or(else_offset);
            self.pos = else_index;
            let orelse = self.parse_suite(Some(join_offset), depth + 1)?;
            if let Some(join_index) = self.index_of(join_offset) {
                self.pos = join_index;
            }
            let _ = target;
            return Ok(Some(Stmt::If {
                test: Expr::Bool("or", parts),
                body,
                orelse,
            }));
        }
        // No jump at the end of the body means the arm leaves on its own (a
        // `return`, a `raise`, ...) and the false target is simply where the
        // following statements resume.
        let body = self.parse_suite(Some(else_offset), depth + 1)?;
        // Only an arm that ends the flow can stop at the false target; anything
        // else falls through and needs the plain `if` handling.
        if !matches!(
            body.last(),
            Some(Stmt::Return(_) | Stmt::Raise { .. } | Stmt::Break | Stmt::Continue)
        ) {
            return Ok(None);
        }
        self.pos = else_index;
        let _ = target;
        Ok(Some(Stmt::If {
            test: Expr::Bool("or", parts),
            body,
            orelse: Vec::new(),
        }))
    }

    fn parse_if(&mut self, stop: Option<usize>, depth: usize) -> Result<Stmt, String> {
        let jump = self.cur().cloned().unwrap();
        let invert = jump.op == 115; // POP_JUMP_IF_TRUE
        let raw_test = self.pop()?;
        let test = if invert {
            invert_test(raw_test.clone())
        } else {
            raw_test.clone()
        };
        let Some(target) = jump.target else {
            return Err("conditional jump without target".into());
        };
        let Some(target_index) = self.index_of(target) else {
            return Err(format!("conditional jump to unknown offset {target}"));
        };
        // A conditional jump to the loop head is the compiler's compact form of
        // a trailing `continue`: fall through only when the test holds.
        if self.loop_head == Some(target) {
            self.pos += 1;
            return Ok(Stmt::If {
                test: if invert { test } else { invert_test(test) },
                body: vec![Stmt::Continue],
                orelse: Vec::new(),
            });
        }
        // A jump past the end of this suite is a guard clause: the branch it
        // skips belongs to the enclosing block, not to this `if`, so there is no
        // `else` to rebuild here.
        if stop.map_or(false, |stop| target >= stop) {
            self.pos += 1;
            let body = self.parse_suite(stop, depth + 1)?;
            self.pos = target_index;
            return Ok(Stmt::If {
                test,
                body,
                orelse: Vec::new(),
            });
        }
        if target_index <= self.pos {
            // A backwards conditional is a loop guard this pass cannot
            // structure; looping on it would re-parse the same region for ever.
            return Err(format!("backwards conditional jump to {target}"));
        }
        // `if a or b:` compiles the first test as POP_JUMP_IF_TRUE straight to
        // the body; the remaining tests follow it, the chain ending with the
        // POP_JUMP_IF_FALSE that reaches the else clause.
        // Only a genuine chain continues with another jump that reaches the
        // same body; a plain `if a: ... else: ...` has its first inner jump
        // pointing at the else clause instead.
        let chain_link = (self.pos + 1..target_index)
            .find(|&i| matches!(self.ins[i].op, 114 | 115));
        let or_chain = jump.op == 115
            && chain_link.map_or(false, |link| match self.ins[link].op {
                // Another term that also reaches the body.
                115 => self.ins[link].target == Some(target),
                // The final term, jumping past the body to the else clause.
                114 => self.ins[link].target.map_or(false, |t| t > target),
                _ => false,
            });
        // A chain is only a chain when its final term jumps past the body to a
        // real else clause. Anything else (a body that ends in `return`, a
        // nested conditional, ...) has to fall back to the plain `if` shape
        // instead of failing the whole code object.
        let start_pos = self.pos;
        if or_chain {
            // In an `or` chain the first term is the value that reaches the
            // body when true, so it must not be the inverted test.
            if let Some(stmt) = self.parse_or_chain(&raw_test, target, target_index, depth)? {
                return Ok(stmt);
            }
            // The plain path below already holds the test value, so the stack
            // must not carry a second copy of it.
            self.pos = start_pos;
        }
        self.pos += 1;
        // If the instruction right before the target is an unconditional jump,
        // the then-arm ends there and an else-arm follows.
        let before_target = target_index.checked_sub(1).unwrap_or(0);
        let stop = self.block_stop;
        let tail_jump = self
            .ins
            .get(before_target)
            .filter(|i| i.is_unconditional())
            .cloned();
        let join = tail_jump
            .as_ref()
            .and_then(|i| i.target)
            // A join has to be a forward jump, and it cannot leave the block.
            .filter(|join| {
                // A join may sit exactly on the end of the block: the compiler
                // reuses the loop's back edge as the join of a trailing `if`.
                *join <= stop
                    && self
                        .index_of(*join)
                        .map_or(false, |join_index| join_index > target_index)
            });
        // A jump back to the loop head ends the then-arm with `continue`, so
        // the arm stops before it even though there is no else clause.
        let back_to_head = tail_jump
            .as_ref()
            .and_then(|i| i.target)
            .map_or(false, |t| t <= jump.offset);
        let then_end = if join.is_some() || back_to_head {
            before_target
        } else {
            target_index
        };
        let mut body = self.parse_suite(Some(self.ins[then_end].offset), depth + 1)?;
        self.pos = then_end;
        if back_to_head && join.is_none() {
            // The arm ends by jumping back to the loop head, which is what
            // `continue` compiles to.
            body.push(Stmt::Continue);
            self.pos = before_target + 1;
        }
        let mut orelse = Vec::new();
        if let Some(join_offset) = join {
            // Skip the jump that ends the then-arm.
            self.pos = target_index;
            let Some(join_index) = self.index_of(join_offset) else {
                return Err(format!("jump to unknown offset {join_offset}"));
            };
            orelse = self.parse_suite(Some(join_offset), depth + 1)?;
            self.pos = join_index;
        } else {
            self.pos = target_index;
        }
        Ok(Stmt::If {
            test,
            body,
            orelse,
        })
    }

    // ----------------------------------------------------------- statements

    fn parse_statement(&mut self, depth: usize) -> Result<Option<Stmt>, String> {
        loop {
            let Some(instr) = self.cur().cloned() else {
                return Ok(None);
            };
            // Offsets only grow inside a suite, so a jump that lands past the
            // requested end must stop the range as well.
            if self.limit.map_or(false, |limit| instr.offset >= limit) {
                return Ok(None);
            }
            // Block statements can begin in the middle of an expression pass
            // (the value they consume is already on the stack).
            match instr.op {
                120 => return self.parse_loop(&instr, depth).map(Some),
                121 => return self.parse_try_except(depth).map(Some),
                122 => return self.parse_try_finally(depth).map(Some),
                143 => return self.parse_with(depth).map(Some),
                123 => return Err(format!("unsupported block opcode {}", instr.name())),
                _ => {}
            }
            // A conditional jump with a value underneath it is usually an
            // inline `a if c else b`; only when that does not fit does the jump
            // belong to a statement (`if`/`while`) and to the suite loop.
            if matches!(instr.op, 114 | 115) {
                if self.probe && !self.stack.is_empty() && self.ternary_shape() {
                    if let Some(expr) = self.try_ternary_expression()? {
                        self.stack.push(Item::Expr(expr));
                        continue;
                    }
                }
                return Ok(None);
            }
            // A POP_BLOCK left over from a block an enclosing construct already
            // consumed is structural padding, not a statement.
            if instr.op == 87 {
                self.pos += 1;
                continue;
            }
            // `END_FINALLY` closes a handler block; when the handler body already
            // jumped away (a `return` then a jump over the epilogue), what is
            // left here is padding for the enclosing structure.
            if instr.op == 88 {
                self.pos += 1;
                continue;
            }
            if matches!(instr.op, 111 | 112) {
                self.parse_bool_chain()?;
                continue;
            }
            // `[e for x in it]` is compiled inline in 2.7: BUILD_LIST, iterate,
            // LIST_APPEND, with no SETUP_LOOP around it.
            if instr.op == 93 {
                match self.parse_comp_loop(depth)? {
                    Some(stmt) => return Ok(Some(stmt)),
                    None => continue,
                }
            }
            if instr.is_unconditional() && instr.target == self.loop_head {
                self.pos += 1;
                self.flush_pending_assign()?;
                return Ok(Some(Stmt::Continue));
            }
            // An unconditional jump onto BREAK_LOOP is a `break` (the compiler
            // emits one after a body that already ended with `continue`).
            if instr.is_unconditional()
                && instr
                    .target
                    .and_then(|target| self.index_of(target))
                    .map_or(false, |index| self.ins[index].op == 80)
            {
                self.pos += 1;
                self.flush_pending_assign()?;
                return Ok(Some(Stmt::Break));
            }
            if instr.is_unconditional() {
                // A jump that leaves the enclosing block ends it; the block
                // owner decides what it means (break, if-join, ...).
                let stop = self.block_stop;
                if let Some(target) = instr.target {
                    if target >= stop {
                        if std::env::var_os("UNMCPK_TRACE").is_some() {
                            eprintln!("  b{:02} terminate at {} -> {} (stop {})", self.id, instr.offset, target, stop);
                        }
                        self.terminated = true;
                        self.exit_target = Some(target);
                        return Ok(None);
                    }
                }
                // A jump to the instruction right after it changes nothing;
                // the compiler leaves these behind where it had to keep a
                // branch target (a store shared by both arms, a block inside
                // `try`). Consuming it is always correct.
                if instr.target == self.ins.get(self.pos + 1).map(|i| i.offset) {
                    self.pos += 1;
                    continue;
                }
                // The same, one step further: a jump that only hops over
                // unreachable jumps of its own branch.
                if let Some(target) = instr.target {
                    let mut index = self.pos + 1;
                    while self.ins.get(index).map(|i| i.is_unconditional()).unwrap_or(false) {
                        index += 1;
                    }
                    if index > self.pos + 1 && self.ins.get(index).map(|i| i.offset) == Some(target) {
                        self.pos = index;
                        continue;
                    }
                }
                // Otherwise it is a construct this pass cannot model.
                return Err(format!("unexpected {} at {}", instr.name(), instr.offset));
            }
            if let Some(stmt) = self.apply(&instr, depth)? {
                self.pos += 1;
                return Ok(Some(stmt));
            }
            self.pos += 1;
        }
    }

    /// Execute one instruction. Returns a statement when it consumes a value
    /// into one.
    fn apply(&mut self, instr: &Instr, depth: usize) -> Result<Option<Stmt>, String> {
        let offset = instr.offset;
        let name = instr.name();
        self.apply_inner(instr, depth)
            .map_err(|err| format!("{err} (in {name} at {offset})"))
    }

    fn apply_inner(&mut self, instr: &Instr, depth: usize) -> Result<Option<Stmt>, String> {
        let op = instr.op;
        if std::env::var_os("UNMCPK_TRACE").is_some() {
            eprintln!("  b{:02} stack[{}] {instr:?}", self.id, self.stack.len());
        }
        match op {
            9 => {}                              // NOP
            1 => {
                // POP_TOP: expression statement, or the discarded result of a
                // statement-like construct.
                match self.stack.last().cloned() {
                    Some(Item::FromModule { module, level }) => {
                        self.stack.pop();
                        let pending = self.pending_import.take().unwrap_or(PendingImport {
                            level,
                            names: Vec::new(),
                            star: false,
                        });
                        if pending.names.is_empty() {
                            return Ok(Some(Stmt::FromImport {
                                module,
                                level: level.max(0) as usize,
                                names: Vec::new(),
                                star: true,
                            }));
                        }
                        return Ok(Some(Stmt::FromImport {
                            module,
                            level: level.max(0) as usize,
                            names: pending.names,
                            star: pending.star,
                        }));
                    }
                    Some(Item::Code(_)) => {
                        self.stack.pop();
                        return Ok(None);
                    }
                    _ => {}
                }
                let value = self.pop()?;
                return Ok(Some(self.finish_value_statement(value)?));
            }
            0 => return Ok(Some(Stmt::Pass)),
            2 => {
                // ROT_TWO
                let a = self.pop()?;
                let b = self.pop()?;
                self.push(a);
                self.push(b);
            }
            3 => {
                // ROT_THREE
                let a = self.pop()?;
                let b = self.pop()?;
                let c = self.pop()?;
                self.push(a);
                self.push(c);
                self.push(b);
            }
            4 => {
                // DUP_TOP
                let value = self.pop()?;
                self.dup_pending += 1;
                self.push(value.clone());
                self.push(value);
            }
            // DUP_TOPX n: duplicate the top n items (`a[:] = b` and chained
            // subscript stores). The argument is a count, not an index.
            99 => {
                let count = instr.arg as usize;
                if count == 0 || count > self.stack.len() {
                    return Err(format!(
                        "DUP_TOPX {count} with only {} values",
                        self.stack.len()
                    ));
                }
                let start = self.stack.len() - count;
                let duplicated: Vec<Item> = self.stack[start..].to_vec();
                self.stack.extend(duplicated);
            }
            5 => {
                // ROT_FOUR
                let a = self.pop()?;
                let b = self.pop()?;
                let c = self.pop()?;
                let d = self.pop()?;
                self.push(a);
                self.push(d);
                self.push(c);
                self.push(b);
            }
            10..=15 => {
                let operand = self.pop()?;
                let name = super::opcodes::unary_op(op).ok_or("bad unary")?;
                let expr = if name == "not " {
                    Expr::Not(Box::new(operand))
                } else if name == "`" {
                    Expr::ReprExpr(Box::new(operand))
                } else {
                    Expr::Unary(name, Box::new(operand))
                };
                self.push(expr);
            }
            19..=24 | 26..=29 | 55..=59 | 62..=67 | 75..=79 => {
                let right = self.pop()?;
                let left = self.pop()?;
                if let Some(name) = super::opcodes::binary_op(op) {
                    return self.binary(left, right, name);
                }
                if let Some(name) = super::opcodes::inplace_op(op) {
                    self.pending_aug = Some((left.clone(), name, right));
                    self.push(left);
                    return Ok(None);
                }
                return Err(format!("bad binary opcode {op}"));
            }
            107 => {
                // COMPARE_OP
                let right = self.pop()?;
                let left = self.pop()?;
                let name = super::opcodes::compare_op(instr.arg);
                if name == "exception match" {
                    return Err("bare exception match".into());
                }
                self.push(Expr::Compare(vec![left, right], vec![name]));
            }
            30..=33 => {
                // SLICE+n
                let base = self.pop()?;
                let hi = if op == 32 || op == 33 {
                    Some(Box::new(self.pop()?))
                } else {
                    None
                };
                let lo = if op == 31 || op == 33 {
                    Some(Box::new(self.pop()?))
                } else {
                    None
                };
                self.push(Expr::Slice(Box::new(base), lo, hi, None));
            }
            133 => {
                // BUILD_SLICE
                let step = if instr.arg == 3 {
                    Some(self.pop()?)
                } else {
                    None
                };
                let hi = self.pop()?;
                let lo = self.pop()?;
                self.stack.push(Item::Slice {
                    lo: Some(lo),
                    hi: Some(hi),
                    step,
                });
            }
            25 => {
                // BINARY_SUBSCR
                let index = self.pop_item()?;
                let base = self.pop()?;
                let expr = subscr(base, index)?;
                self.push(expr);
            }
            100 => {
                let value = self
                    .code
                    .constants
                    .get(instr.arg as usize)
                    .cloned()
                    .ok_or_else(|| format!("constant index {} out of range", instr.arg))?;
                match value {
                    Value::Code(code) => self.stack.push(Item::Code(code)),
                    other => self.push(Expr::Const(other)),
                }
            }
            101 | 116 => {
                let name = self.name_at(instr.arg)?;
                self.push(Expr::Name(name));
            }
            // LOAD_FAST and the cell opcodes index their own tables, never the
            // module's name table.
            124 => {
                let name = self.var_at(instr.arg)?;
                self.push(Expr::Name(name));
            }
            136 | 135 => {
                let name = self.cell_at(instr.arg)?;
                self.push(Expr::Name(name));
            }
            82 => {
                // LOAD_LOCALS: the frame's locals dict, as `locals()` spells it.
                self.push(Expr::Call {
                    func: Box::new(Expr::Name("locals".into())),
                    args: Vec::new(),
                    star: None,
                    kwargs: Vec::new(),
                    star_star: None,
                });
            }
            106 => {
                let base = self.pop()?;
                let attr = self.name_at(instr.arg)?;
                match base {
                    // `import a.b.c as x` reaches its binding through the
                    // attribute chain, so an import keeps collecting attributes.
                    Expr::ImportModule { module, mut attrs } => {
                        attrs.push(attr);
                        self.push(Expr::ImportModule { module, attrs });
                    }
                    other => self.push(Expr::Attribute(Box::new(other), attr)),
                }
            }
            102 | 103 | 104 => {
                let n = instr.arg as usize;
                let mut items = Vec::with_capacity(n);
                for _ in 0..n {
                    items.push(self.pop()?);
                }
                items.reverse();
                self.push(match op {
                    102 => Expr::Tuple(items),
                    103 => Expr::List(items),
                    _ => Expr::Set(items),
                });
            }
            105 => {
                // BUILD_MAP: arg is a hint only; pairs arrive via STORE_MAP.
                self.push(Expr::Dict(Vec::new()));
            }
            54 => {
                // STORE_MAP
                let key = self.pop()?;
                let value = self.pop()?;
                match self.stack.last_mut() {
                    Some(Item::Expr(Expr::Dict(pairs))) => pairs.push((key, value)),
                    _ => {
                        // Report what is on the stack: this shape only appears
                        // when something earlier did not balance.
                        let kinds: Vec<&str> = self
                            .stack
                            .iter()
                            .rev()
                            .take(4)
                            .map(|item| match item {
                                Item::Expr(Expr::Dict(_)) => "dict",
                                Item::Expr(_) => "expr",
                                Item::Code(_) => "code",
                                Item::Class { .. } => "class",
                                Item::FromModule { .. } => "module",
                                Item::FromAttr(_) => "attribute",
                                Item::Iter(_) => "iterator",
                                Item::Unpack { .. } => "unpack",
                                Item::Slice { .. } => "slice",
                            })
                            .collect();
                        return Err(format!(
                            "STORE_MAP without a dictionary (stack: {:?})",
                            kinds
                        ));
                    }
                }
            }
            68 => {
                // GET_ITER: the iterable stays on the stack in a form only
                // FOR_ITER may consume.
                let iterable = self.pop()?;
                self.stack.push(Item::Iter(Box::new(iterable)));
            }
            94 => {
                // LIST_APPEND (comprehension); not modelled yet
                return Err("list comprehension".into());
            }
            70 | 71 | 73 => self.handle_print(instr)?,
            72 | 74 => {
                if instr.op == 74 {
                    // `print >> f` pops the destination stream with the newline.
                    self.print_dest = Some(self.pop()?);
                }
                self.print_newline = true;
                if let Some(stmt) = self.flush_print() {
                    return Ok(Some(stmt));
                }
            }
            84 => {
                // IMPORT_STAR consumes the module object: `from x import *`.
                self.pending_import = None;
                match self.stack.pop() {
                    Some(Item::FromModule { module, level }) => {
                        return Ok(Some(Stmt::FromImport {
                            module,
                            level: level.max(0) as usize,
                            names: Vec::new(),
                            star: true,
                        }));
                    }
                    _ => return Err(String::from("IMPORT_STAR without a module")),
                }
            }
            108 => {
                // IMPORT_NAME: the stack is [level, fromlist]; the name is in
                // this code object's names.
                let fromlist = self.pop_opt();
                let level = match self.pop_opt() {
                    Some(Expr::Const(Value::Int(v))) => v,
                    Some(Expr::Const(Value::Int64(v))) => v as i32,
                    _ => -1,
                };
                let module = self.name_at(instr.arg)?;
                let has_from = matches!(
                    fromlist,
                    Some(Expr::Const(Value::Tuple(_))) | Some(Expr::Const(Value::List(_)))
                );
                if has_from {
                    self.pending_import = Some(PendingImport {
                        level,
                        names: Vec::new(),
                        star: false,
                    });
                    self.stack.push(Item::FromModule { module, level });
                } else {
                    self.push(Expr::ImportModule {
                        module,
                        attrs: Vec::new(),
                    });
                }
            }
            109 => {
                // IMPORT_FROM: an attribute of the module left below it.
                let name = self.name_at(instr.arg)?;
                self.stack.push(Item::FromAttr(name));
            }
            89 => {
                // BUILD_CLASS takes [name, bases, body result]. The body is the
                // function the compiler made from the class body code object,
                // already called to produce the namespace dict.
                let body = self.pop()?;
                let bases = self.pop()?;
                let name = self.pop()?;
                let name = match &name {
                    Expr::Const(value) => const_name(value)
                        .ok_or_else(|| "class name is not a string".to_string())?,
                    _ => return Err("class name is not a constant".into()),
                };
                let code = match body {
                    Expr::Call {
                        func,
                        args,
                        star,
                        kwargs,
                        star_star,
                    } if args.is_empty()
                        && star.is_none()
                        && kwargs.is_empty()
                        && star_star.is_none() =>
                    {
                        match *func {
                            Expr::Lambda(deferred) => deferred.code,
                            _ => return Err("class body is not a function".into()),
                        }
                    }
                    Expr::Lambda(deferred) => deferred.code,
                    _ => return Err("class body is not a function".into()),
                };
                let bases = match bases {
                    Expr::Tuple(items) => items,
                    other => vec![other],
                };
                self.stack.push(Item::Class {
                    name,
                    bases,
                    decorators: Vec::new(),
                    code,
                });
            }
            85 => {
                // EXEC_STMT
                let locals = self.pop()?;
                let globals = self.pop()?;
                let code = self.pop()?;
                return Ok(Some(Stmt::Expr(Expr::Unknown(format!(
                    "exec {}, {}, {}",
                    code.render(),
                    globals.render(),
                    locals.render()
                )))));
            }
            86 => {
                let value = self.pop_opt();
                self.push(Expr::Yield(value.map(Box::new)));
            }
            83 => {
                // RETURN_VALUE
                let value = self.pop_opt();
                let value = match value {
                    Some(Expr::Const(Value::None)) | None => None,
                    other => other,
                };
                self.flush_pending_assign()?;
                return Ok(Some(Stmt::Return(value)));
            }
            130 => {
                let argc = instr.arg;
                let mut parts = Vec::new();
                for _ in 0..argc {
                    parts.push(self.pop()?);
                }
                parts.reverse();
                return Ok(Some(Stmt::Raise {
                    kind: parts.first().cloned(),
                    value: parts.get(1).cloned(),
                    traceback: parts.get(2).cloned(),
                }));
            }
            131 | 140 | 141 | 142 => {
                self.build_call(instr)?;
            }
            132 | 134 => {
                self.build_definition(instr)?;
            }
            92 => {
                // UNPACK_SEQUENCE. A second unpack right after the first one
                // unpacks one of its elements (`msg, (a, b) = value.args`), so
                // the outer item keeps the real value and this one is folded
                // into it as a tuple target when it completes.
                let value = match self.stack.last() {
                    Some(Item::Unpack { .. }) => Expr::Const(crate::nemarshal::Value::None),
                    _ => self.pop()?,
                };
                self.stack.push(Item::Unpack {
                    value,
                    targets: Vec::new(),
                    remaining: instr.arg as usize,
                });
            }
            // 44..=47 DELETE_SLICE+0..3
            44..=47 => {
                // `del w[v:]` pushes the object first and the bounds on top of
                // it, in the order low, high, step.
                // The packer's own opcode naming is one slice operand ahead of
                // what the code pushes, so take what is actually there: the
                // object plus at most the declared number of bounds.
                let declared = (op - 44) as usize;
                let count = declared.min(self.stack.len().saturating_sub(1));
                let mut bounds = Vec::new();
                for _ in 0..count {
                    bounds.push(self.pop()?);
                }
                bounds.reverse();
                let object = self.pop()?;
                let bound = |index: usize| bounds.get(index).cloned().map(Box::new);
                let target = Expr::Slice(
                    Box::new(object),
                    bound(0),
                    bound(1),
                    if count > 2 { bound(2) } else { None },
                );
                return Ok(Some(Stmt::Del(vec![target])));
            }
            // 61 DELETE_SUBSCR
            61 => {
                let index = self.pop()?;
                let object = self.pop()?;
                let target = Expr::Subscript(Box::new(object), Box::new(index));
                return Ok(Some(Stmt::Del(vec![target])));
            }
            // 96 DELETE_ATTR
            96 => {
                let object = self.pop()?;
                let name = self.name_at(instr.arg)?;
                let target = Expr::Attribute(Box::new(object), name);
                return Ok(Some(Stmt::Del(vec![target])));
            }
            // 91 DELETE_NAME, 98 DELETE_GLOBAL
            91 | 98 => {
                let name = self.name_at(instr.arg)?;
                return Ok(Some(Stmt::Del(vec![Expr::Name(name)])));
            }
            // 126 DELETE_FAST
            126 => {
                let name = self.var_at(instr.arg)?;
                return Ok(Some(Stmt::Del(vec![Expr::Name(name)])));
            }
            _ => {
                if let Some(target) = self.store_target(op, instr)? {
                    return Ok(Some(target));
                }
                if matches!(
                    op,
                    40..=43 | 60 | 61 | 90 | 91 | 95 | 96 | 97 | 98 | 125 | 126 | 136 | 137
                ) {
                    return Ok(None);
                }
                return Err(format!("unhandled opcode {} (byte {}) at {}", instr.name(), op, instr.offset));
            }
        }
        let _ = depth;
        Ok(None)
    }

    fn finish_value_statement(&mut self, value: Expr) -> Result<Stmt, String> {
        // A discarded call is an expression statement; a discarded bare value is
        // dead code, which the compiler also emits, so keep it visible.
        self.flush_print();
        Ok(Stmt::Expr(value))
    }

    fn binary(&mut self, left: Expr, right: Expr, name: &'static str) -> Result<Option<Stmt>, String> {
        // Flatten `and`/`or` producers are jumps, not opcodes, so a plain
        // binary operator just folds.
        self.push(Expr::BinOp(name, Box::new(left), Box::new(right)));
        Ok(None)
    }

    fn handle_print(&mut self, instr: &Instr) -> Result<(), String> {
        match instr.op {
            70 => {
                // PRINT_EXPR: interactive echo, keep as an expression statement
                let value = self.pop()?;
                self.print_dest = None;
                self.printed.push((None, value));
                self.print_newline = true;
            }
            71 => {
                let value = self.pop()?;
                self.printed.push((None, value));
            }
            72 => self.print_newline = true,
            73 => {
                let dest = self.pop()?;
                let value = self.pop()?;
                self.printed.push((Some(dest), value));
            }
            74 => {
                self.print_dest = Some(self.pop()?);
                self.print_newline = true;
            }
            _ => {}
        }
        Ok(())
    }

    fn flush_print(&mut self) -> Option<Stmt> {
        if self.printed.is_empty() && !self.print_newline {
            return None;
        }
        let dest = self
            .print_dest
            .take()
            .or_else(|| self.printed.first().and_then(|(d, _)| d.clone()));
        let items: Vec<Expr> = self.printed.drain(..).map(|(_, v)| v).collect();
        let newline = std::mem::replace(&mut self.print_newline, false);
        Some(Stmt::Print {
            dest,
            items,
            newline,
        })
    }

    /// Rebuild a call. The 2.7 stack layout, bottom up, is
    /// `func, positional..., (keyword name, keyword value)*, *args, **kwargs`,
    /// where the star operands are only present for the `CALL_FUNCTION_*`
    /// variants that use them.
    /// Rebuild `[element for target in iter if ...]` from the synthetic code
    /// object the compiler emits for a comprehension.
    fn build_comprehension(
        &self,
        code: &Code,
        iter: Expr,
        kind: CompKind,
    ) -> Result<Expr, String> {
        let mut builder = Builder::new(code);
        // Skip the container creation (list/set/dict) and the `LOAD_FAST .0`
        // that receives the iterator.
        while let Some(instr) = builder.cur() {
            if instr.op == 93 {
                break;
            }
            builder.pos += 1;
        }
        let Some(for_iter) = builder.cur().cloned() else {
            return Err(String::from("comprehension without FOR_ITER"));
        };
        builder.pos += 1;
        let Some(store) = builder.cur().cloned() else {
            return Err(String::from("comprehension without its target"));
        };
        let target = builder.simple_target(store.op, &store)?;
        builder.pos += 1;
        let head = builder.ins.iter().position(|i| i.offset == for_iter.offset);
        let mut ifs = Vec::new();
        loop {
            let start = builder.pos;
            let jump_index = builder
                .ins
                .iter()
                .enumerate()
                .skip(start)
                .take_while(|(_, i)| !matches!(i.op, 86 | 94 | 146 | 147 | 54))
                .find(|(_, i)| matches!(i.op, 114 | 115))
                .map(|(index, _)| index);
            let Some(jump_index) = jump_index else {
                break;
            };
            // `... POP_JUMP_IF_FALSE <loop head>` guards the element.
            let Some(condition) = builder.eval_range(start, jump_index)? else {
                return Err(String::from("comprehension without a condition"));
            };
            let _ = head;
            ifs.push(condition);
            builder.pos = jump_index + 1;
        }
        let element_end = builder
            .ins
            .iter()
            .enumerate()
            .skip(builder.pos)
            .find(|(_, i)| matches!(i.op, 86 | 94 | 146 | 147 | 54))
            .map(|(index, _)| index)
            .ok_or_else(|| format!("comprehension with no element end at {}", builder.offset()))?;
        // A dict comprehension pushes its key and its value, then STORE_MAP.
        let element = if matches!(builder.ins[element_end].op, 54 | 147) {
            let values = builder.eval_values_relaxed(builder.pos, element_end)?;
            match values.as_slice() {
                [key, value] => Expr::Tuple(vec![key.clone(), value.clone()]),
                [.., key, value] => Expr::Tuple(vec![key.clone(), value.clone()]),
                _ => {
                    return Err(format!(
                        "dict comprehension without a pair at {}..{}",
                        builder.offset(),
                        builder.ins[element_end].offset
                    ))
                }
            }
        } else {
            let Some(element) = builder.eval_range(builder.pos, element_end)? else {
                return Err(String::from("comprehension without an element value"));
            };
            element
        };
        Ok(Expr::Comp(Box::new(Comp {
            kind,
            element,
            clauses: vec![Clause { target, iter, ifs }],
        })))
    }

    fn build_call(&mut self, instr: &Instr) -> Result<(), String> {
        let arg = instr.arg;
        let npos = (arg & 0xff) as usize;
        let nkw = ((arg >> 8) & 0xff) as usize;
        // `**kwargs` sits above `*args` when both are present.
        let star_star = if matches!(instr.op, 141 | 142) {
            Some(self.pop()?)
        } else {
            None
        };
        let star = if matches!(instr.op, 140 | 142) {
            Some(self.pop()?)
        } else {
            None
        };
        let mut kwargs: Vec<(Option<String>, Expr)> = Vec::new();
        for _ in 0..nkw {
            let value = self.pop()?;
            let name = self.pop()?;
            match name.as_const().and_then(|c| keyword_name(c)) {
                Some(name) => kwargs.push((Some(name), value)),
                None => {
                    return Err("keyword argument without a name".into());
                }
            }
        }
        kwargs.reverse();
        let mut args = Vec::with_capacity(npos);
        for _ in 0..npos {
            // A generator expression is passed its iterator: `join(x for x in
            // items)` compiles to `<genexpr>; <items>; GET_ITER; CALL_FUNCTION`.
            let item = self.pop_item()?;
            args.push(match item {
                Item::Iter(expr) => Item::Expr(*expr),
                other => other,
            });
        }
        args.reverse();
        // `[e for x in it]` compiles to `<listcomp>; it; GET_ITER; CALL_FUNCTION 1`,
        // so the code object is argument 0 and its iterator argument 1.
        if nkw == 0 && star.is_none() && star_star.is_none() && args.len() == 2 {
            let inner = match &args[0] {
                Item::Expr(Expr::Lambda(deferred)) => Some(deferred.code.clone()),
                _ => None,
            };
            if let Some(inner) = inner {
                if let Some(kind) = comprehension_kind(&inner) {
                    let iter = match &args[1] {
                        Item::Expr(expr) => expr.clone(),
                        _ => return Err(String::from("comprehension without its iterable")),
                    };
                    let comp = self.build_comprehension(&inner, iter, kind)?;
                    args.truncate(1);
                    args[0] = Item::Expr(comp);
                }
            }
        }
        // `[e for x in it]` compiles to `<listcomp>; it; GET_ITER; CALL_FUNCTION 1`,
        // i.e. the code object is the callee and it is handed its iterator.
        if nkw == 0 && star.is_none() && star_star.is_none() && args.len() == 1 {
            let lambda = match self.stack.last() {
                Some(Item::Expr(Expr::Lambda(deferred))) => Some(deferred.code.clone()),
                _ => None,
            };
            if let Some(inner) = lambda {
                if let Some(kind) = comprehension_kind(&inner) {
                    // A shape this pass cannot model must not take the whole
                    // module down: keep the call as it was.
                    if let Item::Expr(iter) = &args[0] {
                        if let Ok(comp) = self.build_comprehension(&inner, iter.clone(), kind) {
                            self.stack.pop();
                            self.stack.push(Item::Expr(comp));
                            return Ok(());
                        }
                    }
                }
            }
        }
        // `@deco class C:` applies the decorator to the freshly built class.
        if nkw == 0
            && star.is_none()
            && star_star.is_none()
            && npos == 1
            && matches!(args.first(), Some(Item::Class { .. }))
        {
            if let Item::Class {
                name,
                bases,
                mut decorators,
                code,
            } = args.remove(0)
            {
                let decorator = match self.pop_item()? {
                    Item::Expr(expr) => expr,
                    other => return Err(format!("decorator is not an expression: {other:?}")),
                };
                decorators.insert(0, decorator);
                self.stack.push(Item::Class {
                    name,
                    bases,
                    decorators,
                    code,
                });
                return Ok(());
            }
            // Not a decorator; put the argument back the way it was.
            let func = self.pop()?;
            self.push(Expr::Call {
                func: Box::new(func),
                args: args
                    .into_iter()
                    .map(|item| match item {
                        Item::Expr(expr) => Ok(expr),
                        other => Err(format!("expected a value, found {other:?}")),
                    })
                    .collect::<Result<Vec<_>, String>>()?,
                star: star.map(Box::new),
                kwargs,
                star_star: star_star.map(Box::new),
            });
            return Ok(());
        }
        let mut exprs = Vec::with_capacity(args.len());
        for item in args {
            match item {
                Item::Expr(expr) => exprs.push(expr),
                other => return Err(format!("expected a value, found {other:?}")),
            }
        }
        let func = self.pop()?;
        self.push(Expr::Call {
            func: Box::new(func),
            args: exprs,
            star: star.map(Box::new),
            kwargs,
            star_star: star_star.map(Box::new),
        });
        Ok(())
    }

    fn build_definition(&mut self, instr: &Instr) -> Result<(), String> {
        // The 2.7 stack for `def f(a=1)` is [defaults..., code]. MAKE_CLOSURE
        // inserts the free-variable tuple *under* the code constant, so it has
        // to be popped after the code and before the defaults.
        let code = match self.stack.pop() {
            Some(Item::Code(code)) => *code,
            Some(other) => {
                self.stack.push(other);
                return Err("MAKE_FUNCTION without a code constant".into());
            }
            None => return Err("MAKE_FUNCTION without a code constant".into()),
        };
        if instr.op == 134 {
            let _closure_cells = self.pop_opt();
        }
        // The argument counts defaults, and they sit on the stack as separate
        // values (`def f(a, b=False, c=40)` pushes `False`, `40`, then the
        // code), so pop one per default and restore source order.
        let mut defaults = Vec::new();
        for _ in 0..instr.arg {
            let Some(value) = self.pop_opt() else {
                return Err("MAKE_FUNCTION without its defaults".into());
            };
            defaults.insert(0, value);
        }
        let expr = Expr::Lambda(Box::new(Deferred {
            code: Box::new(code),
            defaults,
            keyword_defaults: None,
        }));
        self.push(expr);
        Ok(())
    }

    fn name_at(&self, index: u32) -> Result<String, String> {
        let index = index as usize;
        self.code
            .names
            .get(index)
            .or_else(|| self.code.var_names.get(index))
            .or_else(|| self.code.cell_vars.get(index))
            .or_else(|| self.code.free_vars.get(index))
            .map(|b| disasm_text(b))
            .ok_or_else(|| format!("name index {index} out of range"))
    }

    /// A closure index counts cell variables first and free variables after
    /// them, because that is the order `MAKE_CLOSURE` packs the tuple in.
    fn cell_at(&self, index: u32) -> Result<String, String> {
        let index = index as usize;
        if let Some(name) = self.code.cell_vars.get(index) {
            return Ok(disasm_text(name));
        }
        if let Some(name) = self
            .code
            .free_vars
            .get(index.saturating_sub(self.code.cell_vars.len()))
        {
            return Ok(disasm_text(name));
        }
        Err(format!(
            "cell index {index} out of range ({} cell, {} free)",
            self.code.cell_vars.len(),
            self.code.free_vars.len()
        ))
    }

    fn var_at(&self, index: u32) -> Result<String, String> {
        self.code
            .var_names
            .get(index as usize)
            .map(|b| disasm_text(b))
            .ok_or_else(|| format!("var index {index} out of range"))
    }

    /// Stores produce assignments; the target expression is derived from the
    /// opcode and the value from the stack.
    fn store_target(&mut self, op: u8, instr: &Instr) -> Result<Option<Stmt>, String> {
        // Import bookkeeping comes first: an import is a statement, and its
        // value never arrives on the stack as a value.
        match self.stack.last().cloned() {
            Some(Item::FromAttr(name)) => {
                self.stack.pop();
                let stored = self.bound_name(op, instr)?;
                let alias = match stored {
                    Some(stored) if stored != name => Some(stored),
                    _ => None,
                };
                match self.pending_import.as_mut() {
                    Some(pending) => pending.names.push((name, alias)),
                    None => return Err("IMPORT_FROM without IMPORT_NAME".into()),
                }
                return Ok(None);
            }
            _ => {}
        }
        let target = match op {
            90 | 97 | 125 | 137 => {
                let name = if op == 125 {
                    self.var_at(instr.arg)?
                } else {
                    self.name_at(instr.arg)?
                };
                Expr::Name(name)
            }
            95 => {
                let object = self.pop()?;
                Expr::Attribute(Box::new(object), self.name_at(instr.arg)?)
            }
            60 => {
                let index = self.pop_item()?;
                let object = self.pop()?;
                subscr(object, index)?
            }
            40..=43 => {
                let object = self.pop()?;
                let hi = if op == 42 || op == 43 {
                    Some(Box::new(self.pop()?))
                } else {
                    None
                };
                let lo = if op == 41 || op == 43 {
                    Some(Box::new(self.pop()?))
                } else {
                    None
                };
                Expr::Slice(Box::new(object), lo, hi, None)
            }
            _ => return Ok(None),
        };
        self.commit_store(target)
    }

    /// The name a store opcode writes to, when it writes to a name.
    fn bound_name(&self, op: u8, instr: &Instr) -> Result<Option<String>, String> {
        match op {
            90 | 97 => self.name_at(instr.arg).map(Some),
            125 | 137 => self.var_at(instr.arg).map(Some),
            _ => Ok(None),
        }
    }

    fn commit_store(&mut self, target: Expr) -> Result<Option<Stmt>, String> {
        if let Some(Item::Class { .. }) = self.stack.last() {
            if let Some(Item::Class {
                name,
                bases,
                decorators,
                code,
            }) = self.stack.pop()
            {
                return Ok(Some(Stmt::ClassDef(Box::new(ClassDef {
                    name,
                    bases,
                    decorators,
                    code,
                }))));
            }
        }
        // Pending unpack: this store is one of the targets.
        let mut target = target;
        loop {
            let done = match self.stack.last_mut() {
                Some(Item::Unpack {
                    targets, remaining, ..
                }) => {
                    targets.push(target.clone());
                    *remaining -= 1;
                    *remaining == 0
                }
                _ => break,
            };
            if !done {
                return Ok(None);
            }
            let Some(Item::Unpack { value, targets, .. }) = self.stack.pop() else {
                unreachable!("unpack item vanished");
            };
            target = Expr::Tuple(targets);
            // A nested unpack belongs to the item underneath as one target.
            if !matches!(self.stack.last(), Some(Item::Unpack { .. })) {
                return self.emit_assign(vec![target], value);
            }
        }
        let value = match self.stack.pop() {
            Some(Item::Expr(expr)) => expr,
            Some(other) => {
                self.stack.push(other);
                return Err("store without a value".into());
            }
            None => return Err("store without a value".into()),
        };
        if let Expr::ImportModule { module, .. } = &value {
            let alias = match &target {
                Expr::Name(stored) => {
                    let head = module.split('.').next().unwrap_or(module.as_str());
                    if stored == head {
                        None
                    } else {
                        Some(stored.clone())
                    }
                }
                _ => None,
            };
            return Ok(Some(Stmt::Import(vec![(module.clone(), alias)])));
        }
        // Augmented assignment: `INPLACE_*` left its target and operand behind.
        if let Some((left, op, right)) = self.pending_aug.take() {
            if left == value {
                return Ok(Some(Stmt::AugAssign {
                    target,
                    op,
                    value: right,
                }));
            }
            self.push(value);
            return Err("confusing in-place assignment".into());
        }
        if self.dup_pending > 0 {
            self.dup_pending -= 1;
            match &mut self.pending_targets {
                Some((targets, _)) => {
                    targets.push(target);
                    let done = self.dup_pending == 0;
                    if done {
                        let (targets, value) = self.pending_targets.take().unwrap();
                        return self.emit_assign(targets, value);
                    }
                    return Ok(None);
                }
                None => {
                    self.pending_targets = Some((vec![target], value));
                    if self.dup_pending == 0 {
                        let (targets, value) = self.pending_targets.take().unwrap();
                        return self.emit_assign(targets, value);
                    }
                    return Ok(None);
                }
            }
        }
        self.emit_assign(vec![target], value)
    }

    fn emit_assign(&mut self, mut targets: Vec<Expr>, value: Expr) -> Result<Option<Stmt>, String> {
        if let Expr::Lambda(ref deferred) = value {
            if let [Expr::Name(name)] = targets.as_slice() {
                let deferred = deferred.as_ref().clone();
                return Ok(Some(Stmt::FunctionDef(Box::new(FunctionDef {
                    name: Some(name.clone()),
                    code: deferred.code,
                    defaults: deferred.defaults,
                    keyword_defaults: deferred.keyword_defaults,
                    decorators: Vec::new(),
                }))));
            }
        }
        targets.dedup();
        Ok(Some(Stmt::Assign { targets, value }))
    }

    fn flush_pending_assign(&mut self) -> Result<(), String> {
        if let Some((targets, value)) = self.pending_targets.take() {
            if self.dup_pending > 0 {
                self.pending_targets = Some((targets, value));
            } else {
                return Err(format!(
                    "dangling chained assignment to {}",
                    targets.len()
                ));
            }
        }
        Ok(())
    }
}


/// The bracket a synthetic comprehension code object is compiled for.
fn comprehension_kind(code: &Code) -> Option<CompKind> {
    let name = code.name_str();
    let name = name.as_str();
    match name {
        "<listcomp>" => Some(CompKind::List),
        "<setcomp>" => Some(CompKind::Set),
        "<dictcomp>" => Some(CompKind::Dict),
        "<genexpr>" => Some(CompKind::Gen),
        _ => None,
    }
}

/// A keyword argument name, or `None` when the key is not a plain string.
fn keyword_name(value: &Value) -> Option<String> {
    match value {
        Value::Unicode(bytes) | Value::Bytes(bytes) => {
            let text = String::from_utf8_lossy(bytes).into_owned();
            if text.is_empty() {
                None
            } else {
                Some(text)
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code_with(bytecode: Vec<u8>, consts: Vec<Value>, names: Vec<&str>, vars: Vec<&str>) -> Code {
        Code {
            bytecode,
            constants: consts,
            names: names.into_iter().map(|s| s.as_bytes().to_vec()).collect(),
            var_names: vars.into_iter().map(|s| s.as_bytes().to_vec()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn builds_a_simple_return() {
        // return 1
        let code = code_with(
            vec![100, 0, 0, 83],
            vec![Value::Int(1)],
            vec![],
            vec![],
        );
        let built = Builder::new(&code).build().unwrap();
        assert_eq!(built.stmts.len(), 1);
    }

    #[test]
    fn rebuilds_a_boolean_or_in_a_store() {
        // x = a or b  ->  LOAD_NAME a; JUMP_IF_TRUE_OR_POP ->store;
        //                 LOAD_NAME b; store x; LOAD_CONST None; RETURN
        let code = code_with(
            vec![101, 0, 0, 112, 9, 0, 101, 1, 0, 90, 2, 0, 100, 0, 0, 83],
            vec![Value::None],
            vec!["a", "b", "x"],
            vec![],
        );
        let built = Builder::new(&code).build().unwrap();
        match &built.stmts[0] {
            Stmt::Assign { targets, value } => {
                assert_eq!(targets, &vec![Expr::Name(String::from("x"))]);
                assert_eq!(
                    value,
                    &Expr::Bool(
                        "or",
                        vec![
                            Expr::Name(String::from("a")),
                            Expr::Name(String::from("b"))
                        ]
                    )
                );
            }
            other => panic!("expected the assignment, got {other:?}"),
        }
    }

    #[test]
    fn rebuilds_a_chained_comparison() {
        // return 0 <= x < 256  ->  the shared operand stays on the stack and
        // the second comparison reuses it as its left-hand side.
        let code = code_with(
            vec![
                100, 0, 0, // LOAD_CONST 0
                124, 0, 0, // LOAD_FAST x
                4, // DUP_TOP
                3, // ROT_THREE
                107, 2, 0, // COMPARE_OP <=
                111, 20, 0, // JUMP_IF_FALSE_OR_POP -> 20
                100, 1, 0, // LOAD_CONST 256
                107, 0, 0, // COMPARE_OP <
                83, // RETURN_VALUE
            ],
            vec![Value::Int(0), Value::Int(256)],
            vec![],
            vec!["x"],
        );
        let built = Builder::new(&code).build().unwrap();
        match &built.stmts[0] {
            Stmt::Return(Some(Expr::Bool(op, items))) => {
                assert_eq!(op, &"and");
                assert_eq!(items.len(), 2);
            }
            other => panic!("expected an and-expression return, got {other:?}"),
        }
    }

    #[test]
    fn keeps_the_print_stream_off_the_stack() {
        // print >> f, x  ->  LOAD_NAME f; DUP_TOP; LOAD_CONST 0; ROT_TWO;
        //                    PRINT_ITEM_TO; PRINT_NEWLINE_TO
        let code = code_with(
            vec![101, 0, 0, 4, 100, 0, 0, 2, 73, 74],
            vec![Value::Int(1)],
            vec!["f"],
            vec![],
        );
        let built = Builder::new(&code).build().unwrap();
        assert_eq!(
            built.stmts,
            vec![Stmt::Print {
                dest: Some(Expr::Name("f".into())),
                items: vec![Expr::Const(Value::Int(1))],
                newline: true,
            }]
        );
    }

    #[test]
    fn rebuilds_a_stepped_slice() {
        // y = x[a:b:c]  ->  BUILD_SLICE 3 leaves a triple the subscript uses.
        let code = code_with(
            vec![
                101, 0, 0, // LOAD_NAME x
                101, 1, 0, // LOAD_NAME a
                101, 2, 0, // LOAD_NAME b
                101, 3, 0, // LOAD_NAME c
                133, 3, 0, // BUILD_SLICE 3
                25, // BINARY_SUBSCR
                90, 4, 0, // STORE_NAME y
            ],
            vec![],
            vec!["x", "a", "b", "c", "y"],
            vec![],
        );
        let built = Builder::new(&code).build().unwrap();
        assert_eq!(
            built.stmts,
            vec![Stmt::Assign {
                targets: vec![Expr::Name("y".into())],
                value: Expr::Slice(
                    Box::new(Expr::Name("x".into())),
                    Some(Box::new(Expr::Name("a".into()))),
                    Some(Box::new(Expr::Name("b".into()))),
                    Some(Box::new(Expr::Name("c".into()))),
                ),
            }]
        );
    }

    #[test]
    fn rebuilds_deletions() {
        // del a.b ; del a[k] ; del a[:]
        let code = code_with(
            vec![
                101, 0, 0, 96, 1, 0, // LOAD_NAME a; DELETE_ATTR b
                101, 0, 0, 101, 2, 0, 61, // LOAD_NAME a; LOAD_NAME k; DELETE_SUBSCR
                101, 0, 0, 44, // LOAD_NAME a; DELETE_SLICE+0
                100, 0, 0, 83, // LOAD_CONST None; RETURN_VALUE
            ],
            vec![Value::None],
            vec!["a", "b", "k"],
            vec![],
        );
        let built = Builder::new(&code).build().unwrap();
        assert_eq!(
            built.stmts,
            vec![
                Stmt::Del(vec![Expr::Attribute(
                    Box::new(Expr::Name("a".into())),
                    "b".into()
                )]),
                Stmt::Del(vec![Expr::Subscript(
                    Box::new(Expr::Name("a".into())),
                    Box::new(Expr::Name("k".into()))
                )]),
                Stmt::Del(vec![Expr::Slice(
                    Box::new(Expr::Name("a".into())),
                    None,
                    None,
                    None
                )]),
                Stmt::Return(None),
            ]
        );
    }

    #[test]
    fn builds_an_assignment() {
        // x = 1  ->  LOAD_CONST 0; STORE_NAME 0
        let code = code_with(
            vec![100, 0, 0, 90, 0, 0],
            vec![Value::Int(1)],
            vec!["x"],
            vec![],
        );
        let built = Builder::new(&code).build().unwrap();
        assert_eq!(
            built.stmts,
            vec![Stmt::Assign {
                targets: vec![Expr::Name("x".into())],
                value: Expr::Const(Value::Int(1)),
            }]
        );
    }
}
