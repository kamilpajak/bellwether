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
| `seq` | monotonic integer assigned by the store on commit | **The only ordering authority.** Wall clocks are evidence, never order: a fetch issued at 10:00 that commits at 10:01:10 carries data through 10:00 while one issued at 10:01 has already committed data through 10:01, so ordering by commit time returns the older data as current. Assigned since 2026-11-13 from a committed `seq_reservations` row, and read since the segment export: `publish::read_day` returns a publication day's rows through one ordering helper whose key is the `seq` column, across every segment of the partition. Mutation 2026-11-13: keying that helper on `committed_at` turns `the_ordering_helper_sorts_on_seq_and_leaves_no_row_behind` red. `newsvault day --lake` still sorts by (publication time, id) because the predecessor's lake has no `seq` — presentation only, so no consumer may read that array order as commit order. |
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
string; every string ordering is over raw UTF-8 bytes, never a locale's collation. Three of these
digests have a producer since 2026-11-13 — `attempt_id` and `filter_params`, both taken by
`news-provider`'s `AttemptSlot::open`, and `result_digest`, taken by `newsvault ledger` over the item
ids one attempt committed — and the other four do not (§8 row R7), each landing with
one committed test vector, which is what makes two implementations disagree loudly.

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
| `item_ids` | list of `item_id`, ascending `seq` | the ordered list `snapshot_id` digests. **`seq` is the ordering authority (§2)** and the store has assigned one since 2026-11-13, so the list is producible and nothing produces it: `selection_id` has no caller and `derive` has no code; two implementations ordering by `item_id` bytes and by `seq` would otherwise digest one set twice |
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
| `valid_disputed` | bool | **R15.** Set on the ARRIVING row when another `item_id` under the same `story_id` already carries a **different** publication value — whether that is another source or the same source re-stamping its own row, which is the case the archive actually holds (§2, 501 ids). Not "materially" different: a materiality bar is a number, this project has no measurement that would place it, and an unplaced bar is a threshold nobody can defend. Setting it on the arriving row and never on the twin is what keeps the stored row byte-identical. |
| `earliest_valid_at` | `ValidTime?` | **R15.** The earliest publication value the store holds for this `story_id`, carried alongside on the disputed row and absent when nothing disagrees. Neither value is moved: a disputed date is an **unknown** age, never an old one. |
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
alone and no `fs::remove_*` call exists in production code; measured 2026-11-13, the two in the tree
are both inside `store.rs`'s `mod tests`, where one deletes a stored blob to check that an item row
naming a file that is not there is refused.

**One row of a daily index is not dated by that index.** Measured on the same capture: the
2026-11-10 index carries four distinct `Date Filed` values — 20261102, 20261103, 20261105 and
20261110 — so a late submission appears in a later day's index and the index's own day is not the
row's filing date. The column is `YYYYMMDD`, not ISO.

**The file count this layout produces, which is the shape of the failure it has to avoid.** Two
blobs per committed item, not one per attempt: the raw submission `.txt` as received (§3.9's L2,
`raw_blob_ref`) and the extracted article body (L1, `body_ref`) are disjoint and land under separate
digests. Measured 2026-11-13 on window 2026-11-12: **916** distinct blobs — **458** distinct raw
payloads and **458** distinct bodies — against **503** attempts (1 index, ~502 document, 458 of them
committed and ~44 lost to the transient failures R19's no-margin pace leaves), with 918 files on disk
because two more are the daily index payload. The blob count running far above the attempt count is
exactly the observation the earlier one-blob-per-attempt projection named as its own falsifier, so
the two-blob count above is the model. The per-day order of magnitude is the in-universe filing
count: re-derived 2026-11-13 from the real `form.20261110.idx` (1,057,192 bytes, captured through
the predecessor's own SEC client) against the 7,992 distinct CIKs of its `company_tickers.json` of
2026-05-29, **5,429** filing rows, **391** of them 8-K or 8-K/A, and **347** of those with a CIK in
the roster — a floor, the roster being five months old. So about 916 blob files a run and, at 252
session days, about **231,000** a year, which is **3.5** files per directory across the two-level
65,536-directory fan-out (230,832 / 65,536). The predecessor of this shape reached 558,235 loose
files holding 5,197 current values because content addressing dedups identical BYTES and its
payload's last bar was still forming, so 98.7% of what it stored were exact copies. Here the
immutable half is the filing document at a fixed accession URL, which dedups to zero new files on a
re-fetch; the moving tail is the daily index, which grows through the day and mints one blob per
fetch, bounded by the number of runs in a day and not by the number of items. **What is still
unmeasured** is the index re-fetch rate across a whole day; the observation that would now surprise
this paragraph is one filing document whose repeated fetches carry more than one digest.

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
| vendor → `vault` | HTTP, behind **one owner for every vendor conversation** and not one client per vendor | a vendor conversation happens in exactly one place, so quota, retry and cost have one owner. Widened 2026-11-13 when the first adapter shipped: `providers/edgar-press` declares no HTTP dependency at all and the single `ureq` agent sits behind `news-provider`'s `Wire`, which is the only shape in which "a fetch outside a ledger row does not compile" holds in every crate rather than in the ones that do not own an agent. Per-vendor differences are a `TransportProfile` the provider states, since 2026-11-13 including the call budget and the bucket it counts against (row R19) |
| `vault` → `derive` | **file**: snapshot manifest + Arrow | `derive` must be re-runnable years later against the same bytes; a call cannot promise that |
| `derive` → statistics | **file**: panel manifest + Arrow | same, and it keeps the Python tier at arm's length |
| `propose` → `modelgate` | call, idempotent | it spends money per invocation and must meter |
| `modelgate` → `derive` | **file**: the relation map, hashed and versioned (§2) | `has_relation` comes from a model and `derive` reads no network (§1), so the map crosses as an artefact and not a call. Each edge carries the filing accession and span behind it (§3.6) and the model build that produced it, and is valid from an instant, because a map that is not point-in-time dates a relation to before it was disclosed. The field list is not written: `modelgate` has no code and §3.10's trigger has not fired |
| `propose` → person | **file**: immutable brief, hashed | the thing someone acted on must be recoverable byte-for-byte |
| `vault` → private stream | WebSocket, best-effort | a notification, never the transport of record |
| `vault` → `vault` consumers | HTTP read for point-in-time queries. **No code yet, and three CLI commands instead:** `newsvault day`, whose `--at` is still optional — with no `--at` it returns the whole partition and applies no visibility test — and, since 2026-11-13, `newsvault ledger` and `newsvault publish`, whose `--at` is REQUIRED, so a read with no cutoff does not parse. The client `Cargo.lock` has held since 2026-11-13 is outbound and in one crate: `ureq 3.5.2` under `news-provider`, behind `Wire` (row R4) | a point-in-time query is the only read a consumer should need, so there is one place to get the predicate right |

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
| an item's observation precedes its publication | **stored** as `pit::datedness::Datedness::Undated`, visible at no cutoff, with its evidence beside it: the raw blob, the observation, and the refused publication value in `items.valid_refused`. The lake reader still counts such a row unusable, having no blob to keep the evidence in | that is a source bug, not an archive fact — but discarding the row loses a real document to it, and `Undated` is the shape that holds one. Measured 2026-11-12 over 40 partitions of the predecessor's lake, 13,074 rows, **0** are in this state, so the corpus cannot say how often it happens |
| the same item arrives in several runs | first sighting wins, keyed by `story_id` (§2) and never by `item_id`, which moves when a publication time or a title is corrected | **R3.** Keeping the last postdates our own knowledge, by up to 19h59m on the predecessor's store. |
| a panel lacks `exchangeable_units` | `panel-contract validate` calls the manifest not admissible. The statistics tier is *to* refuse to run and does not: it reads an ad-hoc assignment file, not the manifest | **R8** today, **R9** for the refusal |
| a brief carries an order-shaped field | rejected by schema | **R12b** |
| a count is taken over a partition's files | not a count. The partition key is the **publication day** (decision D3, §10), so a backfill appends a second segment into a months-old partition and no partition is ever closed: measured 2026-11-13, a second run exporting one late item leaves two segment files in one `valid_date=` directory, both current. Every count is therefore a `seq`-bounded query over the commit log, and `ledger` reports `unpublished_seq_ranges` so the rows no segment carries are a number rather than a silence | **R13**, and decision D3 |
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
| `input_unparseable` | 2 | the file is present and is not what the contract says. `vault`, `panel-contract` |
| `input_schema_mismatch` | 2 | a required column is absent or has an unusable type. `vault` |
| `io_failed` | 2 | a read or write the filesystem refused for another reason. Every CLI |
| `roster_absent` | 4 | the in-universe roster is missing or names no CIK. `vault`'s `collect`, raised before the first request |
| `credential_contact_missing` | 2 | the User-Agent carries no contact address. A startup refusal, never a vendor event in the coverage ledger |
| `vendor_unreachable` | 7 | nothing committed and every failure transient. `vault`'s `collect`, retryable **yes** |
| `partial_collect` | 8 | something committed and something failed, abandoned or not asked for |
| `vendor_refused_all` | 8 | nothing committed and at least one PERMANENT failure. Retryable **no**: 7 is the retryable class and a total refusal must not invite a timer to hammer it |
| `commit_failed` | 8 | the store refused an attempt's rows, with that attempt named in the envelope |

Those three cells read `2` since 2026-11-13, when the fork below was taken by default as decision D1
(§10): `newsvault` maps every non-usage failure onto `2` and `1` is "the answer is no" and nothing
else. One test pins it, the way the defect was measured — a 7-byte parquet at a day partition's path
exits 2, and putting that arm back on 1 turns
`a_day_whose_parquet_cannot_be_read_exits_two_and_never_one` red. Two codes joined the table with
that commit: `lease_held` at **7**, retryable yes, emitted by `newsvault publish` when another run
holds the flock; and the incomplete report at **8**, which carries its result on stdout and is not
an error object at all — `ledger` writes the days it could read and names the run envelopes it could
not. Every other `retryable` is `no`: no command here makes a network call,
so exit 7 had no member before `publish`. `news-provider`'s `Wire::send` can since 2026-11-13, but no CLI reaches
it, and its two refusals (a missing credential, a failed conversation) are types rather than codes
until `error-codes/` exists (row E1).

**One invariant over the exit codes, readable by a test: stdout is non-empty if and only if the exit
status is 0, 1 or 8** — the companion rule D1 carries, 8 having joined the set with `ledger`'s
incomplete report. **It is one function since 2026-11-13**, `news_provider::Exit::carries_the_result_on_stdout`,
which `collect` branches on and `stdout_carries_the_result_for_exactly_the_exits_zero_one_and_eight`
asserts over every arm of the exit table rather than over the two a run usually takes; returning
`true` unconditionally turns that test red. It covers `collect` alone: `day`, `ledger` and `publish`
still hold the rule by hand.
Measured 2026-11-12, `panel-contract validate` on `{}` exits 1 with ten findings on stdout and
nothing on stderr, while `newsvault day` over a 7-byte `.parquet` exited 1 with an empty stdout and
`error: EOF: Parquet file too small` on stderr; that second case is the one D1 moved, and it exits 2
with an empty stdout today. **What is left of the ceiling:** `1` still carries two sentences, now
reading the same way — `{"1": "the answer is no"}` here and `{"1": "not admissible"}` there — and
nothing checks the iff clause across the two binaries, which is a test over both schemas that does
not exist.

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
| R1 | archive complete, caps are views | `vault` has a write path since 2026-11-13, so this row stopped being vacuous that day. **What is checked is one call.** Every byte the store writes goes through `blobs.rs`'s `BlobStore::put`: it returns the standing digest when the final path is already there, and otherwise creates a staging path with `File::create_new`, writes, fsyncs the file, renames it, and fsyncs the directory holding the new name. `std::fs::File::create` **truncates** and `create_new` refuses — measured 2026-11-13, a 10-byte file rewritten through `File::create` is 2 bytes afterwards while `create_new` on the same path answers `AlreadyExists`, which is why the text grep proposed for this row would not have covered the API the crate uses. Two tests pin it and both ran red before the rule existed: with `File::create` in place of `create_new` a put over a staged file returns `Ok` and the staged bytes are gone, and without the standing-path check the stored file's inode moves (227,299,778 to 227,299,796). **The second creating call is the same shape since 2026-11-13.** `publish.rs`'s `Segment::create_new` is the only constructor of a segment: it refuses a final path that exists (`SegmentExists`) and stages through `File::create_new`, so a staged file a crash left behind is `StagingExists` and its bytes survive. Mutations that day, each turning a named test red: `File::create` in place of `create_new` leaves the staged sentinel truncated to 0 bytes and answers `Ok`, which fails `changing_the_one_segment_constructor_to_file_create_turns_the_write_gate_red`; dropping the final-path check fails `a_second_write_to_an_existing_published_path_fails_with_a_named_error`. The durability ORDER is guarded too, which the blob path's is not: `Segment::commit` takes a `Durability`, and a test recorder asserts the acts are exactly `sync_file, sync_dir, rename, sync_dir` — removing either fsync turns `a_segment_is_renamed_into_place_only_after_its_bytes_and_its_directory_are_fsynced` red, where the same three mutations on `blobs.rs` all left the suite at exit 0. A segment is also read back through this module's own reader before the caller may file its `Published` row, so a file the writer and the reader disagree about never becomes a claim: renaming one written column turns five named tests red, where without the read-back only the round-trip test would have moved. **What is unguarded.** Nothing looks for a THIRD creating call, and since 2026-11-13 there is one: `cmd_collect::write_envelope` stages the run envelope through `File::create`, which truncates, because one run writes its envelope twice by design - with `finished_at` null before the lease and once more at exit - and refusing a staged file a crash left behind would wedge that run id for good. It keeps the same fsync pair and rename, and it is content no reader reconstructs an item from. `File::create`, `OpenOptions::truncate` and `ArrowWriter` are all still unsearched in this crate, and `blobs.rs` keeps its own fsync pair with no journal behind it. An `UPDATE` or a `DELETE` over `items`: nothing forbids either, and the attempt row is updated in place on purpose as its three later events arrive and once more by `record`'s own `items_new` count. No `items` row is ever updated, which §8 row R15 pins with a test rather than with a rule. The cap half has no code — no view builder exists. §3.9 confines deletion to the L2 blob layer and to `vault` | **two creating calls enforced, one of them with its fsync order; a third would be unguarded** |
| R2 | valid and observation time are distinct types | the type system; a `compile_fail` doc test in `pit/src/lib.rs` pins it, and CI runs it — `--no-fail-fast` is why, because one failing unit test makes plain `cargo test` exit before the doc test. The `compile_fail` block is **paired with a positive control**, a normal doc test carrying the same `use pit::{…}` line, because a `compile_fail` block passes whenever its snippet stops compiling for *any* reason: measured 2026-11-13, after `pit` moved out of `newsvault` into its own member, leaving the old `use newsvault::pit::{…}` line in both blocks left the `compile_fail` test reporting `ok` while the control failed with `error[E0433]: cannot find module or crate newsvault`. A `compile_fail,E0308` annotation does not close it — rustdoc reports `ok` for a snippet that fails with a different code than the one declared. The hole rustdoc cannot see is the control being deleted, or left importing something that still compiles, which returns the pair to passing vacuously; two unit tests in the same file read the module header and assert that its first fenced block is the `compile_fail` one and that its second carries the same `use` line | **enforced, narrowly.** The two instants cannot be compared **with each other**, so the 60.2% leak cannot happen by type confusion. They are not otherwise hidden: `Visibility::valid()`, `observed()` and `Cutoff::inner()` are public, so the publication-only predicate is one line from any crate — demonstrated 2026-11-12 by compiling it. |
| R3 | first sighting wins, **within a partition** | **Two stores and two grains since 2026-11-13.** In the new archive the rule is `newsvault::store`'s, it spans the whole store rather than a partition, and its key is `story_id`: `record` counts a row into `items_new` when the store holds no sighting of that `story_id` or when this observation is earlier than the earliest it holds, and it never updates a stored row, so "the kept sighting" is a `MIN(observed_at)` query and not a column anything corrects. Three mutations on 2026-11-13, each turning a named test red: keying the lookup on `item_id` instead of `story_id`, and counting every row rather than the first sightings, both turn `the_same_story_in_three_commits_keeps_the_first_observation_and_writes_items_new_zero` red — the first of those is what makes the test a check on the KEY, because the second and third commits of that fixture carry item_ids the first did not; and `MAX(observed_at)` in place of `MIN` turns both that test and `an_earlier_observation_arriving_later_replaces_the_kept_one_and_is_counted` red, which is the 19h59m postdating this row exists for. What is NOT checked: nothing reconciles `attempts.items_new` against `COUNT(items)` in either direction. **The lake reader keeps its own rule, at the partition grain.** **Its key is the lake's `id` column as received** — §3.1's `source_item_id`, which is what §5 row 5's `story_id` would be a hash of; nothing derives a `story_id` yet (row R7), so the rule is only as good as the vendor's id. A blank one is unusable since 2026-11-12 rather than a dedup key: measured through the binary, three rows whose `id` was a real empty string came back as **one** item with `unusable_rows` 0 and two publication-time disagreements, two stories gone, and now come back as three unusable rows and no item. `probes/parity_baseline.py` rejected that row already, so the two readers meant to agree were two rules until that day; both now reject an empty or blank id. **The two readers are compared by a command since 2026-11-13**, which is what §10's 156-of-156 was missing: `probes/newsvault_parity.py` reads the same rules in pandas and compares every field of every day against `newsvault day --format json`, over a committed corpus holding one partition per shape the archive holds none of. Its first run found a disagreement on one of them — a row whose ingest time precedes a publication value stamped at midnight was dropped by the probe and kept as a day-dated item here, because the ordering test sat inside `Visibility::from_live` and the day branch never reached that constructor. The reader changed; `a_row_fetched_before_its_own_day_stamp_is_unusable_and_not_day_dated` pins it. The gate CI runs is the probe's committed output: `services/newsvault/tests/parity.rs` is its only reader, so no interpreter runs in CI, and two mutations that day each turn a named test red — dropping that ordering test, and making a blank id a dedup key again. The grain is the invariant's other missing half and it changes the count: `newsvault day` collapses repeats across the run files of one UTC day and nothing collapses across days, which is why the archive holds 61,282 distinct ids and the per-day counts sum to 62,507 (§10). A store whose grain is first sighting **across the archive** is a different store and would need the earliest observation of all 156 partitions, not of one. | **enforced at the partition grain** |
| R4 | a **coverage ledger** (`Attempt`, §3.2) beside every store, **and no vendor call outside an open attempt row** | Two clauses. **The ledger has a READER since 2026-11-13, which is what the first clause was missing:** `newsvault ledger --at <t> --from <d> --to <d>` reports, per (run day, source), the outcome counts, the attempts opened and never answered, the attempts answered and never finalised, the vendor rows against the item rows committed, and two freshness clocks — all of it computed against `--at` and never a process clock, so the answer over a fixture store is a golden file (`services/newsvault/tests/fixtures/ledger/expected.json`) and two runs of it are byte-identical. It reads the attempts table ACROSS target windows with one `GROUP BY`, which answers "is this vendor down for every name, or is this one name quiet?" from rows already on disk. `--check` turns the report into one answer and exits 1 for no, on three conditions, each with a mutation that turns a named test red: runs below the day's expected count, an outcome count holding a failure or an interruption, and a source with zero item rows against a non-zero `vendor_rows`. The fourth condition design.md names, the asking-clock age, is NOT built: its threshold has no producer until the collect policy ships. A run envelope the reader cannot take is named with its §5.1 code and sets exit 8 rather than blanking the month, and an attempt whose run has no envelope is named too. **The call clause is the type system.** `news-provider::wire` is the only module in the workspace holding an HTTP client, `Wire::send` is its only socket, and `send` consumes a `SendPermit` whose one field is private — a struct with a private field has no literal form outside its own module, so from any other crate the only route to one is `AttemptSlot::take_permit`, and the only route to a slot is `AttemptSlot::open`, which writes its `Opened` event to a `Sink` and returns `Err` when the sink refuses. `take_permit` answers `PermitAlreadyTaken` the second time, so the ratio is one call per row and not one row per call. **What a request carries instead of the credential, gated since 2026-11-13.** A `HeaderValue` is `Literal` or `FromCredential(CredentialRef)` and the address SEC requires is substituted inside `Wire::send`, so the plan, the envelope and the attempt row hold a NAME (accepted attack A3.7). The enforcement point is over what a run WROTE, not over the code: `a_literal_contact_shaped_header_value_turns_a_named_test_red` reads the envelope on stdout, every `runs/*.json` on disk and every text column of `attempts` and `items` after a real collect, and refuses both the fixture's own address and any value holding an `@` at all. Mutation 2026-11-13: one line adding `"contact": ask.contact` to `cmd_collect::envelope_of` turns it red. It covers the two tables and the envelope and NOT the blob store, where a filing's own body may legitimately hold an address. A `compile_fail` doc test in `news-provider/src/lib.rs` carries the permitless call, **paired with a positive control** for the reason row R2 states, and two unit tests read the two fenced blocks and assert each annotation is **exactly** `compile_fail` and `no_run`: before that, adding `,ignore` to the first fence and replacing the second with `ignore` switched both blocks off with the whole gate at exit 0 and both guards green — measured 2026-11-13, found by review and not by the author. Measured by mutation 2026-11-13: replacing the illegal call in the first block with a legal line turns both the `compile_fail` doc test and `a_send_without_a_permit_does_not_compile` red; leaving the control's `use` line pointing at a moved path turns the control red while the `compile_fail` block still reports `ok`, which is R2's hole reproduced. **The whole call surface has a production caller since 2026-11-13**, which it did not before: `news_provider::collect` is the driver, `newsvault collect` is its CLI, and one HTTP call is one written ledger row on a real path rather than in a test module. What that made reachable: `Wire::send` (through `impl Exchange for Wire`), `AttemptSlot::open`, `take_permit`, `close`, `abandon`, `Committer::open`, `Committer::record`, `Committer::ok_vendor_rows`, all four `Verdict` arms, `OutcomeKind::Interrupted`, `SystemClock` and `AttemptReceipt::closed_at`. `Cost::Slots(0)`, `LedgerError::PermitAlreadyTaken` and `LedgerError::AnswerFromAnotherAttempt` still have none: the driver takes exactly one permit per slot and closes it with its own answer, so all three are guards against a caller that does not exist yet. **The interrupted attempt now has its closer:** the next run's startup sweep closes every attempt with an `Opened` row and no `Finalised` one, before it opens anything of its own; removing the sweep call turns `an_attempt_a_dead_run_left_open_is_closed_interrupted_by_the_next_run` red. **Three holes, measured rather than reasoned.** (1) Field privacy is module-scoped, not crate-scoped, so the hole is two FILES rather than the crate: a `SendPermit` literal inside `ledger.rs` or a child of it reaches the agent with no row, and a `pub fn` in `wire.rs` can call the private `exchange` with no permit at all — measured 2026-11-13, that second one compiles clean and no command in the gate mentions it, while the literal written in `wire.rs` is refused by rustc with `error[E0451]: field attempt_id of struct SendPermit is private`. The declared-dependency allowlist that makes one crate's manifest the whole answer is written since 2026-11-13 and closes the OTHER half of this hole, a second crate declaring a client of its own: `repo_invariants::clients` asks `cargo metadata --no-deps` which members declare one of 20 named HTTP or model client crates and allows only `news-provider`, dev dependencies included. It reads the member set from `cargo metadata` and not from `workspace.members` for row G1's measured reason, which its own test asserts by catching a client declared in a crate reached only by a `path` dependency. Mutation 2026-11-13: `ureq` added to `panel-contract/Cargo.toml` turns `only_the_seam_crate_declares_an_http_client` and `the_seam_crate_declares_the_only_client_this_workspace_holds` red. What a manifest question cannot see is a CALL SITE, so `std::net` and a client crate outside those 20 names both pass it; the two FILES above are untouched by it. (2) `std::process::Command` running curl is reachable from every crate and nothing looks for it. **The clock clause has a gate since 2026-11-13.** No `Provider` method is handed a clock - `plan` takes the run's `ObservationTime` and `Wire::send` reads the two instants either side of the socket through an injected `Clock` - so a provider reading a process clock is a fact outside the type system, and `repo_invariants::clocks` is the text scan that reads it: `Utc::now`, `Local::now`, `SystemTime::now` and `Instant::now` over `news-provider/src` and every member under `providers/`, exempting exactly two files, `clock.rs` and `wire.rs`. The members come from `cargo metadata`, so a second provider is covered by existing. Mutation 2026-11-13: a `chrono::Utc::now()` added to `providers/edgar-press/src/lib.rs` turns `no_provider_and_no_part_of_the_seam_outside_wire_and_clock_reads_a_clock` red at that file and line. It is a text scan with a text scan's two limits, stated in its own failure message: it fires on the names in a comment, and it sees no clock reached under another name. The same grep lived inside `providers/edgar-press`'s own test file until that day, where it covered no other provider. (3) was the write half having no reader and is closed since 2026-11-13: `impl Sink for newsvault::store::Committer` commits `Opened` in a transaction of its own and buffers the other three for `record`'s, and a write the store refuses comes back as `LedgerError::WriteFailed` — a path whose only producer was a test recorder, now reached by a test that drops the `attempts` table and then opens a slot. **The store clause has an enforcement point since 2026-11-13, in two layers.** `newsvault::store::Committer::record` takes an `AttemptReceipt` by value and there is no `add_items`, so a caller holding rows and no closed attempt has no signature to call. A `compile_fail` doc test in `store.rs` carries the receipt-less call, paired with a positive control that records one item through a closed attempt for the reason row R2 states, and the two unit tests over those fenced blocks assert the annotation is **exactly** `compile_fail` and `no_run`: that is what makes `compile_fail,ignore` red (measured 2026-11-13, left `compile_fail,ignore` against right `compile_fail`) instead of switching the block off in silence. The backstop is the schema: `items.attempt_id REFERENCES attempts(attempt_id)` under `PRAGMA foreign_keys = ON`, so an item row naming an attempt the ledger has no row for fails with `FOREIGN KEY constraint failed`. Measured before that pragma was written, the same insert returned `Ok(1)` and its test was red. The same door is checked in the other direction twice: `record` refuses a receipt whose attempt has no `Opened` row in THIS store, which before the check committed nothing and reported `changed: true` on a receipt built against another store; and since 2026-11-13 it also refuses one whose `Finalised` event this store never buffered. `Sink` is a public trait and `close` takes `&mut dyn Sink`, so nothing in the type system says that `open`, `close` and `record` share one sink — review 2026-11-13 closed a slot against a second sink and measured **3 item rows** committed under an attempt whose `closed_at`, `outcome` and `items_new` were all NULL, a row byte-identical to a crash between `open` and `close` and therefore invisible to any ledger reader. Mutation that day: dropping the check turns `a_receipt_closed_against_another_sink_is_refused_rather_than_filing_items_under_it` red. **Two holes of its own.** The foreign key asks only that the attempt EXISTS and not that it is closed, so raw SQL inside this crate can still file a row against an attempt in flight, which `record` itself no longer can; and nothing stops a second connection to `commit.sqlite3`, where `foreign_keys` is off by default. | the call clause **enforced, within the three holes named here**; the store clause **enforced, within its own two** |
| R5 | failed calls must be handled | `cargo clippy --locked --all-targets -- -D warnings` in CI, over the workspace lint set: `unused_must_use` denied under rust (which also makes a local `cargo check` fail, not merely warn) and `let_underscore_must_use` denied under clippy, which is the only form the move newly catches in CI. Never the type system. | **enforced against two of the four ways to discard a `Result`.** Measured 2026-11-12 in a throwaway crate carrying this lint table verbatim: `let _named = f();` and `drop(f());` leave `fmt`, `clippy --all-targets -- -D warnings`, `cargo check` and `cargo test` all at exit 0 with no mention of either line, while the controls go red — `let _ = f()` gives clippy 101 (`non-binding let on an expression with #[must_use] type`) and a bare `f();` gives `cargo check` 101 (`unused Result that must be used`). No lint in the set closes `drop()`. **Two further opt-outs**, these deliberate: a crate- or module-level `#![allow]` switches any of these off and no command notices, which the three test modules in this tree use; and doc tests are reached by nothing, because `--all-targets` does not cover them. A local `macro_rules!` is **not** a third for the two lints this row rests on: measured 2026-11-12 in the same throwaway crate, `let _ = f()` inside one still gives clippy 101 and a bare `f();` inside one still gives `cargo check` 101. It is an escape for the floor's other half, row G1's — `unwrap()`, `expect()`, `panic!()` and `todo!()` written inside a local macro leave clippy at exit 0. |
| R6 | missing input aborts | unit tests cover the missing root, the missing day and, since 2026-11-12, the `tickers` column. It was the one column fetched as an `Option` while the other seven went through `MissingColumn`, so `string_list_at` had six empty exits — five literal `Vec::new()` and one `.unwrap_or_default()` — and an absent or undecodable column became an empty tag list on every row with `unusable_rows` at 0, which is the empty fallback this row forbids. It now reads like the others: measured that day through the binary, a run file with no `tickers` column exits 1 with `column tickers missing from run file` and one whose `tickers` is a `struct<sym: utf8>` exits 1 with `column tickers has unusable type Struct("sym": Utf8)`, where both exited 0 and returned `"tickers": []` before. One empty exit is left and it is the legitimate one, a null cell meaning the vendor tagged nothing. What is still an empty fallback is outside the reader's list handling: a null string cell is read as an empty string, §3.9's measured pair being a null `body` returning `body == ""` with `unusable_rows == 0` against a dropped `body` column failing the whole day. | **partly** |
| R7 | units are types, identity is a content hash | **the hash half: the `canon` crate.** §2's canonical form has one implementation, all seven digests are taken over it, and 15 committed vectors under `canon/vectors/` state input against expected digest so a reader in another language disagrees out loud rather than silently. Two defects found by review 2026-11-13 and closed before the first vector was committed: the form did not name its own digest, so `filter_params` over one literal parameter and `story_id` over a source and an id were the same two fields and the same sha256; and a value holding one of the three separators forged a field boundary, so the one field `a\u{1f}b` and the two fields `a` and `b` had one form — that value is now refused. **Units are not types**: nothing in the tree carries a basis point or a standard deviation. | **enforced for identity as far as a producer exists, absent for units.** A stored row carries an `item_id` since 2026-11-13 — `newsvault::store::ItemRow.item_id` is a `canon::Digest` and the column is `NOT NULL` and deliberately **not** unique — uniqueness is over (`item_id`, `attempt_id`), because a vendor that publishes A, corrects it to B and reverts to A must file a third row, and a uniqueness index over the content digest files nothing on that third act and leaves B standing as current. Mutation 2026-11-13: `UNIQUE(item_id)` turns `an_a_then_b_then_a_revert_yields_three_items_two_blobs_and_three_attempts` red. A row also carries a `story_id` and an optional `doc_id`, so three of the seven digests have a stored column — but the store is HANDED every one of them and never computes one. Measured that day, `grep -rn 'canon::' --include='*.rs'` outside the crate returns three files, `news-provider`'s two and `services/newsvault/src/store.rs`, and the only callers of `canon::digest::item_id` anywhere are that file's positive-control doc block and its test fixture. **`item_id`, `story_id` and `doc_id` have production callers since 2026-11-13**, all three in one place: `impl Archive for Committer`'s row builder, which turns a provider's `CollectedItem` into a store row. The body is stored as a blob of its own first, because `item_id`'s `body_ref` is that blob's digest, so an edit to the extracted body moves the identity. `canon::Stamp` carries the precision into the digest through `provider::stamp_of`, so a day and an instant naming the same date are two different `item_id`s. What is still not checked is that two readers agree on a digest taken over a row this collector wrote: the committed vectors are `canon`'s own, not the archive's. The second reader that does exist since 2026-11-13 is over the LAKE and not over this archive — `newsvault day` emits an `id_digest` and a `content_digest` and `probes/newsvault_parity.py` reproduces both (row R3) — and neither is an `item_id`, so the `items` table still has no second reader at all. `attempt_id` and `filter_params` have production callers, and `result_digest` gained one on 2026-11-13: `newsvault ledger` digests the id set each closed attempt committed and reports, per (source, target window), only the groups whose attempts disagree. That digest is also the one place a second implementation has already been run against this tree — the two values the drift test expects were computed in Python from §2's form, outside the crate, and the Rust reproduced both. The remaining **four** of the seven have no producer. **The three FILE digests of §2 - `rule_version`, `calendar_version` and `universe_version` - have producers since 2026-11-13**, all three through `canon::FileDigest::of`, which is a type of its own and not a `Digest`, so the claim that a `Digest` was taken over the canonical form keeps holding: the hashed rules file of `edgar-press`, the session calendar `Calendar::read` takes, and the roster every `Request` carries and every `Opened` row stores. So what is enforced is that the derivation is one rule with committed vectors, plus that an attempt's identity is taken over it, not that anything has been identified by it |
| R8 | presence counted, never multiplied | `panel-contract`, a workspace member both sides of the panel file boundary can depend on. Six rules over the manifest: the identity and input fields must be present; `exchangeable_units` must carry all three of the keys §3.4 names, each raising its own finding; the attrition chain must reconcile, cardinality product minus attrition equal to `rows`; `rows` equal to a product of the stated cardinalities — a pair, the whole product, or a single pre-multiplied one — needs an explicit `zero_attrition_counted` sentence; `exchangeable_units.count` equal to `rows` needs a `rows_are_exchangeable` sentence; a count above `rows` is an error with no attestation, because no state of the world matches it. Seven committed fixtures, two of which are the historical case: `1 672 × 47 = 78 584` with no attrition and no unit count is rejected and the product is named, and the recounted `78 460` with 124 dropped rows and 47 sessions as the unit is admitted. **The ceiling, four limits, each run on 2026-11-12 rather than reasoned.** (1) It reconciles the numbers the manifest states and never opens the Arrow file, so over the committed `corrected_panel_78460` fixture a single cardinality of 78 708 with an invented attrition stage of **248** rows is admissible — `panel-contract validate --format=json` exits 0 with zero findings. (The arithmetic is the whole check, so it has to be done: 78 708 − 248 = 78 460 = `rows`. The same cardinality with a 124-row stage is *refused*, `attrition_does_not_reconcile`.) (2) Equally admissible: `rows` equal to the product of three of four stated cardinalities with the remainder absorbed by one stage — built as rows = 30 = 2 × 3 × 5 with a fourth cardinality of 7 and one stage absorbing 180, exit 0, zero findings. §3.10.1 is what would close both. (3) Rule e fires on **exact equality only**, so `exchangeable_units.count` = 78 459 over 78 460 rows passes with no attestation — one unit below "every row is its own unit", which is the same independence claim; measured, exit 0, against `units_equal_rows_unattested` at 78 460. A ratio threshold would close it and needs its own construction first. (4) The attestation check is a length floor, and so is the `definition` check: both rule out `"yes"` and `""` and neither can tell a counted check from a confident paragraph. Every message resting on a heuristic says so. **What runs the validator**, which until 2026-11-12 was nothing at all: `git grep` over `*.yml`, `*.py`, `*.sh` and `*.rs` at the commit before found no caller outside the crate's own source, so the rules were tested and the command was not. Since that day an integration test runs the compiled binary over all seven committed fixtures on every `cargo test`, asserting that the exit status is the verdict, that the binary's findings are the library's, and that the fixture table is the whole directory — measured by returning exit 0 from `validate` on a manifest with findings, which left all 21 library tests green and turned that test red. Neither end of the file boundary runs it yet: `probes/panel_recount.py` still writes `assign.json` + `meta.json` and `probes/panel_power.py` still reads them, which is row R9's work and needs §3.4 to name a layout first. | **enforced, within the ceiling stated here** |
| R9 | power from the registered procedure | nothing refuses a panel yet, and the gap is smaller than "a consumer that does not exist". `probes/panel_power.py` already runs the registered permutation, prints the measured standard error against the independent-rows formula and their ratio, prints a Monte-Carlo error per estimate, and derives its own exchangeable-unit count. What it does not do is read the manifest: its inputs are an ad-hoc `assign.json` + `meta.json`, so it has nothing to refuse. The check is that script taking a panel manifest instead, and aborting when `exchangeable_units` is absent. The gap is also narrow in a machine-readable way: `design_effect` and `within_unit_correlation` are properties of a statistic rather than of the row set, so `panel-contract` carries them as `Need::DeferredToR9` rather than requiring them, which tells a reader of the crate which half is deferred instead of leaving it to be rediscovered. | **no enforcement point yet** |
| R10 | provenance labelled, **and a mix disclosed** | two clauses and only the first has an enforcement point. **Labelling has a producer since 2026-11-13.** `Provenance` is a field of `news_provider::Request` and not of the adapter, because the same EDGAR adapter reading today's index is `live_ingest` and reading an earlier day's is `vendor_archive`, and `Provider::plan` is the only method holding the instant that tells them apart; a follow-up inherits its index request's value, so one window is one population. The boundary is the run's own UTC date and no other number, no measurement having placed one (§10, D5). Mutation 2026-11-13: reading every window as a live ingest turns `a_vendor_archive_window_is_refused_until_a_probed_lag_stands_behind_it` red, because that arm is the one with the refusal behind it. The enum field is non-nullable (§3.1), and `bulk_dataset` has no producer - it stays because the enum is §3.1's contract and not the collector's. Disclosure has one producer since 2026-11-13 and it is not the one this row asks for: `newsvault collect`'s run envelope carries `provenance_counts` over the attempts of that run, so a run reading today's index and a 2024 one cannot look alike in the record. `provenance_counts` on the snapshot manifest (§3.0) is the mix of the ITEMS, and nothing produces it - the column does not exist on `items`, so a pooled live/archive read cannot be reconstructed from the archive at all, which is this row's largest remaining hole; a panel's own mix — the one that matters, since the panel is where pooling happens — has no field at all, in the 13-field manifest list row U1 counts or anywhere else. The pair (`provenance`, `source`) is the stronger subject and waits on the two EDGAR body medians being reconciled (§9), so it is not written as a number here | labelling **design**; disclosure **no enforcement point yet** |
| R11 | backfill lag is measured | `from_backfill` requires a `MeasuredLag`, so the **type** cannot be skipped, and since 2026-11-12 deserialisation cannot skip it either: `Visibility` deserialises through `#[serde(try_from)]` over a private wire struct, so a pair read from a file runs the same ordering check as `from_live`. Before that the derived `Deserialize` wrote straight onto the private fields and bypassed **both** constructors — measured from outside the crate, `{"valid":"2026-07-01T12:00:00Z","observed":"2026-06-01T00:00:00Z"}` was accepted with `lag()` at −2,635,200 s, the state §5 row 4 forbids, and it is now refused with `ObservedBeforeValid`. §4 makes vault-to-derive a file boundary and §3.10.2 specifies a JSON stream carrying both instants, so that path is a consumer's normal constructor rather than an exception. **The type still does not require the measurement**: all four `MeasuredLag` fields are `pub` with no validating constructor, so `{probe_id: "", probes: 0, leaks: 0, seconds: 0}` is accepted with `leak_rate()` returning `None`, and a lag of 0 s puts the observation at the publication instant - a backfilled item claiming visibility on its own publication day, which memo R11 says no constructor allows. **One CALLER refuses it since 2026-11-13, which is where the requirement now lives.** `news_provider::provider::probed_lag` is asked before the first request of any `vendor_archive` window and refuses in each of the four ways a lag can be empty - no entry, a blank `probe_id`, zero probes so no leak rate, and zero seconds - each asserted by `a_lag_with_no_probe_behind_it_is_refused_in_each_of_the_four_ways_it_can_be_empty`. `edgar-press::plan` is its one production caller, so such a window is a refusal before any attempt row exists rather than a stored leak (decision D5, §10). Mutation 2026-11-13: dropping that call turns `a_vendor_archive_window_is_refused_until_a_probed_lag_stands_behind_it` red. What a minimum lag should be is still not decided, so a one-second probed lag passes. **The measurement has a producer and a path into a run since 2026-11-13**, which is what the row was missing: `probes/observation_lag/` measured **94,922 s = 26.37 h** for `edgar_press` under `vendor_archive` - the MAX of 22 per-filing lags over 2026-11-10 and 2026-11-12, 27 requests of a registered 30, registered and amended before the first request and logged in `probes/observation_lag/PREREGISTRATION.md` - and `news_provider::lag_policy::LagPolicy` carries it as a hashed FILE keyed by (source, provenance), which `cmd_collect::open_provider` reads once and hands to `EdgarPress::with_observation_lag`. **Seven mutations 2026-11-13, each turning a named test red**: the provider not being handed the policy's lag fails `a_vendor_archive_window_collects_once_the_measured_lag_is_handed_to_the_provider`; the copy into the archive being dropped, or moved ahead of the `--dry-run` return, fail `the_policy_that_dated_a_run_is_in_the_vault_under_its_own_digest` and `a_dry_run_dates_nothing_and_copies_no_policy_into_the_vault`; looking a lag up by source alone fails `a_policy_stating_only_the_other_provenance_leaves_the_archive_window_refused` and `the_lag_is_looked_up_by_source_and_by_provenance_together`; an empty `lags` array read as "no lag" fails `a_policy_naming_no_lag_is_refused_rather_than_read_as_no_lag`, which is R6 applied to this input; `File::create` in place of `create_new` on the copy fails `a_staged_policy_copy_a_crash_left_behind_is_refused_rather_than_overwritten`; and renaming any of the three envelope fields fails `the_run_envelope_names_the_policy_and_the_probe_that_dated_its_archive_windows`. **The lag DATES since 2026-11-13, which is what it was for.** `EdgarPress::backfill_observation` is `Visibility::from_backfill`'s one production caller: a `vendor_archive` item carries the observation its own publication plus the probed lag place it at, and `cmd_collect`'s `rows_of` stores that instead of the fetch instant. A DATE has no instant, so its bound is the END of that day in its own calendar plus the lag - an upper bound on an upper bound, which can only make an item less visible. An archive row whose observation the provider could not place is REFUSED (`ArchiveItemHasNoObservation`) and never filled in from the fetch clock. Three mutations 2026-11-13, each turning a named test red: placing no observation fails `a_backfilled_item_observes_at_its_publication_plus_the_probed_lag_and_not_at_the_fetch` and `a_vendor_archive_window_collects_once_the_measured_lag_is_handed_to_the_provider`; dropping the refusal fails `an_archive_row_with_no_placed_observation_is_refused_rather_than_dated_by_the_fetch`. **The refusal has no reachable failure through this provider**, because `plan` refuses an archive window with no probed lag before a request exists - measured that day, the test that tried to reach it through `extract` was deleted for having a false premise - so it is the gate for the next provider. **One hole, measured rather than reasoned.** `leaks` is now 0 by construction, because the scalar is the max of the sampled lags and a max leaves nothing above it, so `leak_rate()` is `0.0` and the disclosure quantity R10's cell leans on carries no information about this vendor; what does is the off-day row count the probe reports, 0 of 384 and 0 of 549 on the two probed days. | **the type is required; the measurement is required of the one caller that would date a backfill, is produced by a probe, reaches a run through a hashed policy file, and dates a backfilled item** |
| R12a | **no automatic derivation from a proposal to an order** | nothing here, and nothing here is the right answer. Reframed 2026-11-12: the invariant is not "no path". A path is harmless. What the predecessor removed on 2026-09-24 was an automatic brief-to-intent producer, and that is the thing that must not return. A mediating service that **formats a human's decision** into a valid order document is a convenience; one that **derives** which companies and what size from a brief is the deleted thing coming back. The check belongs at that mediator, whose input may not be a `brief_id` alone, and it cannot be written before the mediator exists. | **not this repository's gate** |
| R12b | the brief carries no order-shaped field | the brief schema, over the field list enumerated in §3.6 — one home, because the enumeration *is* the check and two copies of it are two different checks | design |
| R13 | five time columns, sequence is the authority | **four of the five columns exist since 2026-11-13** and the authority half has no enforcement point. `attempts` carries `requested_at` and `received_at`; `items` carries `committed_at` and `seq`, the `seq` taken from a committed `seq_reservations` row, plus `observed_at` and exactly one of three publication columns: `valid_at` for an instant, the pair `valid_day`/`valid_tz` for a DATE with no hour, and `valid_refused` for a value its own observation preceded. The day pair is where §3.1's `valid_precision` row stops being advice: a filing carrying no acceptance instant reaches the archive as its index's own day in New York, and `newsvault items --at` admits such a row only once that day has closed in its OWN calendar. Three mutations 2026-11-13, each turning a named test red: folding the day into a midnight instant fails `a_day_precision_publication_value_is_stored_as_a_day_and_never_as_a_fabricated_midnight`; bounding the day by UTC midnight in the reader instead of `pit::datedness::end_of_day` fails `a_day_dated_item_is_invisible_until_its_own_calendar_day_has_closed` (a UTC-midnight bound is 19:00 New York and admits the afternoon); and renaming the `valid_day` column out of the segment schema fails six tests including `a_day_dated_row_reaches_the_segment_with_its_day_and_its_calendar`. An item offering NO publication value is refused at the door rather than given an invented one (`an_item_offering_no_publication_value_is_refused_rather_than_stored_with_an_invented_one`). `data_through` has no producer and no column: nothing parses a payload yet. Two of the columns are read by a test rather than only written — mutation 2026-11-13, rendering the stored instants at whole seconds turns `a_stored_instant_keeps_the_microsecond_the_item_id_digests` red, which is what keeps the stored value and the digested value one number. **The authority half has one reader since 2026-11-13** and it is a test rather than a type: `publish::read_day` returns a publication day through one ordering helper keyed on the `seq` column, and keying it on `committed_at` turns `the_ordering_helper_sorts_on_seq_and_leaves_no_row_behind` red. The signature does NOT refuse a time column — `SegmentRow` carries four of them and the helper could sort on any — so what is enforced is this helper's key and not the rule. `ledger` reads the column the other way, as `unpublished_seq_ranges`: contiguous spans of committed `seq` no segment carries, and merging a span across a hole (a reserved range a transaction rolled back) turns `an_unpublished_seq_range_is_reported_by_the_ledger_and_exported_by_the_next_run` red | **four columns written, two of them read by a test; the ordering authority has one helper and one span reader, and no signature that refuses a clock** |
| R14 | an attempt is recorded even when empty | `newsvault::store`'s `record` writes the `Finalised` row whatever the outcome and whatever the row count, and `an_attempt_with_zero_items_commits_and_writes_its_row` asserts the `outcome` column reads `empty` after an attempt that committed nothing. Measured 2026-11-13 by returning early from `record` on an empty row set: that test goes red with `Sqlite(InvalidColumnType(0, "outcome", Null))`, the column having stayed NULL. The `outcome` column is nullable on purpose — NULL is "opened and not yet closed", which is the in-flight count the ledger reports. What is not checked is the other direction, `items_new = 0` on a fetch that was not empty, which is row R16's clause | **enforced for the empty fetch** |
| R15 | disputed timestamps marked, never corrected | **the handling has an enforcement point since 2026-11-13.** `items.valid_disputed` and `items.earliest_valid_at` are written by `record` on the ARRIVING row, against the twins already stored under its `story_id`, and no stored row is updated at all — which is what "never corrected" means here and is why the earlier twin's flag stays 0. Five mutations that day, each turning a named test red, and all five land on `a_disputed_publication_time_is_marked_on_the_arriving_row_with_the_earlier_twins_value`: `dispute` returning `false` always; `earliest_valid_at` carrying the arriving row's own time instead of the earlier twin's; joining the twins on `doc_id` instead of `story_id`; an added `UPDATE items SET valid_disputed = 1 WHERE story_id = ?`, which marks the earlier twin too; and an added `UPDATE items SET earliest_valid_at = ? WHERE story_id = ?` that back-fills the earlier twin when a dispute is found. **The second test is narrower than it looks.** `the_first_row_is_byte_identical_after_the_second_arrives` compares all thirteen columns of the first row before and after the second commit, and it caught the back-fill mutation — but the blanket `valid_disputed` one left it GREEN, measured that day, because that write also runs on the first commit and so is inside both reads. A write to a stored row is caught by this test only when the write is conditional on a twin existing. **No materiality threshold ships** and §3.1's row now says so instead of saying "materially earlier": the rule is "a stored twin carries a different publication value", because placing that word is a number and this project has no measurement that would place it. The detection also has code, at a narrower grain than §2's: `lake.rs` keeps the first sighting's publication time when another run of the same UTC day reports a different one for the same id, and counts the disagreement twice over — `valid_time_conflict_events` per row and `items_with_conflicting_valid_time` per id, because one item re-reported in five later runs is five events and one item. Three tests assert both counters since 2026-11-12, one of them over the path that counted nothing before that day: a known id arriving with an *earlier* observation replaced the kept item silently. That path is taken 0 times in the archive's 243,226 rows and arrives with a second ingest schedule or a backfill (§3.1 `provenance`). **The size of the population R15 handles, in both units**, with the construction of each figure and the decomposition that ties them together: memo §5.1, re-derived through both readers on 2026-11-12 and not copied here. The §2 detection, which joins twins on `story_id`, now has code in the store and none in a reader: nothing reads `valid_disputed`, `earliest_valid_at` or `doc_id` back out, so what is enforced is that the columns are written correctly and not that a consumer meets the dispute | **the handling enforced on the write side; the detection counted in two units at the lake grain; no reader** |
| R16 | the **coverage ledger** records poll events | **`items_new` has one writer since 2026-11-13** — `record`'s own count of the rows that became the kept sighting for their `story_id` — and it is written whatever that count is, so a second commit of a story already stored writes an `Attempt` row carrying 0 rather than no row. One writer and not two was the point: `Outcome::decide` is handed an `items_new` by its caller, and only the store holds the index that can count first sightings, so the outcome's copy is no longer stored and `news_provider::Counts.items_new` is now written by `decide` and read by nothing. Mutation 2026-11-13: counting every row instead of the first sightings turns `the_same_story_in_three_commits_keeps_the_first_observation_and_writes_items_new_zero` red. What this row still does not check is the other direction, `items_new` against `COUNT(items)` for one attempt. **One half of that gap closed on 2026-11-13, in the reader:** `ledger --check` exits 1 for a (day, source) whose `vendor_rows` is non-zero while the item rows committed under it are zero — the vendor-answered-and-the-store-holds-nothing case — and removing that condition turns `check_exits_one_when_a_source_returns_zero_items_against_a_non_zero_vendor_row_count` red. A cap cannot hide that finding: `--limit` truncates after the check runs, and reversing the two turns `a_cap_does_not_hide_a_check_finding` red. **The poll events are real events since 2026-11-13**: `newsvault collect` writes one attempt row per HTTP call, including the index attempt that commits no item of its own, and the two counts stay in their own columns because `news_provider::Counts` no longer carries an `items_new` at all - only the store holds the index that can count first sightings. Measured that day over the committed EDGAR fixtures, two runs of one day: six attempts, four item rows and `SUM(items_new) = 2`, and counting every row instead of the first sightings turns both `the_same_story_in_three_commits_keeps_the_first_observation_and_writes_items_new_zero` and `two_identical_collects_leave_the_right_events_one_blob_set_and_items_new_one_then_zero` red. Still unreconciled: `items_new` against `COUNT(items)` where both are non-zero and disagree | **enforced for the poll event; the zero-against-non-zero case is detected by the reader, the rest is not reconciled** |
| R17 | matching survives a broad corpus, over article body | two clauses. **The body half has a producer and a gate since 2026-11-13:** `edgar-press::EdgarPress::extract` returns `ExtractedBody` holding article text, built by dropping the elements the hashed rules name, stripping tags, applying the rules' entity table and collapsing whitespace, and `the_extracted_body_is_article_text_and_never_page_html_or_a_script_payload` asserts the `<script>` data layer of a committed filing fixture does not reach it. Two mutations that day, each turning that test red: skipping the `drop_elements` pass, which lets the data layer read as prose, and turning the tag strip off. The rules file is hashed, and the identity claim is narrower than it was written: `extract_rules_version` is NOT an `item_id` input, so a rules edit reaches the identity only through the bytes it changes - one byte removed from `drop_elements` moves the body and the `item_id`, while a key the extractor does not read moves the version and leaves every `item_id` identical, which is the pair of tests named after both halves. **The corpus half has no enforcement point:** two committed fixtures are not a corpus, their median extracted body is 212 characters, and the one real figure for this source is 14,039 characters over n=103 (§9). The gazetteer regression case is `derive`'s and `derive` has no code | **body text enforced for one provider; the broad corpus is design** |
| R18 | two rule versions, one snapshot, a diff | a `derive diff` subcommand; its absence is the gap | design |
| R19 | **a vendor is called no faster than its stated budget, counted against the bucket the vendor counts it against** | **New row 2026-11-13. Nothing in this table covered the call RATE before it:** R4 covers that a call has a ledger row and R5 that its failure is handled, and a grep of this section for `pace`, `quota`, `req/s`, `budget` and `throttl` matched only the word `workspace`. What the row claims is two things. **The budget has a producer and a scope.** `TransportProfile` carries a `RequestBudget` of a per-IP `min_interval` and, since 2026-11-14, an optional `per_host` interval; the provider states it beside the timeouts, and `providers/edgar-press` states 100 ms for both — 1 s / 10 requests, SEC's fair-access ceiling per IP as the predecessor's vendor-quota table states it (read 2026-11-13 from `apps/alphalens-pipeline/CLAUDE.md`). That ceiling is **not measured here** and the interval paces TO it with no margin under it. `BudgetScope` names the bucket one pace file counts and has two variants. `PerIp(source)`, because the caller names itself in the User-Agent and that does not scope the quota. `PerHost(host)`, keyed on the request url's host lowercased and NOT on the source, for an open-web feed whose outlets each ask their own crawl delay; the host comes from `ureq::http::Uri`, and a per-host pace on a url with no host is refused as `BudgetError::NoHost` before any wait, never left unpaced. **What reaches `PerHost` in production is `edgar-press`'s declaration for `www.sec.gov`**, through `newsvault collect` → `Wire::new`, the only production `Wire`. With one SEC source it changes no wait — both files then advance in step — and it is declared for the bucket it adds: the per-IP file is per SOURCE, so a second SEC source would hold a second per-IP bucket, while the host file is one for both. The per-CREDENTIAL arm, where the bucket follows the key across hosts and a second key is a second bucket, is named in the type's own doc rather than written, since a variant nothing produces is a branch nothing reaches. **The wait happens before the socket and is coordinated through a FILE.** `Wire::send` is the only socket in the workspace (R4's call clause), and it reserves a slot through `Pace::reserve` using the `&dyn Clock` it already takes, then hands the answer to an injected `Sleeper` — unconditionally, so there is no zero-wait branch — before the first byte. The wait sits OUTSIDE the two instants, because `requested_at` is read after it, so `received_at - requested_at` is still the vendor's latency and not the latency plus our own queueing. No other module reads a clock for it, so R4's clock grep keeps the same two exemptions, and a send spends three clock reads, four when its profile states a per-host pace. The files are `<store>/pace/<source>.per-ip` and `<store>/pace/<host>.per-host`, beside the archive the run writes and never a system directory; it holds the next-allowed-call instant as RFC 3339 with microseconds, which is one width for every instant, and the lock is held for the read-reserve-write only and released before the wait, so callers stack reservations instead of serialising on each other's wait. Shape taken from the predecessor's `sec_rate_coordinator.py`, written after the failure it prevents: three processes on one residential IP, each with its own in-process throttle, whose aggregate drew a traffic-threshold 403. **Five tests and six mutations, all run 2026-11-13.** With `Pace::reserve` returning zero — which is what this tree did until that day — all five go red. Reserving and not waiting (`let _unused = self.pace.reserve(..)`) turns `the_second_send_waits_where_the_first_did_not`, `the_wait_is_the_interval_less_the_time_already_elapsed` and `two_wires_sharing_one_pace_file_serialise_against_each_other` red. Sleeping the whole interval instead of the remainder turns the same three red, the first of them because the FIRST send would then wait too. Giving each `Pace` a file of its own — which is what an in-process limiter is — turns only `two_wires_sharing_one_pace_file_serialise_against_each_other` and the two tests about the file name red and leaves both single-`Wire` tests green, which is why that test exists: the two before it send twice through ONE `Wire` and an in-process field satisfies them. Resetting an unparseable pace value to `now` instead of refusing turns `a_pace_file_holding_a_value_this_crate_did_not_write_refuses_before_the_socket` red. Reserving before the credential check turns `a_send_whose_credential_is_absent_refuses_before_the_clock_is_read_at_all` red, which is the test that also asserts no pace file exists after a refusal — a call that cannot be made spends no slot. `ThreadSleeper::sleep` with an empty body turns `the_process_sleeper_actually_waits` red; every other test injects a recorder, so without that one test the production sleeper could be inert with the suite green, which is the hole `SystemClock` had on 2026-11-13. **The waiting is this crate's and not the caller's, since 2026-11-13.** While the sleeper was a `Wire::new` parameter, wiring production to one whose `sleep` is empty left `cargo clippy` and every test target at exit 0 — measured that day — so the limiter was a capability the caller granted. `new` now builds its own `ThreadSleeper` and the injecting constructor is `pub(crate)`, so outside this crate there is no constructor that does not sleep and the mutation no longer type-checks. **Both scopes apply to one call, host first, since 2026-11-14.** `Wire::send` takes the host wait and only then reads the clock again and reserves the per-IP slot, so a call waits out each pace in turn and is held to the stricter of the two, while calls to different hosts share only the per-IP pace. The rejected order was one reservation of both before either wait: the per-IP file holds one next-allowed instant, so an IP slot reserved before a long host wait is either spent unused — and two calls can then leave inside one interval — or, reserved at the host's instant, puts every other host's call behind that wait. **Six tests and six mutations, run 2026-11-14.** Deleting the host wait turns all six per-host tests in `wire.rs` red. Reserving the IP slot before the host wait turns the same six red, `the_ip_slot_is_reserved_after_the_host_wait_and_not_before_it` among them, which reads the per-IP file and finds the 60 ms instant instead of the post-wait one. Keying the host file under the source turns `two_sources_asking_one_host_share_its_pace` and `one_host_spelled_in_two_cases_is_one_bucket_under_its_lowercase_name` red. Dropping the lowercasing turns the latter red only because it asserts the file NAME: asserting the waits alone, it stayed green on this machine's case-insensitive APFS disk, measured that day. Replacing the `NoHost` refusal with an empty host turns `a_per_host_pace_on_a_url_with_no_host_refuses_before_any_wait` red. Setting `edgar-press`'s `per_host` to `None` turns `the_sec_profile_paces_both_our_address_and_the_sec_host_at_ten_a_second` red. **Eight holes, named rather than fixed.** (1) The flock itself has no test: the tests prove the reservation travels through the file, and that two callers cannot hold the lock at once is asserted by nothing, because a lost race needs two threads and an interleaving a test cannot fix. The property was measured by hand once, 2026-11-13: two separate OS processes against one pace file were handed exactly 20 slots of 100 ms with no slot issued twice, and the lock is released before the wait, so callers stack reservations instead of serialising on each other. A measurement is not a gate. (2) The file is per STORE, so one host running two stores holds two pace files and therefore two buckets, which a per-IP quota does not — the predecessor has the same hole across HOMEs. (3) There is no cap on a reservation far in the future: a clock that jumps forward leaves a parseable instant that makes the next send wait that long, where the predecessor resets anything beyond 5 s. (4) There is no bounded lock acquire, so a stuck holder stalls every caller rather than degrading to no wait — the predecessor's bounded `LOCK_NB` spin answers a case this tree does not have yet, and its degrade-to-no-op arm would be a branch no test here could reach. (5) R4's own two-file hole is also a pace hole: a `pub fn` inside `wire.rs` can call the private `exchange` with no permit, and now with no reservation either. (6) The per-host spacing is not exact: a call leaves after its host slot plus any per-IP wait it then owes, so two calls to one host can be closer than `per_host` by that per-IP wait — at most the per-IP queue ahead of it. (7) One `per_host` interval per profile, keyed on the exact host: a feed whose outlets ask different delays cannot state them in one profile, and `www.sec.gov` and `sec.gov` are two buckets although SEC counts one. (8) No test drives `edgar-press`'s declared profile through a real `Wire`: the provider test asserts the declaration and the `wire.rs` tests the enforcement, and the only `Wire::new` call is `newsvault`'s `main.rs`. **The row's scope now covers a RETRIED call, since 2026-11-13 (R21 is the retry's home).** The driver re-attempts a transient (non-permanent) document failure — an HTTP 503 or a transport error, never a permanent 4xx or `vendor_refused` — bounded by `MAX_TRANSIENT_RETRIES` = 2, each retry its own attempt row (R4). A retry re-enters through `attempt_once` → `exchange.send`, the one socket this row paces, so it reserves a slot through `Pace::reserve` exactly as a first attempt does: the budget holds over the larger set of calls and the call RATE is unchanged. Widening `min_interval` or the `RequestBudget` to absorb the loss faster was the rejected alternative (approach B); the rate is the vendor's ceiling and is left at it, so the retry is spaced behind the queued calls and is not a faster path. That a re-queue cannot reach the socket by any faster path is the single `exchange.send` call site (grep 2026-11-13: one), a structural fact and not a test, so a future second send path would fall outside both this guarantee and its proof. The measured reason, the bound, the classification and the three enforcing tests (`a_transient_fetch_failure_is_retried_and_commits_the_item_within_one_run`, `a_permanent_fetch_failure_is_attempted_once_and_never_retried`, `a_request_that_fails_transiently_every_time_is_bounded_to_n_plus_one_attempts`) with their biting mutations live in R21's cell, not a second copy here; the gist, so this row carries its own rationale: ~8% of document fetches failed transiently on 2026-11-13 (run 1 lost 42 of 502, 15 HTTP 503 and 27 connection nulls; run 2 lost 43 of 502, a DIFFERENT subset, which is what proves the loss transient), N is a MECHANISM cap and not a figure read off that rate, and its residual is a coverage gap R20 converges. | **enforced for every call through `Wire::send`, retries included, per IP and, where the profile states one, per remote host, within the eight holes named here** |
| R20 | **a `vendor_archive` backfill converges: a re-run fetches only the uncovered accessions, and the reader reports the gap** | **New row 2026-11-13.** Before it, `attempt_id` was keyed on `run_id`, so a fresh run re-fetched every filing a day named and could not target the gap left by the ~8% of document calls that fail transiently at the no-margin pace (R19). The fix lives in four components, each with one reason to exist. **The covered set is a read the store owns.** `newsvault::store::Committer::covered_story_ids(source, target)` returns the `story_id`s the store holds a COMMITTED item for under that window, keyed on `items` and never on an attempt's outcome: an attempt that returned ok and committed nothing is a gap, not coverage. Mutation: the read is items-driven (`FROM items i JOIN attempts a`), so only a story an item row carries is returned, and a `LEFT JOIN` on that same inner join stays green (measured 2026-11-13); the mutation that bites is reordering it to `FROM attempts a LEFT JOIN items i`, which turns `covered_story_ids_names_a_committed_story_and_not_an_ok_attempt_that_committed_nothing` red with `Sqlite(InvalidColumnType(0, "story_id", Null))`, the empty ok attempt then yielding a NULL story. **The seam is the driver and the `Archive` trait.** `news_provider::collect::attempt_once` reads the covered set once per index attempt and hands it to the provider; `Archive::covered_story_ids` and `Archive::record_planned_coverage` are the two doors, implemented by `Committer` and never by the provider, which depends on neither the store nor `rusqlite`. **The exclusion is the provider's.** `edgar-press::EdgarPress::follow_ups_to_issue` drops a `vendor_archive` follow-up whose story the covered set holds and keeps a `live_ingest` one whatever its coverage (R16), mapping a follow-up to its story by the accession its index-derived url carries — no stored `story_id` column, because the accession IS the vendor row identity (R3/R7). Mutations: returning the follow-ups unfiltered turns `a_vendor_archive_rerun_fetches_only_the_uncovered_accession` and `a_vendor_archive_follow_up_whose_story_is_covered_is_dropped_and_an_uncovered_one_is_kept` red (run 2 asks for the covered Apple again); dropping the `provenance != VendorArchive` guard turns `a_live_rerun_still_polls_every_matched_row_even_when_the_story_is_covered` and `a_live_follow_up_is_issued_even_when_its_story_is_already_covered` red. **The number is the reader's.** The index attempt records how many in-universe follow-ups it named in `attempts.matched` (NULL on a document attempt, a sweep, and a `live_ingest` index — coverage is the `vendor_archive` arm's alone, since a live re-run re-polls every row (R16) so a live shortfall is "not yet", not an unconverged backfill; a NULL `matched` is also how the reader tells an index attempt apart without a phase column); the driver records the FULL pre-exclusion count for an archive window, so the denominator is stable across runs. `newsvault::store::coverage_up_to` reports, per (source, target window), `MAX(matched)` as planned against the distinct committed stories as covered, and `newsvault ledger --check` raises `coverage_incomplete` whenever `covered < planned`. The gate fires on a gap of ANY size and never a percentage — there is no threshold to pick after seeing a result. Mutations on `a_vendor_archive_day_with_an_uncovered_accession_reports_coverage_incomplete`: dropping the `coverage_incomplete` push turns it red; `MAX(matched)` → `SUM(matched)` turns the `(2, 2)` assertion red (two runs' index attempts add to a planned of 4); making `record_planned_coverage` a no-op turns it red with no coverage row at all; and recording `matched` for a `live_ingest` index (dropping the `provenance == VendorArchive` guard in the driver) turns `a_live_window_with_an_uncovered_follow_up_reports_no_coverage_gap` red, the live window then reporting a gap. | **enforced for the `vendor_archive` arm, within these holes, the last four named 2026-11-13 by an adversarial review of this diff: coverage is per STORE, so a window read under two stores counts twice; the denominator rests on the index attempt recording `matched`, which a provider that follows up without an index phase would not; `coverage_up_to` compares a COUNT of distinct committed stories against `MAX(matched)` rather than the planned SET, so across a roster change between runs a covered story outside the planned set can numerically mask an uncovered planned one; `MAX(matched)` never decreases, so a filing that fails and then drops from the roster leaves a permanent `coverage_incomplete` nothing can clear; `record_planned_coverage` is an `UPDATE` outside the commit transaction, so a transient error there aborts the run after the items are already committed and leaves `matched` NULL until the next index run re-records it, hiding the window from `--check` meanwhile; and follow-ups are issued only from the `Index` phase, so a provider that chained documents would have its document-phase follow-ups dropped silently — `edgar-press` returns none today** |
| R21 | **a transient document-fetch failure is re-attempted within the run, bounded, and only a residual gap stands** | **New row 2026-11-13.** Before it, a document fetch that failed transiently was recorded and left: measured over two full-day collects, run 1 lost 42 of 502 document fetches (15 HTTP 503 and 27 connection-level nulls) and run 2 lost 43 of 502, a DIFFERENT subset — which is what proves the loss is transient and not a permanent gap. Nothing re-attempted, so the day stayed incomplete until the next run closed it (R19's no-margin pace is the cause of the 503s, R20's convergence the long-run floor). **The decision is the driver's.** `news_provider::collect`'s queue loop re-queues a failed, non-permanent attempt to the BACK of its `VecDeque` with its retry count incremented, bounded by `MAX_TRANSIENT_RETRIES` = 2; the count rides a `Queued` wrapper and never `Request`, which is hashed into `filter_params`, so a retry asks the byte-identical question and the fresh ordinal alone makes its `attempt_id` distinct. **Re-queue-to-back was chosen over an injected `Sleeper` backoff** so no second clock enters the driver: a retry's socket call is just another `attempt_once` → `exchange.send`, the one seam `Wire::send` paces through `Pace::reserve` (R19), so the limiter already spaces the retry behind the other queued calls and the rate stays wire/budget's, untouched. A bypass of the limiter is impossible here by construction — there is one `exchange.send` call site — rather than by a test. **The classification reuses what exists.** `Attempted::is_transient` is `OutcomeKind::Failed && !permanent`, which is exactly §5.1's retryable class (`vendor_unreachable`): a 503 or a transport error is transient, a 4xx or a `vendor_refused` (`permanent`) is not, and `Ok`/`Empty` are not failures — no new classifier, and a permanent refusal is NEVER re-queued, because retrying one wastes calls and reads as abuse. **The bound is a MECHANISM cap, not a rate**: 2 is a bound on the work spent per request, its only job to stop one persistently failing accession stalling the run, and it is NOT derived from the ~8% loss — picking a retry count off the measured rate would be choosing a threshold after seeing the result, which this project forbids; whatever still fails after it is a coverage gap R20 converges or the next day fills. **Each attempt, including each retry, is its own row (R4):** a retry writes its own `Opened`/`Finalised` and gets a fresh ordinal, so a retried-then-succeeded accession ends as one or more `failed` rows plus one `ok` row plus the committed item. **The exit tally counts a transient only on its FINAL, non-re-queued attempt:** `Collected::absorb` skips the outcome buckets while a transient is being re-queued, so a retried-to-success day is complete (exit 0) and a RESIDUAL transient leaves it incomplete (exit 7/8), unchanged for the residual case; the attempt row and its stored evidence are written either way. **The per-source summary diverges from the exit on purpose:** `tally_of` builds `per_source.failed`/`ok` from the attempt ROWS, so a retried-to-success day carries `exit 0` with `per_source.failed` ≥ 1 (the superseded transient) — `per_source` summarises what was attempted, the exit is the final disposition, and the two answer different questions rather than reconciling once a retry happens. **Three tests and seven mutations, run 2026-11-13 and 2026-11-14.** `a_transient_fetch_failure_is_retried_and_commits_the_item_within_one_run` — a 503-then-filing commits both filings in one run at exit 0 with two attempt rows for that url, `failed` then `ok`; setting `MAX_TRANSIENT_RETRIES` to 0 turns it red (one ask, one item, exit `partial_collect`), dropping the re-queue turns it red the same way, and counting a re-queued transient in the tally (removing `absorb`'s `!retrying` guard) turns it red as `partial_collect` with both items still committed. It also pins the per-source divergence (`exit 0` with `per_source.failed == 1`), so reconciling `per_source` to the exit — dropping the superseded transient from `tally_of` — turns it red. `a_permanent_fetch_failure_is_attempted_once_and_never_retried` — a 403 access-denied is asked exactly once; dropping the `!permanent` guard turns it red, the url then asked `MAX_TRANSIENT_RETRIES + 1` times. `a_request_that_fails_transiently_every_time_is_bounded_to_n_plus_one_attempts` — an always-503 url is asked exactly `MAX_TRANSIENT_RETRIES + 1` = 3 times and the run terminates with the gap residual; widening `retries < MAX_TRANSIENT_RETRIES` to `<=` turns it red (four asks), and dropping the bound entirely makes `collect` non-terminating against the fixture, verified under `timeout` with the run never printing a result. | **enforced for every request the driver attempts, within the holes named here: the limiter spacing the retry is covered by R19 and that a re-queue cannot bypass it is structural, not a test, so a future second send path would be uncovered; the cap is uniform, so a request deserving more or fewer attempts gets two regardless; and a retry re-asks the identical `Request`, so a vendor needing a different url or header to recover is not helped** |
| G1 | **a workspace member inherits the lint floor** | a test in the `repo-invariants` crate, over **seven** members since 2026-11-13, `providers/edgar-press` being the newest and carrying its own `[lints] workspace = true`. It asks `cargo metadata` for the member set rather than reading `workspace.members`, because that list is **not** the member set: measured 2026-11-12, a crate reached only by a `path` dependency is a member, is compiled by `cargo clippy --locked --all-targets`, inherits nothing, and all three CI commands exit 0 over an `unwrap()` in it. The test also rejects `lints = { workspace = true }` written inside `[package]`, which cargo ignores without a warning while the crate passes clippy. It polices its own crate too. **The floor's contents, gated since 2026-11-12.** Until that day the row checked that members inherit the table and nothing checked what the table said: measured on a copy of this tree, deleting `unwrap_used = "deny"` from the root and adding an `unwrap()` to a member left `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings` (not one line of it mentioning `unwrap`) and `cargo test --locked --no-fail-fast` all at exit 0, this row's test included, and the failure message would have printed a floor the root no longer held. The check now reads `[workspace.lints]` from the root manifest, builds that message from what it read instead of from a constant in the gate crate, refuses an entry at `warn` or `allow` because such an entry decorates rather than binds, and compares the set with `repo-invariants/lint-floor.snapshot`. The same mutation now exits 101 naming the entry. **Why a snapshot and not a derived expectation:** every expectation derived from the manifest disappears together with the entry it was meant to protect, so a deleted lint is only visible against a statement of the floor that lives outside the manifest. The snapshot is that statement and the one deliberate second copy of the list; editing it is the change, and the message says so. **The hole it deliberately left is half closed since 2026-11-13, by a glob rather than by a test:** `members` reads `providers/*` and `services/*`, so a crate added under either is a member by existing. A crate on disk that no `members` entry and no `path` dependency reaches is still not a member, is compiled by nothing and is invisible to all of CI — reproduced 2026-11-12 with `services/derive` holding an `unwrap()` and no registration, all three commands green — and that is now only reachable for a new TOP-LEVEL crate, because `*` at the root would match `docs`, `probes` and `target`. The glob's own cost, measured 2026-11-13 on cargo 1.99.1: it matches every directory under those two parents, a dotfile one included, and a directory with no `Cargo.toml` fails EVERY cargo command with "failed to load manifest for workspace member ... referenced via `services/*`" rather than being skipped. A loud failure instead of a silent omission is the trade. The member set it produces is unchanged, 7 members before and after, and this row's test still means what it did: the glob adds no `[lints]` stanza, so every check in this cell runs over the same seven manifests. | **enforced** |
| U1 | **the panel names the roster it was built from** | nothing, and the gap is now one-sided rather than total. **The COLLECTION side has a producer and a column since 2026-11-13:** `universe_version` is a field of every `news_provider::Request`, it is one parameter inside that request's `filter_params`, and `attempts.universe_version` is `NOT NULL`, so an attempt cannot be filed without naming the roster it filtered by. Two mutations that day: dropping the parameter turns `two_runs_whose_rosters_differ_have_two_filter_params_digests` red, and writing a constant into the column turns `the_opened_row_carries_the_roster_digest_the_request_was_built_from` red. The §9 claim that a roster is a derive-side input is false for an EDGAR-shaped provider and is amended there: the daily index carries no ticker, so the filter is a CIK roster lookup before any per-filing request, and an absent, unparseable or empty roster is a refusal (`edgar-press::Refusal`) rather than an empty universe - the predecessor's own roster read returns `{}` on a read failure and the run then reports a quiet day. **The PANEL side is still nothing:** measured 2026-11-12, `panel-contract schema --format=json` lists 13 manifest fields and not one is a roster, and `panel-contract validate panel-contract/fixtures/corrected_panel_78460.json` exits 0 with zero findings over a manifest that names no universe. The same 13 lack `relation_map_version` too, so §2 names five inputs to `panel_id` where the gate requires three. The enforcement point is those fields becoming required in the crate, which also rewrites seven committed fixtures and R8's rule set — a decision, not an omission | **no enforcement point yet** |
| E1 | an error code is never renamed, and no code reaches stderr from outside the table | nothing yet for the stderr codes of §5.1; the check there is a workspace member `error-codes/` holding `const CODES` beside a committed `codes.snapshot`, a test that every snapshot line survives with the same exit status and retryable flag, and an error constructor taking `&'static Code` so a bare literal does not compile. **The renaming half is already closed for the one vocabulary that exists.** `panel-contract` publishes 15 `Finding` codes on stdout, which §5.1 keeps out of this registry, and each is asserted by at least one test: measured 2026-11-12 by renaming each code in the production half alone, all 15 turn a named test red (`cargo test -p panel-contract --lib` exits 101, 20 passed and 1 failed, for every one). So `error-codes/` is mostly the **add** path — a sixteenth code still needs no table entry and no review. Repurposing is not checkable at all, because no test reads meaning. | **no enforcement point yet** for §5.1's codes |

---

## 9. Alternatives considered and rejected

| alternative | why not |
|---|---|
| eleven services, one per bounded context | each deployable is another deploy, log, contract version and place something goes quiet. The predecessor has a recorded case where a boundary between components *removed* a standing alert instead of raising one. The eleven contexts are drawn so the seams exist later. |
| Kafka for the event bus | solves throughput we do not have — a few hundred items a day is one event every few minutes — and adds infrastructure a one-person system must then operate. A Postgres outbox is already durable and is itself the replay log. |
| Common Crawl's CC-NEWS as the archive | probed and rejected: 7 of our own 287 items for one test day, 2 of our 17 publishers crawled at all, zero tier-1 US financial outlets in 72,000 records, 4.19 TB for ten months, and no index at all, so no URL lookup and no aimed range request. |
| **collecting the news ourselves** — publisher RSS, EDGAR, or crawling the open web — instead of buying it | **Not rejected, and not decided either; the row is here so the default is a choice rather than a silence.** The pre-registered usability bar is a median article body of 2,000 characters, because R17 matches over body text (`probes/vendor_probe/PREREGISTRATION.md`, Q3). Against it, the predecessor's own RSS path has a median body of **117** characters — short of the bar by more than an order of magnitude, so RSS alone is a snippet feed whatever it costs. Its EDGAR path clears the bar on either of the two figures the probes carry, and they disagree: `panel_recount.py` and memo §1.9 say 50,622 characters, `matcher_audit.py` says 14,039. The 14,039 one IS re-derivable today in one command through the compiled reader and no Python: `newsvault day 2026-07-01 --lake ~/thematic_news_lake --format json`, grouping body lengths by source, gives `edgar_press_release` n=103 median 14,039 characters (and `rss` n=103 median 120, `polygon` n=76 median 426, `perplexity` n=5 median 235), re-derived 2026-11-12. It is one day's median and therefore provisional. The 50,622 one is not re-derivable: `panel_recount.py` dies on an uninstallable import (`probes/README.md`). So "in-house" has to name its sources before it can be costed: EDGAR-only, RSS and crawling the open web have different bodies, different volumes and different costs, and CC-NEWS in the row above is the cheap version of the third. This row does **not** pick — build-or-buy is the owner's, open alongside D-1. |
| a single news vendor for both the live stream and the archive | no vendor reviewed does both: the one with a 2014 archive filters on publication date only, and the one documenting an index-time filter has it behind a quoted Enterprise plan. So provenance labelling is necessary rather than avoidable. |
| incremental salience, updated per event | makes the result depend on arrival order, which destroys the determinism the panel contract is built on |
| the statistics tier as a separate repository | the panel handover is where the worst measured error happened, and a test can only span it from inside one tree |
| vendor dedup and vendor entity tags as ours | unauditable editorial decisions. Recorded as data, never adopted. |

---

## 10. What this document leaves open

Nothing of the shape. The two numbers this section last asked for are recorded below; what remains
open is the memo's own list, where decision D-1 (which news vendor) blocks the rest.

**Three decisions taken by default on 2026-11-13.** The owner was asked twice and chose neither, and
said to build anyway, so each is the design's own recommendation and is an assumption rather than a
settled fact. Both are one edit to reverse.

- **D1, what `1` means.** `1` is **the answer is no**, so `newsvault ledger --check` exits 1 on a
  coverage shortfall and `panel-contract validate` keeps its verdict there, while `input_unparseable`,
  `input_schema_mismatch` and `io_failed` move to `2` (§5.1). Evidence that it is safe: `git grep -n
  newsvault -- '*.yml' '*.py' '*.sh'` returned two hits on 2026-11-13, a comment in `ci.yml` and a
  docstring in `probes/parity_baseline.py`, neither of which runs the binary. Since that day there
  IS one caller in the tree, `probes/newsvault_parity.py`, and it reads `newsvault day`'s status:
  `0` is a result it compares, `2` is a day that reader refused. It never reads `1`, which `day`
  cannot return. Nothing can be known about a caller outside the tree. **The other fork is two functions:** `exit_code` in
  `services/newsvault/src/main.rs`, beside the constants above it, decides every FAILURE exit, and
  `news_provider::Exit` decides every exit a successful `collect` takes, including the companion
  rule about stdout. Nothing else branches on an exit status.
- **D3, the partition key.** The **publication day**, `items/valid_date=<D>/`, because the label is
  then a true statement about the rows under it. Its cost is §5's own row. The cost of not having it
  is measured: 9 of 287 items in the predecessor's `session_date=2026-07-01` partition were published
  outside that UTC day, 3.14%. The alternative was a seq range, which can be declared complete and
  whose day-query scan cost nobody has measured. **The fork is one constant:** `VALID_DATE` in
  `services/newsvault/src/store.rs`, which `covered_by_a_segment()` now builds from rather than
  repeating — review 2026-11-13 found the expression written out three times and changed one copy to
  a different key with all 279 tests green. `publish::partition_of` is the only place it becomes a path. Since 2026-11-13 the key
  reads `COALESCE(valid_at, valid_day, valid_refused)`, so a day-dated row is partitioned under the
  day it names rather than under an instant nothing published.

- **D5, a backfill with no probe behind it.** Refused, not dated. A `vendor_archive` window is
  refused before its first request unless a `MeasuredLag` with a probe id, a non-zero probe count
  and a non-zero lag stands behind it (§8 row R11). **The lag that stands behind EDGAR, measured
  2026-11-13:** `94,922 s = 26.37 h`, the MAX of 22 per-filing lags over the two days the vendor's
  quarter listing named, each lag being the instant the vendor states it last wrote
  `form.<day>.idx` minus that filing's own `<ACCEPTANCE-DATETIME>`. The registration, the run log
  and the four refusals the run did not reach are `probes/observation_lag/PREREGISTRATION.md`; the
  per-filing numbers are in `observation_lag_results.json` beside it; the run spent 27 requests of
  a registered 30. It falls in the registered band `24 h - 7 d`, so **the backfill is permitted
  only with the session shift written into the panel and no day column may mix backfilled and live
  items** - every backfilled item's membership moves by more than one session. The scalar is the
  max and not the registered p90, which would have been `57,561 s` and inside 24 h: that choice was
  made and exercised offline before the first request and is this document's reason for the harder
  band (the registration's own amendment dated 2026-11-13 carries the arithmetic). Its own
  disclosure field is now empty by construction: `leaks` counts sampled filings above the scalar
  and a max leaves none, so `leak_rate()` is `0.0` for this source and says nothing about the
  vendor - the quantity that does is the count of index rows whose `Date Filed` is not their own
  index's day, **0 of 384 and 0 of 549** on these two days.
  **How the number reaches a run, since 2026-11-13.** A hashed policy file the caller names,
  `--lag-policy`, holding one `MeasuredLag` per (source, provenance) - keyed by both, because the
  same adapter read live and read earlier are two populations (R10) and one lag does not describe
  both. `news_provider::lag_policy::LagPolicy::read` is its only constructor, `cmd_collect::
  open_provider` hands the entry to `EdgarPress::with_observation_lag`, and the bytes are copied
  into the archive at `<store>/policy/<sha256>.json` through `File::create_new` before the plan, so
  a run that dated items keeps the file that stated the lag. The run envelope carries
  `observation_lag_policy_version`, `observation_lag_probe_id` and `observation_lag_seconds`, or
  `null` for a run nothing dated. The ENVELOPE and not the item row: §3.1's `Item` names no
  observation-lag field, so a stored item is traced to the probe through its `attempt_id` and that
  run's envelope, and no column was invented for a reader that does not exist. Measured 2026-11-13 against live EDGAR with the policy in place:
  window `2026-11-12` on run date `2026-11-13` planned 1 index request, took the `vendor_archive`
  arm on all 5 attempts, committed 4 items from a 3-CIK roster out of 7,805 vendor rows, and spent
  5 requests; the same command without `--lag-policy` refuses at exit 2 with `no lag entry`
  before any request. The committed policy is `news-provider/policy/observation-lag.json`.
  **The half that is still open, and it is a decision rather than an omission.**
  `Visibility::from_backfill` has **no caller anywhere in the tree** (grep 2026-11-13, the only
  mentions being `pit`'s own definition and a doc comment), so the measured lag opens the gate and
  dates nothing: every stored row goes through `from_live` and a `vendor_archive` item's
  `observed_at` is the fetch clock. Measured on the run above, the four committed items carry an
  `observed_at` of `2026-11-13T17:44:5x Z` against `valid_at + 94,922 s`, and the difference is
  **not one sign**: `-5.81 h`, `-5.82 h`, `+2.17 h`, `+2.16 h`. The two negative rows are the max
  scalar being loose for a row whose own lag was `5.85 h`, not the clock claiming too much - the
  index that carried them was written `2026-11-13T03:02:22Z`, before the fetch, so the fetch clock
  never asserts knowledge we did not have. The fork is which instant a backfilled row carries: the
  fetch clock is a RECORD of when we in fact read it and goes years late on a real backfill, while
  `valid_at + seconds` is the COUNTERFACTUAL a point-in-time study wants and is one number for
  every row of a source, loose by the spread of acceptance times inside a filing day. **The one place to change it** is
  `newsvault::store`'s row builder, the only caller of `Visibility::from_live` on the commit path.
  **The other fork is three places:** `news_provider::provider::probed_lag` holds the four
  conditions, `edgar-press`'s `plan` is the one caller that asks, and `LagPolicy::read` decides
  what a policy file may say.
- **The provenance boundary, which D5 needs and no measurement places.** A window whose last day is
  on or before the run's own UTC date is a live ingest and an earlier one is a vendor archive. An
  assumption, not a figure: nothing has measured how long an EDGAR daily index keeps growing, so
  there is no quantile to put here. **The one place to change it** is
  `edgar-press`'s `Provider::provenance`, which is also where its fail-closed arm lives - a window
  nothing can read takes the archive arm, the one that refuses.
- **The backfill coverage convergence, a vertical slice 2026-11-13 (§8 row R20).** Three choices,
  each changeable in one place. **Which arm converges:** a `vendor_archive` follow-up whose story
  the store already holds a committed item for is dropped, while a `live_ingest` one is always
  issued (R16), so a re-run fetches only the uncovered accessions. **The one place to change it** is
  `edgar-press`'s `EdgarPress::follow_ups_to_issue`, the provider's own method - a second provider
  with a different vendor-row identity maps a follow-up to its story there and nowhere else. **The
  denominator:** `MAX(matched)` over a window's index attempts, because each run's index records the
  same in-universe count and `SUM` would multiply it by the number of runs. **The one place to
  change it** is `newsvault::store`'s `coverage_up_to`. **The gate:** `newsvault ledger --check`
  raises `coverage_incomplete` whenever `covered < planned`, on a gap of ANY size - no percentage and
  no tolerance, because a threshold chosen after seeing a result is the thing this project forbids.
  **The one place to change it** is the `coverage_incomplete` push in `cmd_ledger`'s `read`.
- **The transient-retry bound, taken by default 2026-11-13 (§8 rows R19, R21).** A document fetch
  that fails transiently (HTTP 503 or a transport error) is re-attempted within the run, bounded by
  `MAX_TRANSIENT_RETRIES` = 2. The bound is a MECHANISM cap on the work spent per request - its only
  job is to stop one persistently failing accession stalling the run - and is NOT a figure read off
  the ~8% transient loss measured that day, because deriving a retry count from the observed rate
  would be choosing a threshold after seeing the result, which this project forbids. Whatever still
  fails after it is a coverage gap R20 converges or the next run fills. **The one place to change
  it** is the `MAX_TRANSIENT_RETRIES` constant in `news_provider::collect` (re-exported from the
  crate root); the rate it is spaced at is R19's, not this knob's.
- **The per-host pace, taken by default 2026-11-14 (§8 row R19).** Three choices. **The key** is
  the request url's host, lowercased, and not the source and not the registrable domain, so two
  sources asking one host share a bucket and `sec.gov` beside `www.sec.gov` would not. **The
  order** is host wait first, then the per-IP reservation, so a slow host never holds an IP slot
  other hosts queue behind. **The one place to change both** is `Wire::wait_out_the_host` in
  `news_provider::wire`, with the file name in `BudgetScope::file_name`. **The SEC declaration**,
  100 ms per host for `www.sec.gov`, the same interval as its per-IP pace, lives in `edgar-press`'s
  `transport()`; setting it to `None` returns the run to the per-IP pace alone.
- **The run's own key, taken by default 2026-11-13.** A run id is `<schedule slot>-<at as
%Y%m%dT%H%M%SZ>`, or `adhoc-<at>` with no slot, and `run_date` is the UTC date of the run's own
instant for BOTH, because no schedule registry exists to take a slot's date from (accepted attack
A1.7 asked for the slot's date where there is one). **The one place to change it** is `run_id` in
`services/newsvault/src/cmd_collect.rs`.

**The calendar, the listing and the roster are FILES the caller names, taken by default
2026-11-13.** The quarter listing is a vendor payload and fetching it would be an HTTP call, so it
would need an attempt row of its own and a third request phase; until that exists the listing is a
hashed input file like the calendar, which keeps a collection reproducible from the files alone and
leaves `EdgarPress::listing_url` with no production caller. **The one place to change it** is
`cmd_collect::inputs`.

**The vendor's timeouts and body limit, the first an assumption and the second now measured.** All
  nine timeouts are one value. The body limit is 8 MiB against a full daily index of **1,057,192 B**
  for 2026-11-10 (5,429 filing rows) and **1,520,327 B** for 2026-11-12 (7,805 rows), both read as
  received by `probes/observation_lag/probe.py` on 2026-11-13. The vendor's own quarter listing
  states those two files as `'1033 KB'` and `'1485 KB'`, rounded to KB and not bytes, so the listing
  is not a source of a byte figure. The earlier "about 600 KB" came from the 110-byte row width of a
  hand-written fixture; the real rows are 194 bytes.
  **The one place to change them** is `edgar-press`'s `Provider::transport`.

**The archive's second copy: the VPS, with the provider's own backup behind it (N1, decided
2026-11-12 by the owner).** Two parts of that stay unanswered and neither is the shape of the
system, so neither blocks it: nobody has restored the backup and nobody has timed a restore. A
backup nobody has restored is not yet a backup; one timed drill, recorded once, closes both.

**What the drill compares, and what the 156-of-156 does not say.** `probes/parity_baseline.py`
computes a sha256 over the sorted distinct **vendor ids**, the lake's `id` column (§3.1's
`source_item_id`). (1) **Not** over `item_id` (§2): that digest has producers since 2026-11-13 but
only in the new archive, so the lake number shows the id **set** came back and would match with
every title, body and url different. (2) It agreed on **156 of 156** days, re-derived 2026-11-12,
over a corpus holding none of the shapes the two readers can disagree on: across 779 run files and
243,226 rows, zero null ids, zero empty-string ids, zero null publication or ingest times, zero
inverted rows, no file missing `tickers`. So `unusable_rows` and §5's rows 4 and 5 have never fired
on real data, and the number says the lake is clean, not that two readers encode one rule.

**The comparator exists since 2026-11-13**, which closes the third thing that was not true of it
and makes the first two measurable rather than merely admitted. `newsvault day` emits an `id_digest`
and a `content_digest` — the second over every field of every item in id order, each value
length-prefixed, so a matching id set with a different title no longer passes. `probes/newsvault_parity.py`
computes both in pandas and compares every reported field day by day, exiting 1 on any difference;
its decision rule was registered before it ran (`probes/newsvault_parity/PREREGISTRATION.md`) and
its run log is in that file. The corpus it runs over is committed, 9 partitions and 11 run files,
one partition per shape the lake lacks: a null id, a real empty-string id, a null publication time,
a null ingest time, an inverted row at both precisions, a re-stamped repeat, a repeat arriving with
an earlier observation, a day-dated value beside an instant one, and a run file missing a column.
Its first run disagreed on exactly one of them and the reader changed (row R3). **CI runs no
Python**, so the gate is the probe's committed output: `services/newsvault/tests/parity.rs` is the
only reader of `expected.json`. What it still does not cover: the corpus is synthetic rather than a
sample, both readers read the same parquet bytes through two arrow bindings, and the new archive's
own rows have no second reader.

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
