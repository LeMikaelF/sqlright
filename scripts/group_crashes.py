"""Group the crash and hang files of a campaign by their panic message.

usage: python3 -I scripts/group_crashes.py "work/camp-norec/default/crashes/id:*" [limit] [seconds]

Runs the harness on every file (at most `limit` files, 100 by default)
with a time limit per file (60 seconds by default). Prints one line per
distinct panic location with the count and one example file. A file that
does not panic again prints as "no panic". Run it under
`ulimit -v 4000000`: a statement can take all memory.
"""
import collections, glob, re, subprocess, sys

H = "crates/sqlright-harness-tursopg/target/release/sqlright-harness-tursopg"


def panic_key(stderr, timed_out):
    if timed_out:
        return "timeout"
    if "has overflowed its stack" in stderr:
        return "stack overflow"
    m = re.search(r"panicked at ([^\n]+):\n([^\n]*)", stderr)
    if m:
        # The index numbers in a message change with the program.
        message = re.sub(r"\d+", "N", m.group(2).strip())
        return f"{m.group(1)}: {message}"
    if "SIGILL" in stderr or "Illegal instruction" in stderr:
        return "SIGILL (harness built for another CPU?)"
    return "no panic"


files = sorted(glob.glob(sys.argv[1]))
limit = int(sys.argv[2]) if len(sys.argv) > 2 else 100
seconds = int(sys.argv[3]) if len(sys.argv) > 3 else 60
groups = collections.Counter()
example = {}
for f in files[:limit]:
    try:
        r = subprocess.run([H, "--replay", f], capture_output=True, text=True, timeout=seconds)
        key = panic_key(r.stderr, False)
    except subprocess.TimeoutExpired:
        key = panic_key("", True)
    groups[key] += 1
    example.setdefault(key, f)
for key, n in groups.most_common():
    print(f"{n:4d}  {key}\n      {example[key]}")
