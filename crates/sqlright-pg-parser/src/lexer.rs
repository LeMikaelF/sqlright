//! The PostgreSQL lexer, ported by hand from `scan.l` as SQLRight adapted it,
//! plus the one-token lookahead filter of `parser.c` (`base_yylex`).
//!
//! The lexer returns Bison token numbers (see `generated::tokens`). Single
//! character tokens use the character code. Zero means end of input.
//!
//! Behavior differences from the C++ lexer are limited to error paths that
//! crash or loop forever in C++ (see the comments that say "C++").

use crate::generated::tokens as tok;
use crate::generated::tokens::{KEYWORDS, MAX_KEYWORD_LEN};
use crate::yyval::YyVal;

/// PostgreSQL `NAMEDATALEN`: identifiers are cut to one byte less.
pub const NAMEDATALEN: usize = 64;

/// A token with its value and its byte offset in the input.
#[derive(Debug, Clone)]
pub struct Token {
    pub kind: i32,
    pub val: YyVal,
    pub start: usize,
}

impl Token {
    fn new(kind: i32, val: YyVal, start: usize) -> Token {
        Token { kind, val, start }
    }

    fn plain(kind: i32, start: usize) -> Token {
        Token::new(kind, YyVal::None, start)
    }
}

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c')
}

fn is_newline(c: u8) -> bool {
    c == b'\n' || c == b'\r'
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c >= 0x80
}

fn is_ident_cont(c: u8) -> bool {
    is_ident_start(c) || c.is_ascii_digit() || c == b'$'
}

fn is_dolq_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c >= 0x80
}

fn is_dolq_cont(c: u8) -> bool {
    is_dolq_start(c) || c.is_ascii_digit()
}

/// The characters that are single-character tokens.
fn is_self(c: u8) -> bool {
    matches!(c, b',' | b'(' | b')' | b'[' | b']' | b'.' | b';' | b':' | b'+' | b'-' | b'*' | b'/' | b'%' | b'^' | b'<' | b'>' | b'=')
}

/// The characters that can make up an `Op` token.
fn is_op_char(c: u8) -> bool {
    matches!(c, b'~' | b'!' | b'@' | b'#' | b'^' | b'&' | b'|' | b'`' | b'?' | b'+' | b'-' | b'*' | b'/' | b'%' | b'<' | b'>' | b'=')
}

/// Bytes to a string. Octal and hex escapes can produce bytes that are not
/// UTF-8; C++ keeps them, Rust replaces them with U+FFFD.
fn bytes_to_string(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Downcase ASCII letters and cut to `NAMEDATALEN - 1` bytes, like
/// `downcase_truncate_identifier`.
pub fn downcase_truncate_identifier(ident: &[u8]) -> String {
    let lower: Vec<u8> = ident.iter().map(|c| c.to_ascii_lowercase()).collect();
    let mut s = bytes_to_string(&lower);
    truncate_identifier(&mut s);
    s
}

/// Cut an identifier to `NAMEDATALEN - 1` bytes at a character boundary.
pub fn truncate_identifier(ident: &mut String) {
    if ident.len() >= NAMEDATALEN {
        let mut cut = NAMEDATALEN - 1;
        while !ident.is_char_boundary(cut) {
            cut -= 1;
        }
        ident.truncate(cut);
    }
}

/// Keyword lookup: case-insensitive on ASCII letters only, like
/// `ScanKeywordLookup`. Returns the token number and the keyword name.
pub fn keyword_lookup(word: &[u8]) -> Option<(i32, &'static str)> {
    if word.len() > MAX_KEYWORD_LEN {
        return None;
    }
    let lower: Vec<u8> = word.iter().map(|c| c.to_ascii_lowercase()).collect();
    let idx = KEYWORDS.binary_search_by(|(name, _, _, _)| name.as_bytes().cmp(lower.as_slice())).ok()?;
    let (name, token, _, _) = KEYWORDS[idx];
    Some((token, name))
}

/// `strtoint`: parse a decimal integer that must fit in an `int`.
fn parse_int32(text: &[u8]) -> Option<i64> {
    let s = std::str::from_utf8(text).ok()?;
    let v: i64 = s.parse().ok()?;
    if v > i32::MAX as i64 || v < i32::MIN as i64 {
        return None;
    }
    Some(v)
}

fn hexval(c: u8) -> u32 {
    match c {
        b'0'..=b'9' => (c - b'0') as u32,
        b'a'..=b'f' => (c - b'a') as u32 + 10,
        b'A'..=b'F' => (c - b'A') as u32 + 10,
        _ => 0,
    }
}

fn is_utf16_surrogate_first(c: u32) -> bool {
    (0xD800..=0xDBFF).contains(&c)
}

fn is_utf16_surrogate_second(c: u32) -> bool {
    (0xDC00..=0xDFFF).contains(&c)
}

fn surrogate_pair_to_codepoint(first: u32, second: u32) -> u32 {
    ((first & 0x3FF) << 10) + 0x10000 + (second & 0x3FF)
}

fn push_unicode(out: &mut Vec<u8>, c: u32) {
    if let Some(ch) = char::from_u32(c) {
        let mut buf = [0u8; 4];
        out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
    }
    // C++ prints "invalid Unicode escape value" for other code points and
    // produces nothing useful; we add nothing.
}

/// Process the Unicode escapes of a `U&'...'` string or `U&"..."` identifier
/// (`str_udeescape`). `escape` is the escape character, `\` by default.
pub fn str_udeescape(input: &str, escape: u8) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut pair_first: u32 = 0;
    let mut i = 0;
    let hex4 = |p: usize| -> Option<u32> {
        if p + 4 <= bytes.len() && bytes[p..p + 4].iter().all(|c| c.is_ascii_hexdigit()) {
            Some((hexval(bytes[p]) << 12) + (hexval(bytes[p + 1]) << 8) + (hexval(bytes[p + 2]) << 4) + hexval(bytes[p + 3]))
        } else {
            None
        }
    };
    let hex6 = |p: usize| -> Option<u32> {
        if p + 6 <= bytes.len() && bytes[p..p + 6].iter().all(|c| c.is_ascii_hexdigit()) {
            let mut v = 0u32;
            for k in 0..6 {
                v = (v << 4) + hexval(bytes[p + k]);
            }
            Some(v)
        } else {
            None
        }
    };
    while i < bytes.len() {
        if bytes[i] == escape {
            if i + 1 < bytes.len() && bytes[i + 1] == escape {
                // C++ reports an error for an unfinished surrogate pair here;
                // we drop the pending half.
                pair_first = 0;
                out.push(escape);
                i += 2;
                continue;
            }
            let (unicode, advance) = if let Some(u) = hex4(i + 1) {
                (Some(u), 5)
            } else if i + 1 < bytes.len() && bytes[i + 1] == b'+' {
                match hex6(i + 2) {
                    Some(u) => (Some(u), 8),
                    None => (None, 1),
                }
            } else {
                (None, 1)
            };
            match unicode {
                Some(mut u) => {
                    if pair_first != 0 {
                        if is_utf16_surrogate_second(u) {
                            u = surrogate_pair_to_codepoint(pair_first, u);
                            pair_first = 0;
                        } else {
                            // invalid pair: drop the first half
                            pair_first = 0;
                        }
                    } else if is_utf16_surrogate_second(u) {
                        // invalid pair: skip it
                        i += advance;
                        continue;
                    }
                    if is_utf16_surrogate_first(u) {
                        pair_first = u;
                    } else {
                        push_unicode(&mut out, u);
                    }
                    i += advance;
                }
                None => {
                    // C++ prints "invalid Unicode escape" and loops forever
                    // on this input. We keep the escape character.
                    out.push(bytes[i]);
                    i += 1;
                }
            }
        } else {
            pair_first = 0;
            out.push(bytes[i]);
            i += 1;
        }
    }
    bytes_to_string(&out)
}

/// True when `c` can be an escape character of a `UESCAPE` clause.
pub fn check_uescapechar(c: u8) -> bool {
    !(c.is_ascii_hexdigit() || c == b'+' || c == b'\'' || c == b'"' || is_space(c))
}

/// Which quoted literal is being scanned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuoteKind {
    /// `'...'` with standard conforming strings.
    Standard,
    /// `E'...'`: backslash escapes.
    Extended,
    /// `U&'...'`: Unicode escapes, processed later by the filter.
    Unicode,
    /// `B'...'`
    Bit,
    /// `X'...'`
    Hex,
}

/// The scanner proper: one token per call, no lookahead.
pub struct Scanner<'a> {
    src: &'a [u8],
    pos: usize,
    literal: Vec<u8>,
    /// Messages the C++ lexer prints to stderr. Kept for debugging.
    pub warnings: Vec<String>,
}

impl<'a> Scanner<'a> {
    pub fn new(src: &'a str) -> Scanner<'a> {
        Scanner { src: src.as_bytes(), pos: 0, literal: Vec::new(), warnings: Vec::new() }
    }

    fn peek(&self, offset: usize) -> Option<u8> {
        self.src.get(self.pos + offset).copied()
    }

    fn at(&self, p: usize) -> Option<u8> {
        self.src.get(p).copied()
    }

    fn warn(&mut self, msg: &str) {
        self.warnings.push(msg.to_string());
    }

    /// The next token. Kind 0 is the end of input.
    pub fn next_token(&mut self) -> Token {
        loop {
            let start = self.pos;
            let Some(c) = self.peek(0) else {
                return Token::plain(0, start);
            };
            // {whitespace}
            if is_space(c) {
                self.pos += 1;
                continue;
            }
            if c == b'-' && self.peek(1) == Some(b'-') {
                // {comment}: to the end of the line, the newline is whitespace
                while let Some(ch) = self.peek(0) {
                    if is_newline(ch) {
                        break;
                    }
                    self.pos += 1;
                }
                continue;
            }
            if c == b'/' && self.peek(1) == Some(b'*') {
                if !self.skip_block_comment() {
                    return Token::plain(0, start);
                }
                continue;
            }
            return self.scan_token(start);
        }
    }

    /// Skip a `/* ... */` comment with nesting. False at end of input.
    fn skip_block_comment(&mut self) -> bool {
        self.pos += 2;
        let mut depth = 0;
        loop {
            let Some(c) = self.peek(0) else {
                self.warn("unterminated /* comment");
                return false;
            };
            if c == b'/' && self.peek(1) == Some(b'*') {
                depth += 1;
                self.pos += 2;
            } else if c == b'*' && self.peek(1) == Some(b'/') {
                self.pos += 2;
                if depth <= 0 {
                    return true;
                }
                depth -= 1;
            } else {
                self.pos += 1;
            }
        }
    }

    fn scan_token(&mut self, start: usize) -> Token {
        let c = self.peek(0).unwrap();
        // Quoted literals and identifiers with a one-letter prefix.
        if is_ident_start(c) {
            let mut end = self.pos + 1;
            while let Some(ch) = self.at(end) {
                if !is_ident_cont(ch) {
                    break;
                }
                end += 1;
            }
            if end - self.pos == 1 {
                let next = self.peek(1);
                match (c.to_ascii_lowercase(), next) {
                    (b'b', Some(b'\'')) => {
                        self.pos += 2;
                        return self.scan_quoted(QuoteKind::Bit, start);
                    }
                    (b'x', Some(b'\'')) => {
                        self.pos += 2;
                        return self.scan_quoted(QuoteKind::Hex, start);
                    }
                    (b'e', Some(b'\'')) => {
                        self.pos += 2;
                        return self.scan_quoted(QuoteKind::Extended, start);
                    }
                    (b'n', Some(b'\'')) => {
                        // National character: return the NCHAR keyword, the
                        // quote starts a normal string next.
                        self.pos += 1;
                        if let Some((token, name)) = keyword_lookup(b"nchar") {
                            return Token::new(token, YyVal::Str(name.to_string()), start);
                        }
                        return Token::new(tok::IDENT, YyVal::Str("n".to_string()), start);
                    }
                    (b'u', Some(b'&')) => match self.peek(2) {
                        Some(b'\'') => {
                            self.pos += 3;
                            return self.scan_quoted(QuoteKind::Unicode, start);
                        }
                        Some(b'"') => {
                            self.pos += 3;
                            return self.scan_delimited_identifier(true, start);
                        }
                        _ => {
                            // {xufailed}: the identifier is just "u"
                            self.pos += 1;
                            return Token::new(tok::IDENT, YyVal::Str(downcase_truncate_identifier(&[c])), start);
                        }
                    },
                    _ => {}
                }
            }
            let word = &self.src[self.pos..end];
            self.pos = end;
            if let Some((token, name)) = keyword_lookup(word) {
                return Token::new(token, YyVal::Str(name.to_string()), start);
            }
            return Token::new(tok::IDENT, YyVal::Str(downcase_truncate_identifier(word)), start);
        }
        match c {
            b'\'' => {
                self.pos += 1;
                self.scan_quoted(QuoteKind::Standard, start)
            }
            b'"' => {
                self.pos += 1;
                self.scan_delimited_identifier(false, start)
            }
            b'$' => self.scan_dollar(start),
            b'0'..=b'9' => self.scan_number(start),
            b'.' => {
                if self.peek(1) == Some(b'.') {
                    self.pos += 2;
                    Token::plain(tok::DOT_DOT, start)
                } else if self.peek(1).is_some_and(|d| d.is_ascii_digit()) {
                    self.scan_number(start)
                } else {
                    self.pos += 1;
                    Token::plain(b'.' as i32, start)
                }
            }
            b':' => {
                if self.peek(1) == Some(b':') {
                    self.pos += 2;
                    Token::plain(tok::TYPECAST, start)
                } else if self.peek(1) == Some(b'=') {
                    self.pos += 2;
                    Token::plain(tok::COLON_EQUALS, start)
                } else {
                    self.pos += 1;
                    Token::plain(b':' as i32, start)
                }
            }
            _ if is_op_char(c) => self.scan_operator(start),
            _ if is_self(c) => {
                self.pos += 1;
                Token::plain(c as i32, start)
            }
            _ => {
                // {other}: any other character is its own token
                self.pos += 1;
                Token::plain(c as i32, start)
            }
        }
    }

    /// `{operator}` and the special two-character tokens.
    fn scan_operator(&mut self, start: usize) -> Token {
        let mut end = self.pos;
        while let Some(ch) = self.at(end) {
            if !is_op_char(ch) {
                break;
            }
            end += 1;
        }
        let run = &self.src[self.pos..end];
        let len = run.len();
        // Two-character tokens win the tie against {operator}.
        if len == 2 {
            if let Some(kind) = two_char_token(run[0], run[1]) {
                self.pos = end;
                return Token::plain(kind, start);
            }
        }
        if len == 1 {
            self.pos = end;
            if is_self(run[0]) {
                return Token::plain(run[0] as i32, start);
            }
            return Token::new(tok::Op, YyVal::Str(bytes_to_string(run)), start);
        }
        // An embedded comment start ends the operator.
        let mut nchars = len;
        for i in 1..len.saturating_sub(1) + 1 {
            if i < len {
                if run.get(i) == Some(&b'/') && run.get(i + 1) == Some(&b'*') {
                    nchars = i;
                    break;
                }
                if run.get(i) == Some(&b'-') && run.get(i + 1) == Some(&b'-') {
                    nchars = i;
                    break;
                }
            }
        }
        // '+' and '-' cannot end a multi-character operator unless the
        // operator has a character that is not in SQL operators.
        if nchars > 1 && (run[nchars - 1] == b'+' || run[nchars - 1] == b'-') {
            let mut has_special = false;
            for i in (0..nchars - 1).rev() {
                if matches!(run[i], b'~' | b'!' | b'@' | b'#' | b'^' | b'&' | b'|' | b'`' | b'?' | b'%') {
                    has_special = true;
                    break;
                }
            }
            if !has_special {
                loop {
                    nchars -= 1;
                    if !(nchars > 1 && (run[nchars - 1] == b'+' || run[nchars - 1] == b'-')) {
                        break;
                    }
                }
            }
        }
        self.pos += nchars;
        let text = &run[..nchars];
        if nchars < len {
            if nchars == 1 && is_self(text[0]) {
                return Token::plain(text[0] as i32, start);
            }
            if nchars == 2 {
                if let Some(kind) = two_char_token(text[0], text[1]) {
                    return Token::plain(kind, start);
                }
            }
        }
        if nchars >= NAMEDATALEN {
            self.warn("operator too long");
        }
        Token::new(tok::Op, YyVal::Str(bytes_to_string(text)), start)
    }

    /// `$1` parameters, `$tag$` dollar quotes, or a lone `$`.
    fn scan_dollar(&mut self, start: usize) -> Token {
        match self.peek(1) {
            Some(d) if d.is_ascii_digit() => {
                let mut end = self.pos + 1;
                while self.at(end).is_some_and(|c| c.is_ascii_digit()) {
                    end += 1;
                }
                let num: i64 = std::str::from_utf8(&self.src[self.pos + 1..end]).unwrap().parse().unwrap_or(0);
                self.pos = end;
                Token::new(tok::PARAM, YyVal::Ival(num), start)
            }
            Some(b'$') => {
                self.pos += 2;
                self.scan_dollar_quoted(b"$$".to_vec(), start)
            }
            Some(c) if is_dolq_start(c) => {
                let mut end = self.pos + 1;
                while self.at(end).is_some_and(is_dolq_cont) {
                    end += 1;
                }
                if self.at(end) == Some(b'$') {
                    let delim = self.src[self.pos..=end].to_vec();
                    self.pos = end + 1;
                    self.scan_dollar_quoted(delim, start)
                } else {
                    // {dolqfailed}: return the '$' alone
                    self.pos += 1;
                    Token::plain(b'$' as i32, start)
                }
            }
            _ => {
                self.pos += 1;
                Token::plain(b'$' as i32, start)
            }
        }
    }

    /// The body of a `$tag$ ... $tag$` string. No escape processing.
    fn scan_dollar_quoted(&mut self, delim: Vec<u8>, start: usize) -> Token {
        self.literal.clear();
        loop {
            let Some(c) = self.peek(0) else {
                self.warn("unterminated dollar-quoted string");
                return Token::plain(0, start);
            };
            if c == b'$' {
                // Is this a delimiter?
                let mut end = self.pos + 1;
                if self.at(end) == Some(b'$') || self.at(end).is_some_and(is_dolq_start) {
                    while self.at(end).is_some_and(is_dolq_cont) {
                        end += 1;
                    }
                    if self.at(end) == Some(b'$') {
                        let candidate = &self.src[self.pos..=end];
                        if candidate == delim.as_slice() {
                            self.pos = end + 1;
                            let s = bytes_to_string(&self.literal);
                            return Token::new(tok::SCONST, YyVal::Str(s), start);
                        }
                        // Not our delimiter: keep "$tag" and rescan the last "$".
                        self.literal.extend_from_slice(&self.src[self.pos..end]);
                        self.pos = end;
                        continue;
                    }
                    // {dolqfailed} inside: "$tag" without a closing "$"
                    self.literal.extend_from_slice(&self.src[self.pos..end]);
                    self.pos = end;
                    continue;
                }
                self.literal.push(b'$');
                self.pos += 1;
                continue;
            }
            let mut end = self.pos;
            while self.at(end).is_some_and(|ch| ch != b'$') {
                end += 1;
            }
            self.literal.extend_from_slice(&self.src[self.pos..end]);
            self.pos = end;
        }
    }

    /// `"..."` and `U&"..."` identifiers.
    fn scan_delimited_identifier(&mut self, unicode: bool, start: usize) -> Token {
        self.literal.clear();
        loop {
            let Some(c) = self.peek(0) else {
                self.warn("unterminated quoted identifier");
                return Token::plain(0, start);
            };
            if c == b'"' {
                if self.peek(1) == Some(b'"') {
                    self.literal.push(b'"');
                    self.pos += 2;
                    continue;
                }
                self.pos += 1;
                if self.literal.is_empty() {
                    self.warn("zero-length delimited identifier");
                }
                if unicode {
                    // De-escaping and truncation happen in the filter.
                    return Token::new(tok::UIDENT, YyVal::Str(bytes_to_string(&self.literal)), start);
                }
                let mut ident = bytes_to_string(&self.literal);
                truncate_identifier(&mut ident);
                return Token::new(tok::IDENT, YyVal::Str(ident), start);
            }
            self.literal.push(c);
            self.pos += 1;
        }
    }

    /// Integer, decimal and real numbers, with the `realfail` and
    /// `decimalfail` rules of the flex scanner.
    fn scan_number(&mut self, start: usize) -> Token {
        let mut end = self.pos;
        while self.at(end).is_some_and(|c| c.is_ascii_digit()) {
            end += 1;
        }
        let mut is_decimal = false;
        if self.at(end) == Some(b'.') {
            if self.at(end + 1) == Some(b'.') {
                // {decimalfail}: "1.." is the integer 1 followed by DOT_DOT
            } else {
                is_decimal = true;
                end += 1;
                while self.at(end).is_some_and(|c| c.is_ascii_digit()) {
                    end += 1;
                }
            }
        }
        // exponent
        if matches!(self.at(end), Some(b'e') | Some(b'E')) {
            let mut e = end + 1;
            if matches!(self.at(e), Some(b'+') | Some(b'-')) {
                e += 1;
            }
            if self.at(e).is_some_and(|c| c.is_ascii_digit()) {
                while self.at(e).is_some_and(|c| c.is_ascii_digit()) {
                    e += 1;
                }
                end = e;
                is_decimal = true;
            }
            // else {realfail1}/{realfail2}: the exponent is not part of the number
        }
        let text = &self.src[self.pos..end];
        self.pos = end;
        if !is_decimal {
            if let Some(v) = parse_int32(text) {
                return Token::new(tok::ICONST, YyVal::Ival(v), start);
            }
        }
        Token::new(tok::FCONST, YyVal::Str(bytes_to_string(text)), start)
    }

    /// A quoted literal. `pos` is just after the opening quote.
    fn scan_quoted(&mut self, kind: QuoteKind, start: usize) -> Token {
        self.literal.clear();
        match kind {
            QuoteKind::Bit => self.literal.push(b'b'),
            QuoteKind::Hex => self.literal.push(b'x'),
            _ => {}
        }
        loop {
            let Some(c) = self.peek(0) else {
                self.warn(match kind {
                    QuoteKind::Bit => "unterminated bit string literal",
                    QuoteKind::Hex => "unterminated hexadecimal string literal",
                    _ => "unterminated quoted string",
                });
                return Token::plain(0, start);
            };
            if c == b'\'' {
                if kind != QuoteKind::Bit && kind != QuoteKind::Hex && self.peek(1) == Some(b'\'') {
                    // {xqdouble}
                    self.literal.push(b'\'');
                    self.pos += 2;
                    continue;
                }
                // Quote stop: look for a continuation on the next line.
                let after = self.pos + 1;
                if let Some(np) = self.quote_continue(after) {
                    self.pos = np;
                    continue;
                }
                self.pos = after;
                let s = bytes_to_string(&self.literal);
                let kind_tok = match kind {
                    QuoteKind::Bit => tok::BCONST,
                    QuoteKind::Hex => tok::XCONST,
                    QuoteKind::Standard | QuoteKind::Extended => tok::SCONST,
                    QuoteKind::Unicode => tok::USCONST,
                };
                return Token::new(kind_tok, YyVal::Str(s), start);
            }
            if kind == QuoteKind::Extended && c == b'\\' {
                self.scan_extended_escape();
                continue;
            }
            self.literal.push(c);
            self.pos += 1;
        }
    }

    /// One backslash escape inside `E'...'`. `pos` is at the backslash.
    fn scan_extended_escape(&mut self) {
        let Some(c1) = self.peek(1) else {
            // "\" just before the end of input: keep it
            self.literal.push(b'\\');
            self.pos += 1;
            return;
        };
        match c1 {
            b'0'..=b'7' => {
                // {xeoctesc}: up to three octal digits
                let mut end = self.pos + 1;
                let mut n = 0;
                while n < 3 && self.at(end).is_some_and(|c| (b'0'..=b'7').contains(&c)) {
                    end += 1;
                    n += 1;
                }
                let v = std::str::from_utf8(&self.src[self.pos + 1..end]).unwrap();
                let byte = u32::from_str_radix(v, 8).unwrap_or(0) as u8;
                self.literal.push(byte);
                self.pos = end;
            }
            b'x' if self.peek(2).is_some_and(|c| c.is_ascii_hexdigit()) => {
                // {xehexesc}: one or two hex digits
                let mut end = self.pos + 2;
                let mut n = 0;
                while n < 2 && self.at(end).is_some_and(|c| c.is_ascii_hexdigit()) {
                    end += 1;
                    n += 1;
                }
                let v = std::str::from_utf8(&self.src[self.pos + 2..end]).unwrap();
                let byte = u32::from_str_radix(v, 16).unwrap_or(0) as u8;
                self.literal.push(byte);
                self.pos = end;
            }
            b'u' | b'U' => {
                let want = if c1 == b'u' { 4 } else { 8 };
                let mut end = self.pos + 2;
                let mut n = 0;
                while n < want && self.at(end).is_some_and(|c| c.is_ascii_hexdigit()) {
                    end += 1;
                    n += 1;
                }
                if n < want {
                    // {xeunicodefail}: C++ prints an error and drops the text
                    self.warn("invalid Unicode escape");
                    self.pos = end;
                    return;
                }
                let v = std::str::from_utf8(&self.src[self.pos + 2..end]).unwrap();
                let c = u32::from_str_radix(v, 16).unwrap_or(0);
                self.pos = end;
                if is_utf16_surrogate_first(c) {
                    // {xeu}: the second half must follow at once
                    if let Some(second) = self.try_unicode_escape() {
                        if is_utf16_surrogate_second(second) {
                            push_unicode(&mut self.literal, surrogate_pair_to_codepoint(c, second));
                        } else {
                            self.warn("invalid Unicode surrogate pair");
                        }
                    } else {
                        self.warn("invalid Unicode surrogate pair");
                    }
                } else if is_utf16_surrogate_second(c) {
                    self.warn("invalid Unicode surrogate pair");
                } else {
                    push_unicode(&mut self.literal, c);
                }
            }
            _ => {
                // {xeescape}: a single escaped character
                let unescaped = match c1 {
                    b'b' => 8,
                    b'f' => 12,
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    other => other,
                };
                self.literal.push(unescaped);
                self.pos += 2;
            }
        }
    }

    /// Read a `\uXXXX` or `\UXXXXXXXX` escape at `pos`, when there is one.
    fn try_unicode_escape(&mut self) -> Option<u32> {
        if self.peek(0) != Some(b'\\') {
            return None;
        }
        let want = match self.peek(1) {
            Some(b'u') => 4,
            Some(b'U') => 8,
            _ => return None,
        };
        let mut end = self.pos + 2;
        let mut n = 0;
        while n < want && self.at(end).is_some_and(|c| c.is_ascii_hexdigit()) {
            end += 1;
            n += 1;
        }
        if n < want {
            return None;
        }
        let v = std::str::from_utf8(&self.src[self.pos + 2..end]).unwrap();
        self.pos = end;
        Some(u32::from_str_radix(v, 16).unwrap_or(0))
    }

    /// `{quotecontinue}`: whitespace with at least one newline, then a quote.
    /// Returns the position after that quote.
    fn quote_continue(&self, mut p: usize) -> Option<usize> {
        // {horiz_whitespace}*
        loop {
            match self.at(p) {
                Some(b' ') | Some(b'\t') | Some(b'\x0c') => p += 1,
                Some(b'-') if self.at(p + 1) == Some(b'-') => {
                    while self.at(p).is_some_and(|c| !is_newline(c)) {
                        p += 1;
                    }
                }
                _ => break,
            }
        }
        // {newline}
        if !self.at(p).is_some_and(is_newline) {
            return None;
        }
        p += 1;
        // {special_whitespace}*
        loop {
            match self.at(p) {
                Some(c) if is_space(c) => p += 1,
                Some(b'-') if self.at(p + 1) == Some(b'-') => {
                    while self.at(p).is_some_and(|c| !is_newline(c)) {
                        p += 1;
                    }
                    // {comment}{newline}: the newline must follow
                    if !self.at(p).is_some_and(is_newline) {
                        return None;
                    }
                    p += 1;
                }
                _ => break,
            }
        }
        if self.at(p) == Some(b'\'') {
            Some(p + 1)
        } else {
            None
        }
    }
}

fn two_char_token(a: u8, b: u8) -> Option<i32> {
    match (a, b) {
        (b'=', b'>') => Some(tok::EQUALS_GREATER),
        (b'>', b'=') => Some(tok::GREATER_EQUALS),
        (b'<', b'=') => Some(tok::LESS_EQUALS),
        (b'<', b'>') => Some(tok::NOT_EQUALS),
        (b'!', b'=') => Some(tok::NOT_EQUALS),
        _ => None,
    }
}

/// The lexer the parser uses: the scanner plus the lookahead filter of
/// `base_yylex`, which merges `NOT BETWEEN`, `NULLS FIRST`, `WITH TIME` into
/// single tokens and resolves `UESCAPE`.
pub struct Lexer<'a> {
    scanner: Scanner<'a>,
    lookahead: Option<Token>,
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Lexer<'a> {
        Lexer { scanner: Scanner::new(src), lookahead: None }
    }

    pub fn warnings(&self) -> &[String] {
        &self.scanner.warnings
    }

    pub fn next_token(&mut self) -> Token {
        let mut cur = match self.lookahead.take() {
            Some(t) => t,
            None => self.scanner.next_token(),
        };
        if !matches!(cur.kind, k if k == tok::NOT || k == tok::NULLS_P || k == tok::WITH || k == tok::UIDENT || k == tok::USCONST) {
            return cur;
        }
        let next = self.scanner.next_token();
        let next_kind = next.kind;
        self.lookahead = Some(next);
        if cur.kind == tok::NOT {
            if matches!(next_kind, k if k == tok::BETWEEN || k == tok::IN_P || k == tok::LIKE || k == tok::ILIKE || k == tok::SIMILAR) {
                cur.kind = tok::NOT_LA;
            }
        } else if cur.kind == tok::NULLS_P {
            if next_kind == tok::FIRST_P || next_kind == tok::LAST_P {
                cur.kind = tok::NULLS_LA;
            }
        } else if cur.kind == tok::WITH {
            if next_kind == tok::TIME || next_kind == tok::ORDINALITY {
                cur.kind = tok::WITH_LA;
            }
        } else {
            // UIDENT or USCONST
            let mut escape = b'\\';
            if next_kind == tok::UESCAPE {
                let third = self.scanner.next_token();
                if third.kind != tok::SCONST {
                    self.scanner.warn("UESCAPE must be followed by a simple string literal");
                } else {
                    let esc = third.val.sref().as_bytes().to_vec();
                    if esc.len() != 1 || !check_uescapechar(esc[0]) {
                        self.scanner.warn("invalid Unicode escape character");
                    } else {
                        escape = esc[0];
                    }
                }
                // All three tokens are consumed.
                self.lookahead = None;
            }
            let mut s = str_udeescape(cur.val.sref(), escape);
            if cur.kind == tok::UIDENT {
                truncate_identifier(&mut s);
                cur.kind = tok::IDENT;
            } else {
                cur.kind = tok::SCONST;
            }
            cur.val = YyVal::Str(s);
        }
        cur
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(sql: &str) -> Vec<(i32, YyVal)> {
        let mut lx = Lexer::new(sql);
        let mut out = Vec::new();
        loop {
            let t = lx.next_token();
            if t.kind == 0 {
                break;
            }
            out.push((t.kind, t.val));
        }
        out
    }

    #[test]
    fn keywords_and_identifiers() {
        let toks = kinds("SELECT Foo FROM \"Bar\"");
        assert_eq!(toks[0], (tok::SELECT, YyVal::Str("select".into())));
        assert_eq!(toks[1], (tok::IDENT, YyVal::Str("foo".into())));
        assert_eq!(toks[2], (tok::FROM, YyVal::Str("from".into())));
        assert_eq!(toks[3], (tok::IDENT, YyVal::Str("Bar".into())));
    }

    #[test]
    fn numbers() {
        let toks = kinds("1 1.5 1e5 1.. 3e 99999999999");
        assert_eq!(toks[0], (tok::ICONST, YyVal::Ival(1)));
        assert_eq!(toks[1], (tok::FCONST, YyVal::Str("1.5".into())));
        assert_eq!(toks[2], (tok::FCONST, YyVal::Str("1e5".into())));
        assert_eq!(toks[3], (tok::ICONST, YyVal::Ival(1)));
        assert_eq!(toks[4].0, tok::DOT_DOT);
        assert_eq!(toks[5], (tok::ICONST, YyVal::Ival(3)));
        assert_eq!(toks[6], (tok::IDENT, YyVal::Str("e".into())));
        assert_eq!(toks[7], (tok::FCONST, YyVal::Str("99999999999".into())));
    }

    #[test]
    fn strings() {
        let toks = kinds("'a''b' E'x\\ny' $$dollar$$ $t$a$b$t$ B'01' X'ff' 'cont'\n'inue'");
        assert_eq!(toks[0], (tok::SCONST, YyVal::Str("a'b".into())));
        assert_eq!(toks[1], (tok::SCONST, YyVal::Str("x\ny".into())));
        assert_eq!(toks[2], (tok::SCONST, YyVal::Str("dollar".into())));
        assert_eq!(toks[3], (tok::SCONST, YyVal::Str("a$b".into())));
        assert_eq!(toks[4], (tok::BCONST, YyVal::Str("b01".into())));
        assert_eq!(toks[5], (tok::XCONST, YyVal::Str("xff".into())));
        assert_eq!(toks[6], (tok::SCONST, YyVal::Str("continue".into())));
        assert_eq!(toks.len(), 7);
    }

    #[test]
    fn operators() {
        let toks = kinds("a <> b != c <= d >= e => f :: g := h + - * ~ @> +- <=>");
        let ops: Vec<i32> = toks.iter().map(|t| t.0).collect();
        assert_eq!(ops[1], tok::NOT_EQUALS);
        assert_eq!(ops[3], tok::NOT_EQUALS);
        assert_eq!(ops[5], tok::LESS_EQUALS);
        assert_eq!(ops[7], tok::GREATER_EQUALS);
        assert_eq!(ops[9], tok::EQUALS_GREATER);
        assert_eq!(ops[11], tok::TYPECAST);
        assert_eq!(ops[13], tok::COLON_EQUALS);
        assert_eq!(ops[15], b'+' as i32);
        assert_eq!(ops[16], b'-' as i32);
        assert_eq!(ops[17], b'*' as i32);
        assert_eq!(toks[18], (tok::Op, YyVal::Str("~".into())));
        assert_eq!(toks[19], (tok::Op, YyVal::Str("@>".into())));
        // "+-" is two tokens: '+' then '-'
        assert_eq!(ops[20], b'+' as i32);
        assert_eq!(ops[21], b'-' as i32);
        assert_eq!(toks[22], (tok::Op, YyVal::Str("<=>".into())));
    }

    #[test]
    fn lookahead_filter() {
        let toks = kinds("a NOT IN (1) NULLS FIRST WITH TIME WITH x");
        let ops: Vec<i32> = toks.iter().map(|t| t.0).collect();
        assert_eq!(ops[1], tok::NOT_LA);
        assert_eq!(ops[2], tok::IN_P);
        assert_eq!(ops[6], tok::NULLS_LA);
        assert_eq!(ops[8], tok::WITH_LA);
        assert_eq!(ops[10], tok::WITH);
    }

    #[test]
    fn unicode_escapes() {
        let toks = kinds("U&'d\\0061t\\+000061' U&'d!0061' UESCAPE '!' U&\"d\\0061\"");
        assert_eq!(toks[0], (tok::SCONST, YyVal::Str("data".into())));
        assert_eq!(toks[1], (tok::SCONST, YyVal::Str("da".into())));
        assert_eq!(toks[2], (tok::IDENT, YyVal::Str("da".into())));
    }

    #[test]
    fn comments() {
        let toks = kinds("a -- c\n /* x /* y */ z */ b");
        assert_eq!(toks.len(), 2);
    }
}
