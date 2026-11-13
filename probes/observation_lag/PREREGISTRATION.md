# Pre-registration: EDGAR observation-lag probe

**Status:** REGISTERED 2026-11-13, before this probe has made a single request and therefore before
any number exists. Nothing below may be edited after the first run; a later disagreement with it
goes in a new section headed with its date.

**What this freeze is, and what it is not.** The text above the dated sections was written before
the probe made a request, and the amendments and the run were appended as new dated sections
rather than edited into it. That is a claim about how it was written, not a fact git can check:
the file's FIRST commit is after the run, so there is no earlier object to compare it against. A
later probe in this tree should be committed before its first request, which costs one commit and
turns the same discipline into evidence.

**Why this document exists.** The project's doctrine is to pre-register before measuring, and this
is a measurement whose answer decides whether a backfill may happen. The predecessor has a measured
case of the failure this prevents: six salience thresholds declared in advance all landed inside one
standard error, which only looked like an honest search because the thresholds came first. Here the
bands come first.

**What is blocked without it, measured 2026-11-13 against live EDGAR and recorded in
`docs/design/architecture.md` §10 D5.** A `vendor_archive` window is refused before its first
request unless a `MeasuredLag` with a probe id, a non-zero probe count and a non-zero lag stands
behind it (§8 row R11, enforced by `news_provider::provider::probed_lag` and asked by
`edgar-press`'s `plan`). Every window earlier than the run's own UTC date takes that arm. On the
other side, the vendor's quarter listing named `form.20261110.idx` and `form.20261112.idx` and
nothing for 2026-11-11 or 2026-11-13, so a same-day run plans zero requests with
`reason: window_not_published`. The two together leave one collecting path: a hand-passed `--at`
inside a published window. This probe is what ends that.

## What is being measured

**One sentence, in the units `pit::MeasuredLag` stores:** the number of whole seconds between a
filing's acceptance instant and the instant the vendor states it last wrote the daily index file
that names that filing.

### The construction

- **Which days.** 2026-11-10 and 2026-11-12. They are the two days the live quarter listing named
  on 2026-11-13, so they are the only two that a run dated after them reaches at all, and both sit
  on the `vendor_archive` side of `edgar-press`'s `Provenance` boundary — the exact arm that is
  being refused. Two days, not three, is a budget decision and its cost is in "What this probe
  cannot settle".
- **Which filings.** Form `8-K` rows only, because `providers/edgar-press/rules/edgar-press.rules.json`
  names `"form_types": ["8-K"]` (read 2026-11-13) and nothing else is stored, so nothing else would
  be dated by the registered lag. `8-K/A` is not included for the same reason: the rules do not name
  it. The roster is deliberately **not** applied — a universe filter would make our own CIK list
  part of a measurement about the vendor's publication, the same reason a market-capitalisation
  condition never goes into a model prompt.
- **How many, and which ones.** 12 per day, chosen before any acceptance instant is read: the day's
  `8-K` rows in the index file's own order, taking row `floor(i × n / 12)` for `i = 0 … 11`, where
  `n` is that day's `8-K` row count. Evenly spaced rather than the first 12, because the file is
  ordered by form type and then company name and the first 12 would be one end of the alphabet.
  For scale: the 2026-11-10 index held 5,429 filing rows of all forms, measured by download
  2026-11-12 (the number is in `edgar-press`'s `BODY_LIMIT` comment with that construction).
- **The two instants that are subtracted.**
  1. **Publication:** `<ACCEPTANCE-DATETIME>` from the filing's own `SEC-HEADER`, parsed
     `%Y%m%d%H%M%S` in `America/New_York` and converted to UTC. This is the same field, format and
     zone `edgar-press` stores as `ValidTime`, so the probe subtracts from the instant production
     will date the item by and not from a second reading of it.
  2. **Observation:** the instant the vendor states it last wrote `form.<day>.idx` — the
     `Last-Modified` response header on that file, cross-read against the `last-modified` field of
     the same entry in the quarter `index.json` when that field is served.
- **Why those two and not "when we read it".** A lag computed from our own fetch time measures our
  schedule, not the vendor's publication. Both instants above are the vendor's own statements and
  neither moves when the probe runs at a different hour.
- **One stated assumption inside the second instant.** `Last-Modified` is an HTTP date and carries
  its own zone. The quarter listing's `last-modified` field does not, and nothing in this tree
  records which zone it is in — the committed `quarter_index.json` fixture has no such field at all,
  so the probe reads it as `America/New_York`, the zone EDGAR files in. If the two instants come
  back about 5 hours apart (4 in summer), that is this assumption being wrong and not a vendor fact:
  the field is UTC, the fix is one constant in `probe.py`, and the run is repeated. Any other
  disagreement is the vendor's.
- **Sign and unit.** `observation − publication`, whole seconds, `i64`, which is what
  `MeasuredLag.seconds` holds. A positive lag means we could first have known the filing that many
  seconds after it was accepted.

### A third instant that exists and is not used

The daily index's own header line `Last Data Received: November 10, 2026` carries a **date and no
hour**. `pit::datedness` forbids folding a date into a midnight instant, so it cannot be subtracted
from an instant and it is read for one thing only: the probe prints it beside each day so a reader
can see whether the file claims the day it is named after.

### What the four `MeasuredLag` fields will hold

| field | value this probe produces |
|---|---|
| `probe_id` | `edgar_index_write-<UTC date of the run, %Y%m%d>`, e.g. `edgar_index_write-20261114`. It names the construction and the run, which is what makes a second run distinguishable from the first. |
| `probes` | the number of sampled filings for which **both** instants parsed. Not the number requested: a filing whose header the fetched prefix did not contain is reported as unusable and is not a probe. |
| `leaks` | the number of those filings whose own measured lag is **greater** than the registered scalar `seconds`. A single scalar dates every backfilled item, so a filing that appeared later than the scalar claims is a filing we would assert we saw before we could have. That is the leak this field counts, and it is the same question the Perplexity retrieval probe asked when the half-question admitted non-visible documents in 60.2% of 69 probes. |
| `seconds` | the registered scalar, chosen by the bands below and by nothing else. |

## The request budget

**27 requests to `www.sec.gov` for the registered run**, and **no run under this registration may
exceed 30**: 1 quarter `index.json`, 2 daily `form.<day>.idx`, and 12 filing documents on each of
the 2 days. The script counts every request, refuses the one that would cross `--budget` (default
30) with the code `budget_exhausted`, and prints the count it spent.

Two things keep each request small: a filing document is fetched with `Range: bytes=0-8191` and the
read is capped at 16 KiB whether or not the vendor honours the range, because
`<ACCEPTANCE-DATETIME>` is on the third line of the header; the two index files are read whole, at
about 600 KB each by the construction above.

**Pace: at most one request every 0.5 s**, 2 per second. SEC's stated fair-access ceiling is 10
requests a second per IP (the predecessor's vendor-quota table, read 2026-11-13; nothing in this
tree has measured it), the bucket is per IP and not per caller, and this host also carries the
predecessor's production EDGAR path and the Rust collector's own limiter. The probe therefore takes
a fifth of a ceiling it shares and does not pace to it.

## What each outcome will mean, decided now

The scalar is read off the **p90** of the per-filing lags and not off the median, because the
consequence of a scalar that is too small is a claim of knowledge we did not have, and the
consequence of one that is too large is losing an item from a cutoff it belonged in. Only the first
is a point-in-time violation.

| p90 of the per-filing lag | what it means |
|---|---|
| **≤ 24 h** | Register `seconds = ceil(p90)`. A backfill of `vendor_archive` windows from this source is permitted. A backfilled item's observation then falls on its publication day or the next one, so day-grain membership moves by at most one session, and `leaks`/`probes` travel with the lag and are disclosed by any panel built on it (R10). |
| **24 h – 7 d** | Register `seconds = ceil(p90)` — it is the measurement — and the backfill is permitted **only** with this written into the panel: every backfilled item's session membership moves by more than one session, so a day-grain study over backfilled days is measuring a shifted calendar. Any panel mixing backfilled and live items in one day column is refused under this band. |
| **> 7 d** | The daily index's write instant does not date filings usefully. This construction is closed and the only remaining path is the polling probe named below, which has not been written. |

| the sign of the lag | what it means |
|---|---|
| **every sampled filing positive** | The construction holds and the band table above decides. |
| **any single filing ≤ 0 s** | **Nothing is registered.** A file that contains a filing cannot have been last written before that filing was accepted, so an instant that says so is not the file's write instant for at least some rows, and we have no second instrument that could say which rows. The share is reported with the accession numbers beside it and is not cleaned away: a negative lag is a fact about the vendor. The threshold is one filing, and it is one because a single impossible pair falsifies what the instant means rather than being an outlier in a distribution. A computed scalar of exactly 0 s falls here too and is **not** raised to 1 s to get past `probed_lag`: 0 s puts the observation at the publication instant, which is the leak itself. |

| the two vendor instants | what it means |
|---|---|
| **agree within 60 s** | Use the `Last-Modified` header. The tolerance is 60 s because `Last-Modified` is served at second granularity and 60 s is three orders of magnitude below the smallest quantity any band above turns on. |
| **differ by more than 60 s on any probed day** | Nothing is registered. Two vendor statements about one file's last write that differ by more than a minute cannot both be that instant, and the probe cannot tell which is served from an origin and which from a cache. Both are reported per day. |
| **neither is served** | Nothing is registered, code `write_instant_absent`. The vendor states no write instant, this construction is unavailable, and the polling probe below is the only path. |

| the sample | what it means |
|---|---|
| **more than half of the sampled documents did not answer or carried no acceptance field** | Nothing is registered, code `sample_too_thin`. A distribution over whichever documents happened to answer is the silent-partial failure this project forbids; the refused count is printed beside every `n` either way. |

## What this probe cannot settle

- **When a given row first appeared in the index.** The vendor serves the file's **last** write and
  nothing else, so each per-filing lag is an **upper bound** on that filing's true appearance lag.
  That is the safe direction for a point-in-time system — an upper bound never claims earlier
  knowledge than we had — but it means the probe cannot tell a filing that appeared within a minute
  from one that appeared hours later. Measuring the per-row instant needs the **polling probe**:
  re-reading one live day's index on a fixed interval and recording the poll at which each row first
  appears, whose error is the poll interval and whose cost is one request per poll. It is named here
  and has not been written. This is the limit the task's own brief names, and the answer to it is
  the upper bound above rather than a sentence written around it.
- **Whether the index is rewritten after its day ends.** If EDGAR rewrites `form.<day>.idx` later,
  `Last-Modified` is that later write and every lag computed from it is inflated by the gap. The
  probe reports each day's write instant beside the day it names so a reader can see whether it sits
  inside that day, and a second run of the same two days **no sooner than 7 days later** answers it
  directly: an unchanged instant means the file is final, a moved one means this construction
  measures the latest rewrite. That second run is scheduled, not optional.
- **How many independent observations it has.** All 12 filings of one day share **one** write
  instant, so the spread of the per-filing lags is mostly the spread of acceptance times within a
  filing day, offset by a single number. For anything about the vendor's writing behaviour the
  exchangeable unit is the **day**, and this run has two. The predecessor measured exactly this
  mistake: treating rows as the unit where sessions were the unit overstated precision by 2.61×
  (`docs/design/successor_system_design_2026_11_11.md`, which carries that number's construction). No
  between-day claim, no standard error and no quantile beyond p90 may be read off this run.
- **The document phase.** `edgar-press` makes a second request per filing (`RequestPhase::Document`)
  and this probe measures only the index. A filing's document could become readable before or after
  its index row; the lag registered is the index's.
- **Whether the number has anywhere to go.** `EdgarPress::with_observation_lag` has **no production
  caller**: `services/newsvault/src/cmd_collect.rs`'s `open_provider` builds the provider without
  it (grep over the tree, 2026-11-13, the only other caller being
  `providers/edgar-press/tests/edgar_press.rs`). Producing the number therefore unblocks `plan` only
  after that one function passes it, which is a code change this probe does not make.
- **Correctness of the vendor's content**, and gate 3 of `docs/inherited/methodology-gates.md`: five
  sector-diverse anchor events triangulated across two sources with a stated tolerance. This probe
  is not a subset or a superset of that gate; it is about one timestamp, and clearing it permits a
  dated backfill and nothing further.

## Running it

```
SEC_CONTACT='Jane Doe jane@example.com' \
  python3 probes/observation_lag/probe.py --days 2026-11-10 2026-11-12
```

No third-party dependency, deliberately: `probes/README.md` records that two probes in this tree
cannot be re-run today because they die on an `import` with no dependency list to install from, and
rule 3 of `CONTRIBUTING.md` keeps a script so its number can be re-derived. The contact address is
read from the environment and never written to the output file, because SEC answers a User-Agent
with no contact with 403 and the address may not reach a committed artefact. The script aborts with
a named code when an input is absent rather than reporting an empty result, which is requirement R6
applied to itself.

---

## 2026-11-13, after review and before the first request: four changes, none to the text above

An adversarial pass over `probe.py` found that four of the registered rules either could not be
reached or guaranteed the thing they existed to prevent. Nothing above is edited; this section is
the disagreement with it, and every change here was made and exercised offline — zero requests —
**before** the probe's first request, which is the only moment at which the choice was still
available.

**1. The registered scalar is the MAX of the per-filing lags, not the p90.** `from_backfill` sets
`observed = valid + seconds`, so the scalar has to cover the WORST sampled lag or every filing
above it is dated before the file that first carried it was written — a point-in-time leak by
arithmetic. The asymmetry argument in "What each outcome will mean" selects the max and the table
then names the p90; this takes the argument. The p90 is still computed, reported and banded, and
decides nothing. Exercised offline on synthetic day-sets: an ordinary two-day shape registers
55,800 s under the max against 52,200 s under the p90, with **2 of 24** sampled filings above the
p90.

**2. `leaks` is 0 by construction under a max, and the report says so instead of presenting it as a
measurement.** The field was `sum(1 for lag in lags if lag > p90)` where p90 is the p90 of those
same lags, so for distinct lags it was pure quantile arithmetic: measured offline, two synthetic
day-sets whose lags differ by ten times both returned `leaks 2/24`, identical, while only `seconds`
moved. Under a max it is 0 and nothing the vendor does can move it. The quantity that CAN move is
reported beside it per day: the count of the day's rows of our form whose index `Date Filed` is not
that index's own day. The field itself keeps the meaning `pit::MeasuredLag` gives it.

**3. A write instant outside the day its file names is a refusal, code
`write_instant_outside_named_day`.** "What this probe cannot settle" names the rewrite question and
then leaves the datum printed and unused, so the realistic outcome space was `{register, permit}`
and no single read could falsify the construction. The allowance is stated rather than tuned: the
named day in `America/New_York` plus 24 hours. Exercised offline: a write instant three days after
the named day aborts, where it previously registered 309,272 s in the "permitted with disclosure"
band; a write at 23:59 the following day registers.

**4. Two boundary corrections and one split code.** `decide` refused on `usable * 2 <= requested`
where the text above refuses when MORE than half did not answer, so 6 usable of 12 refused against
a rule that permits it; it is now `usable * 2 < requested` and 6 of 12 registers, 5 of 12 refuses.
A day whose index holds no row of our form now refuses under its own code `no_rows_of_form` rather
than sharing `sample_too_thin` with "we asked and got nothing back". And ONE of the two vendor
statements is enough: the report names which one it used, because the committed listing fixture
carries the field on no entry at all, so one-served was the expected case and had no row.

Three further repairs with no bearing on the number: the result file is written in a `finally` with
the requests spent and the abort code, so a refused run is still a record of what it cost; a
`--budget` above the registered 30 is refused instead of silently accepted; and the quarter listing
is kept on disk as received, because `collect` wants it as a hashed input FILE and this is the one
request in the tree that fetches it.

## 2026-11-13, the run

```
SEC_CONTACT='<name and address>' python3 -I probes/observation_lag/probe.py \
  --days 2026-11-10 2026-11-12
```

**27 requests of the registered 30**, exit 0, full output in `observation_lag_results.json` beside
this file.

| quantity | value | construction |
|---|---|---|
| registered `seconds` | **94,922 s = 26.37 h** | the MAX of 22 per-filing lags over 2 days, read 2026-11-13 |
| band | **24 h – 7 d** | the band table above, read off the registered scalar |
| `probes` / `leaks` | 22 / 0 | 22 of 24 sampled documents answered with an acceptance field; 0 because the scalar is the max |
| p90, which decides nothing | 57,561 s = 15.99 h | same 22 lags, index `min(floor(0.9 × 22), 21)` ascending; band ≤ 24 h |
| median | 42,085 s = 11.69 h | same 22 lags |
| per day, 2026-11-10 | n=11, median 21,432 s, max **94,922 s** | one write instant, 2026-11-11T03:01:42Z |
| per day, 2026-11-12 | n=11, median 42,085 s, max 57,561 s | one write instant, 2026-11-13T03:02:22Z |
| rows of form `8-K` | 384 of 5,429 and 549 of 7,805 | the two daily indexes as downloaded 2026-11-13 |
| rows whose `Date Filed` is not the index's own day | **0 of 384 and 0 of 549** | the contaminant shape the committed 502-row fixture holds 3 of; the `8-K` population of these two days holds none |
| index size as downloaded | 1,057,192 B and 1,520,327 B | `Content-Length` as read; the vendor's listing states `'1033 KB'` and `'1485 KB'`, rounded to KB and not bytes |

**The p90 rule would have landed in a different band.** 57,561 s is inside 24 h, so the registered
text would have permitted an undisclosed backfill while leaving 2 of 22 sampled filings dated
earlier than the index that carried them. The max does not, and the cost of the difference is one
band: disclosure is now required.

**The one stated assumption is no longer an assumption.** The listing serves `last-modified` as
`11/10/2026 10:01:42 PM` and the `Last-Modified` header the same instant, `0 s` apart on both days.
Had the listing field been UTC the two would have been 5 hours apart and the run would have refused
under `write_instants_disagree`, so the cross-read tested the zone rather than assuming it.

**What the run did not reach.** `write_instant_absent`, the one-statement arm, `no_rows_of_form`,
`sample_too_thin`, `negative_or_zero_lag` and `write_instant_outside_named_day` all stayed closed:
both statements were served on both days, both days carried rows of our form, 11 of 12 documents
answered on each, every lag was positive, and both write instants sat inside the day their file
names. Two documents answered `503` — one per day — and are reported as unusable rather than
dropped. The second run of these same two days, no sooner than 7 days later, is still owed: these
numbers cannot tell a final index file from one rewritten before the probe read it.
