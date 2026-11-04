//! PostgreSQL parser that builds the SQLRight IR.
//!
//! The grammar lives in `grammar/postgres/bison_parser.y`. The tool
//! `grammar/tools/gen_parser.py` turns it into the files under `generated/`:
//! the LALR(1) tables that Bison computed, the token numbers and keyword
//! table, and the semantic actions translated to Rust. The lexer and the
//! parser driver are written by hand.

#[allow(clippy::all)]
pub mod generated {
    pub mod actions;
    pub mod tables;
    pub mod tokens;
}
pub mod driver;
pub mod lexer;
pub mod yyval;

pub use driver::{parse, ParseError};
use sqlright_ir::{util, IrId, IrTree};

/// Parse a query string into an IR tree and number its nodes, like the C++
/// `Mutator::parse_query_str_get_ir_set`. A missing final semicolon is added.
/// Returns `None` when the query does not parse.
pub fn parse_query(query: &str) -> Option<ParsedQuery> {
    let mut sql = query.to_string();
    util::ensure_semicolon_at_query_end(&mut sql);
    let mut tree = parse(&sql).ok()?;
    let root = tree.root?;
    let nodes = tree.assign_uniq_ids(root);
    Some(ParsedQuery { tree, root, nodes })
}

/// A parsed query: the tree, its root, and all nodes in the order of
/// `IrTree::all_nodes` (the root is last).
#[derive(Debug, Clone)]
pub struct ParsedQuery {
    pub tree: IrTree,
    pub root: IrId,
    pub nodes: Vec<IrId>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_create_table() {
        let q = parse_query("CREATE TABLE v0 (v1 INT, v2 TEXT);").expect("parse");
        assert_eq!(q.tree.to_string(q.root), "CREATE TABLE v0 ( v1 INT, v2 text );");
    }

    #[test]
    fn parses_select() {
        let q = parse_query("SELECT COUNT(*) FROM v0 WHERE v1 = 1").expect("parse");
        assert_eq!(q.tree.to_string(q.root), "SELECT count ( * ) FROM v0 WHERE v1 = 1;");
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_query("SELECT FROM WHERE (").is_none());
    }
}
