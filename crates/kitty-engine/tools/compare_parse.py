"""Compare two parse dumps (Python reference vs kitty-dump output). Exit code 1 on any difference.

usage: python compare_parse.py <python.json> <rust.json>
"""
import json
import sys

a_all = json.load(open(sys.argv[1], encoding="utf-8"))
b_all = json.load(open(sys.argv[2], encoding="utf-8"))
bad = 0
for key in a_all:
    a, b = a_all[key], b_all.get(key)
    order_ok = not (isinstance(a, dict) and isinstance(b, dict)) or list(a) == list(b)
    ok = a == b and order_ok
    print("%-20s %s" % (key, "OK" if ok else "DIFF"))
    if ok:
        continue
    bad += 1
    if isinstance(a, list) and isinstance(b, list):
        print("   lengths python/rust:", len(a), len(b))
        for i, (x, y) in enumerate(zip(a, b)):
            if x != y:
                print("   first difference at index", i)
                print("     python:", json.dumps(x, ensure_ascii=False)[:400])
                print("     rust  :", json.dumps(y, ensure_ascii=False)[:400])
                break
    elif isinstance(a, dict) and isinstance(b, dict):
        print("   only python:", len(set(a) - set(b)), "only rust:", len(set(b) - set(a)),
              "key order equal:", list(a) == list(b))
sys.exit(1 if bad else 0)
