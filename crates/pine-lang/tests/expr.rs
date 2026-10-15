// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The Pratt expression parser (§4.6, §7.5), structure-based: each expression statement is
//! rendered as an s-expression from the public `Node` view. The print-based forms of these tests
//! land with the printer (T11).

use pine_ir::{FileId, NodeId, SymTable};
use pine_lang::{parse, Ast, Node, NodeKind};

/// Parse `body` as a v6 script.
fn parse6(body: &str) -> Result<(Ast, SymTable), Vec<u16>> {
    let src = format!("//@version=6\n{body}");
    let mut syms = SymTable::new();
    match parse(&src, FileId::ROOT, &mut syms) {
        Ok(ast) => {
            assert!(ast.validate().is_ok(), "{body:?}");
            Ok((ast, syms))
        }
        Err(d) => Err(d.iter().map(|x| x.code.0).collect()),
    }
}

/// The s-expression of every statement of `body`, joined by `; `.
fn sx(body: &str) -> String {
    let (ast, syms) = parse6(body).unwrap_or_else(|c| panic!("{body:?} refused: {c:?}"));
    let Node::Script { body: stmts } = ast.node(ast.root()) else {
        panic!("root is not a Script")
    };
    let parts: Vec<String> = stmts.iter().map(|&s| render(&ast, &syms, s)).collect();
    parts.join("; ")
}

/// The codes `body` is refused with.
fn codes(body: &str) -> Vec<u16> {
    match parse6(body) {
        Ok(_) => Vec::new(),
        Err(c) => c,
    }
}

fn list(ast: &Ast, syms: &SymTable, ids: &[NodeId]) -> String {
    let parts: Vec<String> = ids.iter().map(|&i| render(ast, syms, i)).collect();
    parts.join(" ")
}

fn render(ast: &Ast, syms: &SymTable, id: NodeId) -> String {
    match ast.node(id) {
        Node::Ident { sym } | Node::Name { sym } => syms.resolve(sym).to_string(),
        Node::IntLit { value } => value.to_string(),
        Node::FloatLit { value } => format!("{value:?}"),
        Node::BoolLit { value } => value.to_string(),
        Node::StrLit { value, .. } => format!("{:?}", syms.resolve(value)),
        Node::ColorLit { rgb, alpha } => format!("#{rgb:06x}{alpha:?}"),
        Node::Unary { op, operand } => format!("({} {})", op.text(), render(ast, syms, operand)),
        Node::Binary { op, lhs, rhs } => {
            format!(
                "({} {} {})",
                op.text(),
                render(ast, syms, lhs),
                render(ast, syms, rhs)
            )
        }
        Node::Ternary {
            cond,
            then,
            otherwise,
        } => format!(
            "(? {} {} {})",
            render(ast, syms, cond),
            render(ast, syms, then),
            render(ast, syms, otherwise)
        ),
        Node::Hist { base, offset } => {
            format!(
                "(hist {} {})",
                render(ast, syms, base),
                render(ast, syms, offset)
            )
        }
        Node::Field { base, member } => {
            format!(
                "(. {} {})",
                render(ast, syms, base),
                render(ast, syms, member)
            )
        }
        Node::Tuple { items } => format!("[{}]", list(ast, syms, items)),
        Node::Call {
            callee,
            type_args,
            args,
        } => {
            let ta = if type_args.is_empty() {
                String::new()
            } else {
                format!("<{}>", list(ast, syms, type_args))
            };
            format!(
                "(call {}{ta} {})",
                render(ast, syms, callee),
                list(ast, syms, args)
            )
        }
        Node::NamedArg { name, value } => {
            format!("{}={}", render(ast, syms, name), render(ast, syms, value))
        }
        Node::TypeRef {
            path,
            args,
            array_suffix,
        } => {
            let a = if args.is_empty() {
                String::new()
            } else {
                format!("<{}>", list(ast, syms, args))
            };
            let s = if array_suffix { "[]" } else { "" };
            format!("{}{a}{s}", render(ast, syms, path))
        }
        other => format!("{other:?}"),
    }
}

#[test]
fn precedence_table() {
    let table: [(&str, &str); 40] = [
        ("a + b * c", "(+ a (* b c))"),
        ("a * b + c", "(+ (* a b) c)"),
        ("a - b - c", "(- (- a b) c)"),
        ("a / b * c", "(* (/ a b) c)"),
        ("a % b / c", "(/ (% a b) c)"),
        ("a + b - c", "(- (+ a b) c)"),
        ("a < b + c", "(< a (+ b c))"),
        ("a + b < c", "(< (+ a b) c)"),
        ("a <= b", "(<= a b)"),
        ("a >= b", "(>= a b)"),
        ("a > b", "(> a b)"),
        ("a == b < c", "(== a (< b c))"),
        ("a != b == c", "(== (!= a b) c)"),
        ("a and b == c", "(and a (== b c))"),
        ("a or b and c", "(or a (and b c))"),
        ("a and b or c", "(or (and a b) c)"),
        ("a or b or c", "(or (or a b) c)"),
        ("not a == b", "(== (not a) b)"),
        ("not a and b", "(and (not a) b)"),
        ("not not a", "(not (not a))"),
        ("-a * b", "(* (- a) b)"),
        ("-a[1]", "(- (hist a 1))"),
        ("+a", "(+ a)"),
        ("- -a", "(- (- a))"),
        ("a ? b : c", "(? a b c)"),
        ("a ? b : c ? d : e", "(? a b (? c d e))"),
        ("a ? b ? c : d : e", "(? a (? b c d) e)"),
        ("a or b ? c : d", "(? (or a b) c d)"),
        ("a ? b + c : d * e", "(? a (+ b c) (* d e))"),
        ("(a + b) * c", "(* (+ a b) c)"),
        ("a * (b + c)", "(* a (+ b c))"),
        ("((a))", "a"),
        ("a.b.c", "(. (. a b) c)"),
        ("a.b[1]", "(hist (. a b) 1)"),
        ("(a[1]).b", "(. (hist a 1) b)"),
        ("a[1][2]", "(hist (hist a 1) 2)"),
        ("f(a)(b)", "(call (call f a) b)"),
        ("f(a).b", "(. (call f a) b)"),
        ("a.f(b) + c", "(+ (call (. a f) b) c)"),
        ("a < b < c", "(< (< a b) c)"),
    ];
    for (src, want) in table {
        assert_eq!(sx(src), want, "{src:?}");
    }
}

#[test]
fn not_binds_tighter_than_comparison() {
    assert_eq!(sx("not a == b"), "(== (not a) b)");
}

#[test]
fn history_outbinds_unary_minus() {
    assert_eq!(sx("-a[1]"), "(- (hist a 1))");
}

#[test]
fn field_then_history_is_hist_of_field() {
    let (ast, _) = parse6("a.b[1]").expect("parses");
    let Node::Script { body } = ast.node(ast.root()) else {
        panic!()
    };
    assert_eq!(ast.kind(body[0]), NodeKind::Hist);
    let Node::Hist { base, .. } = ast.node(body[0]) else {
        panic!()
    };
    assert_eq!(ast.kind(base), NodeKind::Field);
    assert_eq!(sx("(a[1]).b"), "(. (hist a 1) b)");
}

#[test]
fn ternary_is_right_associative() {
    assert_eq!(sx("a ? b : c ? d : e"), "(? a b (? c d e))");
}

#[test]
fn and_binds_tighter_than_or() {
    assert_eq!(sx("a and b or c"), "(or (and a b) c)");
}

#[test]
fn binary_is_left_associative() {
    assert_eq!(sx("a - b - c"), "(- (- a b) c)");
    assert_eq!(sx("a / b / c"), "(/ (/ a b) c)");
}

#[test]
fn chained_comparison_parses_left() {
    assert_eq!(sx("a < b < c"), "(< (< a b) c)");
}

#[test]
fn tuple_literal_vs_history() {
    assert_eq!(sx("[a, b]"), "[a b]");
    assert_eq!(sx("a[b]"), "(hist a b)");
    assert_eq!(sx("[a][0]"), "(hist [a] 0)");
    assert_eq!(codes("[]"), vec![2002]);
}

#[test]
fn type_args_vs_comparison() {
    assert_eq!(sx("a < b"), "(< a b)");
    assert_eq!(sx("a < b > c"), "(> (< a b) c)");
    assert_eq!(sx("array.new<float>()"), "(call (. array new)<float> )");
    assert_eq!(
        sx("map.new<string, float>()"),
        "(call (. map new)<string float> )"
    );
    assert_eq!(
        sx("matrix.new<array<float>>(2, 2)"),
        "(call (. matrix new)<array<float>> 2 2)"
    );
    assert_eq!(sx("a<b>(c)"), "(call a<b> c)");
    assert_eq!(sx("a < b ? c : d"), "(? (< a b) c d)");
    assert_eq!(
        sx("array.new<float[]>(0)"),
        "(call (. array new)<float[]> 0)"
    );
    assert_eq!(
        sx("x + array.new<int>(1)"),
        "(+ x (call (. array new)<int> 1))"
    );
    assert_eq!(
        sx("array.new<chart.point>()"),
        "(call (. array new)<(. chart point)> )"
    );
    // A 70-token pseudo-generic: past the scan's 64 tokens it is a comparison chain.
    let long = format!("a < b{} > (c)", ".b".repeat(34));
    assert!(sx(&long).starts_with("(> (< a (. (. "), "{long}");
}

#[test]
fn named_args_parse() {
    assert_eq!(
        sx("plot(close, title = \"x\", color = color.red)"),
        "(call plot close title=\"x\" color=(. color red))"
    );
    assert_eq!(sx("f(a,)"), "(call f a)");
    assert_eq!(sx("f()"), "(call f )");
}

#[test]
fn duplicate_named_args_are_kept_in_order() {
    assert_eq!(sx("f(x = 1, x = 2)"), "(call f x=1 x=2)");
}

#[test]
fn member_after_dot_may_be_a_keyword() {
    assert_eq!(sx("syminfo.type"), "(. syminfo type)");
    assert_eq!(sx("x.if"), "(. x if)");
    assert_eq!(
        sx("strategy.long and x.not"),
        "(and (. strategy long) (. x not))"
    );
}

#[test]
fn literals_parse_to_values() {
    assert_eq!(sx("-9223372036854775808"), "(- 9223372036854775808)");
    assert_eq!(sx("1.5 + .5"), "(+ 1.5 0.5)");
    assert_eq!(sx("#FF0000"), "#ff0000None");
    assert_eq!(sx("#ff0000a0"), "#ff0000Some(160)");
    assert_eq!(sx("true or false"), "(or true false)");
    assert_eq!(sx("'a\\qb'"), "\"a\\\\qb\"");
}

#[test]
fn malformed_expressions_report_once() {
    assert_eq!(codes("a +"), vec![2002]);
    assert_eq!(codes("f(a b)"), vec![2000]);
    assert_eq!(codes("a ? b"), vec![2000]);
    assert_eq!(codes("a."), vec![2003]);
    assert_eq!(codes("f(a"), vec![1010]);
    assert_eq!(codes("1 + @ + 2"), vec![1000]);
    assert_eq!(codes("a b"), vec![2000]);
}

#[test]
fn structure_as_operand_is_refused() {
    assert_eq!(codes("x = 1 + if c\n    2\n"), vec![2029]);
    assert_eq!(codes("1 + if c\n    2\ny = 3\n"), vec![2029]);
}

#[test]
fn deep_nesting_is_refused_not_overflowed() {
    let src = format!("{}a{}", "(".repeat(600), ")".repeat(600));
    assert_eq!(codes(&src), vec![2026]);
    let neg = format!("{}a", "-".repeat(600));
    assert_eq!(codes(&neg), vec![2026]);
    let chain = vec!["a"; 2000].join(" + ");
    assert_eq!(codes(&chain), vec![2026]);
}

#[test]
fn several_expression_statements_and_doc_runs() {
    let src = "a\n//@variable the b\nb\n\n//@function f\n// plain\nc\n";
    assert_eq!(sx(src), "a; b; c");
    let (ast, syms) = parse6(src).expect("parses");
    let Node::Script { body } = ast.node(ast.root()) else {
        panic!()
    };
    assert!(ast.doc(body[0]).is_empty());
    let d = ast.doc(body[1]);
    assert_eq!(d.len(), 1);
    assert_eq!(syms.resolve(d[0].text), "the b");
    assert!(ast.doc(body[2]).is_empty());
}

#[test]
fn fully_parenthesised_prints() {
    let cases = [
        ("not a == b", "((not a) == b)"),
        ("-a[1]", "(-(a[1]))"),
        ("a.b[1]", "a.b[1]"),
        ("(a[1]).b", "a[1].b"),
        ("a ? b : c ? d : e", "(a ? b : (c ? d : e))"),
        ("a and b or c", "((a and b) or c)"),
        ("a - b - c", "((a - b) - c)"),
        ("a < b < c", "((a < b) < c)"),
        ("[a][0]", "[a][0]"),
        (
            "matrix.new<array<float>>(2, 2)",
            "matrix.new<array<float>>(2, 2)",
        ),
    ];
    for (src, want) in cases {
        let mut syms = SymTable::new();
        let ast = parse(&format!("//@version=6\n{src}\n"), FileId::ROOT, &mut syms).expect(src);
        let mut out = String::new();
        pine_lang::print(&ast, &syms, &mut out).expect("print");
        assert_eq!(out, format!("//@version=6\n{want}\n"), "{src}");
    }
}
