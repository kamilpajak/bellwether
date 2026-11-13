"""Second reading of the archive, compared against the Rust one day by day.

This is the comparator `probes/parity_baseline.py` was missing: that script writes a
baseline nothing reads, so the 156-of-156 agreement in architecture.md section 10 was
compared by hand and is a number from one afternoon. Here the comparison is the
product: this reader's numbers against `newsvault day --format json`, field by field,
with a non-zero exit when any field of any day differs.

What it is run over, and why that is not the lake. The 156-day corpus holds NONE of the
shapes two readers of these rules can disagree on - across 779 run files and 243,226
rows, zero null ids, zero empty-string ids, zero null publication or ingest times, zero
inverted rows (re-derived 2026-11-12). So agreement over it says the lake is clean, not
that two readers encode one rule. The corpus under `newsvault_parity/lake/` is
committed and holds one partition per missing shape; `--write-corpus` rebuilds it and
is how a shape is added.

The rules, mirroring services/newsvault/src/lake.rs:
  1. read every run=*.parquet under session_date=<d>/, in filename order
  2. drop a row whose id is null, empty or blank, or whose publication or ingest time
     is null, and COUNT it
  3. drop a row whose ingest time precedes its publication value, whatever that value's
     precision, and COUNT it
  4. a publication value stamped exactly at UTC midnight is a DAY and not an instant
  5. collapse repeats across runs keeping the FIRST sighting (earliest ingest time)
  6. count a disagreement on the publication value whenever a row re-reports an id,
     whether that row is discarded as a repeat or replaces the kept sighting - and
     count it twice over, as ROW events and as the ITEMS they fall on
  7. a day whose run file lacks one of the seven columns is refused whole

Two digests, because one of them is weak on purpose: `id_digest` is the one the lake
baseline has always computed (sha256 over the newline-joined sorted distinct vendor
ids) and it would match with every title, body and url different; `content_digest`
covers every field of every item, in id order, each value length-prefixed.

Usage:
  newsvault_parity.py [--lake DIR] [--binary PATH]   compare, exit 1 on disagreement
  newsvault_parity.py --write-expected               rewrite the committed expectation
  newsvault_parity.py --write-corpus                 rebuild the committed run files

Exit codes: 0 agreed, 1 disagreed (the answer is no), 2 usage, 4 an input is absent.
"""
from __future__ import annotations

import datetime as dt
import hashlib
import json
import pathlib
import subprocess
import sys

import pandas as pd
import pyarrow as pa
import pyarrow.parquet as pq

HERE = pathlib.Path(__file__).resolve().parent
PACKAGE = HERE / "newsvault_parity"
LAKE = PACKAGE / "lake"
EXPECTED = PACKAGE / "expected.json"
DEFAULT_BINARY = HERE.parent / "target" / "debug" / "newsvault"
SCHEMA = "newsvault.parity/1"

COLS = ["id", "source", "timestamp", "tickers", "title", "body", "url", "ingested_at"]

EXIT_OK, EXIT_NO, EXIT_USAGE, EXIT_ABSENT = 0, 1, 2, 4


# ---------------------------------------------------------------- the second reader
def _text(when: dt.datetime) -> str:
    """An instant as both readers render it: whole microseconds, `Z`, never an offset."""
    return f"{when:%Y-%m-%dT%H:%M:%S}.{when.microsecond:06d}Z"


def _published(valid: dt.datetime) -> str:
    """Rule 4: midnight is a day. The calendar is the lake's, which has no precision column."""
    if (valid.hour, valid.minute, valid.second, valid.microsecond) == (0, 0, 0, 0):
        return f"day {valid:%Y-%m-%d} UTC"
    return _text(valid)


def _field(parts: list[bytes], value: str) -> None:
    raw = value.encode()
    parts.append(f"{len(raw)}:".encode())
    parts.append(raw)


def _content_digest(items: dict[str, dict]) -> str:
    parts: list[bytes] = []
    for rid in sorted(items):
        item = items[rid]
        for value in (
            rid,
            item["source"],
            _published(item["valid"]),
            _text(item["observed"]),
            item["title"],
            item["body"],
            item["url"],
        ):
            _field(parts, value)
        tickers = item["tickers"]
        parts.append(f"{len(tickers)}#".encode())
        for ticker in tickers:
            _field(parts, ticker)
    return hashlib.sha256(b"".join(parts)).hexdigest()


def _tickers(cell) -> list[str]:
    """One null cell is the one legitimate empty list: the vendor tagged nothing."""
    if cell is None or (isinstance(cell, float) and pd.isna(cell)):
        return []
    return [t for t in list(cell) if t is not None]


def read_day(part: pathlib.Path) -> dict:
    runs = sorted(p for p in part.glob("run=*.parquet"))
    date = part.name.split("=")[1]
    for path in runs:
        held = set(pq.read_schema(path).names)
        missing = [c for c in COLS if c not in held]
        if missing:
            return {"date": date, "refused": "missing_column", "column": missing[0]}

    first: dict[str, dict] = {}
    disputed: set[str] = set()
    raw = unusable = events = 0
    for path in runs:
        frame = pd.read_parquet(path, columns=COLS)
        raw += len(frame)
        valid = pd.to_datetime(frame["timestamp"], utc=True)
        ingest = pd.to_datetime(frame["ingested_at"], utc=True)
        for i, rid in enumerate(frame["id"]):
            value, seen = valid.iloc[i], ingest.iloc[i]
            if not isinstance(rid, str) or not rid.strip():
                unusable += 1
                continue
            if pd.isna(value) or pd.isna(seen):
                unusable += 1
                continue
            if seen < value:  # observed before published, at any precision
                unusable += 1
                continue
            standing = first.get(rid)
            if standing is not None:
                # Before the first-sighting test, so both paths count.
                if _published(standing["valid"]) != _published(value):
                    events += 1
                    disputed.add(rid)
                if standing["observed"] <= seen:
                    continue
            first[rid] = {
                "source": frame["source"].iloc[i],
                "valid": value,
                "observed": seen,
                "title": frame["title"].iloc[i],
                "body": frame["body"].iloc[i],
                "url": frame["url"].iloc[i],
                "tickers": _tickers(frame["tickers"].iloc[i]),
            }
    ids = sorted(first)
    return {
        "date": date,
        "runs": len(runs),
        "raw_rows": raw,
        "distinct_items": len(first),
        "unusable_rows": unusable,
        "valid_time_conflict_events": events,
        "items_with_conflicting_valid_time": len(disputed),
        "id_digest": hashlib.sha256("\n".join(ids).encode()).hexdigest(),
        "content_digest": _content_digest(first),
    }


def read_lake(root: pathlib.Path) -> dict | None:
    """None when the corpus is absent, which is an input failure and not an empty result."""
    parts = sorted(root.glob("session_date=*"))
    if not parts:
        print(f"error: no session_date=* partition under {root}", file=sys.stderr)
        return None
    return {"schema": SCHEMA, "reader": "probes/newsvault_parity.py",
            "days": [read_day(p) for p in parts]}


# ------------------------------------------------------------------- the comparison
def rust_day(binary: pathlib.Path, root: pathlib.Path, date: str) -> dict:
    """`newsvault day` over one partition. Exit 2 is how that reader refuses a day."""
    done = subprocess.run(
        [str(binary), "day", date, "--lake", str(root), "--format", "json"],
        capture_output=True, text=True, check=False)
    if done.returncode == EXIT_USAGE:
        column = done.stderr.strip().removeprefix("error: column ").split(" ")[0]
        return {"date": date, "refused": "missing_column", "column": column}
    if done.returncode != EXIT_OK:
        return {"date": date, "refused": f"exit_{done.returncode}", "column": done.stderr.strip()}
    out = json.loads(done.stdout)
    return {k: out[k] for k in read_day_keys(out)}


def read_day_keys(out: dict) -> list[str]:
    wanted = ["date", "runs", "raw_rows", "distinct_items", "unusable_rows",
              "valid_time_conflict_events", "items_with_conflicting_valid_time",
              "id_digest", "content_digest"]
    return [k for k in wanted if k in out]


def compare(mine: dict, theirs: dict) -> list[str]:
    differences = []
    for key in sorted(set(mine) | set(theirs)):
        left, right = mine.get(key), theirs.get(key)
        if key == "runs" and isinstance(right, list):
            right = len(right)
        if left != right:
            differences.append(f"{mine.get('date')} {key}: probe {left!r} rust {right!r}")
    return differences


# ------------------------------------------------------- the corpus the lake lacks
# One partition per shape, so a disagreement names the shape. Dates are plain sequential
# days of 2026-01 and no partition carries real vendor data.
MIDNIGHT = "2026-01-08T00:00:00Z"


def _row(rid, valid, observed, *, source="fixture", title="t", body="b",
         url="https://example.test", tickers=("AAA",)):
    return {"id": rid, "source": source, "timestamp": _instant(valid), "tickers": tickers,
            "title": title, "body": body, "url": url, "ingested_at": _instant(observed)}


def _instant(text: str | None):
    return None if text is None else dt.datetime.fromisoformat(text.replace("Z", "+00:00"))


SHAPES: dict[str, dict[str, list[dict]]] = {
    # A null id and a present-but-blank one are different rows: the blank one used to
    # become the dedup key and collapse every blank-id row of the day into one item.
    "2026-01-01": {"run=20260101T010000000Z.parquet": [
        _row(None, "2026-01-01T10:00:00Z", "2026-01-01T11:00:00Z"),
        _row("kept-1", "2026-01-01T10:00:00Z", "2026-01-01T11:00:00Z")]},
    "2026-01-02": {"run=20260102T010000000Z.parquet": [
        _row("", "2026-01-02T10:00:00Z", "2026-01-02T11:00:00Z"),
        _row("   ", "2026-01-02T10:00:00Z", "2026-01-02T11:00:00Z"),
        _row("kept-2", "2026-01-02T10:00:00Z", "2026-01-02T11:00:00Z")]},
    "2026-01-03": {"run=20260103T010000000Z.parquet": [
        _row("no-valid", None, "2026-01-03T11:00:00Z"),
        _row("kept-3", "2026-01-03T10:00:00Z", "2026-01-03T11:00:00Z")]},
    "2026-01-04": {"run=20260104T010000000Z.parquet": [
        _row("no-observed", "2026-01-04T10:00:00Z", None),
        _row("kept-4", "2026-01-04T10:00:00Z", "2026-01-04T11:00:00Z")]},
    # The second row is the shape that disagreed on 2026-11-13: the ordering test sat
    # inside `Visibility::from_live`, which a midnight value never reaches.
    "2026-01-05": {"run=20260105T010000000Z.parquet": [
        _row("inverted-instant", "2026-01-05T12:00:00Z", "2026-01-05T11:00:00Z"),
        _row("inverted-day", "2026-01-05T00:00:00Z", "2026-01-04T23:00:00Z"),
        _row("kept-5", "2026-01-05T10:00:00Z", "2026-01-05T11:00:00Z")]},
    "2026-01-06": {
        "run=20260106T010000000Z.parquet": [
            _row("restamped", "2026-01-06T10:00:00Z", "2026-01-06T11:00:00Z"),
            _row("steady", "2026-01-06T09:00:00Z", "2026-01-06T11:00:00Z")],
        # A later run re-reporting one id with a new publication time: the kept sighting
        # stays and the disagreement is counted.
        "run=20260106T050000000Z.parquet": [
            _row("restamped", "2026-01-06T12:30:00Z", "2026-01-06T15:00:00Z"),
            _row("late", "2026-01-06T14:00:00Z", "2026-01-06T15:00:00Z")]},
    # The replacement path: a known id arriving with an EARLIER observation. Taken 0
    # times in the archive's 243,226 rows.
    "2026-01-07": {
        "run=20260107T010000000Z.parquet": [
            _row("backfilled", "2026-01-07T10:00:00Z", "2026-01-07T15:00:00Z"),
            _row("backfilled-and-restamped", "2026-01-07T09:00:00Z", "2026-01-07T15:00:00Z")],
        "run=20260107T050000000Z.parquet": [
            _row("backfilled", "2026-01-07T10:00:00Z", "2026-01-07T12:00:00Z"),
            _row("backfilled-and-restamped", "2026-01-07T08:00:00Z", "2026-01-07T12:00:00Z")]},
    # A day-dated value beside an instant one, and the two tag shapes: several tags and
    # the one legitimate empty, a null cell.
    "2026-01-08": {"run=20260108T010000000Z.parquet": [
        _row("day-dated", MIDNIGHT, "2026-01-09T06:00:00Z", tickers=("AAA", "BBB")),
        _row("instant-dated", "2026-01-08T10:00:00Z", "2026-01-08T11:00:00Z", tickers=None),
        _row("unicode", "2026-01-08T10:00:00Z", "2026-01-08T11:00:00Z",
             title="nazwa: \\u0141\\u00f3d\\u017a", body="\\u00e9\\u00e8 \\u4f8b", url="https://example.test/\\u0142")]},
}
# The one shape that is a FILE and not a row: a run file missing a column both readers
# need. It is refused whole rather than read with a hole (section 8 row R6).
MISSING_COLUMN_DAY = "2026-01-09"
MISSING_COLUMN = "tickers"


def _schema() -> pa.Schema:
    stamp = pa.timestamp("us", tz="UTC")
    return pa.schema([("id", pa.string()), ("source", pa.string()), ("timestamp", stamp),
                      ("tickers", pa.list_(pa.string())), ("title", pa.string()),
                      ("body", pa.string()), ("url", pa.string()), ("ingested_at", stamp)])


def _write_run(path: pathlib.Path, rows: list[dict], drop: str | None = None) -> None:
    schema = _schema()
    if drop is not None:
        schema = pa.schema([f for f in schema if f.name != drop])
    columns = {f.name: [r[f.name] for r in rows] for f in schema}
    path.parent.mkdir(parents=True, exist_ok=True)
    pq.write_table(pa.table(columns, schema=schema), path)


def write_corpus(root: pathlib.Path) -> list[str]:
    written = []
    for date, runs in SHAPES.items():
        for name, rows in runs.items():
            _write_run(root / f"session_date={date}" / name, rows)
            written.append(f"session_date={date}/{name}")
    name = f"run=2026{MISSING_COLUMN_DAY[5:7]}{MISSING_COLUMN_DAY[8:]}T010000000Z.parquet"
    _write_run(root / f"session_date={MISSING_COLUMN_DAY}" / name,
               [_row("no-tickers-column", f"{MISSING_COLUMN_DAY}T10:00:00Z",
                     f"{MISSING_COLUMN_DAY}T11:00:00Z")], drop=MISSING_COLUMN)
    written.append(f"session_date={MISSING_COLUMN_DAY}/{name}")
    return written


def main(argv: list[str]) -> int:
    known = {"--write-expected", "--write-corpus", "--lake", "--binary"}
    flags = [a for a in argv if a.startswith("-")]
    if not set(flags) <= known:
        print(__doc__, file=sys.stderr)
        return EXIT_USAGE
    if "--write-corpus" in argv:
        for part in write_corpus(LAKE):
            print(part)
        return EXIT_OK

    root = pathlib.Path(_option(argv, "--lake", str(LAKE))).expanduser()
    if not root.is_dir():
        print(f"error: no lake at {root}", file=sys.stderr)
        return EXIT_ABSENT
    report = read_lake(root)
    if report is None:
        return EXIT_ABSENT
    if "--write-expected" in argv:
        EXPECTED.write_text(json.dumps(report, indent=1) + "\n")
        print(f"wrote {EXPECTED}")
        return EXIT_OK

    binary = pathlib.Path(_option(argv, "--binary", str(DEFAULT_BINARY))).expanduser()
    if not binary.is_file():
        print(f"error: no newsvault binary at {binary} (cargo build)", file=sys.stderr)
        return EXIT_ABSENT
    differences: list[str] = []
    for day in report["days"]:
        differences.extend(compare(day, rust_day(binary, root, day["date"])))
    json.dump({"schema": SCHEMA, "days": len(report["days"]),
               "agreed": not differences, "differences": differences},
              sys.stdout, indent=1)
    sys.stdout.write("\n")
    return EXIT_NO if differences else EXIT_OK


def _option(argv: list[str], name: str, default: str) -> str:
    return argv[argv.index(name) + 1] if name in argv else default


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
