//! String helpers shared by the parser, the mutator and the oracles.
//!
//! These are ports of `utils.cpp` from the C++ SQLRight. The exact behavior
//! matters because the mutator hashes and compares the strings they produce.

/// Normalize the spaces of a query string, in place.
///
/// Leading spaces are removed. Runs of spaces become one space. A space
/// before `;` or `,` is removed. A trailing space is removed. Only the space
/// character counts, not tabs or newlines.
pub fn trim_string(res: &mut String) {
    let bytes = res.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() && bytes[idx] == b' ' {
        idx += 1;
    }
    let mut prev_is_space = false;
    while idx < bytes.len() {
        let c = bytes[idx];
        if c == b' ' {
            if !prev_is_space {
                prev_is_space = true;
                out.push(c);
            }
        } else if c == b';' || c == b',' {
            if prev_is_space {
                let last = out.len() - 1;
                out[last] = c;
            } else {
                out.push(c);
            }
            prev_is_space = false;
        } else {
            prev_is_space = false;
            out.push(c);
        }
        idx += 1;
    }
    if out.last() == Some(&b' ') {
        out.pop();
    }
    // The input was valid UTF-8 and we only removed ASCII spaces.
    *res = String::from_utf8(out).expect("trim_string keeps UTF-8 valid");
}

/// Return a normalized copy of `s`. See [`trim_string`].
pub fn trimmed(s: &str) -> String {
    let mut res = s.to_string();
    trim_string(&mut res);
    res
}

/// Split `s` at every `delimiter`. A trailing delimiter gives a trailing
/// empty piece, like the C++ `string_splitter`.
pub fn string_splitter(s: &str, delimiter: char) -> Vec<String> {
    s.split(delimiter).map(|p| p.to_string()).collect()
}

/// True when the string has only whitespace.
pub fn is_str_empty(s: &str) -> bool {
    s.chars().all(|c| c.is_whitespace() || c == '\0')
}

/// Case-insensitive substring test (ASCII case folding, like `toupper`).
pub fn find_string_in(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let h: Vec<u8> = haystack.bytes().map(|b| b.to_ascii_uppercase()).collect();
    let n: Vec<u8> = needle.bytes().map(|b| b.to_ascii_uppercase()).collect();
    h.windows(n.len()).any(|w| w == n.as_slice())
}

/// Add `; ` when the statement does not end with a semicolon.
pub fn ensure_semicolon_at_query_end(stmt: &mut String) {
    if stmt.is_empty() {
        return;
    }
    if !stmt.ends_with(';') {
        stmt.push_str("; ");
    }
}

/// The 64-bit hash the C++ SQLRight uses for its libraries (MurmurHash64A
/// with a fixed seed).
pub fn fuzzing_hash(key: &[u8]) -> u64 {
    const M: u64 = 0xc6a4a7935bd1e995;
    const R: u32 = 47;
    let len = key.len();
    let mut h: u64 = 0xdeadbeefdeadbeef ^ ((len as u64).wrapping_mul(M));
    let mut chunks = key.chunks_exact(8);
    for chunk in &mut chunks {
        let mut k = u64::from_le_bytes(chunk.try_into().unwrap());
        k = k.wrapping_mul(M);
        k ^= k >> R;
        k = k.wrapping_mul(M);
        h ^= k;
        h = h.wrapping_mul(M);
    }
    let rest = chunks.remainder();
    if !rest.is_empty() {
        for (i, b) in rest.iter().enumerate() {
            h ^= (*b as u64) << (8 * i);
        }
        h = h.wrapping_mul(M);
    }
    h ^= h >> R;
    h = h.wrapping_mul(M);
    h ^= h >> R;
    h
}

/// Hash of a string, as the mutator libraries use it.
pub fn hash_str(s: &str) -> u64 {
    fuzzing_hash(s.as_bytes())
}

/// True when every character is a digit, a dot or a space, or the string is "NaN".
pub fn is_digits(s: &str) -> bool {
    if s == "NaN" {
        return true;
    }
    s.chars()
        .all(|c| c.is_ascii_digit() || c == '.' || c == ' ')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_string_collapses_spaces_and_fixes_punctuation() {
        let mut s = "  SELECT  a ,  b FROM t  ; ".to_string();
        trim_string(&mut s);
        assert_eq!(s, "SELECT a, b FROM t;");
    }

    #[test]
    fn splitter_keeps_trailing_empty_piece() {
        assert_eq!(string_splitter("a;b;", ';'), vec!["a", "b", ""]);
    }

    #[test]
    fn hash_is_stable() {
        assert_eq!(fuzzing_hash(b""), fuzzing_hash(b""));
        assert_ne!(fuzzing_hash(b"a"), fuzzing_hash(b"b"));
        assert_ne!(fuzzing_hash(b"abcdefghi"), fuzzing_hash(b"abcdefghj"));
    }
}
