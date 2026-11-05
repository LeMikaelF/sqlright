import re, subprocess, sys, glob, collections
H = "crates/sqlright-harness-tursopg/target/release/sqlright-harness-tursopg"
def find_marker(text, marker, pos, whole_line):
    # (start, end) of the next marker, or None. In the harness output a marker
    # is a whole line: an error message can repeat a query that holds the text.
    if not whole_line:
        i = text.find(marker, pos)
        return None if i < 0 else (i, i + len(marker))
    m = re.compile(r"^[ \t]*" + re.escape(marker) + r"[ \t]*$", re.M).search(text, pos)
    return None if m is None else (m.start() + len(m.group(0)) - len(m.group(0).lstrip(" \t")), m.end())
def blocks(text, n, whole_line=False):
    # all blocks between BEGIN VERI n and END VERI n, in order
    out = []
    pos = 0
    while True:
        b = find_marker(text, f"BEGIN VERI {n}", pos, whole_line)
        if b is None: break
        e = find_marker(text, f"END VERI {n}", b[1], whole_line)
        if e is None: break
        out.append(text[b[1]:e[0]])
        pos = e[1]
    return out
shapes = collections.Counter()
examples = {}
files = sorted(glob.glob(sys.argv[1]))[:int(sys.argv[2])]
for f in files:
    sql = open(f).read()
    out = subprocess.run([H, "--replay", f], capture_output=True, text=True).stdout
    q0 = blocks(sql, 0); q1 = blocks(sql, 1); r0 = blocks(out, 0, True); r1 = blocks(out, 1, True)
    for k in range(min(len(q0), len(q1), len(r0), len(r1))):
        a = r0[k].strip(); b = r1[k].strip()
        if "ERROR" in a or "ERROR" in b or a == b: continue
        stmt = q0[k].strip().strip(";").strip()
        m = re.search(r"WHERE (.*)$", stmt)
        pred = m.group(1) if m else stmt
        shape = re.sub(r"'[^']*'", "'s'", pred)
        shape = re.sub(r"\b\d+(\.\d+)?\b", "N", shape)
        shapes[shape[:70]] += 1
        examples.setdefault(shape[:70], (f, stmt, a.replace("\n"," | "), b.replace("\n"," | ")))
for shape, n in shapes.most_common(40):
    f, stmt, a, b = examples[shape]
    print(f"{n:4d}  {stmt[:150]}\n      -> {a}  vs  {b}")
