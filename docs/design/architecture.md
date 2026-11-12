# Architecture

**Status:** DRAFT 2026-11-11. The level below `successor_system_design_2026_11_11.md`: that memo
names five services, their boundaries and the eighteen requirements they serve; this document says
what crosses each boundary, in what shape, and where each requirement is actually enforced.

Read the memo first. Every `R`-number here refers to one of its requirements and every requirement
there is paired with the measurement that forced it.

---

## 0. What this document decides, and what it does not

**Decides:** the artefacts that cross service boundaries and their required fields; which boundary
is a file and which is a call; how identity and versioning work, so that any number can name the
inputs that produced it; what each service does when its input is missing, stale or partial; and
for each of the eighteen requirements, the single place it is enforced.

**Defers to the memo:** why there are five services, why the build is parallel to the predecessor,
and the five decisions still open there.

**Excludes on purpose:** the order-placing path and the front-end. Neither is in this repository and
neither is reachable from it. §7 states that as a structural property rather than a policy.

**The test this document is written against.** An architecture is mature when an invariant cannot be
stated without naming where it is enforced. §8 is that table, and it is the part worth attacking
first: three requirements currently have no enforcement point, and they are named.

---

## 1. The five services, in one line each

| service | charter | deployable |
|---|---|---|
| `vault` | Owns every fact we did not derive, and the only conversation with a vendor. | one |
| `derive` | Pure function of (snapshot, rule set). Nothing here reads a clock or a network. | one |
| `modelgate` | The only exit to a language model. Spends money, answers non-deterministically. | one |
| `propose` | Turns a panel into a ranked shortlist and the document a person reads. | one |
| `ledger` | Reads outcomes, which nothing upstream may see. One-way edge. | one |

Statistics stay in Python, inside this repository, on the far side of a file boundary from `derive`
(§3.4). The memo's decision D-4 explains why they are not a sixth service and not a second
repository.

---

## 2. Identity and versioning

Nothing in this system is named by where it came from; everything is named by what it contains.
This is what makes R18 — produce two panels under two rule versions and diff them — a command
rather than an afternoon.

| name | derivation | why not something simpler |
|---|---|---|
| `item_id` | sha256 over the item's canonical form: source id, url, valid time, title, body | A per-process hash is not an identity. The predecessor named typed artefacts with Python's `hash()`, which is randomised per process, so a resumed run could not recognise its own prior output and would have rewritten 5,756 records. |
| `seq` | monotonic integer assigned by the store on commit | **The only ordering authority.** Wall clocks are evidence, never order: a fetch issued at 10:00 that commits at 10:01:10 carries data through 10:00 while one issued at 10:01 has already committed data through 10:01, so ordering by commit time returns the older data as current. |
| `snapshot_id` | sha256 over the ordered list of `item_id` in scope, plus the cutoff | A date range is not reproducible — the archive grows under it. A snapshot is a set. |
| `rule_version` | sha256 of the rules file (§3.3) | Rules as constants in code cannot be diffed or tested. When a hand-written gazetteer wrongly removed a real company, there was no version, no test and no record. |
| `calendar_version` | sha256 of the session calendar, including half-days and DST transitions | A half-day close is a fact that changes results and is easy to get silently wrong. |
| `panel_id` | sha256 over (`snapshot_id`, `rule_version`, `calendar_version`, row set) | A panel that cannot name its inputs cannot be re-derived, and a number whose panel cannot be re-derived is an anecdote. |
| `brief_id` | sha256 over (`panel_id`, scorer version, prompt versions, candidate set) | The proposal a person acted on must be recoverable exactly. |

**Content-addressed blobs.** Raw vendor payloads are stored once under their own digest and
referenced. An unchanged answer costs no storage and, more importantly, does not re-stamp anything
as current — a fresh timestamp on unchanged content is how "latest" starts lying.

---

## 3. The contracts

### 3.1 `Item` — what `vault` stores and publishes

The archive record. Every consumer sees this shape and none sees the storage behind it.

| field | type | why it exists |
|---|---|---|
| `item_id` | content hash | §2 |
| `source` | enum of adapters | provenance of mechanism |
| `source_item_id` | string | the vendor's own id, kept so a vendor row can be traced |
| `provenance` | `live_ingest` \| `vendor_archive` \| `bulk_dataset` | **R10.** A panel mixing them discloses the mix. Two sources with different retrieval models are two different populations and must never be silently pooled. |
| `valid_at` | `ValidTime` | when the world published it |
| `observed_at` | `ObservationTime` | **R2, R3.** Earliest instant we could have known it. A distinct type from `valid_at`; the two cannot be compared. |
| `requested_at`, `received_at` | instants | **R13.** When we asked and when the answer arrived. Separate, because the gap is where stale-but-fresh-looking data comes from. |
| `data_through` | instant | **R13.** The latest fact the payload covers, which is not the same as when it arrived. |
| `committed_at` | instant | **R13.** |
| `seq` | monotonic integer | **R13.** The ordering authority. |
| `valid_precision` | `instant` \| `day` \| `unknown` | A publication time scraped from page metadata is often a date with no hour. An item with `day` precision cannot be assigned to a pre-close window, so the precision must travel with the value rather than being assumed. |
| `valid_disputed` | bool | **R15.** Set when another source carries the same story at a materially earlier time. |
| `earliest_valid_at` | `ValidTime?` | **R15.** The earlier twin's time, carried alongside. Neither value is moved: a disputed date is an **unknown** age, never an old one. |
| `title`, `body`, `url`, `lang` | strings | `body` is article text, not page HTML. **R17.** Superseded 2026-11-12: `body` becomes `body_ref`, §3.9. |
| `raw_blob_ref` | digest | the payload as received, so extraction can be redone when its rules change |
| `vendor_tickers` | list | the vendor's own tagging, **kept and never used for matching** |
| `vendor_duplicate_flag` | bool | the vendor's own dedup verdict, **kept and never filtered on** |

The last two rows are one rule stated twice: a vendor's editorial decisions are recorded as data,
never adopted as ours. Their dedup is unauditable by us; ours is a hashed rule with a version.

**The one visibility query.** `visible_at(cutoff)` is true when `valid_at <= cutoff` **and**
`observed_at <= cutoff`. There is no public way to ask only half of it. That is not fastidiousness:
the half-question was asked in a retrieval test and admitted documents that were not visible at the
claimed instant in 60.2% of 69 probes, while the full question leaked in none.

### 3.2 `Attempt` — the coverage ledger

One row per attempt, written **whether or not anything came back**. This is the difference between
"nothing happened" and "we did not look", and the predecessor could not tell them apart: one day
held 59 items against a median of 181 and nothing recorded which it was.

| field | notes |
|---|---|
| `attempt_id`, `source`, `target_window` | what was asked for |
| `filter_params` | the exact parameters sent, canonicalised — so a later reader knows what the vendor was asked, not what we meant to ask |
| `requested_at`, `received_at`, `transport_status` | |
| `outcome` | `ok` \| `empty` \| `failed` \| `collapsed` |
| `items_returned`, `items_new` | **R16.** A row is written when `items_new` is zero, so poll events are recoverable and not only content changes. Content-hash dedup is right for storage and destroys exactly this. |
| `cost` | vendor units and currency, so the archive knows what it cost |
| `result_digest` | sha256 of the returned id set, so a repeated query's drift is detectable |

`collapsed` is its own outcome: a payload that shrank to a fraction of what the store already holds
is a degraded fetch, not a quieter world. A throttled success looks exactly like a quiet day.

### 3.3 `RuleSet` — what `derive` is a function of

A file, hashed, versioned, and the only place these choices exist. Constants in code cannot be
diffed, tested or attributed.

| group | contents |
|---|---|
| `entity_rules` | corporate-suffix list (complete, no ellipsis), case policy per token count, minimum core length, and three gazetteers: generic name components, non-companies, places |
| `salience_rule` | counting unit (**open: D-8, occurrences or documents**), window definition, baseline length, which order statistic, the floor, and the tie rule — written as code, not prose |
| `calendar_version` | sessions, half-days, DST |
| `panel_rule` | presence conditions, winsorisation cuts, market adjustment |

Each gazetteer entry is a line in a file and therefore a diff. The case that motivated this: a
hand-written gazetteer removed `Premier`, which is a real company and a real counterparty, and
nothing recorded the error because the list lived inside a script.

### 3.4 `Panel` — `derive` to `propose`, and to the statistics tier

An Arrow file plus a manifest. Not a call: the panel is an artefact that outlives the process.

| manifest field | why it is **required** |
|---|---|
| `panel_id`, `snapshot_id`, `rule_version`, `calendar_version` | the panel names every input that made it |
| `cutoff` | the as-of instant; every downstream claim quotes it |
| `rows` | one per (ticker, session) actually **present**, counted — **R8.** Never a product of two lengths. The predecessor's panel was `len(universe) * len(sessions)`, which is a rectangle wearing a count's clothes. |
| `exchangeable_units` | **R9. The count and the definition of the unit.** |
| `design_effect` | measured ratio of the statistic's standard error to the independent-rows formula |
| `within_unit_correlation` | measured, not assumed |
| `attrition` | counted per stage, so a missing row is accounted rather than absent |

**The gate this field set exists for.** The statistics tier **refuses to run** when
`exchangeable_units` is absent. The predecessor computed power as 88.2% from the independent-rows
formula; measured through its own randomization procedure the answer was 27.4%, and on a corrected
archive 10.9%. The formula was not wrong, it answered a different question, and nothing in the
system could tell. A required field and a refusing consumer is the smallest change that makes that
impossible.

`design_effect` and `within_unit_correlation` are required rather than optional for the same reason:
a panel that cannot say how dependent its rows are invites the formula that assumes they are not.

### 3.5 `ModelRequest` / `ModelResponse` — the only exit to a model

| request | notes |
|---|---|
| `prompt_version` | hashed, so an answer can be attributed to the question |
| `state` | the text the model reads |
| `schema` | **text and choice only. The schema cannot express a number, a date or a threshold.** |
| `idempotency_key` | derived from (prompt_version, state digest, schema, draw index) — a retry never pays twice |
| `draw_index` | which of N draws this is |

| response | notes |
|---|---|
| `value`, `span` | the answer and a verified character range in the state that supports it |
| `confidence` | the model's own, recorded and never thresholded without a measurement |
| `generation_id`, `model_build` | the dated build that answered, not the alias that was requested |
| `cost` | charged against a per-run cap that is a hard stop, not a warning |

Two rules are structural rather than advisory. **A model is never asked for a number**: the schema
has no numeric type, so the rule cannot be broken by a careless prompt. And **a bracket condition
never enters a prompt**: filtering by market capitalisation, date or threshold happens afterwards in
code, where it can be tested.

Draw voting lives here, not in callers. The instability is a property of the model, so it is
absorbed at the boundary: N draws, a recorded vote, and the per-draw answers kept. Measured on the
predecessor, name proposal agreed across draws at Jaccard 0.683 while relation typing was unanimous
on 99.1% of pairs — so the unstable step and the stable step are different calls and are metered
separately.

### 3.6 `Brief` — `propose` to a person

| field | notes |
|---|---|
| `brief_id`, `panel_id`, `scorer_version`, `prompt_versions` | full provenance |
| `cutoff` | the as-of instant the proposal stands on |
| `candidates[]` | ranked; each carries the ticker, the counterparty that was in the news, the filing accession and the **character span** in it that disclosed the relationship, and the salience value at `cutoff` |
| `limitations[]` | carried into the document, not left in a memo |

**Fields that must be absent, enumerated so their absence is checkable:** no order, no side, no
quantity, no position size, no entry price, no stop, no ladder. A brief that carries any of them is
rejected by its own schema.

### 3.7 `StreamMessage` — the private item stream (memo decision D-7)

A derived surface on `vault`, never a window onto the outbox.

| field | notes |
|---|---|
| `seq` | the store's ordering authority, so a consumer can order by ours instead of trusting clocks |
| `item_id`, `valid_at`, `observed_at`, `valid_precision`, `source`, `provenance`, and from 2026-11-12 `valid_disputed` with `earliest_valid_at` (§3.10.2) | the consumer makes its own point-in-time cut |
| `title`, `url`, `body_ref` | |
| `schema_version` | the item contract, versioned apart from the storage row. Renamed to `schema` 2026-11-12, §3.10.2 |

Plus a **heartbeat** carrying `seq`, an instant and items-since. Silence must be distinguishable
from a dead stream, because a consumer reading our outage as "no news" turns our failure into theirs.

No delivery guarantee and no latency promise. The archive is the source of truth and reconciliation
is an HTTP read of it; a latency promise would need a freshness number the design does not yet have.

### 3.8 `Reading` — `ledger`, one-way

`ledger` consumes briefs, prices and realised outcomes and produces readings. It holds no edge back
upstream: a service that sees outcomes must not influence the service that produces proposals, or
selection happens under the result. The absence of that edge is enforced in §8, not trusted.

### 3.9 `RawBlob` and `Expiry` — the layer a retention number may name (decided 2026-11-12)

| layer | addressed by | may a number expire it |
|---|---|---|
| L0 item row (§3.1 minus the text) | `item_id` | **never** — it carries R1's completeness claim |
| L1 extracted body | `body_ref` | **not as a setting.** Matching runs over body text (R17), so an expired body cannot be re-matched under a new `rule_version`; a number here reopens memo D-2 instead of configuring it. |
| L2 payload as received | `raw_blob_ref` | **yes.** The cost is re-extraction, and memo §6b says an index timestamp is not an immutable archive, so a re-fetch may not recover it. |

**This changes §3.1**, which holds `body` as a string while §3.7 already carries `body_ref`. `body`
becomes `body_ref`, beside each blob's length and a `present`/`expired` state; a digest is what
proves a restored copy is the same bytes.

**An expiry is its own row type, under the same `seq`, and not an `Attempt`.** An `Attempt` is keyed by
what a vendor was asked (§3.2), so a deletion filed as `outcome=empty` would count as a quiet vendor
inside the reconstruction R16 exists for. `Expiry` carries `blob_digest`, `layer`, `bytes`,
`items_affected`, `policy_version`, `decided_at`, `deleted_at` and `seq`, and `policy_version` hashes a
retention file as §3.3 hashes a rule file — the mechanism by which a later number is a setting. A
consumer meets the absence at the item row's state, not at a failed blob fetch, which is why §5.1 gives
it no code. Only `vault` may delete, and only at L2.

**Measured 2026-11-12 at 42322eb, and no gate proposed.** Two temporary fixtures in `lake.rs`, run and
reverted: a null `body` returns `body == ""` with `unusable_rows == 0`, while dropping the column fails
the whole day as `LakeError::MissingColumn("body")`. The inherited layout has no third state, which is
why L1 and L2 are blobs beside the row. Nor can anything delete yet — `ls services` returns `newsvault`
alone and no `fs::remove_*` call exists in any `*.rs` outside `target/`.

**The ceiling: this settles the layer, not the number.** N4 still needs a count of days for L2 and a
medium for what is kept past it, one number per provenance class (§3.1) being the next smallest
form. Until N1 names the second copy every number is a delete, not a demotion. The memo's §3 charter
bars deleting "to another consumer's budget", which an L2 expiry under its own `policy_version` is not.

### 3.10 Wire formats — two of the eight contracts, decided 2026-11-12

§10's first bullet made serialisation conditional on a second consumer. Counted over §3.1–3.8: `Panel`
has two — `propose`, and the statistics tier whose live half is `probes/panel_power.py`, reading an
ad-hoc `assign.json` + `meta.json` where the panel belongs; its manifest half is `panel-contract`'s
and R8's, not this section's. `Item` has three, from §4's boundary table, none with code and none
specified here. `StreamMessage` has none at all, so §3.10.2 is here on D-7's instruction that the
shape go in from the start, not on §10's trigger. The other five wait on a consumer that has code.

#### 3.10.1 `panel.data/1` — the Arrow data file beside the manifest

Arrow IPC **file** format, one schema, rows sorted by (`session`, `ticker`) so a `panel_id` has one
byte sequence.

| column | Arrow type | null | unit, and who reads it |
|---|---|---|---|
| `ticker` | `utf8` | no | both consumers |
| `session`, `next_session` | `date32` | no | labels in the calendar named by `calendar_version`, never instants. `next_session` is stored rather than re-derived, because a half-day is §2's named way to get this silently wrong |
| `has_relation` | `bool` | no | the statistics tier's control arm. Per row, not per ticker: constant across a ticker's rows today, and a point-in-time relation map makes it vary without a grain change |
| `salient_counterparties` | `list<struct<name: utf8 not null, salience: int64 not null>>` | no; an untreated row carries an empty list | `salience` counts in the unit named by the `salience_unit` metadata key. `propose` takes the counterparty, the tier takes treatment |

`salience` is an integer, never a float that could lose a count (`Shape::Count` in
`panel-contract`). No `present` or `treated` column: presence is the grain, since a row exists only
when both closes are there (R8 in the shape), and treatment is a non-empty list. Three `utf8`
metadata keys: `schema` = `panel.data/1`, so the manifest holds no second copy of the version;
`panel_id`, so a stray pairing is detectable; and `salience_unit` = `occurrences` or `documents`, so
a D-8 flip shows in the data. **What never changes:** one row per (ticker, session) present, so the
manifest's `rows` is this file's row count — the identity §8 row R8's ceiling asks a second reader
for.

#### 3.10.2 `newsvault.stream/1` — the private item stream

One JSON object per WebSocket text frame, never an array, each carrying `type` (`item` or
`heartbeat`) and `schema` = `newsvault.stream/1` — on every frame and not only at connect, so a
reconnecting consumer sees it. An item frame carries §3.7's field list as amended below and nothing
of its own, instants being RFC 3339 UTC `Z`; a heartbeat carries `at`, `items_since` on this
connection, `heartbeat_seconds`, and `seq`, the last the store committed. **What never changes:**
`seq` is the commit order, strictly increasing, never re-used or re-ordered, and gaps are allowed
because D-7 promises no delivery — a gap is closed by the HTTP read, never read as an ordering. **A
ceiling:** the frame carries `heartbeat_seconds` rather than naming one here, no measurement and no
owner decision having produced a number; it is a liveness interval, never N2's freshness target.

**Two amendments to §3.7, made now because they are breaking changes after the first connection:**
`schema_version` becomes the key `schema`, the spelling `newsvault.day/1` and `panel.manifest/1`
already use, and `valid_disputed` with `earliest_valid_at` join the field list, because a consumer
cutting on an undisputed-looking `valid_at` makes exactly the R15 error; `body` against `body_ref`
there is settled by §3.9. The key is `seq`: §2 is where the ordering authority is named, and it is a
different number from the per-invocation `sequence` of the owner's NDJSON convention, which no
format here offers.
**Nothing here is enforced:** no validator exists and nothing produces either artefact, and an unknown
version is refused as `panel-contract` refuses one (`unknown_schema`).

---

## 4. Which boundary is a file, and which is a call

| boundary | form | why |
|---|---|---|
| vendor → `vault` | HTTP, behind one canonical client per vendor | a vendor conversation happens in exactly one place, so quota, retry and cost have one owner |
| `vault` → `derive` | **file**: snapshot manifest + Arrow | `derive` must be re-runnable years later against the same bytes; a call cannot promise that |
| `derive` → statistics | **file**: panel manifest + Arrow | same, and it keeps the Python tier at arm's length |
| `derive`, `propose` → `modelgate` | call, idempotent | it spends money per invocation and must meter |
| `propose` → person | **file**: immutable brief, hashed | the thing someone acted on must be recoverable byte-for-byte |
| `vault` → private stream | WebSocket, best-effort | a notification, never the transport of record |
| `vault` → `vault` consumers | HTTP read for point-in-time queries | the only query surface, so the half-predicate has nowhere to hide |

The pattern: **anything a future reader must reproduce is a file; anything that only has to happen
now is a call.**

---

## 5. Failure semantics

What each service does when its input is wrong, rather than what it does when everything works.

| condition | behaviour | requirement |
|---|---|---|
| a required input file is missing | abort with a named error | **R6.** An empty fallback is forbidden: a missing dictionary once produced an audit result of `0`, indistinguishable from a clean pass. |
| a network call fails | the result type must be handled for the program to compile; exhausted retries are an `Attempt` row with `outcome=failed` | **R5.** Uncaught exceptions once dropped 37 of 1,672 companies silently, and all of them then sat in a control arm as eligible. |
| a vendor returns far less than the store holds | `outcome=collapsed`, not a value | a throttled success looks like a quiet day |
| an item's observation precedes its publication | counted as unusable, never dated | that is a source bug, not an archive fact |
| the same item arrives in several runs | first sighting wins | **R3.** Keeping the last postdates our own knowledge, by up to 19h59m on the predecessor's store. |
| a panel lacks `exchangeable_units` | the statistics tier refuses to run | **R9** |
| a brief carries an order-shaped field | rejected by schema | **R12b** |
| an event is lost | latency, not data: the archive is the source of truth and the hourly sweep closes the gap | memo §4 rule 2 |
| a consumer is silently dead | detected by lag and backlog depth, not by staleness | memo §4 rule 3 |

### 5.1 The error-code registry (decided 2026-11-12)

**What a code is.** The `code` field of the error object a command writes to stderr before exiting
non-zero, and nothing else. The nine rows above are four mechanisms and only one is that object, so
this registry points at the other three rather than naming a condition twice: §3.2's outcome values,
the counted rows of rows 4 and 5, and `panel-contract`'s `Finding.code`.

Eight of the nine rows therefore take no code; row 1 is the one that does. Four belong to services that
do not exist — measured 2026-11-12 at 42322eb, `grep -rl` over `*.rs` outside `target/` matches nothing
for `Attempt`, `collapsed` or `brief`, and no crate depends on an HTTP client — so a code for them would
be §8 row R12a written again. Two are reachable and not errors, an observation before its
publication counted and a repeat resolved (§3.9 owns a third), and the last two are monitoring rows.

| code | exit | the condition, and who can emit it |
|---|---|---|
| `invalid_argument` | 2 | the caller's own mistake, which §5 has no row for. Every CLI |
| `input_absent` | 4 | a required input file is missing (row 1). `vault`, `panel-contract` |
| `day_absent` | 4 | narrower: the store is there, the day is not in it (row 1). `vault` |
| `input_unparseable` | 2 | the file is present and is not what the contract says. `vault`, `panel-contract` |
| `input_schema_mismatch` | 2 | a required column is absent or has an unusable type. `vault` |
| `io_failed` | 2 | a read or write the filesystem refused for another reason. Every CLI |

`day_absent` is separate for the reason the coverage ledger exists; measured 2026-11-12, two states
reach it through one `LakeError::MissingDay`, an absent partition directory and one holding no run
file. Every `retryable` is `no`: nothing here makes a network call, so exit 7 has no member yet.

**One invariant over the exit codes, readable by a test: stdout is non-empty if and only if the exit
status is 0 or 1.** It does not hold today. Measured 2026-11-12 at 42322eb, `panel-contract
validate` on `{}` exits 1 with ten findings on stdout and nothing on stderr, while `newsvault day`
over a 7-byte `.parquet` exits 1 with an empty stdout and `error: EOF: Parquet file too small` on
stderr; each tool also publishes its own `schema` map, `{"1": "failed"}` against `{"1": "not
admissible"}`. **The ceiling:** `1` is undocumented, and the fork is the owner's: either it means
"the answer is no" and the three error classes above move to `2`, changing `newsvault` only, or it
means an error and `panel-contract validate` stops carrying its verdict in the exit status.

---

## 6. The event path

```
vendor  --poll/stream-->  vault: write  --same transaction-->  outbox
                                                                  |
                                                            NOTIFY / stream
                                                                  v
                                                        derive: compute over a CLOSED window
```

Four rules, from the memo, restated here because the contracts depend on them:

1. **An event triggers computation; it never accumulates it.** `derive` stays a pure function over
   a closed window. Incremental salience would make results depend on arrival order.
2. **Write, then emit, in one transaction.** The archive is the source of truth; the event says it
   grew.
3. **The hourly sweep stays**, as a backstop and as the thing that fills the coverage ledger even
   when no event fires.
4. **Salience is a function of a cutoff, not of a day**: `salient(core, T)`. A rolling threshold
   computed from a partial day differs from the same threshold at the close, so every downstream
   claim names its `T`.

Transport is a Postgres outbox with `LISTEN`/`NOTIFY`. The outbox table in the same transaction as
the write gives rule 2 atomically and *is* the replay log. A broker would solve a throughput problem
this system does not have: at a few hundred items a day, one event arrives every few minutes.

---

## 7. What is not reachable from here

The order-placing path is in a different repository, holds the only credentials that matter, and
accepts hand-written documents only. Nothing here holds a credential for it.

**Where the wall is enforced, and where it is not.** Not here. A check in this repository for
order-placing calls would pass unconditionally, because there is no such call to forbid; the
predecessor's equivalent gate guards code that exists. If a mediating service is ever built between
this repository and the executor, that service is where the enforcement belongs, and the condition
is stated in §8 row R12a: its input may not be a brief alone. A mediator that formats a decision a
person made is a convenience. A mediator that derives the decision from a proposal is the producer
the predecessor deleted, returning under a new name. The same rule in the other direction: no backward edge from `ledger`.

Lower latency makes this boundary *more* important, not less. "We must act now" is exactly the
argument that talks a person into automating a pick, and the boundary has to hold at the moment it
becomes inconvenient.

---

## 8. Where each invariant is enforced

The maturity test. An invariant with no enforcement point is a wish, and three of these are wishes
today — named rather than hidden.

| # | invariant | enforced by | status |
|---|---|---|---|
| R1 | archive complete, caps are views | `vault` has no truncating write path; the cap lives in a view builder; §3.9 confines deletion to the L2 blob layer and to `vault` | design |
| R2 | valid and observation time are distinct types | the type system; a `compile_fail` doc test pins it, and CI runs it — `--no-fail-fast` is why, because one failing unit test makes plain `cargo test` exit before the doc test | **enforced, narrowly.** The two instants cannot be compared **with each other**, so the 60.2% leak cannot happen by type confusion. They are not otherwise hidden: `Visibility::valid()`, `observed()` and `Cutoff::inner()` are public, so the publication-only predicate is one line from any crate — demonstrated 2026-11-12 by compiling it. |
| R3 | first sighting wins | unit tests in `newsvault` | **enforced** |
| R4 | coverage ledger beside every store | a write to the archive without an `Attempt` row fails the store's own test | design |
| R5 | failed calls must be handled | `cargo clippy --locked --all-targets -- -D warnings` in CI, over the workspace lint set: `unused_must_use` denied under rust (which also makes a local `cargo check` fail, not merely warn) and `let_underscore_must_use` denied under clippy, which is the only form the move newly catches in CI. Never the type system. | **enforced against the accidental case only** — three escapes measured 2026-11-12: a violation inside a local `macro_rules!` passes; a crate- or module-level `#![allow]` switches any of these off and no command notices; and doc tests are reached by nothing, because `--all-targets` does not cover them. |
| R6 | missing input aborts | unit tests cover the missing root and the missing day. But `lake.rs` `string_list_at` has five `Vec::new()` exits, four of which swallow a real decoding error while `unusable_rows` stays 0 — so the loader does have empty-fallback branches. | **partly** |
| R7 | units are types, identity is a content hash | nothing. `grep -rn item_id services/newsvault/src` returns no match: there is no derivation and no test. Unit newtypes are also absent. | **no enforcement point yet**, both halves |
| R8 | presence counted, never multiplied | `panel-contract`, a workspace member both sides of the panel file boundary can depend on. Six rules over the manifest: the identity and input fields must be present; `exchangeable_units` must carry a count **and** a definition of the unit; the attrition chain must reconcile, cardinality product minus attrition equal to `rows`; `rows` equal to a product of the stated cardinalities — a pair, the whole product, or a single pre-multiplied one — needs an explicit `zero_attrition_counted` sentence; `exchangeable_units.count` equal to `rows` needs a `rows_are_exchangeable` sentence; a count above `rows` is an error with no attestation, because no state of the world matches it. Seven committed fixtures, two of which are the historical case: `1 672 × 47 = 78 584` with no attrition and no unit count is rejected and the product is named, and the recounted `78 460` with 124 dropped rows and 47 sessions as the unit is admitted. **The ceiling:** it reconciles the numbers the manifest states and never opens the Arrow file, so a cardinality of 78 708 with an invented attrition stage of 124 rows is admissible, as is `rows` equal to the product of three of four stated cardinalities with the remainder absorbed by one stage — both measured 2026-11-12. §3.10.1 is what would close it. The attestation check is a length floor: it rules out `"yes"` and `""` and cannot tell a counted check from a confident paragraph, and every message resting on a heuristic says so. | **enforced, within the ceiling stated here** |
| R9 | power from the registered procedure | the statistics tier refuses a panel without `exchangeable_units`; `params_version` names the power method | **no enforcement point yet** |
| R10 | provenance labelled | enum field, non-nullable | design |
| R11 | backfill lag is measured | `from_backfill` requires a `MeasuredLag`, so the **type** cannot be skipped. But all four of its fields are `pub` with no validating constructor, so `{probe_id: "", probes: 0, leaks: 0, seconds: 0}` is accepted — demonstrated 2026-11-12 from outside the crate. And `Visibility` derives `Deserialize` over private fields, which bypasses `from_live`. | **the type is required; the measurement is not** |
| R12a | **no automatic derivation from a proposal to an order** | nothing here, and nothing here is the right answer. Reframed 2026-11-12: the invariant is not "no path". A path is harmless. What the predecessor removed on 2026-09-24 was an automatic brief-to-intent producer, and that is the thing that must not return. A mediating service that **formats a human's decision** into a valid order document is a convenience; one that **derives** which companies and what size from a brief is the deleted thing coming back. The check belongs at that mediator, whose input may not be a `brief_id` alone, and it cannot be written before the mediator exists. | **not this repository's gate** |
| R12b | the brief carries no order-shaped field | the brief schema: no side, quantity, size, entry price, stop or ladder, enumerated as absent in §3.6 so the absence is checkable rather than assumed | design |
| R13 | five time columns, sequence is the authority | store schema; ordering helpers take `seq` only | design |
| R14 | an attempt is recorded even when empty | the four-outcome enum is non-nullable; a test asserts a row on an empty fetch | design |
| R15 | disputed timestamps marked, never corrected | `valid_disputed` + `earliest_valid_at`; a test asserts neither value moves | design |
| R16 | ledger records poll events | `items_new = 0` still writes a row; a test asserts it | design |
| R17 | matching survives a broad corpus, over article body | the rules file is hashed and versioned; the gazetteer regression case is a test | design |
| R18 | two rule versions, one snapshot, a diff | a `derive diff` subcommand; its absence is the gap | design |
| G1 | **a workspace member inherits the lint floor** | a test in the `repo-invariants` crate. It asks `cargo metadata` for the member set rather than reading `workspace.members`, because that list is **not** the member set: measured 2026-11-12, a crate reached only by a `path` dependency is a member, is compiled by `cargo clippy --locked --all-targets`, inherits nothing, and all three CI commands exit 0 over an `unwrap()` in it. The test also rejects `lints = { workspace = true }` written inside `[package]`, which cargo ignores without a warning while the crate passes clippy. It polices its own crate too. | **enforced** |
| E1 | an error code is never renamed, and no code reaches stderr from outside the table | nothing yet. §5.1 specifies the registry; the check is a workspace member `error-codes/` holding `const CODES` beside a committed `codes.snapshot`, a test that every snapshot line survives with the same exit status and retryable flag, and an error constructor taking `&'static Code` so a bare literal does not compile — measured 2026-11-12, the nearest vocabulary that exists is `panel-contract`'s 14 `Finding` codes, which go to stdout and which §5.1 keeps out of this registry; all 14 are bare `&'static str` literals, so a fifteenth needs no table entry and no review. Repurposing is not checkable at all, because no test reads meaning. | **no enforcement point yet** |

Two things this table does not say, and both are measured rather than argued.

**A lint is a floor against the accidental case.** None of R5's survives an author who writes
`#![allow(...)]` at the top of a file, and no command in this gate notices that they did.

**One hole is deliberately nobody's job here.** A crate on disk that no `members` entry and no
`path` dependency reaches is not a member, is compiled by nothing, and is invisible to all of CI —
reproduced 2026-11-12 with `services/derive` holding an `unwrap()` and no registration, all three
commands green. A glob in `members` removes the mistake instead of testing for it, which is a
separate change with its own behaviour to argue about.

**R9 is the one gap left**, and it is the smaller half of the pair R8 belonged to. Its check is a
refusing consumer that does not exist. `design_effect` and `within_unit_correlation` are properties
of a statistic rather than of the row set, so `panel-contract` carries them as `Need::DeferredToR9`
instead of requiring them: the narrowness is machine-readable rather than left to be rediscovered.

---

## 9. Alternatives considered and rejected

| alternative | why not |
|---|---|
| eleven services, one per bounded context | each deployable is another deploy, log, contract version and place something goes quiet. The predecessor has a recorded case where a boundary between components *removed* a standing alert instead of raising one. The eleven contexts are drawn so the seams exist later. |
| Kafka for the event bus | solves throughput we do not have — a few hundred items a day is one event every few minutes — and adds infrastructure a one-person system must then operate. A Postgres outbox is already durable and is itself the replay log. |
| Common Crawl's CC-NEWS as the archive | probed and rejected: 7 of our own 287 items for one test day, 2 of our 17 publishers crawled at all, zero tier-1 US financial outlets in 72,000 records, 4.19 TB for ten months, and no index at all, so no URL lookup and no aimed range request. |
| a single news vendor for both the live stream and the archive | no vendor reviewed does both: the one with a 2014 archive filters on publication date only, and the one documenting an index-time filter has it behind a quoted Enterprise plan. So provenance labelling is necessary rather than avoidable. |
| incremental salience, updated per event | makes the result depend on arrival order, which destroys the determinism the panel contract is built on |
| the statistics tier as a separate repository | the panel handover is where the worst measured error happened, and a test can only span it from inside one tree |
| vendor dedup and vendor entity tags as ours | unauditable editorial decisions. Recorded as data, never adopted. |

---

## 10. What this document leaves open

Two things, both waiting on a number nobody has supplied. Everything else this section once listed
is decided and lives in the section that decides it: wire formats in §3.10 (which also counts the
consumers of the contracts it leaves alone), the error-code registry in §5.1 with its enforcement
point at §8 row E1, and the layer a retention number may name in §3.9.

- **No backup design.** Memo question N1. The archive is called irreplaceable in both documents and
  neither says where its second copy lives. Of everything unresolved here this is the one whose
  absence costs most if it is still absent in a year.
- **No freshness target**, so §5's lag-and-backlog detection has nothing to alert on yet. Memo
  question N2.

Memo question N4 keeps the count of days and the medium for retention; §3.9 settles only the layer.
Which invariants are unenforced is §8's own status column and is not listed again here.
