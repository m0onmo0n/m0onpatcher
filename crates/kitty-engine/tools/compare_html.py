"""Compare two patched HTML files (e.g. Python raw output vs kitty-patch output).

Splits both into passages (by name) and "everything else", normalises CRLF, and reports what differs.
Exit code 0 when identical, 1 otherwise.

usage: python compare_html.py <a.html> <b.html> [--show N]
"""
import difflib
import re
import sys

show = 3
args = [a for a in sys.argv[1:] if not a.startswith("--")]
if "--show" in sys.argv:
    show = int(sys.argv[sys.argv.index("--show") + 1])
    args = [a for a in args if a != str(show)]


def load(p):
    t = open(p, encoding="utf-8", newline="").read()
    return t.replace("\r\n", "\n").replace("\r", "\n")


a, b = load(args[0]), load(args[1])
if a == b:
    print("IDENTICAL (after CRLF normalisation), %d chars" % len(a))
    sys.exit(0)

PAS = re.compile(r'<tw-passagedata\b([^>]*)>(.*?)</tw-passagedata>', re.S)


def parts(t):
    d = {}
    order = []
    for m in PAS.finditer(t):
        nm = re.search(r'name="([^"]*)"', m.group(1))
        key = nm.group(1) if nm else "(no name)"
        if key not in d:
            order.append(key)
        d.setdefault(key, []).append((m.group(1), m.group(2)))
    return d, order, PAS.sub("<P/>", t)


pa, oa, ra = parts(a)
pb, ob, rb = parts(b)
print("chars: a=%d b=%d | passages: a=%d b=%d" % (len(a), len(b), len(oa), len(ob)))
only_a = [k for k in oa if k not in pb]
only_b = [k for k in ob if k not in pa]
if only_a:
    print("only in a:", only_a[:20])
if only_b:
    print("only in b:", only_b[:20])
diff = [k for k in oa if k in pb and pa[k] != pb[k]]
print("passages with different content:", len(diff))
for k in diff[:show]:
    x, y = pa[k][0], pb[k][0]
    print("  --", k)
    if x[0] != y[0]:
        print("     tag a:", x[0][:200])
        print("     tag b:", y[0][:200])
    sm = difflib.SequenceMatcher(None, x[1], y[1], autojunk=False)
    n = 0
    for op, i1, i2, j1, j2 in sm.get_opcodes():
        if op != "equal":
            print("     %s a[%d:%d]=%r b[%d:%d]=%r" % (op, i1, i2, x[1][i1:i2][:100], j1, j2, y[1][j1:j2][:100]))
            n += 1
            if n >= 3:
                break
print("order of passages equal:", [k for k in oa if k in pb] == [k for k in ob if k in pa])
if ra != rb:
    print("text outside passages differs: len a=%d b=%d" % (len(ra), len(rb)))
    i = 0
    while i < min(len(ra), len(rb)) and ra[i] == rb[i]:
        i += 1
    print("  first difference at %d" % i)
    print("   a:", repr(ra[max(0, i - 60): i + 160]))
    print("   b:", repr(rb[max(0, i - 60): i + 160]))
else:
    print("text outside passages identical")
sys.exit(1)
