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
| `title`, `body`, `url`, `lang` | strings | `body` is article text, not page HTML. **R17.** |
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
| `item_id`, `valid_at`, `observed_at`, `valid_precision`, `source`, `provenance` | the consumer makes its own point-in-time cut |
| `title`, `url`, `body_ref` | |
| `schema_version` | the item contract, versioned apart from the storage row |

Plus a **heartbeat** carrying `seq`, an instant and items-since. Silence must be distinguishable
from a dead stream, because a consumer reading our outage as "no news" turns our failure into theirs.

No delivery guarantee and no latency promise. The archive is the source of truth and reconciliation
is an HTTP read of it; a latency promise would need a freshness number the design does not yet have.

### 3.8 `Reading` — `ledger`, one-way

`ledger` consumes briefs, prices and realised outcomes and produces readings. It holds no edge back
upstream: a service that sees outcomes must not influence the service that produces proposals, or
selection happens under the result. The absence of that edge is enforced in §8, not trusted.

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
| R1 | archive complete, caps are views | `vault` has no truncating write path; the cap lives in a view builder | design |
| R2 | valid and observation time are distinct types | the type system; a `compile_fail` doc test pins it, and CI runs it | **enforced, but narrower than §3.1 claims** — see the correction below |
| R3 | first sighting wins | unit tests in `newsvault` | **enforced** |
| R4 | coverage ledger beside every store | a write to the archive without an `Attempt` row fails the store's own test | design |
| R5 | failed calls must be handled | `cargo clippy --locked --all-targets -- -D warnings` in CI, over the workspace lint set: `unused_must_use` denied under rust (which also makes a local `cargo check` fail, not merely warn) and `let_underscore_must_use` denied under clippy, which is the only form the move newly catches in CI. Never the type system. | **enforced against the accidental case only** — three escapes measured 2026-11-12: a violation inside a local `macro_rules!` passes; a crate- or module-level `#![allow]` switches any of these off and no command notices; and doc tests are reached by nothing, because `--all-targets` does not cover them. |
| R6 | missing input aborts | unit tests cover the missing root and the missing day. But `lake.rs` `string_list_at` has five `Vec::new()` exits, four of which swallow a real decoding error while `unusable_rows` stays 0 — so the loader does have empty-fallback branches. | **partly** |
| R7 | units are types, identity is a content hash | nothing. `grep -rn item_id services/newsvault/src` returns no match: there is no derivation and no test. Unit newtypes are also absent. | **no enforcement point yet**, both halves |
| R8 | presence counted, never multiplied | the panel manifest requires `rows` and `exchangeable_units`; a CI check rejects a manifest whose row count equals a product of two cardinalities | **no enforcement point yet** |
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

**Correction, 2026-11-12, after an adversarial review.** Four rows above said **enforced** and did
not earn it, and one structural problem outranked all of them: before this date there was no CI, so
"enforced" meant *a test exists*, not *a test ran*. The project's own principle — a gate nobody
replaced is a rule that quietly stopped applying — covers a gate nobody runs. A twenty-line workflow
now runs `fmt`, `clippy -D warnings` and `test --no-fail-fast`, and the flag matters: measured on
this tree, one failing unit test made plain `cargo test` exit without ever running the doc test,
which is R2's only enforcement point.

The R2 wording in §3.1 and in the README was also wrong, and the sentence is deleted rather than
defended. "There is no public way to ask only half of it" is false: `Visibility::valid()`,
`observed()` and `Cutoff::inner()` are public, so the publication-only predicate is one line from
any crate — demonstrated by compiling it. What is true is narrower and still worth having: the two
instants cannot be compared **with each other**, so the 60.2% leak cannot happen by type confusion.
A claim nobody can enforce is worse than an accessor.

**Second correction, 2026-11-12.** The lint floor moved from the one package's manifest to
`[workspace.lints]` at the root, because the root's own comment says four more service crates arrive
and a package table covers exactly one of them. That closes the "a new crate inherits nothing" half
of the problem and exposes the other half, now row G1: inheritance is opt-in, so a crate that omits
the stanza is silently unlinted and CI still goes green. Two further escapes were measured and are
named in the R5 row rather than left to be rediscovered, and a third was closed in the same change
by denying `todo`, `unimplemented` and `unreachable` — `clippy::panic` covers only `panic!`, so until
then all three passed.

**What a lint can and cannot be.** These are a floor against the accidental case. None of them
survives an author who writes `#![allow(...)]` at the top of a file, and no command in this gate
notices that they did. Section 8 should not be read as saying otherwise.

**Third correction, 2026-11-12.** G1 is closed, and closing it overturned the design it was given.
The obvious gate — read `workspace.members` and check each entry — would have reported every member
compliant while cargo built an unlinted third crate, because a `path` dependency from inside the
workspace makes a member without appearing in that list. The gate asks cargo instead. Two traps are
pinned by tests rather than described: that case, and `lints = { workspace = true }` inside
`[package]`, which cargo silently ignores.

One hole stays open and is deliberately not this gate's job: a crate on disk that no `members` entry
and no `path` dependency reaches is not a member, is compiled by nothing, and is invisible here as it
is to all of CI. Reproduced on this tree — `services/derive` with an `unwrap()` and no registration,
all three commands green. A glob in `members` removes the mistake instead of testing for it, and
that is a separate change with its own behaviour to argue about.

**Fourth correction, 2026-11-12.** R12 was called the most serious gap in this document for most of a
day, and it was not a gap in this repository at all. An AST check forbidding order-placing calls, in
a repository with no broker code, passes unconditionally — the same shape as the four enforcement
claims corrected above, except this one was written here rather than inherited.

Worse, the invariant was wrong as well as unenforceable. "No path to the order queue" protects
nothing: a path is harmless. The thing the predecessor deleted on 2026-09-24 was an **automatic
brief-to-intent producer**, and a mediating service between this repository and the existing executor
is a perfectly good idea as long as its input is a decision a person made rather than a brief. That
is why every pick in the predecessor carries `"source": "manual"` — the label is not provenance
metadata, it **is** the invariant, written into the data.

So R12 is split. R12a moves to the mediator that does not exist yet, and leaves this document rather
than sitting in it as a permanent unmet gap. R12b, the half that is real here, stays: the brief
schema has no field an executor would accept.

Two gaps now, and they are not equivalent. **R8** and **R9** protect the number that was wrong by a
factor of three, and their checks are cheap: a manifest validator and a refusing consumer.

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

Beyond the memo's own open decisions:

- **No wire formats.** Field names and types are given; the serialisation is not. Arrow schema and
  JSON shapes come with the first service that has a second consumer.
- **No error-code registry.** §5 gives behaviours, not stable codes, and the CLI contract wants a
  registry whose codes are never renamed or repurposed.
- **No retention policy.** How long raw blobs live, and on what medium, is memo question N4 and the
  answer changes the storage design rather than a setting.
- **No backup design.** Memo question N1. The archive is called irreplaceable in both documents and
  neither says where its second copy lives. Of everything unresolved here this is the one whose
  absence costs most if it is still absent in a year.
- **No freshness target**, so §5's lag-and-backlog detection has nothing to alert on yet. Memo
  question N2.
- **The two enforcement gaps of §8**, R8 and R9. R12a left the list on 2026-11-12 because it belongs to
  a service that does not exist; it is not closed, it is relocated, and §7 says where.
