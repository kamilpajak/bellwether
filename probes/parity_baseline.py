"""Independent pandas reading of the news lake, to be compared against the Rust reader.

This is the input-parity baseline of the successor memo §5.1 (input parity, not output
parity, which §5.1 drops). The point is NOT that pandas is authoritative - it is that two
implementations written from the same stated rules must agree, and where they do not,
the rules were ambiguous rather than one side being wrong.

The rules, mirroring services/newsvault/src/lake.rs exactly:
  1. read every run=*.parquet under session_date=<d>/, in filename order
  2. drop a row whose id is null, empty or blank, or whose publication or ingest
     time is null, and COUNT it
  3. drop a row whose ingest time precedes its publication time, and COUNT it
  4. collapse repeats across runs keeping the FIRST sighting (earliest ingest time)
  5. count a disagreement on publication time whenever a row re-reports an id,
     whether that row is discarded as a repeat or replaces the kept sighting -
     and count it twice over, as ROW events and as the ITEMS they fall on
  6. order the survivors by (publication time, id)

Digest: sha256 over the newline-joined sorted distinct ids, so the two sides can be
compared by one number per day instead of by eye.
"""
from __future__ import annotations
import hashlib, json, os, pathlib, sys

import pandas as pd

COLS = ["id", "source", "timestamp", "title", "body", "url", "ingested_at"]


def read_day(part: pathlib.Path) -> dict:
    runs = sorted(p for p in part.glob("run=*.parquet"))
    first: dict[str, tuple] = {}
    disputed: set[str] = set()
    raw = unusable = events = 0
    for path in runs:
        df = pd.read_parquet(path, columns=COLS)
        raw += len(df)
        ts = pd.to_datetime(df["timestamp"], utc=True)
        ing = pd.to_datetime(df["ingested_at"], utc=True)
        for i, rid in enumerate(df["id"]):
            v, o = ts.iloc[i], ing.iloc[i]
            if not isinstance(rid, str) or not rid.strip() or pd.isna(v) or pd.isna(o):
                unusable += 1
                continue
            if o < v:                      # observed before published: source bug
                unusable += 1
                continue
            prev = first.get(rid)
            if prev is not None:
                if prev[1] != v:           # before the first-sighting test, so both paths count
                    events += 1
                    disputed.add(rid)
                if prev[0] <= o:
                    continue
            first[rid] = (o, v)
    ids = sorted(first)
    digest = hashlib.sha256("\n".join(ids).encode()).hexdigest()
    obs = [v[0] for v in first.values()]
    return {
        "date": part.name.split("=")[1],
        "runs": len(runs),
        "raw_rows": raw,
        "distinct_items": len(first),
        "unusable_rows": unusable,
        "valid_time_conflict_events": events,
        "items_with_conflicting_valid_time": len(disputed),
        "first_sighting": str(min(obs)) if obs else None,
        "last_sighting": str(max(obs)) if obs else None,
        "id_digest": digest,
    }


# ---------------------------------------------------------------------- inputs
# Paths come from the environment, with a named abort when one is absent. This is
# requirement R6 ("a missing input aborts, never becomes an empty result") applied
# to the measurement scripts themselves, and it is why these files are committed:
# a number quoted in a design document needs the code that produced it to still run.
def _path(var: str, what: str) -> pathlib.Path:
    v = os.environ.get(var)
    if not v:
        raise SystemExit(f"brak {var} w srodowisku ({what})")
    p = pathlib.Path(v).expanduser()
    if not p.exists():
        raise SystemExit(f"{var} wskazuje na nieistniejaca sciezke: {p}")
    return p


def _out(var: str, default: str) -> pathlib.Path:
    return pathlib.Path(os.environ.get(var) or default).expanduser()


def main() -> int:
    root = pathlib.Path(sys.argv[1]) if len(sys.argv) > 1 else \
        _path("NEWS_LAKE", "hive-partitioned news lake, session_date=*/run=*.parquet")
    if not root.is_dir():
        raise SystemExit(f"brak jeziora: {root}")
    parts = sorted(root.glob("session_date=*"))
    print(f"partycji: {len(parts)}", flush=True)
    out = []
    for i, p in enumerate(parts, 1):
        out.append(read_day(p))
        if i % 20 == 0:
            print(f"  [{i}/{len(parts)}]", flush=True)
    dest = root.parent / "news_baseline_pandas.json"
    dest.write_text(json.dumps(out, indent=1))
    tot_raw = sum(r["raw_rows"] for r in out)
    tot_d = sum(r["distinct_items"] for r in out)
    tot_u = sum(r["unusable_rows"] for r in out)
    tot_c = sum(r["valid_time_conflict_events"] for r in out)
    tot_ci = sum(r["items_with_conflicting_valid_time"] for r in out)
    print(f"\nsurowych wierszy      : {tot_raw:,}")
    print(f"unikatow (po 1. spojrzeniu): {tot_d:,}")
    print(f"nieuzytecznych wierszy: {tot_u:,}")
    print(f"kolizji czasu zdarzenia (wiersze): {tot_c:,}")
    print(f"kolizji czasu zdarzenia (pozycje w partycji): {tot_ci:,}")
    print(f"dni z zerem unikatow  : {sum(1 for r in out if r['distinct_items'] == 0)}")
    print(f"\nzapisano {dest}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
