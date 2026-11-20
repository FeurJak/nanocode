use ruff_python_ast::{Expr, Stmt, StmtExpr, StmtImportFrom};
use ruff_python_parser::parse_module;

pub fn top_level(source: &str) -> Vec<String> {
    let Ok(parsed) = parse_module(source) else {
        return Vec::new();
    };
    let mut names: Vec<String> = Vec::new();
    let mut add = |name: &str| {
        if !names.iter().any(|known| known == name) {
            names.push(name.to_owned());
        }
    };
    for statement in &parsed.syntax().body {
        match statement {
            Stmt::Import(import) => {
                for alias in &import.names {
                    add(alias.name.as_str());
                }
            }
            Stmt::ImportFrom(StmtImportFrom {
                module: Some(module),
                level: 0,
                ..
            }) => add(module.as_str()),
            Stmt::Expr(StmtExpr { value, .. }) if matches!(**value, Expr::StringLiteral(_)) => {}
            _ => break,
        }
    }
    names
}
