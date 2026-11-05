---
name: fuzz-campaign
description: Set up a cloud sandbox, build SQLRight and the tursopg harness, start a long fuzzing campaign detached from the shell, watch it, triage its reports and record the findings. Use when asked to run, resume, watch or triage a SQLRight campaign against tursopg.
---

# Run a SQLRight campaign in a sandbox

This skill is for an agent in a cloud sandbox like the one SQLRight was
written in: Linux x86-64, 4 cores, 15 GB of memory, a Rust toolchain,
network access through a proxy, and a tool that stops a background
command after 2 hours. Read `AGENTS.md` first. It explains the layout,
the run-time flow and the known false-positive classes.

## 1. Set up the sandbox (once, about 20 minutes)

Run `scripts/setup.sh` from the repository root, or do its steps by hand:

1. `git submodule update --init --depth 1` (the turso source).
2. `cargo install cargo-afl && cargo afl config --build` (builds AFL++;
   needs a C compiler and `make`).
3. `cargo build --release` (the `sqlright` command and `libsqlright_afl.so`).
4. Build the harness for a common CPU level, not for the build machine:
   `cd crates/sqlright-harness-tursopg && RUSTFLAGS="-C target-cpu=x86-64-v3" cargo afl build --release`.
   `cargo afl build` uses `-C target-cpu=native` by default. A container
   can restart on another machine. Then the harness stops with SIGILL on
   every input, and afl-fuzz saves every input as a crash.
5. Make sure that the harness works:
   `printf 'SELECT 1;\n' > /tmp/one.sql && crates/sqlright-harness-tursopg/target/release/sqlright-harness-tursopg --replay /tmp/one.sql`
   must print `1`.
6. Build the seeds. The harness filter leaves out the statements that
   tursopg rejects, and transaction statements are left out by default:
   ```bash
   H=$PWD/crates/sqlright-harness-tursopg/target/release/sqlright-harness-tursopg
   target/release/sqlright seeds --from corpus/postgres/inputs --out work/seeds-pg --harness $H
   target/release/sqlright seeds --from turso/postgres/conformance/pg-sqltests --out work/seeds-turso --harness $H
   mkdir -p work/seeds && for f in work/seeds-pg/*; do cp "$f" "work/seeds/pg_$(basename $f)"; done && for f in work/seeds-turso/*; do cp "$f" "work/seeds/turso_$(basename $f)"; done
   ```

`work/` and `target*/` are ignored by git. Never commit them.

## 2. Start the campaign

Start one afl-fuzz instance per oracle (NOREC, TLP, INDEX) on the 4 cores.
The sandbox tool kills a background command after 2 hours, so start each
instance detached from the shell. `scripts/launch.sh` does this:

```bash
.claude/skills/fuzz-campaign/scripts/launch.sh camp 43200 NOREC TLP INDEX
```

The first argument names the output directories (`work/camp-norec`,
`work/camp-tlp`, `work/camp-index`), the second is the run time in
seconds (43200 is 12 hours). The script passes `--mem-mb 4096` when
`sqlright run` has that option. Then check that three processes run:

```bash
pgrep -fa "afl-fuzz" | grep -o "camp-[a-z]*"
```

`sqlright run` sets the AFL++ environment (custom mutator only, no trim,
no frameshift, no startup calibration) and runs the harness on
`SELECT 1` first when the checkout has that check. The instances stop on
their own after the run time. If one dies early, read
`work/camp-<oracle>/logs/main.log` and start it again with the time left.

## 3. Watch

Do not sleep in the foreground and do not poll in a tight loop. Use the
Monitor tool with a 30-minute timeout and re-arm it when it expires, or
a background `until` loop. Every 30 to 60 minutes run:

```bash
.claude/skills/fuzz-campaign/scripts/status.sh camp
```

It prints, per instance, the runs, the test cases checked, the pass,
fail and error counts, the reports, the crashes and the hangs, and the
free memory. Look for:

- `crashes` in `default/crashes/`: a panic in turso. The most valuable finding.
- `hangs` in `default/hangs/`: a statement that does not finish.
- `reports` in `bugs/bug_*.sql`: an oracle found two results that differ.
- Less than 2 GB of free memory: find the process and stop it.
- Fewer than three afl-fuzz processes: restart the missing one.

## 4. Triage

Run every replay under limits. A statement with a huge `NUMERIC` scale
or a huge `generate_series` can take all memory and restart the container:

```bash
H=$PWD/crates/sqlright-harness-tursopg/target/release/sqlright-harness-tursopg
(ulimit -v 4000000; timeout 60 $H --replay case.sql)
```

1. Group first. Hundreds of reports usually have one cause:
   `(ulimit -v 4000000; python3 -I scripts/group_failures.py "work/camp-norec/bugs/*.sql" 100)`
2. Replay one: `(ulimit -v 4000000; timeout 60 target/release/sqlright replay case.sql --oracle NOREC --harness $H --raw)`
3. Minimize: `(ulimit -v 4000000; timeout 600 target/release/sqlright minimize case.sql --oracle NOREC --harness $H --out work/min/case.sql)`.
   Then reduce by hand to a few statements.
4. Classify:
   - A turso logic bug: the same data gives different answers in two
     forms that must agree (an index lookup against an expression, a
     partition that drops a row, a DDL statement that changes whether a
     DML statement succeeds). Record it.
   - A PostgreSQL-compatibility gap: tursopg accepts a statement
     PostgreSQL rejects, and the difference follows from that. Record it
     briefly, with the PostgreSQL error.
   - A SQLRight false positive: the oracle assumed something that does
     not hold. See "When a campaign reports hundreds of failures" in
     `AGENTS.md`. Fix the oracle in `crates/sqlright-oracle`, add a unit
     test, run `cargo test --release` and `cargo clippy --release
     --workspace --all-targets`. A rebuilt `libsqlright_afl.so` takes
     effect only in a restarted instance.
   - A crash: reproduce under the limits, find the statement by bisecting
     prefixes of the file, record the panic message and location.
   - A hang: find the slow statement with `timeout 30` and record the
     measured time. Do not try to make it finish.
5. Report each new turso finding as a GitHub issue when you find it (see
   section 5). A short entry in `docs/turso-bugs.md` is welcome but
   secondary.
6. Commit documentation and oracle fixes with a lowercase imperative
   subject and a body that says why, and push.

## 5. Report a finding as a GitHub issue

The deliverable of a campaign is one issue per new turso problem in
tursodatabase/turso, opened as soon as the reproducer is minimal.

1. Search the open issues first. Use the gh CLI for search, with
   `is:issue` in the query and several key words per finding (the
   function or statement name, the type, "panic", "tursopg", "index"):
   `gh api -X GET search/issues -f q='repo:tursodatabase/turso is:issue is:open make_date' --jq '.items[] | "\(.number) \(.state) \(.title)"'`
   An open issue that covers the finding means: do not file it. A closed
   issue counts as covered only when its fix is newer than the turso
   commit under test.
2. Open the issue with the GitHub MCP tool `issue_write` (method
   `create`). Write in simple English, every sentence under 20 words:
   - Title: the problem in one sentence, for example
     "tursopg: make_date panics on a large day number".
   - Body: FIRST the minimal reproducer in a ```sql block. Then
     "## Observed" with the output or the panic message and location.
     Then "## Expected" with the PostgreSQL behavior. Then "## Version"
     with the turso commit of the submodule. Then the sentence
     "Found with SQLRight (https://github.com/LeMikaelF/sqlright), a
     logic-bug fuzzer, running against tursopg."
   - One issue per root cause. Group compatibility gaps of the same kind
     into one issue. A variant of a known finding (another column type,
     another function of the same family) goes as a comment on the
     existing issue, not as a new issue.
   - End the body with the attribution footer your environment requires.
   - Never put an AI model name in an issue or a comment.
3. Append one line per issue or comment to `work/issues-filed.log`:
   `<UTC time> <URL> <title>`. A watcher can relay that file.

## 6. Rules

- Do not run `cargo afl build` in `crates/sqlright-harness-tursopg` while
  fuzzers run. It replaces the harness binary under them. Build into
  another directory with `CARGO_TARGET_DIR=.../target-next` and use that
  build only for replays until the campaign ends.
- Do not change anything in the `turso` submodule. SQLRight does not fix turso bugs.
- Never force-push, never rewrite history.
- Instability warnings from AFL++ are normal: tursopg's coverage changes
  a little with hash map order.
