# Pre-registration: news-vendor probe

**Status:** REGISTERED 2026-11-11, before any API key exists and therefore before any number is
known. Nothing below may be edited after the first run; a later disagreement with it goes in a new
section headed with its date.

**Why this document exists.** The project's own doctrine is to pre-register before testing, and a
vendor evaluation is a test. A cost threshold chosen after seeing the cost is not a threshold, and
a vendor "passing" a bar drawn around its own result tells us nothing. The predecessor has a
measured case of exactly this failure mode: six salience thresholds declared in advance all landed
inside one standard error, which only looked like an honest search because the thresholds came
first. Here the thresholds come first.

## What is being probed

Two vendors, three days each, on a **broad** query.

The query is deliberately not narrowed to our universe. Narrowing it would make the vendor's filter
an unauditable part of our measurement, which is the same reason a market-capitalisation condition
never goes into a model prompt. We query broadly and filter locally.

- **NewsAPI.ai / Event Registry.** Archive to 2014, full article body, $90/month for 5,000 tokens,
  archive search 5 tokens per searched year, 100 articles per search, $0.015 per overage token.
  Documented filters apply to publication date only; `dateTime` and `dateTimePub` both appear in
  the response but neither is a documented filter or sort key.
- **NewsCatcher, self-serve.** `/articles` covers 365 days, `page_size` max 100 and
  `page * page_size` at most 1,000. Results are documented as carrying a snippet. `by_parse_date`,
  `parse_date` and `published_date_precision` belong to the Enterprise product, which is quoted
  individually. Probing it anyway is deliberate: the difference between a feature in the
  documentation and a feature on the plan we can buy is part of what is being measured.

## The four questions, and what each answer will mean

### Q1 — Volume, and therefore the cost of a year of history

Measured as articles returned per day for our broad query, converted to tokens by the vendor's
published pricing. For reference, our own archive holds 287–678 distinct items per day.

| cost of one year of history | what it means, decided now |
|---|---|
| **≤ $300** | The archive route is open. Proceed to a backfill proposal for the owner. |
| **$300 – $1,000** | Narrow the query **once**, by language and by publisher class only, never by our company list, and re-measure. If it stays above $300, treat as the band below. |
| **> $1,000** | The archive route is closed at this vendor. The panel then grows by calendar time, which the memo measures at about six months to reach 246 sessions, and the backfill question returns only if a cheaper source appears. |

### Q2 — Archive stability

One run cannot answer this, so this run records a sha256 digest of the returned id set per vendor
per day and does nothing else with it. A repeat run follows **no sooner than 7 days later** with
`--compare`.

| outcome | what it means |
|---|---|
| identical digests on all probed days | The vendor's archive is reproducible for our purposes. Still snapshotted locally, because an index timestamp is not an immutable archive and a changed article body would not show in an id digest. |
| **any** day's id set differs | The vendor archive is not a reproducible source. The backfill becomes a one-shot read whose result cannot be re-derived, and that fact is recorded as a limitation of every panel built on it. |

### Q3 — Article body, or a snippet

Measured as the median length of whatever text field the vendor supplies.

| median body length | what it means |
|---|---|
| **≥ 2,000 characters** | Usable for entity matching, which requirement R17 says must run over article body text. |
| 1,000 – 2,000 | Marginal. Record it and decide against our own sources rather than in the abstract. |
| **< 1,000** | The vendor supplies snippets and adds nothing over what we already have: our own RSS path has a median body of 117 characters and our EDGAR path 50,622. A snippet vendor cannot improve recall on the long tail, which is where small-cap counterparties appear. |

### Q4 — Publication-to-index lag

The distribution of (vendor index time − publication time), which is the direct input to
requirement R11: a backfilled item's observation time derives from a measured lag carrying the
probe identity, the probe count and the leak count.

| outcome | what it means |
|---|---|
| both timestamps present, lag measurable | R11 is satisfiable from the vendor. The median and p90 become the registered lag for backfilled items from this source. |
| only a publication timestamp | R11 cannot be satisfied from this vendor. Backfilled items then need a lag established by a separate retrieval-leak probe, the shape already used for Perplexity: documents fetched under a declared cutoff and checked for whether they could have been visible at that instant. |
| lag is negative for a material share | The vendor's two timestamps do not mean what their names suggest, and the share is reported rather than discarded, because a negative lag is a fact about the vendor and not an outlier to clean. |

## What this probe cannot settle

It does not clear gate 3 of `docs/inherited/methodology-gates.md`. That gate requires five
sector-diverse anchor events triangulated across two sources with a stated tolerance, and it fires
before a vendor's data may enter any pre-registration of a study. This probe is a superset on
volume, stability, body text and lag, and a strict subset on correctness: it checks nothing about
whether the vendor's content is accurate. Passing here permits a backfill proposal and nothing
further.

It also measures nothing about coverage of our universe. The probe asks what the vendor returns for
a broad query, not whether what it returns contains the companies we care about. Common Crawl's
CC-NEWS failed exactly there — 2.4% of our own items for one day — after looking adequate on every
other axis, so coverage against our own archive is a separate measurement and is not claimed here.

## Running it

```
NEWSAPI_AI_KEY=...  NEWSCATCHER_API_KEY=...  \
  uv run --with httpx python probes/vendor_probe/probe.py --days 2026-07-01 2026-07-15 2026-08-12
```

The three days are chosen to match days already measured in our own archive, which held 287, 560
and 678 distinct items respectively. The script aborts with a named error if a key is absent rather
than reporting an empty result, which is requirement R6 applied to itself.
