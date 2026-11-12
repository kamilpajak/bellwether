"""Inter-arrival gap distribution of PUBLICATION times in a news archive.

Answers one question: how stale may the newest item be before that is an incident?
Section 10 of the design, memo question N2. The thresholds it produced on 2026-11-12 —
2 hours inside the US session, 6 hours outside it — are quantiles of this output, not a
guess, and they are provisional on the vendor (memo decision D-1 is open).

PUBLICATION and not OBSERVATION, deliberately. The predecessor ran on a clock, so the
spacing of its `ingested_at` values measures its scheduler. The spacing of publication
instants measures the world and the vendor's coverage of it, which is what transfers to
an event-driven system.

What it cannot answer: whether a gap is quiet news or a dead collector. Both look the
same here, which is why the alert fires on either.

Two bucketings, and they give different answers. The quantiles below put a gap in the
window its FIRST instant falls in. The alert asks something else - is the age of the
newest item over the threshold at some instant inside the window - so a gap that starts
outside the session and runs into it is an in-session alert. `firings` answers that one,
and it is the number a threshold choice has to be made against: measured 2026-11-12,
gap-start bucketing says 2 h in session would have fired 0 times and the alert's own
question says 4.
"""

from __future__ import annotations

import glob
import os
import pathlib

import numpy as np
import pyarrow.parquet as pq

SESSION_START_UTC = 13  # 13:00-20:00 UTC covers the US session across DST
SESSION_END_UTC = 20
TAIL_HOURS = (1, 2, 3, 4, 6, 12, 24)
QUANTILES = (50, 90, 99, 99.9)


def _path(var: str, what: str) -> pathlib.Path:
    v = os.environ.get(var)
    if not v:
        raise SystemExit(f"brak {var} w srodowisku ({what})")
    p = pathlib.Path(v).expanduser()
    if not p.is_dir():
        raise SystemExit(f"{var}={p} nie jest katalogiem")
    return p


def publication_instants(root: pathlib.Path) -> np.ndarray:
    """Every distinct item's publication time, sorted, first sighting wins."""
    files = sorted(glob.glob(str(root / "session_date=*" / "*.parquet")))
    if not files:
        raise SystemExit(f"{root} nie zawiera partycji session_date=*/*.parquet")
    seen: set[str] = set()
    out: list[float] = []
    for f in files:
        t = pq.read_table(f, columns=["id", "timestamp"])
        for i, s in zip(t.column("id").to_pylist(), t.column("timestamp").to_pylist()):
            if i not in seen and s is not None:
                seen.add(i)
                out.append(s.timestamp())
    print(f"run files {len(files)}  distinct items with a publication time {len(out)}")
    return np.sort(np.array(out))


def firings(instants: np.ndarray, threshold_h: float, inside: bool) -> list[tuple[float, float]]:
    """Gaps during which the newest item's age exceeds the threshold inside (outside) the session.

    A gap [x, y] puts that age above T over the open interval (x + T, y). The rule fires when
    that interval holds an instant in the window it watches, on any UTC day it spans.
    """
    t = threshold_h * 3600.0
    hit = []
    for x, y in zip(instants[:-1], instants[1:]):
        start = x + t
        if start >= y:
            continue
        first, last = int(start // 86400), int(y // 86400)
        covered = 0.0
        for day in range(first, last + 1):
            lo = max(day * 86400 + SESSION_START_UTC * 3600, start)
            hi = min(day * 86400 + SESSION_END_UTC * 3600, y)
            covered += max(0.0, hi - lo)
        if (covered > 0.0) if inside else (y - start - covered > 0.0):
            hit.append((x, y))
    return hit


def main() -> None:
    a = publication_instants(_path("NEWS_LAKE", "append-only lake root"))
    gaps = np.diff(a) / 60.0
    span_days = (a[-1] - a[0]) / 86400.0
    print(f"span {np.datetime64(int(a[0]), 's')} .. {np.datetime64(int(a[-1]), 's')}"
          f"  ({span_days:.0f} days, {len({int(x // 86400) for x in a})} of them holding an item)")

    for h in TAIL_HOURS:
        n = int((gaps > h * 60).sum())
        print(f"  gaps over {h:2d} h: {n:5d}   {n / span_days:.2f} per day")

    hour = ((a[:-1] / 3600) % 24).astype(int)
    in_session = (hour >= SESSION_START_UTC) & (hour < SESSION_END_UTC)
    for name, mask in ((f"{SESSION_START_UTC}-{SESSION_END_UTC} UTC", in_session), ("outside", ~in_session)):
        g = gaps[mask]
        qs = "  ".join(f"p{q} {np.percentile(g, q):.1f}" for q in QUANTILES)
        print(f"{name:<12} n={len(g):6d}  {qs}  min   worst {g.max() / 60:.1f} h")

    print("firings, by the question the alert asks rather than by the hour a gap starts:")
    for hours, inside in ((2.0, True), (6.0, False)):
        hit = firings(a, hours, inside)
        where = "in session" if inside else "outside"
        print(f"  {hours:.0f} h {where:<10} fires {len(hit)} times")
        for x, y in hit:
            print(f"      {np.datetime64(int(x), 's')} -> {np.datetime64(int(y), 's')}"
                  f"  gap {(y - x) / 3600:.1f} h")


if __name__ == "__main__":
    main()
