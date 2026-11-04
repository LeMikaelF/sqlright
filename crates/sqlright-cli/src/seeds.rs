//! Build a seed directory for afl-fuzz from SQL files and from the
//! `.sqltest` files of the turso conformance suite.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::harness;

/// The marker SELECT between two statements when the harness filters a seed.
const MARK: &str = "SQLRIGHT MARK";

pub fn build(from: &[PathBuf], out: &Path, max_stmts: usize, harness_bin: Option<&Path>, timeout: Duration) -> Result<()> {
    std::fs::create_dir_all(out).with_context(|| format!("cannot create {}", out.display()))?;
    let mut files = Vec::new();
    for p in from {
        collect_files(p, &mut files)?;
    }
    files.sort();
    let mut seeds = 0usize;
    let mut kept = 0usize;
    let mut dropped = 0usize;
    let mut rejected = 0usize;
    for file in &files {
        let text = std::fs::read_to_string(file).with_context(|| format!("cannot read {}", file.display()))?;
        let is_sqltest = file.extension().map(|e| e == "sqltest").unwrap_or(false);
        let groups = if is_sqltest { sqltest_groups(&text) } else { vec![split_sql(&text)] };
        let stem = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "seed".to_string());
        let mut k = 0;
        for group in groups {
            let mut good = Vec::new();
            for stmt in group {
                if sqlright_pg_parser::parse_query(&stmt).is_some() {
                    good.push(stmt);
                } else {
                    dropped += 1;
                }
            }
            if let Some(h) = harness_bin {
                let before = good.len();
                good = drop_failing_statements(h, good, timeout)?;
                rejected += before - good.len();
            }
            for chunk in good.chunks(max_stmts.max(1)) {
                let mut body = String::new();
                for s in chunk {
                    body.push_str(s);
                    body.push_str(";\n");
                }
                std::fs::write(out.join(format!("{stem}_{k:03}.sql")), body)?;
                k += 1;
                seeds += 1;
                kept += chunk.len();
            }
        }
    }
    eprintln!(
        "{} files read, {seeds} seeds written to {}, {kept} statements kept, {dropped} statements do not parse, {rejected} statements fail on the harness",
        files.len(),
        out.display()
    );
    Ok(())
}

/// Run the statements on the harness, with a marker SELECT after each one,
/// and remove the statements that report an error. A statement that fails
/// because an earlier one was removed goes in the next pass.
fn drop_failing_statements(harness_bin: &Path, mut stmts: Vec<String>, timeout: Duration) -> Result<Vec<String>> {
    for _pass in 0..4 {
        if stmts.is_empty() {
            break;
        }
        let mut text = String::new();
        for (i, s) in stmts.iter().enumerate() {
            text.push_str(s);
            text.push_str(";\n");
            text.push_str(&format!("SELECT '{MARK} {i}';\n"));
        }
        let output = harness::run(harness_bin, &text, timeout)?;
        let mut failed = std::collections::HashSet::new();
        let mut current = 0usize;
        for line in output.lines() {
            if let Some(rest) = line.strip_prefix(MARK) {
                if let Ok(n) = rest.trim().parse::<usize>() {
                    current = n + 1;
                }
            } else if line.starts_with("ERROR:") {
                failed.insert(current);
            }
        }
        if failed.is_empty() {
            break;
        }
        stmts = stmts.into_iter().enumerate().filter(|(i, _)| !failed.contains(i)).map(|(_, s)| s).collect();
    }
    Ok(stmts)
}

fn collect_files(p: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if p.is_dir() {
        for entry in std::fs::read_dir(p).with_context(|| format!("cannot read {}", p.display()))? {
            collect_files(&entry?.path(), out)?;
        }
    } else if p.extension().map(|e| e == "sql" || e == "sqltest").unwrap_or(false) {
        out.push(p.to_path_buf());
    }
    Ok(())
}

/// Split SQL text into statements at the semicolons outside quotes,
/// dollar quotes and comments. The statements come back without the
/// semicolon. Lines that start with `\` (psql commands) are left out.
pub fn split_sql(text: &str) -> Vec<String> {
    let mut stmts = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match c {
            '-' if next == Some('-') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            '/' if next == Some('*') => {
                i += 2;
                while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                    i += 1;
                }
                i += 2;
                continue;
            }
            '\'' | '"' => {
                cur.push(c);
                i += 1;
                while i < chars.len() {
                    cur.push(chars[i]);
                    if chars[i] == c {
                        if chars.get(i + 1) == Some(&c) {
                            cur.push(c);
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
                continue;
            }
            '$' => {
                // $tag$ ... $tag$
                let mut j = i + 1;
                while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
                    j += 1;
                }
                if j < chars.len() && chars[j] == '$' && (i + 1 == j || !chars[i + 1].is_ascii_digit()) {
                    let tag: String = chars[i..=j].iter().collect();
                    let tag_chars: Vec<char> = tag.chars().collect();
                    let mut k = j + 1;
                    while k + tag_chars.len() <= chars.len() && chars[k..k + tag_chars.len()] != tag_chars[..] {
                        k += 1;
                    }
                    let end = (k + tag_chars.len()).min(chars.len());
                    cur.extend(&chars[i..end]);
                    i = end;
                    continue;
                }
                cur.push(c);
                i += 1;
                continue;
            }
            ';' => {
                push_stmt(&mut stmts, &mut cur);
                i += 1;
                continue;
            }
            _ => {
                cur.push(c);
                i += 1;
            }
        }
    }
    push_stmt(&mut stmts, &mut cur);
    stmts
}

fn push_stmt(stmts: &mut Vec<String>, cur: &mut String) {
    let lines: Vec<&str> = cur.lines().filter(|l| !l.trim_start().starts_with('\\')).collect();
    let s = lines.join("\n").trim().to_string();
    if !s.is_empty() {
        stmts.push(s);
    }
    cur.clear();
}

/// The statement groups of a `.sqltest` file: one group per `test` block,
/// with the statements of its `@setup` blocks first. Blocks marked
/// `@backend postgres` run only on PostgreSQL and are left out.
pub fn sqltest_groups(text: &str) -> Vec<Vec<String>> {
    let mut groups = Vec::new();
    let mut setups: Vec<(String, Vec<String>)> = Vec::new();
    let mut pending_setups: Vec<String> = Vec::new();
    let mut pending_backend: Option<String> = None;
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if let Some(rest) = t.strip_prefix('@') {
            let mut parts = rest.split_whitespace();
            match parts.next() {
                Some("setup") => pending_setups.extend(parts.map(|s| s.trim_end_matches(',').to_string())),
                Some("backend") => pending_backend = parts.next().map(|s| s.to_string()),
                _ => {}
            }
            continue;
        }
        if !t.ends_with('{') {
            continue;
        }
        let header: Vec<&str> = t.trim_end_matches('{').split_whitespace().collect();
        let mut body = String::new();
        for l in lines.by_ref() {
            if l.trim() == "}" {
                break;
            }
            body.push_str(l);
            body.push('\n');
        }
        let skip = matches!(pending_backend.as_deref(), Some(b) if b != "turso" && b != "tursopg");
        match header.first().copied() {
            Some("setup") if !skip => {
                let name = header.get(1).copied().unwrap_or("").to_string();
                setups.push((name, split_sql(&body)));
            }
            Some("test") if !skip => {
                let mut group = Vec::new();
                for name in &pending_setups {
                    if let Some((_, stmts)) = setups.iter().find(|(n, _)| n == name) {
                        group.extend(stmts.iter().cloned());
                    }
                }
                group.extend(split_sql(&body));
                if !group.is_empty() {
                    groups.push(group);
                }
            }
            _ => {}
        }
        if header.first().copied() != Some("expect") {
            pending_setups.clear();
            pending_backend = None;
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_at_semicolons_outside_quotes_and_comments() {
        let stmts = split_sql("SELECT 'a;b'; -- c;\nSELECT $$x;y$$, \"q;\"; /* ; */ SELECT 1;\n\\d t\nSELECT 2");
        assert_eq!(stmts, vec!["SELECT 'a;b'", "SELECT $$x;y$$, \"q;\"", "SELECT 1", "SELECT 2"]);
    }

    #[test]
    fn groups_sqltest_tests_with_their_setup() {
        let text = "# comment\n@database :memory:\nsetup s1 {\n    CREATE TABLE t (a INT);\n    INSERT INTO t VALUES (1);\n}\n\n@setup s1\ntest one {\n    SELECT a FROM t;\n}\nexpect {\n    1\n}\n\n@backend postgres\ntest pg_only {\n    SELECT 2;\n}\n\ntest two {\n    SELECT 3;\n}\n";
        let groups = sqltest_groups(text);
        assert_eq!(groups, vec![vec!["CREATE TABLE t (a INT)", "INSERT INTO t VALUES (1)", "SELECT a FROM t"], vec!["SELECT 3"]]);
    }
}
