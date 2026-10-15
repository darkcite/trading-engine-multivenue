// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Declarations (§4.7, §7.7), structure-based until the printer lands (T11).

use pine_ir::{FileId, NodeId, SymTable};
use pine_lang::{parse, Ast, Node, NodeKind};

fn parse_v(v: u8, body: &str) -> Result<(Ast, SymTable), Vec<u16>> {
    let src = format!("//@version={v}\n{body}");
    let mut syms = SymTable::new();
    match parse(&src, FileId::ROOT, &mut syms) {
        Ok(ast) => {
            assert!(ast.validate().is_ok(), "{body:?}");
            Ok((ast, syms))
        }
        Err(d) => Err(d.iter().map(|x| x.code.0).collect()),
    }
}

fn stmts(body: &str) -> (Ast, SymTable, Vec<NodeId>) {
    let (ast, syms) = parse_v(6, body).unwrap_or_else(|c| panic!("{body:?} refused: {c:?}"));
    let Node::Script { body: b } = ast.node(ast.root()) else {
        panic!("root")
    };
    let v = b.to_vec();
    (ast, syms, v)
}

fn codes(v: u8, body: &str) -> Vec<u16> {
    parse_v(v, body).err().unwrap_or_default()
}

#[test]
fn functions_and_methods() {
    let (ast, syms, v) = stmts("f(x, simple int y = 2, float[] z) =>\n    x + y\ng(a) => a * 2\n");
    assert_eq!(v.len(), 2);
    let Node::FuncDecl {
        name,
        params,
        method,
        export,
        body,
    } = ast.node(v[0])
    else {
        panic!()
    };
    assert!(!method && !export);
    let Node::Name { sym } = ast.node(name) else {
        panic!()
    };
    assert_eq!(syms.resolve(sym), "f");
    assert_eq!(params.len(), 3);
    let Node::Param {
        qual, ty, default, ..
    } = ast.node(params[1])
    else {
        panic!()
    };
    assert_eq!(qual, Some(pine_ir::Qual::Simple));
    assert!(ty.is_some() && default.is_some());
    assert_eq!(ast.kind(body), NodeKind::Block);
    let (ast, _, v) = stmts("method set(InfoLabel this, string s) =>\n    this.s := s\n");
    let Node::FuncDecl { method, .. } = ast.node(v[0]) else {
        panic!()
    };
    assert!(method);
    let (ast, _, v) = stmts("export f(float x) => x\nexport method m(T t) => t\n");
    let Node::FuncDecl { export, .. } = ast.node(v[0]) else {
        panic!()
    };
    assert!(export);
    let Node::FuncDecl { export, method, .. } = ast.node(v[1]) else {
        panic!()
    };
    assert!(export && method);
}

#[test]
fn types_enums_imports() {
    let src = "type P\n    float x = 0.0\n    varip int n\n    array<float> xs\ntype Empty\n\
               enum E\n    a\n    b = \"Bee\"\nimport user/lib/7 as l\nimport u/l/12\n";
    let (ast, _, v) = stmts(src);
    let kinds: Vec<NodeKind> = v.iter().map(|&s| ast.kind(s)).collect();
    use NodeKind::*;
    assert_eq!(kinds, vec![TypeDecl, TypeDecl, EnumDecl, Import, Import]);
    let Node::TypeDecl { fields, .. } = ast.node(v[0]) else {
        panic!()
    };
    assert_eq!(fields.len(), 3);
    let Node::FieldDecl { varip, default, .. } = ast.node(fields[1]) else {
        panic!()
    };
    assert!(varip && default.is_none());
    let Node::EnumDecl { members, .. } = ast.node(v[2]) else {
        panic!()
    };
    assert_eq!(members.len(), 2);
    let Node::Import { version, alias, .. } = ast.node(v[3]) else {
        panic!()
    };
    assert_eq!(version, 7);
    assert!(alias.is_some());
    let (ast, _, v) =
        stmts("export type T\n    int a\nexport enum F\n    x\nexport const float K = 1.0\n");
    let Node::VarDecl { export, qual, .. } = ast.node(v[2]) else {
        panic!()
    };
    assert!(export);
    assert_eq!(qual, Some(pine_ir::Qual::Const));
}

#[test]
fn declaration_errors() {
    assert_eq!(codes(6, "if c\n    f(x) => x\n"), vec![2027]);
    assert_eq!(codes(6, "if c\n    type T\n"), vec![2027]);
    assert_eq!(codes(6, "type T\n    x\n"), vec![2004]);
    assert_eq!(codes(6, "type T\n    var int x\n"), vec![2004]);
    assert_eq!(codes(6, "import u/l\n"), vec![2012]);
    assert_eq!(codes(6, "import u/l/1.2\n"), vec![2013]);
    assert_eq!(codes(6, "import u/l/0\n"), vec![2013]);
    assert_eq!(codes(6, "import u/l/65536\n"), vec![2013]);
    assert_eq!(codes(6, "export x = 1\n"), vec![2000]);
    assert_eq!(codes(5, "export const float K = 1.0\n"), vec![2104]);
    assert_eq!(codes(6, "f(x) =>\nx\n"), vec![2020]);
    assert_eq!(
        codes(6, "f(x) =>\n    for i = 0 to 1\n        break\n    break\n"),
        vec![2010]
    );
}

#[test]
fn soft_keywords_stay_identifiers() {
    let (ast, _, v) = stmts("type = 1\nmethod = 2\nimport = 3\nenum = 4\nexport = 5\n");
    assert!(v.iter().all(|&s| ast.kind(s) == NodeKind::VarDecl));
}
