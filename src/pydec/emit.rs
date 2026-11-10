//! Render rebuilt statements as Python 2.7 source.

use super::build::{ClassDef, FunctionDef, Stmt};
use crate::nemarshal::Value;
use super::expr::Expr;

const INDENT: &str = "    ";

pub fn module(stmts: &[Stmt]) -> String {
    let mut out = String::new();
    let mut first = true;
    let mut stmts = stmts;
    if let Some(doc) = docstring(stmts.first()) {
        out.push_str(&doc);
        first = false;
        stmts = &stmts[1..];
    }
    // Every code object ends with `RETURN_VALUE`, including modules and class
    // bodies, where a trailing `return` is not even valid Python.
    let stmts = match stmts.last() {
        Some(Stmt::Return(None)) => &stmts[..stmts.len() - 1],
        Some(Stmt::Return(Some(Expr::Const(Value::None)))) => &stmts[..stmts.len() - 1],
        _ => stmts,
    };
    for stmt in stmts {
        // A leading string literal is the module docstring; keep it first.
        write_stmt(&mut out, stmt, 0, &mut first);
    }
    if out.is_empty() {
        out.push_str("pass\n");
    }
    out
}

pub fn suite(stmts: &[Stmt], indent: usize) -> String {
    let mut out = String::new();
    if stmts.is_empty() {
        push_indent(&mut out, indent);
        out.push_str("pass\n");
        return out;
    }
    let mut first = false;
    for stmt in stmts {
        write_stmt(&mut out, stmt, indent, &mut first);
    }
    out
}

/// `__doc__ = b'...'` at the top of a module is really the docstring.
fn docstring(stmt: Option<&Stmt>) -> Option<String> {
    let Some(Stmt::Assign { targets, value }) = stmt else {
        return None;
    };
    if targets.len() != 1 || targets[0] != Expr::Name(String::from("__doc__")) {
        return None;
    }
    let text = match value {
        Expr::Const(crate::nemarshal::Value::Bytes(bytes))
        | Expr::Const(crate::nemarshal::Value::Unicode(bytes)) => {
            String::from_utf8(bytes.clone()).ok()?
        }
        _ => return None,
    };
    if text.contains("'''") || text.contains('\r') || text.ends_with('\\') {
        return None;
    }
    let mut out = String::from("'''");
    if !text.starts_with('\n') {
        out.push('\n');
    }
    out.push_str(&text);
    if !text.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("'''\n");
    Some(out)
}

fn push_indent(out: &mut String, indent: usize) {
    for _ in 0..indent {
        out.push_str(INDENT);
    }
}

fn write_stmt(out: &mut String, stmt: &Stmt, indent: usize, _first: &mut bool) {
    match stmt {
        Stmt::Expr(expr) => {
            push_indent(out, indent);
            out.push_str(&expr.render());
            out.push('\n');
        }
        Stmt::Assign { targets, value } => {
            push_indent(out, indent);
            for (i, target) in targets.iter().enumerate() {
                if i > 0 {
                    out.push_str(" = ");
                }
                out.push_str(&render_target(target));
            }
            out.push_str(" = ");
            out.push_str(&value.render());
            out.push('\n');
        }
        Stmt::AugAssign { target, op, value } => {
            push_indent(out, indent);
            out.push_str(&render_target(target));
            out.push(' ');
            out.push_str(op);
            out.push_str("= ");
            out.push_str(&value.render());
            out.push('\n');
        }
        Stmt::Print {
            dest,
            items,
            newline,
        } => {
            push_indent(out, indent);
            out.push_str("print");
            if dest.is_none() && !items.is_empty() {
                out.push(' ');
            }
            if let Some(dest) = dest {
                out.push_str(" >>");
                out.push_str(&dest.render());
                if !items.is_empty() {
                    out.push(',');
                }
            }
            for (i, item) in items.iter().enumerate() {
                if i > 0 || dest.is_some() {
                    out.push_str(" ");
                }
                out.push_str(&item.render());
                if i + 1 < items.len() {
                    out.push(',');
                }
            }
            if !*newline {
                out.push(',');
            }
            out.push('\n');
        }
        Stmt::Return(value) => {
            push_indent(out, indent);
            match value {
                Some(value) => {
                    out.push_str("return ");
                    out.push_str(&value.render());
                }
                None => out.push_str("return"),
            }
            out.push('\n');
        }
        Stmt::If { test, body, orelse } => {
            push_indent(out, indent);
            out.push_str("if ");
            out.push_str(&test.render());
            out.push_str(":\n");
            out.push_str(&suite(body, indent + 1));
            if !orelse.is_empty() {
                push_indent(out, indent);
                if orelse.len() == 1 && matches!(orelse[0], Stmt::If { .. }) {
                    // elif chain: splice the nested if in at this indentation
                    let mut text = String::new();
                    let mut first = false;
                    write_stmt(&mut text, &orelse[0], indent, &mut first);
                    let spliced = text.trim_start_matches(|c: char| c == ' ');
                    match spliced.strip_prefix("if ") {
                        Some(rest) => {
                            out.push_str("elif ");
                            out.push_str(rest);
                        }
                        None => out.push_str(spliced),
                    }
                } else {
                    out.push_str("else:\n");
                    out.push_str(&suite(orelse, indent + 1));
                }
            }
        }
        Stmt::While { test, body, orelse } => {
            push_indent(out, indent);
            out.push_str("while ");
            out.push_str(&test.render());
            out.push_str(":\n");
            out.push_str(&suite(body, indent + 1));
            if !orelse.is_empty() {
                push_indent(out, indent);
                out.push_str("else:\n");
                out.push_str(&suite(orelse, indent + 1));
            }
        }
        Stmt::For {
            target,
            iter,
            body,
            orelse,
        } => {
            push_indent(out, indent);
            out.push_str("for ");
            out.push_str(&render_target(target));
            out.push_str(" in ");
            out.push_str(&iter.render());
            out.push_str(":\n");
            out.push_str(&suite(body, indent + 1));
            if !orelse.is_empty() {
                push_indent(out, indent);
                out.push_str("else:\n");
                out.push_str(&suite(orelse, indent + 1));
            }
        }
        Stmt::With { ctx, target, body } => {
            push_indent(out, indent);
            out.push_str("with ");
            out.push_str(&ctx.render());
            if let Some(target) = target {
                out.push_str(" as ");
                out.push_str(&render_target(target));
            }
            out.push_str(":
");
            out.push_str(&suite(body, indent + 1));
        }
        Stmt::Try {
            body,
            handlers,
            orelse,
            finally,
        } => {
            if handlers.is_empty() && finally.is_empty() {
                out.push_str(&suite(body, indent));
                return;
            }
            push_indent(out, indent);
            out.push_str("try:\n");
            out.push_str(&suite(body, indent + 1));
            for handler in handlers {
                push_indent(out, indent);
                match (&handler.kind, &handler.name) {
                    (Some(kind), None) => {
                        out.push_str(&format!("except {}:\n", kind.render()))
                    }
                    // `except E as (a, b):` is not valid Python 2.7 grammar (`as`
                    // only takes a name); a tuple target needs the comma form.
                    (Some(kind), Some(name @ Expr::Tuple(_))) => out.push_str(&format!(
                        "except {}, {}:
",
                        kind.render(),
                        name.render()
                    )),
                    (Some(kind), Some(name)) => out.push_str(&format!(
                        "except {} as {}:\n",
                        kind.render(),
                        name.render()
                    )),
                    (None, _) => out.push_str("except:\n"),
                }
                out.push_str(&suite(&handler.body, indent + 1));
            }
            if !orelse.is_empty() {
                push_indent(out, indent);
                out.push_str("else:\n");
                out.push_str(&suite(orelse, indent + 1));
            }
            if !finally.is_empty() {
                push_indent(out, indent);
                out.push_str("finally:\n");
                out.push_str(&suite(finally, indent + 1));
            }
        }
        Stmt::Break => {
            push_indent(out, indent);
            out.push_str("break\n");
        }
        Stmt::Continue => {
            push_indent(out, indent);
            out.push_str("continue\n");
        }
        Stmt::Pass => {
            push_indent(out, indent);
            out.push_str("pass\n");
        }
        Stmt::Import(items) => {
            push_indent(out, indent);
            out.push_str("import ");
            for (i, (module, alias)) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(module);
                if let Some(alias) = alias {
                    out.push_str(" as ");
                    out.push_str(alias);
                }
            }
            out.push('\n');
        }
        Stmt::FromImport {
            module,
            level,
            names,
            star,
        } => {
            push_indent(out, indent);
            out.push_str("from ");
            out.push_str(&".".repeat(*level));
            out.push_str(module);
            out.push_str(" import ");
            if *star {
                out.push('*');
            } else {
                for (i, (name, alias)) in names.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(name);
                    if let Some(alias) = alias {
                        out.push_str(" as ");
                        out.push_str(alias);
                    }
                }
            }
            out.push('\n');
        }
        Stmt::FunctionDef(def) => write_function(out, def, indent),
        Stmt::ClassDef(def) => write_class(out, def, indent),
        Stmt::Raise {
            kind,
            value,
            traceback,
        } => {
            push_indent(out, indent);
            out.push_str("raise");
            let mut parts: Vec<&Expr> = Vec::new();
            if let Some(kind) = kind {
                parts.push(kind);
            }
            if let Some(value) = value {
                parts.push(value);
            }
            if let Some(traceback) = traceback {
                parts.push(traceback);
            }
            for (i, part) in parts.iter().enumerate() {
                if i == 0 {
                    out.push(' ');
                } else {
                    out.push_str(", ");
                }
                out.push_str(&part.render());
            }
            out.push('\n');
        }
        Stmt::Assert { test, message } => {
            push_indent(out, indent);
            out.push_str("assert ");
            out.push_str(&test.render());
            if let Some(message) = message {
                out.push_str(", ");
                out.push_str(&message.render());
            }
            out.push('\n');
        }
        Stmt::Del(targets) => {
            push_indent(out, indent);
            out.push_str("del ");
            for (i, target) in targets.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&target.render());
            }
            out.push('\n');
        }
        Stmt::Global(names) => {
            push_indent(out, indent);
            out.push_str("global ");
            out.push_str(&names.join(", "));
            out.push('\n');
        }
        Stmt::Yield(value) => {
            push_indent(out, indent);
            match value {
                Some(value) => {
                    out.push_str("yield ");
                    out.push_str(&value.render());
                }
                None => out.push_str("yield"),
            }
            out.push('\n');
        }
        Stmt::Comment(text) => {
            push_indent(out, indent);
            out.push_str("# ");
            out.push_str(text);
            out.push('\n');
        }
        Stmt::Raw(lines) => {
            for line in lines {
                push_indent(out, indent);
                out.push_str("# ");
                out.push_str(line);
                out.push('\n');
            }
        }
    }
}

fn write_function(out: &mut String, def: &FunctionDef, indent: usize) {
    write_decorators(out, &def.decorators, indent);
    push_indent(out, indent);
    out.push_str("def ");
    out.push_str(&def.name());
    out.push('(');
    out.push_str(&render_args(&def.code, &def.defaults, def.keyword_defaults.as_ref()));
    out.push_str("):\n");
    let body = super::decompile_code(&def.code);
    out.push_str(&indent_text(&body, indent + 1));
}

fn write_decorators(out: &mut String, decorators: &[Expr], indent: usize) {
    for decorator in decorators {
        push_indent(out, indent);
        out.push('@');
        out.push_str(&decorator.render());
        out.push('\n');
    }
}

fn write_class(out: &mut String, def: &ClassDef, indent: usize) {
    write_decorators(out, &def.decorators, indent);
    push_indent(out, indent);
    out.push_str("class ");
    out.push_str(&def.name);
    // `class C():` and `class C:` compile to the same empty base tuple.
    let bases: Vec<&Expr> = def
        .bases
        .iter()
        .filter(|base| match base {
            Expr::Tuple(items) => !items.is_empty(),
            Expr::Const(Value::Tuple(items)) => !items.is_empty(),
            Expr::Const(Value::List(items)) => !items.is_empty(),
            _ => true,
        })
        .collect();
    if !bases.is_empty() {
        out.push('(');
        let mut sep = "";
        for base in bases {
            out.push_str(sep);
            sep = ", ";
            out.push_str(&base.render());
        }
        out.push(')');
    }
    out.push_str(":
");
    let body = super::decompile_code(&def.code);
    out.push_str(&indent_text(&body, indent + 1));
}

pub fn render_args(code: &crate::nemarshal::Code, defaults: &[Expr], keyword_defaults: Option<&Expr>) -> String {
    const CO_VARARGS: i32 = 0x04;
    const CO_VARKEYWORDS: i32 = 0x08;

    let positional = code.arg_count.max(0) as usize;
    let mut parts: Vec<String> = Vec::new();
    let skip = positional.saturating_sub(defaults.len());
    for i in 0..positional {
        let name = match code.var_names.get(i) {
            Some(bytes) => super::disasm::text(bytes),
            None => format!("arg{i}"),
        };
        if i >= skip {
            match defaults.get(i - skip) {
                Some(default) => parts.push(format!("{name}={}", default.render())),
                None => parts.push(name),
            }
        } else {
            parts.push(name);
        }
    }
    let mut index = positional;
    if code.flags & CO_VARARGS != 0 {
        let name = match code.var_names.get(index) {
            Some(bytes) => super::disasm::text(bytes),
            None => "args".to_string(),
        };
        parts.push(format!("*{name}"));
        index += 1;
    }
    if code.flags & CO_VARKEYWORDS != 0 {
        let name = match code.var_names.get(index) {
            Some(bytes) => super::disasm::text(bytes),
            None => "kwargs".to_string(),
        };
        parts.push(format!("**{name}"));
    }
    let _ = keyword_defaults;
    parts.join(", ")
}

pub fn render_target(target: &Expr) -> String {
    match target {
        Expr::Tuple(_) => target.render(),
        other => other.render(),
    }
}

/// Indent a rendered block by `levels`, dropping the trailing newline so callers
/// can concatenate.
pub fn indent_text(text: &str, levels: usize) -> String {
    let mut out = String::with_capacity(text.len() + levels * 4 * 8);
    for line in text.lines() {
        if !line.is_empty() {
            push_indent(&mut out, levels);
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pydec::build::Handler;
    use crate::nemarshal::{Code, Value};

    #[test]
    fn renders_assignment_and_return() {
        let stmts = vec![
            Stmt::Assign {
                targets: vec![Expr::Name("x".into())],
                value: Expr::Const(Value::Int(1)),
            },
            Stmt::Return(Some(Expr::Name("x".into()))),
        ];
        assert_eq!(module(&stmts), "x = 1\nreturn x\n");
    }

    #[test]
    fn drops_the_trailing_return_every_code_object_carries() {
        // Modules and class bodies end with RETURN_VALUE too, and a bare
        // `return` there is not even valid Python.
        let stmts = vec![
            Stmt::Assign {
                targets: vec![Expr::Name("x".into())],
                value: Expr::Const(Value::Int(1)),
            },
            Stmt::Return(None),
        ];
        assert_eq!(module(&stmts), "x = 1
");
        let explicit = vec![Stmt::Return(Some(Expr::Const(Value::None)))];
        // Nothing left to say, but an empty body still needs a statement.
        assert_eq!(module(&explicit), "pass
");
    }

    #[test]
    fn renders_a_tuple_except_target_the_python_2_way() {
        // `except E as (a, b):` is a syntax error in 2.7; only the comma form
        // is accepted, so a tuple target must be emitted that way.
        let stmt = Stmt::Try {
            body: vec![Stmt::Pass],
            handlers: vec![Handler {
                kind: Some(Expr::Name(String::from("socket.error"))),
                name: Some(Expr::Tuple(vec![
                    Expr::Name(String::from("code")),
                    Expr::Name(String::from("reason")),
                ])),
                body: vec![Stmt::Pass],
            }],
            orelse: Vec::new(),
            finally: Vec::new(),
        };
        let text = module(&[stmt]);
        assert!(text.contains("except socket.error, (code, reason):"), "{text}");
    }

    #[test]
    fn renders_class_decorators() {
        let class = ClassDef {
            name: "C".into(),
            bases: vec![],
            decorators: vec![Expr::Name("register".into())],
            code: Box::new(Code {
                bytecode: vec![100, 0, 0, 83],
                constants: vec![Value::None],
                ..Default::default()
            }),
        };
        let text = module(&[Stmt::ClassDef(Box::new(class))]);
        // The body's returning `None` leaves nothing behind, only `pass`.
        assert_eq!(text, "@register
class C:
    pass
");
    }

    #[test]
    fn renders_nested_if_with_elif() {
        let inner = Stmt::If {
            test: Expr::Name("b".into()),
            body: vec![Stmt::Pass],
            orelse: vec![],
        };
        let outer = Stmt::If {
            test: Expr::Name("a".into()),
            body: vec![Stmt::Pass],
            orelse: vec![inner],
        };
        let text = module(&[outer]);
        assert_eq!(text, "if a:\n    pass\nelif b:\n    pass\n");
    }

    #[test]
    fn renders_print_statement() {
        let stmt = Stmt::Print {
            dest: None,
            items: vec![Expr::Name("x".into())],
            newline: false,
        };
        assert_eq!(module(&[stmt]), "print x,\n");
    }
}
