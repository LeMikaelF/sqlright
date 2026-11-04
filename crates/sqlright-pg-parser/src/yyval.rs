//! The semantic value of a grammar symbol (the Bison `%union`).

use sqlright_ir::IrId;

#[derive(Debug, Clone, PartialEq, Default)]
pub enum YyVal {
    #[default]
    None,
    /// An IR node (`<ir>` symbols).
    Ir(IrId),
    /// A string: identifiers, keywords, string constants (`<str>` symbols).
    Str(String),
    /// An integer: `ICONST` and `PARAM` (`<ival>` symbols).
    Ival(i64),
}

impl YyVal {
    /// The IR node of an `<ir>` value. The generator makes sure the symbol has
    /// that type, so another variant is a bug in the generated code.
    pub fn ir(&self) -> IrId {
        match self {
            YyVal::Ir(id) => *id,
            other => panic!("expected an IR value, got {other:?}"),
        }
    }

    /// A copy of the string of a `<str>` value.
    pub fn str(&self) -> String {
        match self {
            YyVal::Str(s) => s.clone(),
            YyVal::None => String::new(),
            other => panic!("expected a string value, got {other:?}"),
        }
    }

    /// The string of a `<str>` value.
    pub fn sref(&self) -> &str {
        match self {
            YyVal::Str(s) => s.as_str(),
            YyVal::None => "",
            other => panic!("expected a string value, got {other:?}"),
        }
    }

    pub fn ival(&self) -> i64 {
        match self {
            YyVal::Ival(v) => *v,
            other => panic!("expected an integer value, got {other:?}"),
        }
    }

    /// Move the value out, leaving `None`.
    pub fn take(&mut self) -> YyVal {
        std::mem::take(self)
    }
}
