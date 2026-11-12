"""Audit the counterparty matcher on OUR OWN corpus, not on a foreign one.

The CC-NEWS run measured 50.0% entity-correct hits (1 SE 6.5 pp) on general, largely
non-English news pages. That number governs every count built on this matcher - the
42.8% of bracket companies with a disclosed relation, the 4,715 treated rows - but it
was measured somewhere we do not operate. This measures it where we do.

Three things it establishes, none of which has been looked at before:

  1. COMPOSITION. Our lake is not one corpus. EDGAR press releases carry real article
     text (median 14,039 chars) while RSS and Polygon carry 120-430 char snippets, so
     recall and precision cannot be the same across sources and must be reported apart.
  2. WHAT THE 4,000-CHAR CAP COST. measure_v2.py and count_cases.py both truncated
     title+body to 4,000 characters. Against a median EDGAR body of 14,039 that discards
     most of the only source with full text. Counted here as occurrences beyond the cap.
  3. ENTITY PRECISION. Occurrences are dumped with context for hand labelling from a
     seeded uniform sample, so the 50% can be confirmed or refuted on our own data.

Matching mirrors production: multi-token cores case-insensitive, single-token cores
case-sensitive, word boundary either side. HTML tags are stripped first because 35.9% of
our items carry them, all of them from the EDGAR path.

Reads no outcome.
"""
from __future__ import annotations
import collections, os, json, pathlib, random, re, sys

import ahocorasick
import pandas as pd

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


LAKE = _path("NEWS_LAKE", "hive-partitioned news lake")
TARGETS = _path("TARGETS_JSON", '{"cores": [...], "tickers": [...]}')
OUT = _out("OUT_DIR", "./matcher_audit_out")
CAP = 4000
SEED = 20261111
SAMPLE = 60
CTX = 110

TAG = re.compile(r"<[^>]{0,400}>")
WS = re.compile(r"\s+")
WORD = re.compile(r"[0-9A-Za-z]")

# Hand-written gazetteer. The only filter the CC-NEWS run found safe: it removed 11.9%
# of occurrences and none of the named real companies. Written out so the number is
# reproducible and so a reader can see exactly what was treated as not-a-company.
GEO = {
    "Los Angeles", "South Korea", "Saudi Arabia", "Australia", "Qatar", "New York",
    "United States", "United Kingdom", "Hong Kong", "New Zealand", "South Africa",
    "Costa Rica", "Puerto Rico", "San Francisco", "San Diego", "San Jose", "Las Vegas",
    "Abu Dhabi", "Czech Republic", "Dominican Republic", "Sri Lanka", "El Salvador",
    "North Carolina", "South Carolina", "North Dakota", "South Dakota", "West Virginia",
    "New Jersey", "New Mexico", "New Hampshire", "Rhode Island",
}


def build(pats):
    a = ahocorasick.Automaton()
    for p in pats:
        a.add_word(p, p)
    a.make_automaton()
    return a


def scan(auto, hay, original, back=None):
    """Yield (core, start) for matches with non-word characters either side."""
    for end, pat in auto.iter(hay):
        start = end - len(pat) + 1
        before = original[start - 1] if start > 0 else " "
        after = original[end + 1] if end + 1 < len(original) else " "
        if WORD.match(before) or WORD.match(after):
            continue
        yield (back[pat] if back else pat), start


def main() -> int:
    OUT.mkdir(exist_ok=True)
    t = json.loads(TARGETS.read_text())
    cores = [c for c in t["cores"] if len(c) >= 5]
    multi = [c for c in cores if len(c.split()) >= 2]
    single = [c for c in cores if len(c.split()) == 1]
    auto_ci, auto_cs = build([c.lower() for c in multi]), build(single)
    back = {c.lower(): c for c in multi}
    print(f"rdzeni {len(cores)}: {len(multi)} wielotokenowych, {len(single)} jednotokenowych",
          flush=True)

    parts = sorted(LAKE.glob("session_date=*"))
    occ = []                       # every occurrence, for sampling
    hits_by_src = collections.Counter()
    items_by_src = collections.Counter()
    hit_items_by_src = collections.Counter()
    core_hits = collections.Counter()
    beyond_cap = 0
    total_occ = 0
    body_lens = collections.defaultdict(list)

    for n, part in enumerate(parts, 1):
        day = part.name.split("=")[1]
        first = {}
        for f in sorted(part.glob("run=*.parquet")):
            df = pd.read_parquet(f, columns=["id", "source", "title", "body", "url",
                                             "timestamp", "ingested_at"])
            ing = pd.to_datetime(df["ingested_at"], utc=True)
            for i, rid in enumerate(df["id"]):
                if not isinstance(rid, str) or not rid:
                    continue
                if rid not in first or ing.iloc[i] < first[rid][0]:
                    first[rid] = (ing.iloc[i], df.iloc[i])
        for rid, (_, row) in first.items():
            src = str(row["source"])
            items_by_src[src] += 1
            text = WS.sub(" ", TAG.sub(" ", f"{row['title'] or ''} {row['body'] or ''}")).strip()
            body_lens[src].append(len(str(row["body"] or "")))
            low = text.lower()
            found = list(scan(auto_ci, low, low, back)) + list(scan(auto_cs, text, text))
            if found:
                hit_items_by_src[src] += 1
            for core, pos in found:
                total_occ += 1
                hits_by_src[src] += 1
                core_hits[core] += 1
                if pos >= CAP:
                    beyond_cap += 1
                occ.append({"day": day, "id": rid, "source": src, "core": core, "pos": pos,
                            "url": str(row["url"] or ""), "text_len": len(text),
                            "context": text[max(0, pos - CTX):pos + len(core) + CTX]})
        if n % 30 == 0:
            print(f"  [{n}/{len(parts)}] wystapien {total_occ:,}", flush=True)

    items = sum(items_by_src.values())
    print(f"\npozycji unikatowych        : {items:,}")
    print(f"wystapien rdzeni ogolem    : {total_occ:,}")
    print(f"z nich ZA progiem {CAP} zn.: {beyond_cap:,} = {100*beyond_cap/max(total_occ,1):.1f}%"
          f"   <- tyle gubilo moje liczenie")
    geo_occ = sum(1 for o in occ if o["core"] in GEO)
    print(f"wystapien nazw GEOGRAFICZNYCH: {geo_occ:,} = {100*geo_occ/max(total_occ,1):.1f}%")
    print(f"roznych rdzeni trafionych  : {len(core_hits)} z {len(cores)}")

    print(f"\n{'zrodlo':22} {'pozycji':>8} {'z trafieniem':>13} {'%':>7} {'wystapien':>10} "
          f"{'mediana body':>13}")
    for s in sorted(items_by_src, key=lambda x: -items_by_src[x]):
        med = int(pd.Series(body_lens[s]).median())
        print(f"{s:22} {items_by_src[s]:8,} {hit_items_by_src[s]:13,} "
              f"{100*hit_items_by_src[s]/items_by_src[s]:6.1f}% {hits_by_src[s]:10,} {med:13,}")

    print(f"\nnajczestsze rdzenie:")
    for c, k in core_hits.most_common(20):
        tag = " [GEO]" if c in GEO else ""
        print(f"   {c[:34]:34} {k:6}{tag}")

    rng = random.Random(SEED)
    pool = [o for o in occ if o["core"] not in GEO]
    sample = rng.sample(pool, min(SAMPLE, len(pool)))
    (OUT / "occurrences_sample.json").write_text(json.dumps(sample, indent=1))
    (OUT / "summary.json").write_text(json.dumps({
        "items": items, "occurrences": total_occ, "beyond_cap": beyond_cap,
        "geo_occurrences": geo_occ, "distinct_cores_hit": len(core_hits),
        "cores_total": len(cores), "seed": SEED, "sample_n": len(sample),
        "by_source": {s: {"items": items_by_src[s], "hit_items": hit_items_by_src[s],
                          "occurrences": hits_by_src[s],
                          "body_median": int(pd.Series(body_lens[s]).median())}
                      for s in items_by_src},
        "top_cores": core_hits.most_common(60),
    }, indent=1))
    print(f"\nprobka {len(sample)} wystapien (ziarno {SEED}, bez nazw geograficznych)"
          f" -> {OUT/'occurrences_sample.json'}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
