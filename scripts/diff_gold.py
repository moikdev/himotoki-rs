#!/usr/bin/env python3
"""
Compare Rust dump output against the Python golden corpus.

Usage:
    python scripts/diff_gold.py [--rust DIR] [--gold DIR]
        [--only candidates|paths|output] [--show N] [--normalize-ids]

Semantic JSON comparison: dict key order ignored; int==float equal;
reports a per-input status plus the first differing field paths.

--normalize-ids ignores values that are internal to a particular database
build: generated conjugation seqs (>= 10,000,000) and conjugation row ids.
Use it when the Rust side ran against a freshly built database rather than
the one the fixtures were dumped from.
"""

import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
GOLD = ROOT / "tests" / "golden"
RUST = Path("/tmp/rust-gold")
GENERATED_SEQ_MIN = 10_000_000


def normalize_ids(x):
    """Mask database-build-specific ids (see --normalize-ids)."""
    if isinstance(x, dict):
        out = {}
        for k, v in x.items():
            if k == "seq" and isinstance(v, int) and v >= GENERATED_SEQ_MIN:
                out[k] = "<generated>"
            elif k == "conjugations" and isinstance(v, list):
                # Row ids differ per build; keep the list's length and shape.
                out[k] = ["<conj-id>" if isinstance(c, int) else normalize_ids(c) for c in v]
            else:
                out[k] = normalize_ids(v)
        return out
    if isinstance(x, list):
        return [normalize_ids(v) for v in x]
    return x


def load(path):
    recs = {}
    if not path.exists():
        return recs
    for line in path.read_text().splitlines():
        if line.strip():
            r = json.loads(line)
            recs[r["i"]] = r
    return recs


def diffs(a, b, path="", out=None, limit=20):
    """Yield dotted paths where a != b (structural compare)."""
    if out is None:
        out = []
    if len(out) >= limit:
        return out
    if type(a) != type(b) and not (
        isinstance(a, (int, float)) and isinstance(b, (int, float))
    ):
        out.append(f"{path}: type {type(a).__name__}!={type(b).__name__}")
        return out
    if isinstance(a, dict):
        for k in sorted(set(a) | set(b)):
            if k not in a:
                out.append(f"{path}.{k}: missing in A")
            elif k not in b:
                out.append(f"{path}.{k}: missing in B")
            else:
                diffs(a[k], b[k], f"{path}.{k}", out, limit)
    elif isinstance(a, list):
        if len(a) != len(b):
            out.append(f"{path}: len {len(a)}!={len(b)}")
        for i, (x, y) in enumerate(zip(a, b)):
            diffs(x, y, f"{path}[{i}]", out, limit)
    elif a != b:
        out.append(f"{path}: {a!r} != {b!r}")
    return out


def main():
    only = None
    show = 10
    args = sys.argv[1:]
    if "--only" in args:
        only = args[args.index("--only") + 1]
    if "--show" in args:
        show = int(args[args.index("--show") + 1])
    if "--rust" in args:
        global RUST
        RUST = Path(args[args.index("--rust") + 1])
    if "--gold" in args:
        global GOLD
        GOLD = Path(args[args.index("--gold") + 1])
    norm = normalize_ids if "--normalize-ids" in args else (lambda x: x)

    kinds = [only] if only else ["candidates", "paths", "output"]
    failed = False
    for kind in kinds:
        gold = {i: norm(r) for i, r in load(GOLD / f"{kind}.jsonl").items()}
        rust = {i: norm(r) for i, r in load(RUST / f"{kind}.jsonl").items()}
        same, diff, missing = 0, [], []
        for i, g in gold.items():
            r = rust.get(i)
            if r is None:
                missing.append(i)
                continue
            d = diffs(g, r)
            if d:
                diff.append((i, g["text"], d))
            else:
                same += 1
        extra = sorted(set(rust) - set(gold))
        print(f"== {kind}: {same} identical, {len(diff)} differ, "
              f"{len(missing)} missing in rust, {len(extra)} extra")
        for i, text, d in diff[:show]:
            print(f"  #{i} {text[:40]}")
            for line in d[:6]:
                print(f"      {line}")
        if diff or missing:
            failed = True
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
