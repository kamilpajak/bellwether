"""Build step 2 of the successor memo: the three-day probe at two news vendors.

Answers four questions that no documentation can answer, because they depend on OUR
query rather than on the vendor's feature list:

  Q1 VOLUME     how many articles a day does a BROAD query return? This is the whole
                cost of history: at 300/day a year is ~$82, at 10,000/day ~$2,738.
                The query is deliberately NOT narrowed to our bracket - a vendor-side
                filter would become an unauditable part of the measurement, the same
                reason a market-cap condition never goes into a prompt.
  Q2 STABILITY  does the same past-day query, repeated later, return the same set?
                One run cannot answer this. This run records a digest of the id set;
                re-run with --compare <file> after some days to get the answer.
  Q3 BODY       is a full article body returned, or a snippet? Measured as the length
                distribution of whatever text field the vendor provides.
  Q4 LAG        the distribution of (index time - publication time). This is the input
                to requirement R11: a backfilled item's observation time derives from a
                measured lag, never from an assumption.

Scratchpad code on purpose. If a vendor is adopted, its calls move behind a canonical
client under alphalens_pipeline/data/ with a test_no_raw_<vendor>_http.py beside it, per
the repo's per-vendor rule. Nothing here is meant to land as-is.

Reads NO outcome and spends only vendor quota.
"""
from __future__ import annotations
import argparse, hashlib, json, os, pathlib, statistics, sys
import datetime as dt

import httpx

OUT = pathlib.Path(__file__).parent / "vendor_probe_results.json"
PAGE_CAP = 12          # stop paging; a day needing more than 1200 articles is answer enough
TIMEOUT = 60.0


def _need(name: str) -> str:
    """Missing input aborts. An empty fallback is forbidden - requirement R6."""
    v = os.environ.get(name)
    if not v:
        raise SystemExit(
            f"brak klucza {name} w srodowisku.\n"
            f"  NEWSAPI_AI_KEY     -> darmowy plan na newsapi.ai (2000 wyszukiwan)\n"
            f"  NEWSCATCHER_API_KEY -> darmowy plan na newscatcherapi.com (10000 kredytow)"
        )
    return v


# --------------------------------------------------------------------------- vendors

def probe_newsapi_ai(day: str) -> dict:
    """Event Registry / newsapi.ai. Broad query: English business news for one UTC day."""
    key = _need("NEWSAPI_AI_KEY")
    # Documented setup passes the key as a URL parameter; requests stay sequential
    # because the documented rate limit caps simultaneous requests at 5.
    url = f"https://eventregistry.org/api/v1/article/getArticles?apiKey={key}"
    items, pages, cost_note = [], 0, None
    with httpx.Client(timeout=TIMEOUT) as c:
        for page in range(1, PAGE_CAP + 1):
            body = {
                "resultType": "articles",
                "lang": "eng",
                "dateStart": day,
                "dateEnd": day,
                "articlesPage": page,
                "articlesCount": 100,
                "articlesSortBy": "date",
                "isDuplicateFilter": "keepAll",   # their dedup is THEIR editorial call
                "includeArticleBody": True,
            }
            r = c.post(url, json=body)
            r.raise_for_status()
            j = r.json()
            block = (j.get("articles") or {})
            got = block.get("results") or []
            pages += 1
            items += got
            total = block.get("totalResults")
            if total is not None:
                cost_note = f"totalResults={total}"
            if len(got) < 100:
                break
    return {
        "vendor": "newsapi.ai", "day": day, "pages": pages, "note": cost_note,
        "items": [
            {
                "id": str(a.get("uri") or a.get("url")),
                "published": a.get("dateTimePub") or a.get("dateTime"),
                "indexed": a.get("dateTime"),
                "body_len": len(a.get("body") or ""),
                "is_duplicate": bool(a.get("isDuplicate")),
            }
            for a in items
        ],
    }


def probe_newscatcher(day: str) -> dict:
    """NewsCatcher self-serve News API, /articles.

    Corrected 2026-11-11 after reading the vendor's own reference. The earlier version
    of this function targeted a DIFFERENT product surface: `by_parse_date`, `parse_date`,
    `published_date_precision` and `x-api-token` belong to the Enterprise News API, which
    is custom-priced. The self-serve endpoint below has no index-time filter and returns
    a snippet rather than a body, so Q3 and Q4 are expected to come back negative here.
    Measuring that is the point: it is the difference between a documented feature and a
    feature on the plan we can actually buy.

    Limits from the reference: history 365 days, page_size max 100, page * page_size at
    most 1000, and `window.to` echoed back as `to_date` to keep paging stable.
    """
    key = _need("NEWSCATCHER_API_KEY")
    url = "https://api.newscatcherapi.com/news-api/articles"
    nxt = (dt.date.fromisoformat(day) + dt.timedelta(1)).isoformat()
    items, pages, total, window_to = [], 0, None, None
    with httpx.Client(timeout=TIMEOUT, headers={"x-api-key": key}) as c:
        for page in range(1, min(PAGE_CAP, 10) + 1):      # page * 100 <= 1000
            body = {
                "q": "*",
                "languages": "en",
                "from_date": day,
                "to_date": window_to or nxt,
                "page": page,
                "page_size": 100,
            }
            r = c.post(url, json=body)
            if r.status_code == 422:
                break                                      # documented deep-page refusal
            r.raise_for_status()
            j = r.json()
            got = j.get("results") or j.get("articles") or []
            total = j.get("total_hits", total)
            window_to = ((j.get("window") or {}).get("to")) or window_to
            pages += 1
            items += got
            if len(got) < 100 or page >= (j.get("total_pages") or page):
                break
    return {
        "vendor": "newscatcher", "day": day, "pages": pages,
        "note": f"total_hits={total} cap=1000/query history=365d snippet_only",
        "items": [
            {
                "id": str(a.get("id") or a.get("url") or a.get("link")),
                "published": a.get("published_at") or a.get("published_date"),
                # crawled_at is the vendor's crawl time; it is NOT filterable on this plan
                "indexed": a.get("crawled_at") or a.get("parse_date"),
                "body_len": len(a.get("snippet") or a.get("content") or ""),
                "precision": a.get("published_date_precision"),
            }
            for a in items
        ],
    }


VENDORS = {"newsapi.ai": probe_newsapi_ai, "newscatcher": probe_newscatcher}


# --------------------------------------------------------------------------- readout

def summarise(res: dict) -> dict:
    items = res["items"]
    ids = sorted({i["id"] for i in items if i["id"]})
    bodies = [i["body_len"] for i in items]
    lags = []
    for i in items:
        p, x = i.get("published"), i.get("indexed")
        if not p or not x:
            continue
        try:
            pt = dt.datetime.fromisoformat(p.replace("Z", "+00:00"))
            xt = dt.datetime.fromisoformat(x.replace("Z", "+00:00"))
        except ValueError:
            continue
        lags.append((xt - pt).total_seconds())
    q = lambda xs, f: sorted(xs)[min(int(f * len(xs)), len(xs) - 1)] if xs else None
    return {
        "vendor": res["vendor"], "day": res["day"], "pages": res["pages"],
        "note": res.get("note"),
        "articles": len(items),
        "distinct_ids": len(ids),
        "id_digest": hashlib.sha256("\n".join(ids).encode()).hexdigest(),
        "paging_exhausted": res["pages"] < PAGE_CAP,
        "body_len_median": int(statistics.median(bodies)) if bodies else 0,
        "body_len_p90": q(bodies, 0.9),
        "body_empty_share": round(sum(1 for b in bodies if b == 0) / max(len(bodies), 1), 4),
        "lag_measured_for": len(lags),
        "lag_seconds_median": q(lags, 0.5),
        "lag_seconds_p90": q(lags, 0.9),
        "lag_negative_share": round(sum(1 for l in lags if l < 0) / max(len(lags), 1), 4),
    }


def main() -> int:
    ap = argparse.ArgumentParser(description="three-day probe at two news vendors")
    ap.add_argument("--days", nargs="+", required=True, help="UTC days, YYYY-MM-DD")
    ap.add_argument("--vendors", nargs="+", default=list(VENDORS), choices=list(VENDORS))
    ap.add_argument("--compare", help="earlier results file, to answer Q2 (stability)")
    args = ap.parse_args()

    rows = []
    for v in args.vendors:
        for day in args.days:
            try:
                res = VENDORS[v](day)
            except SystemExit:
                raise
            except Exception as e:
                print(f"  {v} {day}: PORAZKA {type(e).__name__}: {e}", flush=True)
                rows.append({"vendor": v, "day": day, "error": f"{type(e).__name__}: {e}"})
                continue
            s = summarise(res)
            rows.append(s)
            print(f"  {v} {day}: {s['articles']} art., {s['pages']} stron, "
                  f"cialo mediana {s['body_len_median']} zn., "
                  f"opoznienie mediana {s['lag_seconds_median']} s", flush=True)

    stamp = dt.datetime.now(dt.timezone.utc).isoformat()
    OUT.write_text(json.dumps({"fetched_at": stamp, "rows": rows}, indent=1))

    print(f"\n{'dostawca':14} {'dzien':12} {'art.':>6} {'wyczerp.':>9} {'cialo med.':>11} "
          f"{'opozn. med.':>12} {'opozn. p90':>11}")
    for r in rows:
        if "error" in r:
            print(f"{r['vendor']:14} {r['day']:12} BLAD: {r['error'][:40]}")
            continue
        print(f"{r['vendor']:14} {r['day']:12} {r['articles']:6} "
              f"{str(r['paging_exhausted']):>9} {r['body_len_median']:11} "
              f"{str(r['lag_seconds_median']):>12} {str(r['lag_seconds_p90']):>11}")

    # Q1: cost of a year, from the vendor's published token pricing
    print("\nQ1 koszt roku historii (cennik newsapi.ai: 5 tokenow/rok na wyszukiwanie,")
    print("   100 art./wyszukiwanie, nadwyzka $0,015/token):")
    for r in rows:
        if "error" in r or r["articles"] == 0:
            continue
        searches = -(-r["articles"] // 100)
        tok_year = searches * 5 * 365
        print(f"   {r['vendor']:14} {r['day']}: {searches} wyszukiwan/dzien -> "
              f"{tok_year:,} tokenow/rok -> ${tok_year * 0.015:,.0f}")

    if args.compare:
        prev = json.loads(pathlib.Path(args.compare).read_text())
        old = {(r.get("vendor"), r.get("day")): r.get("id_digest")
               for r in prev.get("rows", [])}
        print("\nQ2 stabilnosc archiwum (ten sam zbior po czasie?):")
        for r in rows:
            k = (r.get("vendor"), r.get("day"))
            if k not in old or "error" in r:
                continue
            same = old[k] == r["id_digest"]
            print(f"   {k[0]:14} {k[1]}: {'TEN SAM zbior' if same else 'ZBIOR SIE ZMIENIL'}")
    else:
        print("\nQ2 stabilnosc: zapisano odciski. Powtorz ten sam przebieg po kilku dniach")
        print(f"   z --compare {OUT.name}, bo jeden przebieg tego nie rozstrzyga.")
    print(f"\nzapisano {OUT}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
