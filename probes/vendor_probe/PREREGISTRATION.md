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
cargo run --manifest-path probes/vendor_probe/Cargo.toml -- \
  --days 2026-07-01 2026-07-15 2026-08-12
```

Rewritten from Python to Rust 2026-11-14 (a standalone crate with its own HTTP client, so it stays
outside the workspace's one-client invariant, `repo_invariants::clients`). The keys come from the
environment — `NEWSAPI_AI_KEY`, `NEWSCATCHER_API_KEY`, `WEBZ_API_KEY`, `PERIGON_API_KEY`, loaded from
`~/.secrets` — and the probe aborts with a named error if one is absent rather than reporting an
empty result, which is requirement R6 applied to itself. The three days are chosen to match days
already measured in our own archive, which held 287, 560 and 678 distinct items respectively.

## Amendment 2026-11-14 — a third vendor, Webz.io

Added **after** the two vendors above were registered and **after** a Webz.io key existed, so it is
disclosed here rather than hidden. This section changes the **scope** — the decision to probe a
third vendor — and nothing else. The decision rules Q1–Q4 are **not** re-opened: they were
registered 2026-11-11 before any key, they are vendor-agnostic, and they apply to Webz.io unchanged.
Because the thresholds pre-date every key, measuring Webz.io against them is not a threshold chosen
after seeing a number; only the choice to add Webz.io is after-the-fact, and that is what this dated
section records (the mechanism this document names for a later change).

**Webz.io.** Archive to 2008 (vendor claim), full article body, two timestamps (`published`,
`crawled`). Its archive tier is sales-gated and its self-serve/lite tier is recency-limited, so a
query for the 2026-07 days may return nothing on this key. **That empty result is itself the Q1 /
archive-access answer for Webz.io, recorded as a finding, not treated as a probe failure.** The same
three days and the same broad, un-narrowed query are used; the `WEBZ_API_KEY` adapter is added to
`probe.py` in the same change and **before any Webz.io call is made**, so no Webz.io number is seen
before this section exists.

## Amendment 2026-11-14 — a fourth vendor, Perigon

Added after the first three vendors were registered and after a Perigon key existed, by the same
disclosed mechanism as the Webz.io section above. It changes only the **scope**; the Q1–Q4 rules
registered 2026-11-11 are unchanged and apply to Perigon.

**Perigon.** Archive since 2016 (10+ years on Commercial), full article body, and — the reason it
was added — a **first-ingestion / index-time filter** (`addDate`, `addDateFrom`/`addDateTo`). That
is the point-in-time feature NewsCatcher was shortlisted for but which proved Enterprise-gated and
count-only on that key; Perigon offers it self-serve, so it can answer Q4 (publication-to-index
lag) directly from both timestamps. Historical depth may be a paid tier, so a 2026-07 query may be
refused on this key — and that refusal is itself the Q1/archive-access finding, recorded, not a
probe failure. The same three days and the same broad query are used; the `PERIGON_API_KEY` adapter
is added to the Rust probe before any Perigon call is made.

## Amendment 2026-11-14 — choosing the vendor for a one-time 12-month history

Written before either number below is measured. The owner's need changed: history is wanted
**once**, twelve full calendar months, to train and tune; the forward feed is built in-house. Q1's
"cost of a year" therefore becomes a one-month purchase, and the two vendors left with full bodies
and two timestamps are NewsAPI.ai and Webz.io Archive. Q1–Q4 above are not re-opened.

**Volume, one construction per vendor, both a last-30-days extrapolation.**
- NewsAPI.ai: `totalResults` of one query — `categoryUri=news/Business`,
  `sourceLocationUri=<United_States>`, `lang=eng`, `dataType=news`, `isDuplicateFilter=keepAll` —
  over the last 30 complete days the key reaches, × 365/30. Call it `N_er`.
- Webz.io Archive: `estimatedTotalPostCount` from `setArchiveQuery` for the same 12 months with the
  closest query its language allows (`site_type:news language:english thread.country:US` plus
  business terms), never confirmed. Webz states this figure is itself last-30-days × months. Call it
  `N_wz`.
- Known limit: one 30-day window, so seasonality is not measured; the two queries are not identical,
  so `N_er ≠ N_wz` is a fact about the vendors, not an error.

**Cost, from published prices, month-partitioned so no query crosses 31 December:**
`cost_er = 90 + 0.015 × max(0, 5 × ⌈N_er / 100⌉ − 5000)` USD and
`cost_wz = max(120, 0.0002 × N_wz)` USD.

**Lag, which can veto a vendor.** Per row, `index time − publication time` (`dateTime − dateTimePub`,
`crawled − published`) over at least 10 distinct days. The registered lag is the **maximum**, as for
EDGAR. **Any row ≤ 0 s registers nothing for that vendor** (the `observation_lag` rule), and a vendor
with no registrable lag cannot be dated by `from_backfill`, so it is out.

| outcome | decision |
|---|---|
| both vendors register a lag | the lower of `cost_er`, `cost_wz`; a tie (±10 USD) goes to NewsAPI.ai, which bills what it delivers and pages deterministically |
| only one registers a lag | that one, at its cost |
| neither registers a lag | no purchase; the history question returns to the owner |
| the chosen cost exceeds 300 USD | Q1's middle band: narrow once by publisher class, never by company list, and re-measure |

## Amendment 2026-11-14 (later the same day) — written AFTER the first measurement

**Disclosure.** Everything in this section was written after the numbers below were seen, and it
changes how the lag is constructed. It is therefore weaker evidence than the section above, and is
recorded as such rather than edited into it.

**What was measured** (NewsAPI.ai free key, 2026-11-14, Query A above): 315,375 articles over
2026-10-15..2026-11-13, so `N_er` = 3,837,062 and `cost_er` = 2,893 USD. Lag over 2,400 rows on 12
days: 0 rows ≤ 0 s, median 354 s, p90 16,940 s, **maximum 407,505,260 s (12.9 years)**, set by 3 rows
published in 2013, 2014 and 2022 and indexed now. Webz.io: `N_wz` = 8,331,168, `cost_wz` = 1,666 USD,
no lag (its free sample is only reachable in the logged-in console). Under the rule above only
NewsAPI.ai registers a lag, and it costs more than 300 USD, so Q1's middle band applies.

**Why the lag construction changes.** Registering the maximum dates every backfilled item 12.9 years
after its publication, so nothing in a 12-month window would ever be visible: the rule fails by
construction, not by measurement. An old article indexed today is not news of today. Two defects of
the first measurement are corrected with it: the sample took only the first and last page of each
day (index hours 00 and 23 UTC), and it treated a date-only publication as midnight.

**The narrowed query (Q1's one narrowing, by publisher class).** Query A plus `sourceUri` restricted
to this list of US financial press, fixed now and kept in this file: reuters.com, cnbc.com,
marketwatch.com, wsj.com, bloomberg.com, ft.com, barrons.com, finance.yahoo.com, seekingalpha.com,
fool.com, investing.com, benzinga.com, businessinsider.com, forbes.com, thestreet.com,
foxbusiness.com, investors.com, zacks.com, nasdaq.com, morningstar.com, kiplinger.com. A domain the
vendor cannot resolve is recorded as missing, never substituted. Webz.io gets the same list as
`site:` terms.

**The new lag construction.**
- A row is **kept** only if its publication falls inside the window being pulled. A kept row's lag
  is `index time − publication time`. The registered lag is the **maximum over kept rows**.
- A publication value of exactly `T00:00:00Z` is treated as a **date**, not an instant. It is
  counted separately and kept out of the instant lag, because a date has no instant to subtract.
- Any kept instant row ≤ 0 s still registers nothing for that vendor.
- Pages are sampled **uniformly at random across each day's result pages**, from a recorded seed,
  on at least 10 distinct days.
- Reported alongside, never instead: how many rows the publication filter dropped.

The decision table above is unchanged and is re-applied to the narrowed numbers. A registered
maximum still measured in months would date the backfill months late; that outcome is reported, not
re-negotiated here.

## Amendment 2026-11-14 (third) — a one-month Webz.io order, written before it is placed

**Narrowed results, recorded.** NewsAPI.ai: `N_er` = 659,750, `cost_er` = 509.85 USD, lag registers
(0 kept instant rows ≤ 0 s over 8,331 rows on 30 days), maximum 1,124,475 s (13.0 days), p90 9,023 s,
median 485 s; `sourceLocationUri` = United States returns 0 rows for reuters.com, ft.com and
investing.com. Webz.io: `N_wz` = 243,804, `cost_wz` = 120 USD; its free sample held 100 rows, all
crawled on **one** day (2026-10-15), and **one** row had `published` equal to `crawled` to the
millisecond (lag 0 s). Under the rules above NewsAPI.ai is closed (above 300 USD after its one
narrowing) and Webz.io registers nothing (fewer than 10 days, one row ≤ 0 s).

**Disclosure.** This section is written after seeing that one 0 s row, so its classification rule
below is post hoc. It is written before the order is placed and before any of the order's rows are
seen.

**The order.** The narrowed Webz.io query (with `thread.country:US`), one month of range: October
2026. The smallest order Webz.io sells, priced at the 10 USD per-month minimum. Its rows are the lag
sample; no other Webz.io number is used for the lag.

**Classification of a row, applied before any lag is computed.**
- `published` equal to `crawled` to the millisecond: **publication unknown**. Webz.io writes its crawl
  time when a page states no publication time, so this is a missing value, not a lag of 0 s. Such a
  row is counted and reported, kept out of the lag, and would be stored undated.
- `published` exactly at local midnight (`T00:00:00.000` in the row's own offset): **a date**, counted
  separately and kept out of the instant lag.
- `published` outside October 2026: dropped by the publication filter, counted, largest lag reported.
- Every other row is a kept instant row, and its lag is `crawled − published`.

**What registers.** Kept instant rows on at least 10 distinct crawl days and **none ≤ 0 s**: the
maximum registers. A single kept instant row with `crawled` before `published` still registers
nothing. If publication-unknown rows exceed 10% of all rows, that share is reported as a defect of
the vendor's dating, whatever the lag result. If Webz.io registers, the decision table of the first
2026-11-14 section is re-applied with both vendors' narrowed numbers.
