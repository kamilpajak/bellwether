"""Recount the map and the panel after two defects found on 2026-11-11, each attributed.

DEFECT 1 — the 4,000-character cap. measure_v2.py and count_cases.py truncated
title+body to 4,000 characters before matching. Measured over the whole lake, 50.2% of
all core occurrences lie beyond that point, and the median EDGAR press-release body is
50,622 characters, so the only source carrying full article text was read at about an
eighth of its length.

DEFECT 2 — bad cores. Hand-labelling 60 occurrences (seed 20261111) put entity precision
at 36/60 = 60.0%, Wilson [47.4; 71.4]. Sixteen of the seventeen wrong-entity errors came
from the CORE being wrong, not from an ambiguous match: generic name components
("Therapeutics" matching "Relay Therapeutics"), financial terms ("Enterprise" matching
"Enterprise Value"), table words ("Institutions"), government programmes ("Medicare") and
one bankruptcy term ("Actions"). Only one error was a genuine matching ambiguity: "Target"
inside "Talos Target Coulomb Interest".

The two corrections pull in OPPOSITE directions - dropping cores lowers the counts,
lifting the cap raises them - so this run reports four configurations rather than one
number, and prints every core the gazetteers remove so a reader can check for a real
company among them.

Also fixes a third thing that is plainly right: HTML tags are stripped before matching.
35.9% of lake items carry them, all from the EDGAR path.

Reads no outcome.
"""
from __future__ import annotations
import collections, os, datetime as dt, json, pathlib, re, sys
from zoneinfo import ZoneInfo

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
PX = _path("PRICE_STORE", "one parquet per session, columns T and c")
EDGES = _path("EDGES_JSON", '{"edges": {ticker: [cores]}, "tickers": [...]}')
ET = ZoneInfo("America/New_York")
BASE, TOPK, CAP = 40, 3, 4000
FIRST_SESSION, LAST_SESSION = "2026-06-29", "2026-09-02"
TAG = re.compile(r"<[^>]{0,400}>")
WS = re.compile(r"\s+")
WORD = re.compile(r"[0-9A-Za-z]")

# Generic corporate-name components. None of these is a company on its own; each is the
# tail of many company names, which is how a truncated extraction produces it.
GENERIC = {
    "Therapeutics", "Pharmaceuticals", "Pharmaceutical", "Laboratories", "Laboratory",
    "Technologies", "Technology", "Holdings", "Holding", "Industries", "Industrial",
    "Enterprises", "Enterprise", "Solutions", "Systems", "Services", "Service",
    "Sciences", "Science", "Resources", "Partners", "Partnership", "Capital",
    "International", "Corporation", "Company", "Companies", "Institutions",
    "Institution", "Brands", "Properties", "Communications", "Networks", "Materials",
    "Products", "Instruments", "Electronics", "Financial", "Bancorp", "Bancshares",
    "Insurance", "Healthcare", "Hospitality", "Logistics", "Transportation",
    "Automotive", "Entertainment", "Resorts", "Airlines", "Midstream", "Packaging",
    "Textiles", "Apparel", "Stores", "Distribution", "Wholesale", "Builders",
    "Construction", "Engineering", "Development", "Ventures", "Investments",
    "Investment", "Securities", "Advisors", "Management", "Consulting", "Analytics",
    "Semiconductor", "Semiconductors", "Devices", "Equipment", "Machinery",
    "Components", "Interactive", "Regional", "Northern", "Southern", "Eastern",
    "Western", "Central", "Allied", "Standard", "Popular", "Superior",
    "Universal", "Consolidated", "Integrated", "Advanced", "Applied", "Precision",
    # "Premier" removed from this set 2026-11-11: Premier Inc. (PINC) is a real
    # company and a real counterparty of ANIP, so dropping the core dropped a
    # genuine edge. The ambiguity it carries is irreducible by any word list.
    "Dynamics", "Innovations", "Innovation",
}
# Not companies: programmes, institutions of state, legal terms.
NON_COMPANY = {
    "Medicare", "Medicaid", "Medicare Advantage", "U.S. government", "US government",
    "Actions", "Congress", "Treasury", "Federal Reserve", "European Union",
    "United Nations", "World Bank", "Social Security", "Internal Revenue Service",
    "Department of Defense", "Department of Energy", "European Commission",
}
# Places. The one filter the CC-NEWS run verified as removing no real company.
GEO = {
    "Los Angeles", "South Korea", "Saudi Arabia", "Australia", "Qatar", "New York",
    "United States", "United Kingdom", "Hong Kong", "New Zealand", "South Africa",
    "Costa Rica", "Puerto Rico", "San Francisco", "San Diego", "San Jose", "Las Vegas",
    "Abu Dhabi", "Czech Republic", "Dominican Republic", "Sri Lanka", "El Salvador",
    "North Carolina", "South Carolina", "North Dakota", "South Dakota", "West Virginia",
    "New Jersey", "New Mexico", "New Hampshire", "Rhode Island", "Latin America",
    "North America", "South America", "Middle East", "Southeast Asia",
}
DROP = GENERIC | NON_COMPANY | GEO


def build(pats):
    a = ahocorasick.Automaton()
    for p in pats:
        a.add_word(p, p)
    a.make_automaton()
    return a


def scan(auto, hay, original, back=None):
    for end, pat in auto.iter(hay):
        start = end - len(pat) + 1
        before = original[start - 1] if start > 0 else " "
        after = original[end + 1] if end + 1 < len(original) else " "
        if WORD.match(before) or WORD.match(after):
            continue
        yield (back[pat] if back else pat), start, len(pat)


def close_utc(day):
    return pd.Timestamp(dt.datetime.combine(day, dt.time(16, 0), tzinfo=ET)).tz_convert("UTC")


def salient_sets(cnt, cal, sessions, prev_sess, cores):
    """A3 salience per session: max over the session's calendar windows, against the
    third order statistic of the 40 calendar days before the session."""
    out = {}
    for d in sessions:
        p = prev_sess[d]
        win = [x for x in cal
               if dt.date.fromisoformat(p) < x <= dt.date.fromisoformat(d)]
        bl = [dt.date.fromisoformat(d) - dt.timedelta(k) for k in range(1, BASE + 1)]
        s = set()
        for c in cores:
            cc = cnt.get(c)
            if not cc:
                continue
            today = max((cc.get(x, 0) for x in win), default=0)
            if today <= 0:
                continue
            prev = sorted((cc.get(b, 0) for b in bl), reverse=True)
            thr = prev[TOPK - 1] if len(prev) >= TOPK else 0
            if today >= max(thr, 1):
                s.add(c)
        out[d] = s
    return out


def main() -> int:
    E = json.loads(EDGES.read_text())
    edges_all = {k: set(v) for k, v in E["edges"].items()}
    uni = E["tickers"]
    cores_all = sorted({c for v in edges_all.values() for c in v})
    removed = sorted(c for c in cores_all if c in DROP)
    cores_keep = [c for c in cores_all if c not in DROP]
    edges_keep = {k: (v - DROP) for k, v in edges_all.items()}
    edges_keep = {k: v for k, v in edges_keep.items() if v}
    print(f"rdzeni ogolem {len(cores_all)} | usunietych {len(removed)} | zostaje {len(cores_keep)}")
    print(f"spolek z krawedzia: {len(edges_all)} -> {len(edges_keep)}")

    # --- one matching pass, positions kept so both cap settings are derivable
    multi = [c for c in cores_all if len(c.split()) >= 2]
    single = [c for c in cores_all if len(c.split()) == 1]
    auto_ci, auto_cs = build([c.lower() for c in multi]), build(single)
    back = {c.lower(): c for c in multi}

    px_days = sorted(p.stem for p in PX.glob("*.parquet"))
    sessions = [d for d in px_days if FIRST_SESSION <= d <= LAST_SESSION]
    sessions = [d for d in sessions if any(x > d for x in px_days)]
    nxt = {d: [x for x in px_days if x > d][0] for d in sessions}
    prev_sess = {d: [x for x in px_days if x < d][-1] for d in sessions}
    need_lo = (dt.date.fromisoformat(sessions[0]) - dt.timedelta(BASE + 1)).isoformat()
    parts = sorted(p for p in LAKE.glob("session_date=*")
                   if need_lo <= p.name.split("=")[1] <= sessions[-1])
    cal_lo = dt.date.fromisoformat(parts[0].name.split("=")[1]) + dt.timedelta(1)
    cal = [cal_lo + dt.timedelta(i)
           for i in range((dt.date.fromisoformat(sessions[-1]) - cal_lo).days + 1)]
    bounds = {x: (close_utc(x - dt.timedelta(1)), close_utc(x)) for x in cal}
    print(f"sesji {len(sessions)} | okien kalendarzowych {len(cal)} | partycji {len(parts)}")

    cnt_all = collections.defaultdict(lambda: collections.defaultdict(int))
    cnt_cap = collections.defaultdict(lambda: collections.defaultdict(int))
    items = occ_all = occ_cap = 0
    for n, part in enumerate(parts, 1):
        first = {}
        for f in sorted(part.glob("run=*.parquet")):
            df = pd.read_parquet(f, columns=["id", "title", "body", "timestamp", "ingested_at"])
            ing = pd.to_datetime(df["ingested_at"], utc=True)
            ts = pd.to_datetime(df["timestamp"], utc=True)
            for i, rid in enumerate(df["id"]):
                if not isinstance(rid, str) or not rid or pd.isna(ts.iloc[i]):
                    continue
                if rid not in first or ing.iloc[i] < first[rid][0]:
                    first[rid] = (ing.iloc[i], ts.iloc[i],
                                  f"{df['title'].iloc[i] or ''} {df['body'].iloc[i] or ''}")
        for rid, (_, when, raw) in first.items():
            win = next((x for x in cal if bounds[x][0] < when <= bounds[x][1]), None)
            if win is None:
                continue
            items += 1
            text = WS.sub(" ", TAG.sub(" ", raw)).strip()
            low = text.lower()
            for core, pos, ln in list(scan(auto_ci, low, low, back)) + list(scan(auto_cs, text, text)):
                cnt_all[core][win] += 1
                occ_all += 1
                if pos + ln <= CAP:
                    cnt_cap[core][win] += 1
                    occ_cap += 1
        if n % 30 == 0:
            print(f"  [{n}/{len(parts)}] pozycji {items:,} wystapien {occ_all:,}", flush=True)

    print(f"\npozycji w oknach: {items:,}")
    print(f"wystapien bez progu {occ_all:,} | w progu {CAP}: {occ_cap:,} "
          f"= {100*occ_cap/max(occ_all,1):.1f}%")

    present = {}
    for d in sorted(set(sessions) | set(nxt.values())):
        df = pd.read_parquet(PX / f"{d}.parquet", columns=["T", "c"])
        present[d] = set(df.loc[df["c"].notna(), "T"].astype(str))

    print(f"\n{'konfiguracja':34} {'rdzeni':>7} {'spolek':>7} {'leczonych':>10} {'zmiana':>9}")
    base = None
    res = {}
    for label, cnt, ed, cs in (
            ("a) prog 4000, wszystkie rdzenie", cnt_cap, edges_all, cores_all),
            ("b) BEZ progu, wszystkie rdzenie", cnt_all, edges_all, cores_all),
            ("c) prog 4000, rdzenie odsiane", cnt_cap, edges_keep, cores_keep),
            ("d) BEZ progu, rdzenie odsiane", cnt_all, edges_keep, cores_keep)):
        sal = salient_sets(cnt, cal, sessions, prev_sess, cs)
        treated = 0
        for d in sessions:
            s = sal[d]
            for t in uni:
                if t in present.get(d, ()) and t in present.get(nxt[d], ()) and (ed.get(t, set()) & s):
                    treated += 1
        if base is None:
            base = treated
        print(f"{label:34} {len(cs):7} {len(ed):7} {treated:10,} "
              f"{100*(treated-base)/max(base,1):+8.1f}%")
        res[label] = {"cores": len(cs), "companies": len(ed), "treated": treated}

    print(f"\n=== USUNIETE RDZENIE ({len(removed)}), z liczba wystapien ===")
    for c in sorted(removed, key=lambda x: -sum(cnt_all.get(x, {}).values())):
        k = sum(cnt_all.get(c, {}).values())
        grp = "GEN" if c in GENERIC else ("NIE-FIRMA" if c in NON_COMPANY else "GEO")
        print(f"   {c[:32]:32} {k:7}  [{grp}]")

    # config (d) assignment, for the power measurement
    sal_d = salient_sets(cnt_all, cal, sessions, prev_sess, cores_keep)
    assign = {}
    for d in sessions:
        pres = [t for t in uni if t in present.get(d, ()) and t in present.get(nxt[d], ())]
        assign[d] = {"next": nxt[d], "present": pres,
                     "treated": [t for t in pres if edges_keep.get(t, set()) & sal_d[d]]}
    outd = _out("PANEL_DIR", "./panel_out"); outd.mkdir(parents=True, exist_ok=True)
    (outd / "assign.json").write_text(json.dumps(assign))
    (outd / "meta.json").write_text(json.dumps({
        "edge_holders": sorted(edges_keep), "tickers": uni,
        "cores_kept": len(cores_keep), "removed_cores": removed,
        "config": "d: full lake, tags stripped, no 4000-char cap, cores sieved",
    }))
    print(f"\nzrzucono przypisanie konfiguracji (d) -> {outd}")

    _out("OUT_JSON", "./panel_recount.json").write_text(json.dumps({
        "configs": res, "removed_cores": removed,
        "occurrences_uncapped": occ_all, "occurrences_capped": occ_cap,
        "items_in_windows": items, "sessions": len(sessions),
    }, indent=1))
    print(f"\nzapisano {_out('OUT_JSON', './panel_recount.json')}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
