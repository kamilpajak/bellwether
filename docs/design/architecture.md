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
stated without naming where it is enforced. §8 is that table and it is the part worth attacking
first; its status column, and no sentence anywhere, is the count of what is enforced.

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

Nothing is named by where it came from; everything is named by what it contains, with one exception
stated below: a dedup key must survive a correction of the contents, so `story_id` names the vendor
row. This is what makes R18 — two panels, two rule versions, diffed — a command, not an afternoon.

**The hazard that cheapness creates:** a new `rule_version` costs a command and nothing counts how
many were tried against one snapshot — the postmortem's Pattern 6, 26 tests with Bonferroni at n=2
where true n was ≥ 15 (`docs/inherited/paradigm_failures_postmortem.md:173`), and there a rule
change was at least a countable commit. The smallest count: a registration field on the manifest and
an append-only ledger of the pairs a `derive diff` compared; whether it gates is the owner's.

| name | derivation | why not something simpler |
|---|---|---|
| `item_id` | sha256 over the item's canonical form: `source`, `source_item_id`, `url`, `valid_at`, `title`, `body_ref` | A per-process hash is not an identity. The predecessor named typed artefacts with Python's `hash()`, which is randomised per process, so a resumed run could not recognise its own prior output and would have rewritten 5,756 records. This is the **content** witness and nothing else: a re-stamped time or an edited title is a new `item_id` under one `story_id`, which is how "the bytes changed" is visible at all. |
| `story_id` | sha256 over (`source`, `source_item_id`) — the vendor's own identity, the one part of a row a re-stamp does not touch | **The dedup key (§5 row 5) and R15's join key**, which `item_id` cannot be: it hashes `valid_at` and `title`, the two fields under dispute, so a re-stamp would mint a fresh item instead of a disputed twin and R15 would have nothing to attach to. Measured 2026-11-12 over the lake §10 describes: of the 501 ids that appear in more than one partition, all 501 keep one url throughout, and **every one of §10's 1,225 repeat appearances carries a publication time that id did not have before** — median span between an id's first and last stamp 4 days 15:16, worst 159 days. |
| `seq` | monotonic integer assigned by the store on commit | **The only ordering authority.** Wall clocks are evidence, never order: a fetch issued at 10:00 that commits at 10:01:10 carries data through 10:00 while one issued at 10:01 has already committed data through 10:01, so ordering by commit time returns the older data as current. Not built: `grep -rnw seq` over the crates returns nothing, and `newsvault day` instead sorts by (publication time, id) — presentation only, so no consumer may read that array order as commit order. |
| `snapshot_id` | sha256 over the canonical form of (the list of `item_id` in scope in ascending `seq`, the cutoff) | A date range is not reproducible — the archive grows under it. A snapshot is a set. The list and the cutoff are also fields of the manifest (§3.0), because neither can be read back out of a digest. |
| `rule_version` | sha256 of the rules file (§3.3) | Rules as constants in code cannot be diffed or tested. When a hand-written gazetteer wrongly removed a real company, there was no version, no test and no record. |
| `calendar_version` | sha256 of the session calendar, half-days and DST included. **Its own file, beside the rules file and not a group inside it** (§3.3), so a calendar edit and a rule edit are two diffs rather than one — which is what R18 needs | A half-day close is a fact that changes results and is easy to get silently wrong. |
| `universe_version` | sha256 of the roster the panel's tickers are drawn from, as of the snapshot's cutoff, with the delisted names either included or named as absent | **Survivorship, which the predecessor measured in both directions:** 18 tickers added retrospectively on 2026-04-19 contributed about 0.3 of an OOS α t and a PIT-95 sub-universe moved that t from 1.70 to 1.36; including delisted M&A targets flipped a train α t from +0.80 to −0.86 (`docs/inherited/paradigm_failures_postmortem.md:64` and `:67`). Without the roster as a named input a panel is reproducible and survivorship-biased at once, and `panel_id` hashes it happily. Enforcement: §8 row U1. |
| `relation_map_version` | sha256 of the relation map `derive` reads (§4) | `has_relation` is the statistics tier's control arm and a model build sets it. Without this input a flipped control arm cannot be attributed to the build that flipped it, which `brief_id` can already do for prompts. |
| `panel_id` | sha256 over (`snapshot_id`, `rule_version`, `calendar_version`, `universe_version`, `relation_map_version`, row set) | A panel that cannot name its inputs cannot be re-derived, and a number whose panel cannot be re-derived is an anecdote. The row set means §3.10.1's rows in canonical form, not the Arrow file's bytes. Measured 2026-11-12 over one four-row panel conforming to §3.10.1, two counterparties in its first row: arrow-rs 60.1.0 writes 2,578 bytes and pyarrow 24.0.0 writes 2,050 of the same rows, and splitting them into two record batches makes 2,634 — so the container cannot be the digest input. Those three sizes hold for that panel's own strings and metadata and no fixture is committed, so a reader measuring a four-row panel of their own reproduces the **disagreement** and not the sizes: pyarrow 24.0.0 over one built to this table on 2026-11-12 gives 2,202 in one batch against 2,786 in two. The disagreement is the claim. |
| `brief_id` | sha256 over (`panel_id`, scorer version, prompt versions, candidate set) | The proposal a person acted on must be recoverable exactly. |

**Canonical form, the one convention every digest above takes** — two implementations of one digest
rule being the defect the parity run already paid for (memo §5.1: a mismatch on all 156 days, a
locale sort against code-point order). Fields are rendered UTF-8 and NFC-normalised and joined with
`0x1f`, no trailing separator; a list joins its members with `0x1e` in the order its own field
states; an instant is RFC 3339 UTC `Z` at microsecond precision beside its `valid_precision` (§3.1),
so a `day` value and an `instant` value cannot collide; an absent optional field contributes an empty
string; every string ordering is over raw UTF-8 bytes, never a locale's collation. Nothing derives
any of these digests yet (§8 row R7), so the first producer lands with one committed test vector per
digest — what makes two implementations disagree loudly.

**Content-addressed blobs.** Raw vendor payloads are stored once under their own digest and
referenced. An unchanged answer costs no storage and, more importantly, does not re-stamp anything
as current — a fresh timestamp on unchanged content is how "latest" starts lying.

---

## 3. The contracts

### 3.0 `Snapshot` — `vault` to `derive`, the reproducibility boundary (decided 2026-11-12)

The manifest beside the Arrow file §4 names, and `derive`'s primary input, so an unstated field here
is a guess in the next service to be built. The Arrow half carries §3.1's rows and has no wire format
yet, on §3.10's trigger: `derive` has no code.

| field | type | why it exists |
|---|---|---|
| `schema` | `snapshot.manifest/1` | a reader refuses a version it does not know rather than reading it optimistically (§3.10.2) |
| `snapshot_id` | content hash | §2 |
| `cutoff` | instant | the visibility bound: every listed item satisfies `visible_at(cutoff)` (§3.1). It is inside `snapshot_id` and therefore unreadable from it, which is why it is also a field |
| `items` | count | counted, never the length of a date range — §2's reason a range is not a snapshot |
| `item_ids` | list of `item_id`, ascending `seq` | the ordered list `snapshot_id` digests. **`seq` is the ordering authority (§2) and nothing assigns one yet**, so no snapshot can be produced today; two implementations ordering by `item_id` bytes and by `seq` would otherwise digest one set twice |
| `seq_high_water` | integer | the last `seq` the store had committed when the cut was taken, so a reader can tell a prefix of the archive from a filtered subset of it |
| `provenance_counts` | one count per §3.1 `provenance` value, summing to `items` | **R10**, the disclosure clause. This is the mix of the items; a panel's own mix is a different number and has no field (§8 row R10) |
| `calendar_version` | content hash | §2. The calendar the cutoff was read against, so "the close of session `d`" is reproducible |

### 3.1 `Item` — what `vault` stores and publishes

The archive record. Every consumer sees this shape and none sees the storage behind it.

| field | type | why it exists |
|---|---|---|
| `item_id` | content hash | §2 |
| `story_id` | hash over the vendor row's identity | §2. The key **first sighting wins** (§5 row 5) and R15's detection both join on, because `item_id` moves when a disputed field is corrected |
| `source` | enum of adapters | provenance of mechanism |
| `source_item_id` | string | the vendor's own id, kept so a vendor row can be traced |
| `provenance` | `live_ingest` \| `vendor_archive` \| `bulk_dataset` | **R10.** A panel mixing them discloses the mix. Two sources with different retrieval models are two different populations and must never be silently pooled. |
| `valid_at` | `ValidTime` | when the world published it |
| `observed_at` | `ObservationTime` | **R2, R3.** Earliest instant we could have known it. A distinct type from `valid_at`; the two cannot be compared. |
| `requested_at`, `received_at` | instants | **R13.** When we asked and when the answer arrived. Separate, because the gap is where stale-but-fresh-looking data comes from. |
| `data_through` | instant | **R13.** The latest fact the payload covers, which is not the same as when it arrived. |
| `committed_at` | instant | **R13.** |
| `seq` | monotonic integer | **R13.** The ordering authority. |
| `valid_precision` | `instant` \| `day` \| `unknown` | A publication time scraped from page metadata is often a date with no hour. An item with `day` precision cannot be assigned to a pre-close window, so the precision is the **shape** of `pit::datedness::Datedness` rather than a column read beside the value: a `day` time is carried as a `NaiveDate` and no constructor folds it into an instant. |
| `valid_disputed` | bool | **R15.** Set when another `item_id` under the same `story_id` carries a materially earlier time — whether that is another source or the same source re-stamping its own row, which is the case the archive actually holds (§2, 501 ids). |
| `earliest_valid_at` | `ValidTime?` | **R15.** The earlier twin's time, carried alongside. Neither value is moved: a disputed date is an **unknown** age, never an old one. |
| `title`, `url`, `lang` | strings | |
| `body_ref` | digest, plus the blob's length and a `present` \| `expired` state | **R17.** Article text, never page HTML, and held as a blob beside the row (§3.9) rather than in it, so an L1 expiry is a state a consumer can read instead of a failed fetch. |
| `raw_blob_ref` | digest | the payload as received, so extraction can be redone when its rules change |
| `vendor_tickers` | list | the vendor's own tagging, **kept and never used for matching** |
| `vendor_duplicate_flag` | bool | the vendor's own dedup verdict, **kept and never filtered on** |

The last two rows are one rule stated twice: a vendor's editorial decisions are recorded as data,
never adopted as ours. Their dedup is unauditable by us; ours is a hashed rule with a version.

**The one visibility query.** `visible_at(cutoff)` is true when `valid_at <= cutoff` **and**
`observed_at <= cutoff`. That is not fastidiousness: the half-question was asked in a retrieval test
and admitted documents not visible at the claimed instant in 60.2% of 69 probes, while the full
question leaked in none. How far the type system goes towards making the half-question unaskable,
and where it stops, is §8 row R2's and nowhere else's.

A `day`-precision item answers it through its own arm: visible once the cutoff passes the end of that day
**in the calendar the item names**, never UTC midnight, which is 19:00 in New York and admits the afternoon.
An undated item is visible at no cutoff. The arm has one producer, the lake reader, which reads a
publication value stamped exactly at midnight as a date, the predecessor's lake having no precision
column: measured 2026-11-13 over its 779 run files and 243,226 rows, **7,767 rows (3.19%)** are stamped
`00:00:00.000000` and for source `perplexity` it is **7,141 of 7,141**, every row. Read as instants those
were visible from 00:00Z of their own day, so `newsvault day --at` returned them up to a day early.

### 3.2 `Attempt` — the coverage ledger

One row per attempt, written **whether or not anything came back**. This is the difference between
"nothing happened" and "we did not look", and the predecessor could not tell them apart: one day
held 59 items against a median of 181 and nothing recorded which it was.

| field | notes |
|---|---|
| `attempt_id`, `source`, `target_window` | what was asked for |
| `filter_params` | the exact parameters sent, in §2's canonical form — so a later reader knows what the vendor was asked, not what we meant to ask |
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
| `salience_rule` | six keys, each a value in the file and not an expression to evaluate: `counting_unit` (**D-8's, open**), `window_sessions`, `baseline_sessions`, `order_statistic`, `floor` (counted in `counting_unit`, so **D-8's too**), `tie_rule`. **Salience** is how often a counterparty's **core** — the name token a match is made on — occurs in the window ending at T, in `counting_unit`; the core is salient when that count is at or above `floor` and at or above the `order_statistic` of its own baseline |
| `panel_rule` | `presence`: which (ticker, session) rows exist at all, and nothing else — that is the whole of what `derive` decides about the panel's shape. The winsorisation cuts and the market adjustment are the statistics tier's, below |

Each gazetteer entry is a line in a file and therefore a diff. The case that motivated this: a
hand-written gazetteer removed `Premier`, which is a real company and a real counterparty, and
nothing recorded the error because the list lived inside a script.

**The file, what `rule_version` digests, and the two keys still blank.** One JSON object, keys sorted,
LF endings, one trailing newline; `rule_version` is sha256 of those bytes as committed, so a
whitespace edit is a new version — wrong in the safe direction and needing no second canonicalisation.
Gazetteers and the calendar are their own files (§2), named by path and digest so one `rule_version`
still covers the set. The two `null`s are D-8's and only D-8's; every other value is the example's
own, the shape being what is contracted:

```json
{"schema": "rules/1",
 "entity_rules": {"suffixes": "suffixes.txt@sha256:3f2a…", "min_core_chars": 4, "case_policy": {"1": "exact", "2+": "fold"}},
 "salience_rule": {"counting_unit": null, "window_sessions": 1, "baseline_sessions": 60, "order_statistic": "q90", "floor": null, "tie_rule": "name_ascending_bytes"},
 "panel_rule": {"presence": "close_on_session_and_next"}}
```

**Where the winsorisation cuts and the market adjustment live.** Not in the rule set: both act on a
return and §3.10.1's panel carries none, while `probes/panel_power.py` already holds both as
literals beside its floors, alpha, draw counts and seed. Their home is that tier's own hashed params
file, whose digest — `params_version`, named here and nowhere else — goes beside every power number.

### 3.4 `Panel` — `derive` to `propose`, and to the statistics tier

An Arrow file plus a manifest, and not a call: the panel outlives the process. The data file is
§3.10.1. **The manifest's field list is not here**; it is published by the crate that checks it —
`panel-contract schema --format=json` names every field, whether it is always required, and why. The
cost is that a reader of this section runs a binary; what it buys is one list, which the document
can neither over-promise nor under-report.

**Why the field set exists**, which no field list carries. The predecessor computed power as 88.2%
from the independent-rows formula; measured through its own randomization procedure the answer was
27.4%, and on a corrected archive 10.9%. The formula answered a different question and nothing could
tell. Hence `rows` is **counted**, never multiplied (the predecessor's panel was
`len(universe) * len(sessions)`, a rectangle wearing a count's clothes), and `exchangeable_units`
carries three keys — `count`, `unit`, `definition` — the unit's name alone not saying what made two
exchangeable. A required field is half the fix; the other half is a consumer that **refuses** a panel
lacking it, and that does not exist: `panel-contract` requires the field (§8 row R8) while the
statistics code reads an ad-hoc `assign.json` + `meta.json` and counts its own units (row R9).

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
| `outcome` | §3.2's four values, `ok` \| `empty` \| `failed` \| `collapsed`, for the reason the coverage ledger has them: a call that failed and a call that found nothing must not arrive as the same answer. Memo §1.5 is that defect costing 37 of 1,672 companies, and R5 is satisfied by a handler that writes an empty answer |
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
predecessor, proposing which names are in play disagreed from draw to draw while typing the relation
between two named companies did not — so the unstable step and the stable step are different calls,
metered separately. The two figures are deliberately not quoted: their construction sits in the
predecessor's archive, which D-4 leaves behind, and a number whose construction a reader cannot
reach is an anecdote. They belong here once re-measured on a real run.

### 3.6 `Brief` — `propose` to a person

| field | notes |
|---|---|
| `brief_id`, `panel_id`, `scorer_version`, `prompt_versions` | full provenance |
| `cutoff` | the as-of instant the proposal stands on |
| `candidates[]` | ranked; each carries the ticker, the counterparty that was in the news, the filing accession and the **character span** in it that disclosed the relationship, and the salience value at its session's T (§3.10.1) |
| `limitations[]` | carried into the document, not left in a memo |

**Fields that must be absent, enumerated so their absence is checkable:** no order, no side, no
quantity, no position size, no entry price, no stop, no ladder. A brief that carries any of them is
rejected by its own schema.

### 3.7 `StreamMessage` — the private item stream (memo decision D-7)

A derived surface on `vault`, never a window onto the outbox.

| field | notes |
|---|---|
| `seq` | the store's ordering authority, so a consumer can order by ours instead of trusting clocks |
| `item_id`, `valid_at`, `observed_at`, `valid_precision`, `source`, `provenance` | the consumer makes its own point-in-time cut |
| `valid_disputed`, `earliest_valid_at` | **R15.** A consumer cutting on a `valid_at` that looks undisputed makes exactly the R15 error, so the dispute travels with the time |
| `title`, `url`, `body_ref` | |
| `schema` | the item contract, versioned apart from the storage row |

Plus a **heartbeat** carrying `seq`, an instant and items-since. Silence must be distinguishable
from a dead stream, because a consumer reading our outage as "no news" turns our failure into theirs.

No delivery guarantee and no latency promise. The archive is the source of truth and reconciliation
is an HTTP read of it; a latency promise would need a freshness number the design does not yet have.

### 3.8 `Reading` — `ledger`, one-way

`ledger` consumes briefs, prices and realised outcomes and produces readings. It holds no edge back
upstream: a service that sees outcomes must not influence the service that produces proposals, or
selection happens under the result. **Where that is enforced: nowhere yet, and nowhere here.** No
`ledger` exists (§3.9's service count), so a check here would pass unconditionally — §7's argument in
the other direction. The enforcement point is `ledger`'s own test, under the condition that nothing
it produces is an input to `derive` or `propose`.

### 3.9 `RawBlob` and `Expiry` — the layer a retention number may name (decided 2026-11-12)

| layer | addressed by | may a number expire it |
|---|---|---|
| L0 item row (§3.1 minus the text) | `item_id` | **never** — it carries R1's completeness claim |
| L1 extracted body | `body_ref` | **not as a setting.** Matching runs over body text (R17), so an expired body cannot be re-matched under a new `rule_version`; a number here reopens memo D-2 instead of configuring it. |
| L2 payload as received | `raw_blob_ref` | **yes.** The cost is re-extraction, and memo §6b says an index timestamp is not an immutable archive, so a re-fetch may not recover it. |

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
form. N1 is decided (§10), so an L2 expiry demotes to the second copy rather than deleting — but
only once that copy has been restored once, which §10 records as undone. The memo's §3 charter bars
deleting "to another consumer's budget", which an L2 expiry under its own `policy_version` is not.

### 3.10 Wire formats — two of §3.0–3.8's nine contracts, decided 2026-11-12

**The trigger for writing a wire format down: a second consumer that exists in code.** With one
consumer a format is that consumer's own serialisation; with two it is a contract, and the second is
where an unstated assumption becomes a defect. Counted over §3.0–3.8: `Panel` has two — `propose`,
and the statistics tier whose live half is `probes/panel_power.py`, reading an ad-hoc `assign.json` +
`meta.json` where the panel belongs. `Item` has three (§4's boundary table), none with code.
`StreamMessage` has none, so §3.10.2 is here on D-7's instruction that the shape go in from the
start, not on the trigger. `Snapshot` has one, `derive`. The other five wait.

**Nothing in §3.10 is enforced:** no validator exists for either format and nothing produces either
artefact, so both subsections specify rather than check.

#### 3.10.1 `panel.data/1` — the Arrow data file beside the manifest

Arrow IPC **file** format, one schema, rows in ascending `session` then ascending `ticker` as raw
UTF-8 bytes. That is a canonical row **order**, not a canonical file: `panel_id` digests the rows in
canonical form (§2), the Arrow bytes being a writer's and a chunking's.

| column | Arrow type | null | unit, and who reads it |
|---|---|---|---|
| `ticker` | `utf8` | no | both consumers |
| `session`, `next_session` | `date32` | no | labels in the calendar named by `calendar_version`, never instants. `next_session` is stored rather than re-derived, because a half-day is §2's named way to get this silently wrong |
| `has_relation` | `bool` | **yes, and a null means unresolved** | the statistics tier's control arm, read from the relation map (§4) and never from `derive`'s own work. Per row, not per ticker: constant across a ticker's rows today, and a point-in-time map makes it vary without a grain change. A null row is dropped or stratified, **never counted as a control**: an extraction that failed and a company with no relation were one value in the predecessor, which is how 37 of 1,672 companies sat in the control arm as eligible (memo §1.5) |
| `salient_counterparties` | `list<struct<name: utf8 not null, salience: int64 not null>>` | no; an untreated row carries an empty list | `salience` counts in the unit named by the `salience_unit` metadata key, at T — and **T is the close of `session`** in the calendar named by `calendar_version`, while the manifest's single `cutoff` is the snapshot's visibility bound and not the treatment cut, so no row sees past its own session close. That forbids an intraday panel, which is the choice being made. The list is ordered by descending `salience`, ties by `name` ascending: unstated, it is a third way one row set digests twice. `propose` takes the counterparty, the tier takes treatment |

`salience` is an integer, never a float: it counts occurrences or documents and a float can lose one.
No `present` or `treated` column: presence is the grain, a row existing only when both closes are
there (R8 in the shape), and treatment is a non-empty list. Three `utf8` metadata keys: `schema` =
`panel.data/1`, so the manifest holds no second copy of the version; `panel_id`, so a stray pairing
is detectable; `salience_unit` = `occurrences` or `documents`, so a D-8 flip shows in the data.
**What never changes:** one row per (ticker, session) present, so the manifest's `rows` is this
file's row count — the identity §8 row R8's ceiling asks a second reader for.

#### 3.10.2 `newsvault.stream/1` — the private item stream

One JSON object per WebSocket text frame, never an array, each carrying `type` (`item` or
`heartbeat`) and `schema` = `newsvault.stream/1` — on every frame and not only at connect, so a
reconnecting consumer sees it, the key spelled as `newsvault.day/1` and `panel.manifest/1` spell it.
An item frame carries §3.7's field list and nothing of its own, instants being RFC 3339 UTC `Z`; a
heartbeat carries `at`, `items_since` on this connection, `heartbeat_seconds`, and `seq`, the last
the store committed. **What never changes:** `seq` is the commit order, strictly increasing, never
re-used or re-ordered, with gaps allowed because D-7 promises no delivery — a gap is closed by the
HTTP read, never read as an ordering. **A ceiling:** the frame carries `heartbeat_seconds` rather
than naming one here, no measurement and no owner decision having produced one; it is a liveness
interval, never N2's freshness target.

`seq` is §2's number, not the per-invocation `sequence` of the owner's NDJSON convention, which no
format here offers. A reader is to refuse an unknown `schema` value rather than read it
optimistically, as `panel-contract` refuses one (`unknown_schema`).

---

## 4. Which boundary is a file, and which is a call

| boundary | form | why |
|---|---|---|
| vendor → `vault` | HTTP, behind one canonical client per vendor | a vendor conversation happens in exactly one place, so quota, retry and cost have one owner |
| `vault` → `derive` | **file**: snapshot manifest + Arrow | `derive` must be re-runnable years later against the same bytes; a call cannot promise that |
| `derive` → statistics | **file**: panel manifest + Arrow | same, and it keeps the Python tier at arm's length |
| `propose` → `modelgate` | call, idempotent | it spends money per invocation and must meter |
| `modelgate` → `derive` | **file**: the relation map, hashed and versioned (§2) | `has_relation` comes from a model and `derive` reads no network (§1), so the map crosses as an artefact and not a call. Each edge carries the filing accession and span behind it (§3.6) and the model build that produced it, and is valid from an instant, because a map that is not point-in-time dates a relation to before it was disclosed. The field list is not written: `modelgate` has no code and §3.10's trigger has not fired |
| `propose` → person | **file**: immutable brief, hashed | the thing someone acted on must be recoverable byte-for-byte |
| `vault` → private stream | WebSocket, best-effort | a notification, never the transport of record |
| `vault` → `vault` consumers | HTTP read for point-in-time queries. **No code yet:** `Cargo.lock` holds no HTTP client, and the surface that exists is `newsvault day`, a CLI whose `--at` is optional — with no `--at` it returns the whole partition and applies no visibility test | a point-in-time query is the only read a consumer should need, so there is one place to get the predicate right |

The pattern: **anything a future reader must reproduce is a file; anything that only has to happen
now is a call.**

---

## 5. Failure semantics

What each service does when its input is wrong, rather than what it does when everything works.

| condition | behaviour | requirement |
|---|---|---|
| a required input file is missing | abort with a named error | **R6.** An empty fallback is forbidden: a missing dictionary once produced an audit result of `0`, indistinguishable from a clean pass. |
| a network call fails | the `Result` is handled, or the build is red — in two of the four ways of discarding one. `f();` fails `cargo check`; `let _ = f()` fails clippy; `let _named = f()` and `drop(f())` are silent under every lint in the floor (row R5). Exhausted retries are an `Attempt` row with `outcome=failed` | **R5.** Uncaught exceptions once dropped 37 of 1,672 companies silently, and all of them then sat in a control arm as eligible. |
| a vendor returns far less than the store holds | `outcome=collapsed`, not a value | a throttled success looks like a quiet day |
| an item's observation precedes its publication | counted as unusable, never dated | that is a source bug, not an archive fact |
| the same item arrives in several runs | first sighting wins, keyed by `story_id` (§2) and never by `item_id`, which moves when a publication time or a title is corrected | **R3.** Keeping the last postdates our own knowledge, by up to 19h59m on the predecessor's store. |
| a panel lacks `exchangeable_units` | `panel-contract validate` calls the manifest not admissible. The statistics tier is *to* refuse to run and does not: it reads an ad-hoc assignment file, not the manifest | **R8** today, **R9** for the refusal |
| a brief carries an order-shaped field | rejected by schema | **R12b** |
| an event is lost | latency, not data: the archive is the source of truth and the hourly sweep closes the gap | memo §4 rule 2 |
| a consumer is silently dead | detected by lag and backlog depth, not by staleness; the threshold on the age of the newest item is §10's N2 | memo §4 rule 3 |

### 5.1 The error-code registry (decided 2026-11-12)

**What a code is.** The `code` field of the error object a command writes to stderr before exiting
non-zero, and nothing else. §5's nine rows are four mechanisms and only one is that object, so this
registry points at the other three rather than naming a condition twice: §3.2's outcome values, the
counted rows of rows 4 and 5, and `panel-contract`'s `Finding.code`. Eight rows therefore take no
code and row 1 is the one that does. Four belong to services that do not exist — measured 2026-11-12,
`grep -rlwE 'Attempt|Collapsed|Brief'` over `*.rs` outside `target/` matches nothing (word-bounded:
the bare words appear in comments) and no crate depends on an HTTP client — so a code for them would
be §8 row R12a again. Two are reachable and not errors (§3.9 owns a third), the last two monitoring.

`exit` is the contract: what a caller may branch on and a new command must implement. Where a
binary does something else today the cell says so, measured 2026-11-12 against both.

| code | exit | the condition, and who can emit it |
|---|---|---|
| `invalid_argument` | 2 | the caller's own mistake, which §5 has no row for. Every CLI. Alone among them it prefixes its stderr line `usage error:` rather than `error:`, so a caller matching text sees two shapes |
| `input_absent` | 4 | a required input file is missing (row 1). `vault`, `panel-contract` |
| `day_absent` | 4 | narrower: the store is there, the day is not in it (row 1), separate for the reason the coverage ledger exists. `vault`. Measured 2026-11-12, two states reach it through one `LakeError::MissingDay`: an absent partition directory, and one holding no run file |
| `input_unparseable` | 2 — **`newsvault` returns 1** | the file is present and is not what the contract says. `vault`, `panel-contract` |
| `input_schema_mismatch` | 2 — **`newsvault` returns 1** | a required column is absent or has an unusable type. `vault` |
| `io_failed` | 2 — **`newsvault` returns 1** | a read or write the filesystem refused for another reason. Every CLI |

Those three bold cells are one defect, `newsvault` mapping every non-usage failure onto `1`, and are
the diff the fork below must close. Every `retryable` is `no`: nothing here makes a network call, so
exit 7 has no member yet.

**One invariant over the exit codes, readable by a test: stdout is non-empty if and only if the exit
status is 0 or 1.** It does not hold. Measured 2026-11-12, `panel-contract validate` on `{}` exits 1
with ten findings on stdout and nothing on stderr, while `newsvault day` over a 7-byte `.parquet`
exits 1 with an empty stdout and `error: EOF: Parquet file too small` on stderr. **The ceiling:** `1`
carries two meanings and both are published, `{"1": "failed"}` against `{"1": "not admissible"}`. The
fork is the owner's: either `1` means "the answer is no" and the three bold cells move to `2`,
changing `newsvault` only, or it means an error and `validate` stops carrying its verdict there.

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
this repository and the executor, that service is where the enforcement belongs, under the condition
§8 row R12a states. The same shape in the other direction: no backward edge from `ledger`, enforced
at `ledger` when it exists (§3.8).

Lower latency makes this boundary *more* important, not less. "We must act now" is exactly the
argument that talks a person into automating a pick, and the boundary has to hold at the moment it
becomes inconvenient.

---

## 8. Where each invariant is enforced

The maturity test. An invariant with no enforcement point is a wish, and the wishes are named rather
than hidden. **The status column is the count** — no sentence anywhere repeats it as a number,
because a repeated count is a count that goes stale.

**How to read the status column.** `design` means the enforcement point is specified and the code it
names does not exist, so that row's `enforced by` cell says what *will* check the invariant, not what
does. Where a row's clauses differ, its status splits with them.

| # | invariant | enforced by | status |
|---|---|---|---|
| R1 | archive complete, caps are views | `vault` has no truncating write path — vacuously, because it has no write path at all: measured 2026-11-12, every `File::create` and `ArrowWriter` under `services/newsvault/src` is inside `mod tests`, so the archive reader is read-only and nothing yet forbids a writer that truncates. The cap lives in a view builder; §3.9 confines deletion to the L2 blob layer and to `vault` | design |
| R2 | valid and observation time are distinct types | the type system; a `compile_fail` doc test in `pit/src/lib.rs` pins it, and CI runs it — `--no-fail-fast` is why, because one failing unit test makes plain `cargo test` exit before the doc test. The `compile_fail` block is **paired with a positive control**, a normal doc test carrying the same `use pit::{…}` line, because a `compile_fail` block passes whenever its snippet stops compiling for *any* reason: measured 2026-11-13, after `pit` moved out of `newsvault` into its own member, leaving the old `use newsvault::pit::{…}` line in both blocks left the `compile_fail` test reporting `ok` while the control failed with `error[E0433]: cannot find module or crate newsvault`. A `compile_fail,E0308` annotation does not close it — rustdoc reports `ok` for a snippet that fails with a different code than the one declared. The hole rustdoc cannot see is the control being deleted, or left importing something that still compiles, which returns the pair to passing vacuously; two unit tests in the same file read the module header and assert that its first fenced block is the `compile_fail` one and that its second carries the same `use` line | **enforced, narrowly.** The two instants cannot be compared **with each other**, so the 60.2% leak cannot happen by type confusion. They are not otherwise hidden: `Visibility::valid()`, `observed()` and `Cutoff::inner()` are public, so the publication-only predicate is one line from any crate — demonstrated 2026-11-12 by compiling it. |
| R3 | first sighting wins, **within a partition** | unit tests in `newsvault`. **The key is the lake's `id` column as received** — §3.1's `source_item_id`, which is what §5 row 5's `story_id` would be a hash of; nothing derives a `story_id` yet (row R7), so the rule is only as good as the vendor's id. A blank one is unusable since 2026-11-12 rather than a dedup key: measured through the binary, three rows whose `id` was a real empty string came back as **one** item with `unusable_rows` 0 and two publication-time disagreements, two stories gone, and now come back as three unusable rows and no item. `probes/parity_baseline.py` rejected that row already, so the two readers meant to agree were two rules until that day; both now reject an empty or blank id. The grain is the invariant's other missing half and it changes the count: `newsvault day` collapses repeats across the run files of one UTC day and nothing collapses across days, which is why the archive holds 61,282 distinct ids and the per-day counts sum to 62,507 (§10). A store whose grain is first sighting **across the archive** is a different store and would need the earliest observation of all 156 partitions, not of one. | **enforced at the partition grain** |
| R4 | a **coverage ledger** (`Attempt`, §3.2) beside every store | a write to the archive without an `Attempt` row fails the store's own test | design |
| R5 | failed calls must be handled | `cargo clippy --locked --all-targets -- -D warnings` in CI, over the workspace lint set: `unused_must_use` denied under rust (which also makes a local `cargo check` fail, not merely warn) and `let_underscore_must_use` denied under clippy, which is the only form the move newly catches in CI. Never the type system. | **enforced against two of the four ways to discard a `Result`.** Measured 2026-11-12 in a throwaway crate carrying this lint table verbatim: `let _named = f();` and `drop(f());` leave `fmt`, `clippy --all-targets -- -D warnings`, `cargo check` and `cargo test` all at exit 0 with no mention of either line, while the controls go red — `let _ = f()` gives clippy 101 (`non-binding let on an expression with #[must_use] type`) and a bare `f();` gives `cargo check` 101 (`unused Result that must be used`). No lint in the set closes `drop()`. **Two further opt-outs**, these deliberate: a crate- or module-level `#![allow]` switches any of these off and no command notices, which the three test modules in this tree use; and doc tests are reached by nothing, because `--all-targets` does not cover them. A local `macro_rules!` is **not** a third for the two lints this row rests on: measured 2026-11-12 in the same throwaway crate, `let _ = f()` inside one still gives clippy 101 and a bare `f();` inside one still gives `cargo check` 101. It is an escape for the floor's other half, row G1's — `unwrap()`, `expect()`, `panic!()` and `todo!()` written inside a local macro leave clippy at exit 0. |
| R6 | missing input aborts | unit tests cover the missing root, the missing day and, since 2026-11-12, the `tickers` column. It was the one column fetched as an `Option` while the other seven went through `MissingColumn`, so `string_list_at` had six empty exits — five literal `Vec::new()` and one `.unwrap_or_default()` — and an absent or undecodable column became an empty tag list on every row with `unusable_rows` at 0, which is the empty fallback this row forbids. It now reads like the others: measured that day through the binary, a run file with no `tickers` column exits 1 with `column tickers missing from run file` and one whose `tickers` is a `struct<sym: utf8>` exits 1 with `column tickers has unusable type Struct("sym": Utf8)`, where both exited 0 and returned `"tickers": []` before. One empty exit is left and it is the legitimate one, a null cell meaning the vendor tagged nothing. What is still an empty fallback is outside the reader's list handling: a null string cell is read as an empty string, §3.9's measured pair being a null `body` returning `body == ""` with `unusable_rows == 0` against a dropped `body` column failing the whole day. | **partly** |
| R7 | units are types, identity is a content hash | **the hash half: the `canon` crate.** §2's canonical form has one implementation, all seven digests are taken over it, and 15 committed vectors under `canon/vectors/` state input against expected digest so a reader in another language disagrees out loud rather than silently. Two defects found by review 2026-11-13 and closed before the first vector was committed: the form did not name its own digest, so `filter_params` over one literal parameter and `story_id` over a source and an id were the same two fields and the same sha256; and a value holding one of the three separators forged a field boundary, so the one field `a\u{1f}b` and the two fields `a` and `b` had one form — that value is now refused. **Units are not types**: nothing in the tree carries a basis point or a standard deviation. | **enforced for identity as far as a producer exists, absent for units.** No stored row carries an `item_id` yet, because no store exists: `grep -rn canon:: --include='*.rs'` outside the crate returns nothing, so what is enforced is that the derivation is one rule with committed vectors, not that anything has been identified by it |
| R8 | presence counted, never multiplied | `panel-contract`, a workspace member both sides of the panel file boundary can depend on. Six rules over the manifest: the identity and input fields must be present; `exchangeable_units` must carry all three of the keys §3.4 names, each raising its own finding; the attrition chain must reconcile, cardinality product minus attrition equal to `rows`; `rows` equal to a product of the stated cardinalities — a pair, the whole product, or a single pre-multiplied one — needs an explicit `zero_attrition_counted` sentence; `exchangeable_units.count` equal to `rows` needs a `rows_are_exchangeable` sentence; a count above `rows` is an error with no attestation, because no state of the world matches it. Seven committed fixtures, two of which are the historical case: `1 672 × 47 = 78 584` with no attrition and no unit count is rejected and the product is named, and the recounted `78 460` with 124 dropped rows and 47 sessions as the unit is admitted. **The ceiling, four limits, each run on 2026-11-12 rather than reasoned.** (1) It reconciles the numbers the manifest states and never opens the Arrow file, so over the committed `corrected_panel_78460` fixture a single cardinality of 78 708 with an invented attrition stage of **248** rows is admissible — `panel-contract validate --format=json` exits 0 with zero findings. (The arithmetic is the whole check, so it has to be done: 78 708 − 248 = 78 460 = `rows`. The same cardinality with a 124-row stage is *refused*, `attrition_does_not_reconcile`.) (2) Equally admissible: `rows` equal to the product of three of four stated cardinalities with the remainder absorbed by one stage — built as rows = 30 = 2 × 3 × 5 with a fourth cardinality of 7 and one stage absorbing 180, exit 0, zero findings. §3.10.1 is what would close both. (3) Rule e fires on **exact equality only**, so `exchangeable_units.count` = 78 459 over 78 460 rows passes with no attestation — one unit below "every row is its own unit", which is the same independence claim; measured, exit 0, against `units_equal_rows_unattested` at 78 460. A ratio threshold would close it and needs its own construction first. (4) The attestation check is a length floor, and so is the `definition` check: both rule out `"yes"` and `""` and neither can tell a counted check from a confident paragraph. Every message resting on a heuristic says so. **What runs the validator**, which until 2026-11-12 was nothing at all: `git grep` over `*.yml`, `*.py`, `*.sh` and `*.rs` at the commit before found no caller outside the crate's own source, so the rules were tested and the command was not. Since that day an integration test runs the compiled binary over all seven committed fixtures on every `cargo test`, asserting that the exit status is the verdict, that the binary's findings are the library's, and that the fixture table is the whole directory — measured by returning exit 0 from `validate` on a manifest with findings, which left all 21 library tests green and turned that test red. Neither end of the file boundary runs it yet: `probes/panel_recount.py` still writes `assign.json` + `meta.json` and `probes/panel_power.py` still reads them, which is row R9's work and needs §3.4 to name a layout first. | **enforced, within the ceiling stated here** |
| R9 | power from the registered procedure | nothing refuses a panel yet, and the gap is smaller than "a consumer that does not exist". `probes/panel_power.py` already runs the registered permutation, prints the measured standard error against the independent-rows formula and their ratio, prints a Monte-Carlo error per estimate, and derives its own exchangeable-unit count. What it does not do is read the manifest: its inputs are an ad-hoc `assign.json` + `meta.json`, so it has nothing to refuse. The check is that script taking a panel manifest instead, and aborting when `exchangeable_units` is absent. The gap is also narrow in a machine-readable way: `design_effect` and `within_unit_correlation` are properties of a statistic rather than of the row set, so `panel-contract` carries them as `Need::DeferredToR9` rather than requiring them, which tells a reader of the crate which half is deferred instead of leaving it to be rediscovered. | **no enforcement point yet** |
| R10 | provenance labelled, **and a mix disclosed** | two clauses and only the first has an enforcement point. Labelling: the enum field, non-nullable (§3.1). Disclosure: `provenance_counts` on the snapshot manifest (§3.0) is the mix of the items, and nothing produces it; a panel's own mix — the one that matters, since the panel is where pooling happens — has no field at all, in the 13-field manifest list row U1 counts or anywhere else. The pair (`provenance`, `source`) is the stronger subject and waits on the two EDGAR body medians being reconciled (§9), so it is not written as a number here | labelling **design**; disclosure **no enforcement point yet** |
| R11 | backfill lag is measured | `from_backfill` requires a `MeasuredLag`, so the **type** cannot be skipped, and since 2026-11-12 deserialisation cannot skip it either: `Visibility` deserialises through `#[serde(try_from)]` over a private wire struct, so a pair read from a file runs the same ordering check as `from_live`. Before that the derived `Deserialize` wrote straight onto the private fields and bypassed **both** constructors — measured from outside the crate, `{"valid":"2026-07-01T12:00:00Z","observed":"2026-06-01T00:00:00Z"}` was accepted with `lag()` at −2,635,200 s, the state §5 row 4 forbids, and it is now refused with `ObservedBeforeValid`. §4 makes vault-to-derive a file boundary and §3.10.2 specifies a JSON stream carrying both instants, so that path is a consumer's normal constructor rather than an exception. **The measurement is still not required**, in two ways: all four `MeasuredLag` fields are `pub` with no validating constructor, so `{probe_id: "", probes: 0, leaks: 0, seconds: 0}` is accepted with `leak_rate()` returning `None`; and a lag of 0 s puts the observation at the publication instant, so a backfilled item *can* claim visibility on its own publication day, which memo R11 says no constructor allows. What a minimum lag should be is not decided, so nothing refuses it. | **the type is required; the measurement is not** |
| R12a | **no automatic derivation from a proposal to an order** | nothing here, and nothing here is the right answer. Reframed 2026-11-12: the invariant is not "no path". A path is harmless. What the predecessor removed on 2026-09-24 was an automatic brief-to-intent producer, and that is the thing that must not return. A mediating service that **formats a human's decision** into a valid order document is a convenience; one that **derives** which companies and what size from a brief is the deleted thing coming back. The check belongs at that mediator, whose input may not be a `brief_id` alone, and it cannot be written before the mediator exists. | **not this repository's gate** |
| R12b | the brief carries no order-shaped field | the brief schema, over the field list enumerated in §3.6 — one home, because the enumeration *is* the check and two copies of it are two different checks | design |
| R13 | five time columns, sequence is the authority | store schema; ordering helpers take `seq` only | design |
| R14 | an attempt is recorded even when empty | the four-outcome enum is non-nullable; a test asserts a row on an empty fetch | design |
| R15 | disputed timestamps marked, never corrected | `valid_disputed` + `earliest_valid_at`, neither of which exists yet; a test asserts neither value moves. The detection does have code, at a narrower grain than §2's: `lake.rs` keeps the first sighting's publication time when another run of the same UTC day reports a different one for the same id, and counts the disagreement twice over — `valid_time_conflict_events` per row and `items_with_conflicting_valid_time` per id, because one item re-reported in five later runs is five events and one item. Three tests assert both counters since 2026-11-12, one of them over the path that counted nothing before that day: a known id arriving with an *earlier* observation replaced the kept item silently. That path is taken 0 times in the archive's 243,226 rows and arrives with a second ingest schedule or a backfill (§3.1 `provenance`). **The size of the population R15 handles, in both units**, with the construction of each figure and the decomposition that ties them together: memo §5.1, re-derived through both readers on 2026-11-12 and not copied here. The §2 detection, which joins twins on `story_id`, has no code | **the handling is design; the detection is counted in two units and both are asserted** |
| R16 | the **coverage ledger** records poll events | `items_new = 0` still writes an `Attempt` row; a test asserts it | design |
| R17 | matching survives a broad corpus, over article body | the rules file is hashed and versioned; the gazetteer regression case is a test | design |
| R18 | two rule versions, one snapshot, a diff | a `derive diff` subcommand; its absence is the gap | design |
| G1 | **a workspace member inherits the lint floor** | a test in the `repo-invariants` crate. It asks `cargo metadata` for the member set rather than reading `workspace.members`, because that list is **not** the member set: measured 2026-11-12, a crate reached only by a `path` dependency is a member, is compiled by `cargo clippy --locked --all-targets`, inherits nothing, and all three CI commands exit 0 over an `unwrap()` in it. The test also rejects `lints = { workspace = true }` written inside `[package]`, which cargo ignores without a warning while the crate passes clippy. It polices its own crate too. **The floor's contents, gated since 2026-11-12.** Until that day the row checked that members inherit the table and nothing checked what the table said: measured on a copy of this tree, deleting `unwrap_used = "deny"` from the root and adding an `unwrap()` to a member left `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings` (not one line of it mentioning `unwrap`) and `cargo test --locked --no-fail-fast` all at exit 0, this row's test included, and the failure message would have printed a floor the root no longer held. The check now reads `[workspace.lints]` from the root manifest, builds that message from what it read instead of from a constant in the gate crate, refuses an entry at `warn` or `allow` because such an entry decorates rather than binds, and compares the set with `repo-invariants/lint-floor.snapshot`. The same mutation now exits 101 naming the entry. **Why a snapshot and not a derived expectation:** every expectation derived from the manifest disappears together with the entry it was meant to protect, so a deleted lint is only visible against a statement of the floor that lives outside the manifest. The snapshot is that statement and the one deliberate second copy of the list; editing it is the change, and the message says so. **The hole it deliberately leaves:** a crate on disk that no `members` entry and no `path` dependency reaches is not a member, is compiled by nothing and is invisible to all of CI — reproduced 2026-11-12 with `services/derive` holding an `unwrap()` and no registration, all three commands green. A glob in `members` removes the mistake instead of testing for it, which is a separate change with its own behaviour to argue about. | **enforced** |
| U1 | **the panel names the roster it was built from** | nothing. `universe_version` (§2) has no producer and no consumer: measured 2026-11-12, `panel-contract schema --format=json` lists 13 manifest fields and not one is a roster, and `panel-contract validate panel-contract/fixtures/corrected_panel_78460.json` exits 0 with zero findings over a manifest that names no universe. The same 13 lack `relation_map_version` too, so §2 names five inputs to `panel_id` where the gate requires three. The enforcement point is those fields becoming required in the crate, which also rewrites seven committed fixtures and R8's rule set — a decision, not an omission | **no enforcement point yet** |
| E1 | an error code is never renamed, and no code reaches stderr from outside the table | nothing yet for the stderr codes of §5.1; the check there is a workspace member `error-codes/` holding `const CODES` beside a committed `codes.snapshot`, a test that every snapshot line survives with the same exit status and retryable flag, and an error constructor taking `&'static Code` so a bare literal does not compile. **The renaming half is already closed for the one vocabulary that exists.** `panel-contract` publishes 15 `Finding` codes on stdout, which §5.1 keeps out of this registry, and each is asserted by at least one test: measured 2026-11-12 by renaming each code in the production half alone, all 15 turn a named test red (`cargo test -p panel-contract --lib` exits 101, 20 passed and 1 failed, for every one). So `error-codes/` is mostly the **add** path — a sixteenth code still needs no table entry and no review. Repurposing is not checkable at all, because no test reads meaning. | **no enforcement point yet** for §5.1's codes |

---

## 9. Alternatives considered and rejected

| alternative | why not |
|---|---|
| eleven services, one per bounded context | each deployable is another deploy, log, contract version and place something goes quiet. The predecessor has a recorded case where a boundary between components *removed* a standing alert instead of raising one. The eleven contexts are drawn so the seams exist later. |
| Kafka for the event bus | solves throughput we do not have — a few hundred items a day is one event every few minutes — and adds infrastructure a one-person system must then operate. A Postgres outbox is already durable and is itself the replay log. |
| Common Crawl's CC-NEWS as the archive | probed and rejected: 7 of our own 287 items for one test day, 2 of our 17 publishers crawled at all, zero tier-1 US financial outlets in 72,000 records, 4.19 TB for ten months, and no index at all, so no URL lookup and no aimed range request. |
| **collecting the news ourselves** — publisher RSS, EDGAR, or crawling the open web — instead of buying it | **Not rejected, and not decided either; the row is here so the default is a choice rather than a silence.** The pre-registered usability bar is a median article body of 2,000 characters, because R17 matches over body text (`probes/vendor_probe/PREREGISTRATION.md`, Q3). Against it, the predecessor's own RSS path has a median body of **117** characters — short of the bar by more than an order of magnitude, so RSS alone is a snippet feed whatever it costs. Its EDGAR path clears the bar on either of the two figures the probes carry, and they disagree: `panel_recount.py` and memo §1.9 say 50,622 characters, `matcher_audit.py` says 14,039, and neither can be re-derived today (both scripts die on an uninstallable import, `probes/README.md`). So "in-house" has to name its sources before it can be costed: EDGAR-only, RSS and crawling the open web have different bodies, different volumes and different costs, and CC-NEWS in the row above is the cheap version of the third. This row does **not** pick — build-or-buy is the owner's, open alongside D-1. |
| a single news vendor for both the live stream and the archive | no vendor reviewed does both: the one with a 2014 archive filters on publication date only, and the one documenting an index-time filter has it behind a quoted Enterprise plan. So provenance labelling is necessary rather than avoidable. |
| incremental salience, updated per event | makes the result depend on arrival order, which destroys the determinism the panel contract is built on |
| the statistics tier as a separate repository | the panel handover is where the worst measured error happened, and a test can only span it from inside one tree |
| vendor dedup and vendor entity tags as ours | unauditable editorial decisions. Recorded as data, never adopted. |

---

## 10. What this document leaves open

Nothing of the shape. The two numbers this section last asked for are recorded below; what remains
open is the memo's own list, where decision D-1 (which news vendor) blocks the rest.

**The archive's second copy: the VPS, with the provider's own backup behind it (N1, decided
2026-11-12 by the owner).** Two parts of that stay unanswered and neither is the shape of the
system, so neither blocks it: nobody has restored the backup and nobody has timed a restore. A
backup nobody has restored is not yet a backup; one timed drill, recorded once, closes both.

**What the drill compares, and three things not true of it.** `probes/parity_baseline.py` computes a
sha256 over the sorted distinct **vendor ids**, the lake's `id` column (§3.1's `source_item_id`).
(1) **Not** over `item_id` (§2), which nothing derives — `grep -rn item_id` over the Rust sources
returns nothing, row R7 — so it shows the id **set** came back and would match with every title, body
and url different. (2) It agreed on **156 of 156** days, re-derived 2026-11-12, over a corpus holding
none of the shapes the two readers can disagree on: across 779 run files and 243,226 rows, zero null
ids, zero empty-string ids, zero null publication or ingest times, zero inverted rows, no file
missing `tickers`. So `unusable_rows` and §5's rows 4 and 5 have never fired on real data, and the
number says the lake is clean, not that two readers encode one rule. (3) **No comparator** exists:
the probe writes a baseline nothing reads, `newsvault` emits no digest field, and the two sides were
compared by hand; the gate `probes/README.md` claims needs it committed, one run file per shape.

**The risk the owner accepted, stated rather than implied.** Both copies sit in one provider and one
account, so a disk, a kernel or a bad deployment is covered and a billing, suspension or credential
loss is not. A copy at a second provider was the alternative and was not taken.

**Freshness: 2 hours inside the US session, 6 hours outside it (N2, measured 2026-11-12).** The
alert is the age of the newest item, per §5's "a consumer is silently dead" row. Measured over the
predecessor's lake — 156 partitions, 779 run files, 243,226 raw rows, publication times from
2026-05-19 to 2026-11-10 — as gaps between consecutive publication instants over **61,282** distinct
items, the union of distinct `id` values across the archive, a gap being a property of the archive
and not of a partition. Memo §5.1's **62,507** is the same archive counted per partition and summed,
double-counting 1,225 repeats of 501 ids (row R3's grain); both re-derived 2026-11-12 in one pass.

| window | p50 | p90 | p99 | p99.9 | worst |
|---|---|---|---|---|---|
| 13:00–20:00 UTC, the US session | 1.0 min | 10.1 min | 26.7 min | 50.3 min | **1.2 h** |
| outside it | 0.3 min | 13.0 min | 45.0 min | 105.1 min | 332.6 h |

**What would have fired is not what that table says.** A gap's quantile is bucketed by the hour it
**starts**, while the alert asks whether the newest item's age is over the threshold at an instant
inside the window — so a gap starting 12:10 UTC and running to 15:00 is an out-of-session gap and an
in-session alert. `probes/news_gap.py` now prints both, measured 2026-11-12: **2 h in
session fires four times** (2026-05-19 → 06-02, 332.6 h; 06-02 → 06-03, 16.8 h; 06-03 12:10 → 15:00,
**2.8 h**; 06-05 → 06-08, 93.2 h) and 6 h outside fires three, those same episodes without the
2.8-hour one. All four start outside the session, which is why gap-start bucketing reports zero
there, and the control reproduces that zero. **The threshold choice survives**: three of the four are
outages the 6-hour rule catches anyway, so 2 hours adds one alert in 175 days, 2026-06-03's.

**What makes those two numbers wrong.** They measure the predecessor's **retrieval** coverage, not
the world's news, so a different vendor moves them: provisional on D-1, to be re-measured against the
first real vendor. The publication gap transfers where an observation gap would not, the
predecessor's `ingested_at` spacing being its scheduler rather than the news. The record also holds
outages already — 161 of the 175 days hold an item, 14 hold none, and the 332.6-hour gap is one.
