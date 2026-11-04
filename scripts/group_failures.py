import re, subprocess, sys, glob, collections
H = "crates/sqlright-harness-tursopg/target/release/sqlright-harness-tursopg"
def blocks(text, n):
    # all blocks between BEGIN VERI n and END VERI n, in order
    out = []
    b = f"BEGIN VERI {n}"; e = f"END VERI {n}"
    pos = 0
    while True:
        i = text.find(b, pos)
        if i < 0: break
        j = text.find(e, i)
        if j < 0: break
        out.append(text[i+len(b):j])
        pos = j + len(e)
    return out
shapes = collections.Counter()
examples = {}
files = sorted(glob.glob(sys.argv[1]))[:int(sys.argv[2])]
for f in files:
    sql = open(f).read()
    out = subprocess.run([H, "--replay", f], capture_output=True, text=True).stdout
    q0 = blocks(sql, 0); q1 = blocks(sql, 1); r0 = blocks(out, 0); r1 = blocks(out, 1)
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
