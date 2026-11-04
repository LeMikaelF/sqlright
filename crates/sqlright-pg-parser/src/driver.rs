//! The LALR(1) parser driver. It is the algorithm of Bison's `yacc.c`
//! skeleton, run on the tables that Bison computed for the grammar.

use sqlright_ir::IrTree;

use crate::generated::actions::reduce;
use crate::generated::tables::*;
use crate::lexer::{Lexer, Token};
use crate::yyval::YyVal;

/// A syntax error. The C++ parser returns no detail, so the fields are for
/// debugging only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// Byte offset of the token the parser could not accept.
    pub position: usize,
    /// The Bison token number of that token (0 at the end of input).
    pub token: i32,
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} at byte {} (token {})", self.message, self.position, self.token)
    }
}

impl std::error::Error for ParseError {}

/// Translate a token number to a symbol number.
fn translate(token: i32) -> i32 {
    if (0..=YYMAXUTOK).contains(&token) {
        YYTRANSLATE[token as usize] as i32
    } else {
        2 // YYSYMBOL_YYUNDEF
    }
}

/// Parse `sql` into an IR tree. The root of the tree is a `ParseToplevel` node.
pub fn parse(sql: &str) -> Result<IrTree, ParseError> {
    let mut tree = IrTree::new();
    let mut lexer = Lexer::new(sql);
    let mut states: Vec<i32> = vec![0];
    let mut values: Vec<YyVal> = vec![YyVal::None];
    let mut lookahead: Option<Token> = None;
    let mut rhs_buf: Vec<YyVal> = Vec::new();

    loop {
        let state = *states.last().unwrap();
        if state == YYFINAL {
            break;
        }
        let mut yyn = YYPACT[state as usize];
        let mut reduce_rule: Option<usize> = None;
        if yyn == YYPACT_NINF {
            // default action
        } else {
            let tok = match &lookahead {
                Some(t) => t,
                None => {
                    lookahead = Some(lexer.next_token());
                    lookahead.as_ref().unwrap()
                }
            };
            let symbol = translate(tok.kind);
            yyn += symbol;
            if !(0..=YYLAST).contains(&yyn) || YYCHECK[yyn as usize] as i32 != symbol {
                // default action
            } else {
                let action = YYTABLE[yyn as usize] as i32;
                if action <= 0 {
                    if action == 0 || action == YYTABLE_NINF {
                        return Err(syntax_error(&lookahead));
                    }
                    reduce_rule = Some((-action) as usize);
                } else {
                    // shift
                    let tok = lookahead.take().unwrap();
                    states.push(action);
                    values.push(tok.val);
                    continue;
                }
            }
        }
        let rule = match reduce_rule {
            Some(r) => r,
            None => {
                let r = YYDEFACT[state as usize] as i32;
                if r == 0 {
                    return Err(syntax_error(&lookahead));
                }
                r as usize
            }
        };
        // reduce
        let len = YYR2[rule] as usize;
        let split = values.len() - len;
        rhs_buf.clear();
        rhs_buf.extend(values.drain(split..));
        states.truncate(states.len() - len);
        let value = reduce(rule, &mut rhs_buf, &mut tree);
        let lhs = YYR1[rule] as i32;
        let top = *states.last().unwrap();
        let goto_index = YYPGOTO[(lhs - YYNTOKENS) as usize] as i32 + top;
        let next_state = if (0..=YYLAST).contains(&goto_index) && YYCHECK[goto_index as usize] as i32 == top {
            YYTABLE[goto_index as usize] as i32
        } else {
            YYDEFGOTO[(lhs - YYNTOKENS) as usize] as i32
        };
        states.push(next_state);
        values.push(value);
    }
    if tree.root.is_none() {
        return Err(ParseError { position: 0, token: 0, message: "the grammar did not set a root".to_string() });
    }
    Ok(tree)
}

fn syntax_error(lookahead: &Option<Token>) -> ParseError {
    match lookahead {
        Some(t) => ParseError { position: t.start, token: t.kind, message: "syntax error".to_string() },
        None => ParseError { position: 0, token: 0, message: "syntax error".to_string() },
    }
}
