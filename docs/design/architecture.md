# Architecture

- **Status:** DRAFT 2026-11-11, last amended 2026-11-15.
- The level below `successor_system_design_2026_11_11.md` (the memo). Read the memo first.
  - The memo names five services, their boundaries and requirements R1–R18, each paired with the
    measurement that forced it (memo §2.1, §2.1b).
  - This document says what crosses each boundary, in what shape, and where each requirement is
    actually enforced.
- R-numbers: R1–R18 are the memo's; R12a and R12b split the memo's R12; R19–R21 exist only in §8.

---

## 0. What this document decides, and what it does not

- **Decides:**
  - the artefacts that cross service boundaries and their required fields;
  - which boundary is a file and which is a call;
  - how identity and versioning work, so that any number can name the inputs that produced it;
  - what each service does when its input is missing, stale or partial;
  - for each requirement, the single place it is enforced (§8).
- **Defers to the memo:** why there are five services, why the build is parallel to the
  predecessor, and the decisions still open there.
- **Excludes:** the order-placing path and the front-end.
  - Neither is in this repository and neither is reachable from it; §7 states that as a structural
    property, not a policy.
- **The test this document is written against:** an architecture is mature when an invariant cannot
  be stated without naming where it is enforced.
  - §8 is that table and the part worth attacking first.
  - §8's status column, and no sentence anywhere, is the count of what is enforced.

---

## 1. The five services, in one line each

| service | charter | code |
|---|---|---|
| `vault` | Owns every fact we did not derive, and the only conversation with a vendor. | `services/newsvault`, `news-provider`, `providers/*` |
| `derive` | Pure function of (snapshot, rule set). Nothing here reads a clock or a network. | none |
| `modelgate` | The only exit to a language model. Spends money, answers non-deterministically. | none |
| `propose` | Turns a panel into a ranked shortlist and the document a person reads. | none |
| `ledger` | Reads outcomes, which nothing upstream may see. One-way edge. | none |

- Each service is one deployable.
- Statistics stay in Python, inside this repository, on the far side of a file boundary from
  `derive` (§3.4, §4). Memo D-4 is why they are not a sixth service and not a second repository.

---

## 2. Identity and versioning

- Nothing is named by where it came from; everything is named by what it contains.
- One exception: a dedup key must survive a correction of the contents, so `story_id` names the
  vendor row.
- This is what makes R18 — two panels, two rule versions, diffed — a command, not an afternoon.
- **The hazard that cheapness creates:** a new `rule_version` costs a command, and nothing counts
  how many were tried against one snapshot.
  - Precedent: the postmortem's Pattern 6, 26 tests with Bonferroni at n=2 where true n was ≥ 15
    (`docs/inherited/paradigm_failures_postmortem.md:173`); there a rule change was at least a
    countable commit.
  - The smallest count: a registration field on the manifest and an append-only ledger of the pairs
    a `derive diff` compared. Neither exists, and `derive diff` is itself `design` (§8 row R18).
  - Whether it gates is the owner's.

| name | derivation | why not something simpler |
|---|---|---|
| `item_id` | sha256 over `source`, `source_item_id`, `url`, `valid_at` (with its precision), `title`, `body_ref`, in that order (`canon::digest::item_id`) | A per-process hash is not an identity: the predecessor named typed artefacts with Python's `hash()`, randomised per process, so a resumed run could not recognise its own prior output and would have rewritten 5,756 records. The **content** witness and nothing else: a re-stamped time or an edited title is a new `item_id` under one `story_id`, which is how "the bytes changed" is visible at all. |
| `story_id` | sha256 over (`source`, `source_item_id`) (`canon::digest::story_id`), the vendor's own identity and the one part of a row a re-stamp does not touch | **The dedup key (§5 row 5) and R15's join key.** `item_id` cannot be either: it hashes the publication value and `title`, the two fields under dispute, so a re-stamp would mint a fresh item instead of a disputed twin and R15 would have nothing to attach to. Measured 2026-11-12 over the lake §10 describes: all 501 ids appearing in more than one partition keep one url throughout, and every one of their 1,225 repeat appearances carries a publication time that id did not have before; median span between an id's first and last stamp 4 days 15:16, worst 159 days. |
| `seq` | monotonic integer assigned by the store on commit, since 2026-11-13 from a committed `seq_reservations` row | **The only ordering authority.** Wall clocks are evidence, never order: a fetch issued at 10:00 that commits at 10:01:10 carries data through 10:00, while one issued at 10:01 has already committed data through 10:01, so ordering by commit time returns the older data as current. Read by `publish::read_day` through one helper keyed on `seq` across every segment of a partition (§8 row R13); mutation 2026-11-13: keying it on `committed_at` turns `the_ordering_helper_sorts_on_seq_and_leaves_no_row_behind` red. `newsvault day` reads the predecessor's lake, which has no `seq`, and sorts by (publication time, id): presentation only, so no consumer may read that order as commit order. |
| `snapshot_id` | sha256 over the cutoff, then the list of `item_id` in scope in ascending `seq` (`canon::digest::snapshot_id`). No production caller (§8 row R7) | A date range is not reproducible: the archive grows under it. A snapshot is a set. The list and the cutoff are also manifest fields (§3.0), because neither can be read back out of a digest. |
| `rule_version` | sha256 of the rules file's bytes (§3.3) | Rules as constants in code cannot be diffed or tested: when a hand-written gazetteer wrongly removed a real company, there was no version, no test and no record (§3.3). |
| `calendar_version` | sha256 of the session calendar file, half-days and DST included | A half-day close is a fact that changes results and is easy to get silently wrong. Its own file, not a group inside the rules file (§3.3): a calendar edit and a rule edit are then two diffs, which R18 needs. |
| `hosts_version` | sha256 of one SourceId's hosts file (§11.1), `OpenWebFeed::hosts_version`. Carried by `news_provider::Request.hosts_version` through `AttemptEvent::Opened` into `attempts.hosts_version`, outside `filter_params`; every `open-web-feed` request carries it (`every_planned_request_is_a_live_index_carrying_the_hosts_version`); `newsvault collect --source open_web_wires` writes it to every attempt row and to its envelope's `hosts_version` (null for `edgar_press`): `the_run_envelope_names_the_hosts_file_it_was_allowed_to_use`, and over the real GNW index with 4 attempt rows `an_open_web_run_collects_a_globenewswire_day_end_to_end` (mutation 2026-11-15, envelope field null: both red) | Names which hosts and paces a run was allowed to use. Kept apart from `universe_version`, the ticker roster. Rejected: inside `filter_params`; it decides admission and pace, not what the vendor is asked, and edgar's `filter_params` and `attempt_id` digests stay byte-identical (`two_runs_whose_rosters_differ_have_two_filter_params_digests` and the `canon` vectors unchanged, 2026-11-15). Outside `filter_params`: `requests_that_differ_only_in_hosts_version_have_one_filter_params_digest` (`None`, `Some(a)`, `Some(b)`; mutation 2026-11-15, folded in when `Some`: red, green before the test). `attempt_id` takes run, source, window and ordinal only (`canon::digest::attempt_id`), so no request field reaches it |
| `universe_version` | sha256 of the roster the panel's tickers are drawn from, as of the snapshot's cutoff, with delisted names either included or named as absent | **Survivorship, measured by the predecessor in both directions:** 18 tickers added retrospectively on 2026-04-19 contributed about 0.3 of an OOS α t, and a PIT-95 sub-universe moved that t from 1.70 to 1.36; including delisted M&A targets flipped a train α t from +0.80 to −0.86 (`docs/inherited/paradigm_failures_postmortem.md:64` and `:67`). Without the roster as a named input a panel is reproducible and survivorship-biased at once, and `panel_id` hashes it happily. Enforcement: §8 row U1. |
| `relation_map_version` | sha256 of the relation map `derive` reads (§4) | `has_relation` is the statistics tier's control arm and a model build sets it. Without this input a flipped control arm cannot be attributed to the build that flipped it; `brief_id` already does that for prompts. |
| `panel_id` | sha256 over (`snapshot_id`, `rule_version`, `calendar_version`, `universe_version`, `relation_map_version`, row set); the gate requires three of these (§8 row U1) | A panel that cannot name its inputs cannot be re-derived, and a number whose panel cannot be re-derived is an anecdote. The row set means §3.10.1's rows in canonical form, not the Arrow file's bytes. Measured 2026-11-12 over one four-row panel conforming to §3.10.1, two counterparties in its first row: arrow-rs 60.1.0 writes 2,578 bytes, pyarrow 24.0.0 writes 2,050, and two record batches make 2,634, so the container cannot be the digest input. Those sizes hold for that panel's own strings and metadata and no fixture is committed: a reader's own four-row panel reproduces the **disagreement**, not the sizes (pyarrow 24.0.0 over one built to this table on 2026-11-12: 2,202 in one batch, 2,786 in two). The disagreement is the claim. |
| `brief_id` | sha256 over (`panel_id`, scorer version, prompt versions, candidate set) | The proposal a person acted on must be recoverable exactly. |

- **Canonical form: the convention every digest above takes, except those over a file's bytes**:
  `rule_version`, `calendar_version`, `universe_version` and `hosts_version` (`canon::FileDigest`).
  - Rejected: two implementations of one digest rule. The parity run paid for it (memo §5.1: a
    mismatch on all 156 days, a locale sort against code-point order).
  - Fields are rendered UTF-8, NFC-normalised, joined with `0x1f`, no trailing separator.
  - A list joins its members with `0x1e` in the order its own field states.
  - An instant is RFC 3339 UTC `Z` at microsecond precision beside its `valid_precision` (§3.1), so
    a `day` value and an `instant` value cannot collide.
  - A `minute` value is its first microsecond in that same format beside the tag `minute`
    (`canon::Stamp::Minute`), so it cannot collide with the instant at :00
    (`canon/vectors/item_id_minute_precision.json` against `item_id_midnight_instant.json`).
  - A `minute` value with non-zero seconds or sub-seconds is refused, `CanonError::MinuteWithSeconds`:
    `a_minute_stamp_carrying_seconds_is_refused_and_a_whole_minute_is_not` (mutation 2026-11-15,
    the check removed: red). No producer passes one: each goes through `pit::datedness::Minute`.
  - An absent optional field contributes an empty string.
  - Every string ordering is over raw UTF-8 bytes, never a locale's collation.
- **The seven `canon::Domain` digests.** The variant's name is the form's first field
  (`canon::Domain::name`), so two digests whose other fields coincide still differ.

  | `Domain` | digest | field list stated by |
  |---|---|---|
  | `ItemId` | `item_id` (above) | §2 row `item_id`; `canon::digest::ItemFields` |
  | `StoryId` | `story_id` (above) | `canon::digest::story_id` |
  | `DocId` | `doc_id`: cross-source identity over a url canonicalised outside `canon` | `canon::digest::doc_id` |
  | `AttemptId` | `attempt_id` (§3.2) | `canon::digest::attempt_id` |
  | `FilterParams` | `filter_params` (§3.2) | `canon::digest::filter_params` |
  | `ResultDigest` | `result_digest` (§3.2) | `canon::digest::result_digest` |
  | `SnapshotId` | `snapshot_id` (above) | `canon::digest::snapshot_id` |

  - Six have a production caller since 2026-11-13; `SnapshotId` has none (§8 row R7).
    - `item_id`, `story_id`, `doc_id`: `Committer::rows_of`, called by `impl Archive for Committer`
      (`services/newsvault/src/cmd_collect.rs`); `story_id` also `edgar-press`'s `Provider::story_of`.
    - `attempt_id`, `filter_params`: `news-provider`'s `AttemptSlot::open`.
    - `result_digest`: `newsvault ledger`, over the `item_id`s each closed attempt committed
      (§3.2; `drift` in `services/newsvault/src/cmd_ledger.rs`).
  - Each of the seven has a committed vector under `canon/vectors/`, which is what makes two
    implementations disagree loudly.
- **Content-addressed blobs** (§3.9).
  - Raw vendor payloads are stored once under their own digest and referenced.
  - An unchanged answer costs no storage.
  - More importantly, it re-stamps nothing as current: a fresh timestamp on unchanged content is how
    "latest" starts lying.

---

## 3. The contracts

### 3.0 `Snapshot` — `vault` to `derive`, the reproducibility boundary (decided 2026-11-12)

- The manifest beside the Arrow file (§4); `derive`'s primary input. A field unstated here is a
  guess in the next service built.
- The Arrow half carries §3.1's rows; no wire format yet, on §3.10's trigger: `derive` has no code.

| field | type | why it exists |
|---|---|---|
| `schema` | `snapshot.manifest/1` | a reader refuses an unknown version rather than reading it optimistically (§3.10.2) |
| `snapshot_id` | content hash | §2 |
| `cutoff` | instant | the visibility bound: every listed item satisfies `visible_at(cutoff)` (§3.1). A field, since `snapshot_id` cannot be read back |
| `items` | count | counted, never the length of a date range (§2) |
| `item_ids` | list of `item_id`, ascending `seq` | the list `snapshot_id` digests. Ordered by `seq` (§2), never by `item_id` bytes: two orderings digest one set twice. Producible since 2026-11-13 (the store assigns `seq`); no producer: `canon::digest::snapshot_id` has no caller (§8 row R7) and `derive` has no code |
| `seq_high_water` | integer | the last `seq` committed at the cut, so a reader tells a prefix of the archive from a filtered subset |
| `provenance_counts` | one count per §3.1 `provenance` value, summing to `items` | **R10**, the disclosure clause: the mix of the items. No producer: `items` has no provenance column; a panel's own mix has no field (§8 row R10) |
| `calendar_version` | content hash | §2; the calendar the cutoff was read against, so "the close of session `d`" is reproducible |

### 3.1 `Item` — what `vault` stores and publishes

- The archive record; every consumer sees this shape, none the storage behind it.
- Stored today: `items` in `services/newsvault/src/store.rs` at `PRAGMA user_version` 3: the
  frozen v0 DDL `SCHEMA_V0` plus `MIGRATIONS[0]`, which rebuilds `items` (SQLite's table-rebuild
  procedure: foreign keys off around one transaction, `PRAGMA foreign_key_check` before commit),
  plus `MIGRATIONS[1]`, which creates `robots` (§3.2), plus `MIGRATIONS[2]`, which adds
  `attempts.verdict`, `attempts.story_id` and `attempts.held` (§3.2); exported as `publish::SegmentRow` with `source`
  joined from `attempts`; `requested_at` and `received_at` live on `attempts`, reached through
  `attempt_id`.
- Schema versioning (§11.1), since 2026-11-15:
  - a store is brought to user_version 3 in one transaction by `Committer::open`: `SCHEMA_V0`
    (`CREATE TABLE IF NOT EXISTS`) creates each v0 table the store lacks, then each
    `MIGRATIONS[i]` from the version it holds.
  - a store at user_version `n` below 3 is compared first with the columns of `SCHEMA_V0` replayed
    with `MIGRATIONS[..n]` in memory (`PRAGMA table_info`: name, type, NOT NULL, default, primary
    key); a difference is refused with `StoreError::ShapeDiffers { found, columns }` naming each
    `table.column`, exit 2, store left at user_version `n`, no repair step: §8 row M1.
  - at user_version 0 a table the store lacks is skipped, because `SCHEMA_V0` creates it whole; at
    1 and 2 every column of a missing table is named.
  - a migration step SQLite refuses is `StoreError::MigrationFailed { from, step, cause }`, exit
    2, store left at user_version `from`; `cause` is SQLite's message without the SQL batch
    (review 2026-11-15 measured 2,040 bytes of stderr, most of it the batch, before):
    `a_failed_migration_step_is_named_without_its_sql_and_leaves_user_version_zero` (mutations
    2026-11-15, the step's error left unwrapped / the SQL kept in `cause`: each red).
  - a failure inside `SCHEMA_V0` is `step: MigrationStep::CreatingTheV0Tables`, written "creating
    the v0 tables the store lacks"; review 2026-11-15 read "the step to user_version 0" for it:
    `a_failure_creating_the_v0_tables_is_named_as_that_step` (mutations 2026-11-15, labelled
    `ToUserVersion(0)` / left unwrapped: each red).
  - a user_version above 3 is refused (`StoreError::SchemaVersion`):
    `a_store_at_an_unknown_higher_user_version_is_refused` (mutation 2026-11-15, refusal removed:
    red).
  - `collect` refuses that store before its policy copy, run envelope and lease, so the store
    is left byte-identical (`store::refuse_an_unreadable_version`):
    `a_run_against_a_store_at_a_newer_user_version_leaves_the_store_unchanged` (mutation
    2026-11-15, the check removed: red; review 2026-11-15 measured a `runs/` file and a `LOCK`
    left behind before it).
  - the same check refuses a store below 3 whose columns differ from its version's shape,
    read-only (`store::refuse_an_unreadable_version`):
    `a_run_against_a_v0_store_of_another_shape_leaves_the_store_unchanged` (mutation 2026-11-15,
    the v0 check removed: red; review 2026-11-15 measured `LOCK` and a `runs/` file left behind).
  - `robots refresh` calls it before its dry-run answer, envelope and lease, so a refresh, dry run
    included, refuses that store and leaves it as found:
    `a_robots_refresh_refuses_a_store_of_another_shape_and_leaves_it_as_found` (mutations
    2026-11-15, the call removed / placed after the dry-run return: each red).
  - unguarded: a v0 store whose migration step SQLite refuses still gets `LOCK` and a run
    envelope with no `finished_at`, because `Committer::open` runs after `write_envelope` (read
    from the code 2026-11-15, not run).
  - a v0 store opens, migrates and takes a `minute` row: §8 row M1.
  - a v0 item row naming an absent attempt is refused (`StoreError::MigrationFoundADanglingReference`,
    exit 2) and the store stays at user_version 0:
    `a_v0_store_holding_an_item_with_no_attempt_is_refused_rather_than_migrated` (mutation
    2026-11-15, the `foreign_key_check` result ignored: red).
  - `items` and `ledger` open read-only, cannot migrate, and refuse every user_version but 3 with
    `StoreError::SchemaVersion`, exit 2: `a_reader_refuses_a_store_at_another_user_version`
    (mutation 2026-11-15, the check removed from either reader: red).
  - so a store below 3 is unreadable by `items` and `ledger` until a writer (`collect`, `publish`)
    opens it.
  - `store::RobotsLedger::open` (§8 row R4, robots clause) accepts any user_version up to 3, so
    `collect` reads a v2 store's `robots` table before the same run migrates it.
- No column today: `source_item_id`, `provenance` (§8 row R10), `data_through` (§8 row R13),
  `title`, `url`, `lang`, `body_ref`'s `present` \| `expired` state,
  `vendor_tickers`, `vendor_duplicate_flag`.

| field | type | why it exists |
|---|---|---|
| `item_id` | content hash | §2 |
| `story_id` | hash over the vendor row's identity | §2. The join key of **first sighting wins** (§5 row 5) and of R15's detection, because `item_id` moves when a disputed field is corrected |
| `source` | enum of adapters | provenance of mechanism |
| `source_item_id` | string | the vendor's own id, so a vendor row can be traced |
| `provenance` | `live_ingest` \| `vendor_archive` \| `bulk_dataset` | **R10.** Two retrieval models are two populations: a panel mixing them discloses the mix, never pools silently |
| `valid_at` | `ValidTime` | when the world published it |
| `observed_at` | `ObservationTime` | **R2, R3.** The earliest instant we could have known it; a distinct type from `valid_at`, not comparable with it |
| `requested_at`, `received_at` | instants | **R13.** Separate, because the gap between asking and arriving is where stale-but-fresh-looking data comes from |
| `data_through` | instant | **R13.** The latest fact the payload covers, not when it arrived |
| `committed_at` | instant | **R13.** |
| `seq` | monotonic integer | **R13.** The ordering authority |
| `valid_raw`, `valid_field`, `valid_source` | strings; `valid_source` is `vendor_published` \| `our_extraction` (`ValidSource::name`) | the evidence beside the publication value, so a replay re-parses it and a fabricated stamp has to forge evidence. Copied from `CollectedItem` by `Committer::rows_of`: `an_items_row_keeps_the_raw_publication_value_its_field_and_its_source` (mutations 2026-11-15, `rows_of` writes an empty `valid_raw` / `record` writes NULL `valid_source`: red). Nullable, NULL only on rows committed at user_version 0: such a row has no evidence and an invented one is the empty fallback; nothing yet reads them. `CHECK (valid_source IN ('vendor_published', 'our_extraction'))`: `a_valid_source_no_variant_names_is_refused_by_the_schema` (mutation 2026-11-15, CHECK removed: red) |
| `valid_precision` | `instant` \| `minute` \| `day` \| `unknown` | A publication time scraped from page metadata is often a date with no hour, which no pre-close window can take. So precision is the **shape** of `pit::datedness::Datedness`, not a column beside the value: a `day` value is a `NaiveDate` no constructor folds into an instant. A `minute` value (GNW's RSS `pubDate`, `Sat, 14 Nov 2026 19:00 GMT`) is a `pit::datedness::Minute`, whose only constructor refuses non-zero seconds or sub-seconds (`a_minute_constructor_refuses_a_value_with_seconds`, mutation 2026-11-15, refusal removed: red), stored in `items.valid_minute` as its first microsecond and never in `valid_at`; exactly one of `valid_at`, `valid_day`, `valid_minute`, `valid_refused` is set (schema CHECK). `ValidPrecision` orders `instant` < `minute` < `day`, which `no_item_is_finer_than_the_ceiling` reads: `a_minute_is_coarser_than_an_instant_and_finer_than_a_day` (mutation 2026-11-15, `Minute` after `Day`: red); `stamp_of` hands `canon` a minute: `a_minute_publication_value_reaches_canon_and_the_label_as_a_minute` (mutation 2026-11-15, an instant stamp: red) |
| `valid_disputed` | bool | **R15.** Set on the ARRIVING row when another `item_id` under its `story_id` carries a **different** publication value, from another source or the same source re-stamping its own row (the archive's case: §2, 501 ids). Rejected: "materially different", a threshold no measurement places. Never set on the twin, so the stored row stays byte-identical |
| `earliest_valid_at` | `ValidTime?` | **R15.** The earliest publication value the store holds for this `story_id`, on the disputed row only. Neither value is moved: a disputed date is an **unknown** age, never an old one. Compared within the arriving row's own column, so its precision is that row's: a `minute` row's value is a minute |
| `title`, `url`, `lang` | strings | |
| `body_ref` | digest, plus the blob's length and a `present` \| `expired` state | **R17.** Article text, never page HTML, held as a blob beside the row (§3.9), so an L1 expiry is a state a consumer reads instead of a failed fetch |
| `raw_blob_ref` | digest | the payload as received, so extraction can be redone when its rules change |
| `vendor_tickers` | list | the vendor's own tagging, **kept and never used for matching** |
| `vendor_duplicate_flag` | bool | the vendor's own dedup verdict, **kept and never filtered on** |

- The last two rows are one rule, the rejected alternative in §9: a vendor's editorial decisions
  are data, never ours. Ours is a hashed rule with a version.

**The one visibility query.**

- `visible_at(cutoff)` is true when `valid_at <= cutoff` **and** `observed_at <= cutoff`.
- Measured in a retrieval test: the half-question admitted documents not visible at the claimed
  instant in 60.2% of 69 probes; the full question leaked in none.
- How far the types make the half-question unaskable, and where they stop: §8 row R2 only.
- A `day` item is visible once the cutoff passes the end of that day **in the calendar the item
  names** and its observation is admitted (`Datedness::visible_at`); never UTC midnight, which is
  19:00 in New York and admits the afternoon.
- A `minute` item is visible once the cutoff reaches the minute's close, the start of the next
  minute (`Minute::closes`, exclusive), and its observation is admitted (`Datedness::of_minute`,
  `Datedness::visible_at`); visible from the minute's start is the fold the `day` arm refuses.
  - `pit::datedness::tests::a_minute_value_is_invisible_until_its_minute_has_closed`;
    `newsvault::cmd_items::tests::a_minute_dated_item_is_invisible_until_its_minute_has_closed`
    (mutations 2026-11-15, visible from the minute's start in `pit` / in `cmd_items`: red).
  - exclusive since 2026-11-15: before, the minute 19:00 was visible at cutoff
    19:00:59.999999Z, a whole microsecond before it closed (review 2026-11-15, temporary test).
    Both tests now assert 19:00:59.999999Z invisible and 19:01:00Z visible; the `pit` test also
    asserts 19:00:59.999999999Z invisible, because `Cutoff` keeps nanoseconds. Mutations
    2026-11-15: the last microsecond restored: both red; the cutoff read 1 ns late: the `pit`
    test red.
  - the observation half: `a_minute_fetched_after_it_closed_is_invisible_until_we_had_fetched_it`,
    `a_minute_dated_item_fetched_after_its_minute_is_invisible_until_the_fetch` (minute 19:00
    observed 19:05: invisible at 19:02, visible at 19:05; mutation 2026-11-15,
    `admits_observation` dropped from the minute arm: both red, nothing red before them).
  - stored `Undated` (`valid_refused`) only when the observation precedes the minute's START; a
    page read inside its own minute is not refused:
    `an_observation_inside_its_own_minute_is_not_refused` (mutation 2026-11-15, compared against
    the minute's end: red), `an_observation_before_its_minute_starts_is_refused_to_undated`.
  - the ledger's publication clock counts a minute from its close, `strftime(…, valid_minute,
    '+1 minute') <= at`, and reports the stored minute: `newsvault::store::newest_visible_publication`,
    `a_minute_is_not_the_newest_publication_until_its_minute_has_closed` (00:02:59.999999Z
    invisible, 00:03:00Z visible; mutations 2026-11-15, the minute's start compared / the last
    microsecond `'59.999999Z' <= at` restored: red); its observation half:
    `a_minute_fetched_after_it_closed_is_not_the_newest_publication_before_the_fetch` (mutation
    2026-11-15, `AND observed_at <= ?1` dropped from the minute branch: red, nothing red before it).
  - producer: `open-web-feed`'s document extraction, from GNW's `pubDate` under the rules file's
    exact minute format (`a_globenewswire_pubdate_is_a_minute_and_never_an_instant`), driven by
    `newsvault collect --source open_web_wires` since 2026-11-15; through the real store and
    `items --at`: `an_open_web_run_collects_a_globenewswire_day_end_to_end` (§11.5 step 2).
- An undated item is visible at no cutoff.
- The `day` arm has two producers:
  - the lake reader (`newsvault::lake::datedness`) reads a value stamped exactly at midnight as a
    date, because the predecessor's lake has no precision column;
  - `edgar-press` (`publication` in `providers/edgar-press/src/lib.rs`) falls back to the
    `Date Filed` of the daily-index row the follow-up came from (`Request.listed_date`, raw value as
    the column reads), not the index's own day, when the acceptance instant is missing or does not
    parse; for a late submission the two differ (§3.9, "One row of a daily index is not dated by
    that index"): `a_late_row_with_no_acceptance_instant_lands_on_its_own_date_filed_and_not_the_index_day`
    (mutation 2026-11-15, fallback set back to the index day: red).
    - a `Date Filed` that is absent or not `YYYYMMDD` is `Published::Unknown`, never the index
      day: `a_filing_with_no_acceptance_instant_and_no_readable_date_filed_is_unusable_rather_than_undated`
      (same mutation: red); its archive observation is none:
      `an_archived_filing_with_no_readable_date_filed_places_no_observation` (mutation 2026-11-15,
      `Unknown` bounded by the index day: red).
    - the backfill observation of a `day` item is the LATER end of the row's `Date Filed` and of
      the index's day, plus the lag: a late row is first listed on the index's day;
      `a_late_rows_backfilled_observation_is_dated_by_its_index_day_and_not_its_date_filed`
      (mutation 2026-11-15, bound set to `Date Filed` only: red);
      `a_rows_backfilled_observation_is_not_before_the_end_of_a_date_filed_later_than_its_index_day`
      (mutation 2026-11-15, bound set to the index day only: red).
    - the backfill observation of an `instant` item whose readable `Date Filed` is the index's
      day is the LATER of acceptance and the START of the index's day, plus the lag; those are the
      only rows the lag was probed on (0 off-day rows, §8 row R11):
      `an_on_time_row_with_an_acceptance_instant_observes_at_its_acceptance_plus_the_lag`
      (mutation 2026-11-15, every instant bounded by the index day's end: red, observed
      2026-11-10T06:59:59.999999Z instead of 2026-11-09T23:32:15Z);
      `an_on_time_row_accepted_before_its_index_day_is_not_observed_before_that_day_starts_plus_the_lag`
      (Friday 18:00 New York, listed Monday; mutation 2026-11-15, start-of-day floor removed: red,
      observed 2026-11-07T01:00:00Z instead of 2026-11-09T07:00:00Z).
    - any other `instant` item, a late row or one with no readable `Date Filed`, is bounded by the
      LATER of acceptance and the END of the index's day, plus the lag:
      `a_late_row_with_an_acceptance_instant_is_not_observed_before_the_end_of_its_index_day_plus_the_lag`
      (mutation 2026-11-15, bound set to acceptance only: red);
      `an_acceptance_instant_row_with_no_readable_date_filed_is_bounded_by_its_index_days_end`
      (mutation 2026-11-15, an unreadable `Date Filed` treated as on time: red, observed
      2026-11-09T07:00:00Z instead of 2026-11-10T06:59:59.999999Z).
- Measured 2026-11-13 over the lake's 779 run files and 243,226 rows: **7,767 rows (3.19%)** are
  stamped `00:00:00.000000`; for source `perplexity`, **7,141 of 7,141**. Read as instants they were
  visible from 00:00Z of their own day, so `newsvault day --at` returned them up to a day early.

### 3.2 `Attempt` — the coverage ledger

- One row per attempt, written **whether or not anything came back**, so "nothing happened" and "we
  did not look" differ. The predecessor could not tell them apart: one day held 59 items against a
  median of 181 and nothing recorded which it was.
- Stored as `attempts` in `services/newsvault/src/store.rs` (user_version 3, §3.1), written from
  `news_provider::ledger::AttemptEvent`; every field below is a column there except
  `result_digest`, and the table holds more (`universe_version`, `matched`, `baseline_*`, `closed_at`).

| field | notes |
|---|---|
| `attempt_id`, `source`, `target_window` | what was asked for |
| `filter_params` | what the vendor was asked, not what we meant to ask: sha256, in §2's canonical form, over every sent (name, value) pair plus the roster digest `universe_version`, ordered by name over raw UTF-8 bytes, a credential by name only (`Request::filter_params`) |
| `hosts_version` | the hosts file the request's host was admitted under (§2), from `Request.hosts_version` via `Opened`; outside `filter_params` (`requests_that_differ_only_in_hosts_version_have_one_filter_params_digest`). NULL for a provider that reads no hosts file (`edgar-press`); `open-web-feed` sets it on every request: `the_opened_row_carries_the_hosts_version_the_request_was_built_from` (mutation 2026-11-15, `Opened` carries `None`: red) |
| `requested_at`, `received_at`, `transport_status` | |
| `outcome` | `ok` \| `empty` \| `failed` \| `interrupted` \| `collapsed`; the built four are `news_provider::ledger::OutcomeKind::name`, `collapsed` is designed and NOT built (below) |
| `verdict` | `usable` \| `empty` \| `permanent_failure` \| `transient_failure` (`Verdict::name`, `CHECK`ed), set by `Outcome::decide` and carried by `Finalised` inside `Outcome`; tells the two `failed` kinds apart for §11.1's held table. NULL for an abandoned attempt and for every row closed before user_version 3, not backfilled (`failed` names neither kind): `every_verdict_reaches_the_verdict_column_by_its_own_name_and_an_abandoned_attempt_none` (mutation 2026-11-15, `apply` writes NULL: red); `every_verdict_name_is_one_the_verdict_check_admits`, control `failed` refused (mutation that day, `transient_failure` removed from the CHECK: red); `news_provider::ledger::tests::a_finalised_outcome_carries_its_verdict_and_an_abandoned_one_none` (mutation that day, `decide` stores `Usable`: red). Read by `Committer::held_story_ids` (§11.1), called by the live arm of `attempt_once` |
| `story_id` | the story a `Document` attempt fetched: `Provider::story_of(request)`, computed by the driver (`news_provider::collect::attempt_once`) and written at `Opened`, before the socket, so a swept or in-flight attempt still names it. NULL for an `Index` attempt (open-web's `item_key` can key an index url, which would then hold a story), for a robots attempt, and for every row opened before user_version 3, not backfilled: `the_opened_row_carries_the_story_the_driver_named_and_an_index_row_none` (mutation 2026-11-15, `commit_opened` binds NULL: red); `a_document_attempt_opens_under_the_story_its_provider_names` (`services/newsvault/tests/collect.rs`, edgar fixture day; mutations that day, the driver passes None: red; the driver names a story for the index too: red). Read by `Committer::held_story_ids` (§11.1). No index on (`source`, `story_id`): no cost measured |
| `held` | follow-ups a `live_ingest` index attempt did not issue because §11.1's held set already held their story: `matched - issued`, written by `Committer::record_held` after the commit, 0 included (edgar live always 0). NULL on a `vendor_archive` index, a `Document` or robots attempt, a live index attempt with no extracted answer (failed or abandoned), a live index whose commit the store refused, and every row before user_version 3; NULL means not computed: `an_all_held_index_attempt_records_its_held_count` (3 on an all-held index, 0 on the first poll's, NULL on documents; mutation 2026-11-15, `record_held` a no-op: red); `a_vendor_archive_rerun_fetches_only_the_uncovered_accession` asserts NULL on both archive index rows (mutation that day, `held` written on the archive arm too: red); `services/newsvault/tests/open_web_collect.rs::a_live_index_whose_commit_was_refused_records_no_held` (a store refusing the second poll's index commit: `vendor_rows` and `held` NULL; mutation 2026-11-15, `record_held` called whatever the commit answered: red, `held` 3). Read by `newsvault ledger` (`SUM` of `COALESCE(held, 0)` over rows with non-NULL `vendor_rows`, per source and day) and subtracted by `--check` (§8 row R4) |
| `vendor_rows`, `items_new` | **R16.** Rows the payload held before our filtering; first sightings per `story_id` (§8 row R16). Written when `items_new` is zero, so poll events are recoverable, not only content changes: content-hash dedup is right for storage and destroys exactly this |
| `slots` | the only cost recorded: 1 when the send permit was taken, else 0; NULL when abandoned (`Cost::Slots`) |
| `result_digest` | sha256 over the set of `item_id`s the attempt committed, ordered over raw UTF-8 bytes and deduplicated, so a repeated query's drift is detectable and a reordered answer is not drift. Not a column: `newsvault ledger` computes it and reports drift per (`source`, `target_window`) |

- No money arm (owner, 2026-11-15): one is added to `Cost` with the first metered vendor. Rejected
  now: an arm nothing produces is a branch nothing reaches.

**`robots`: one row per answered robots.txt fetch (user_version 2, `MIGRATIONS[1]`).**

| column | notes |
|---|---|
| `attempt_id` | primary key and foreign key to the fetch's `attempts` row, `source` `robots` (§10) |
| `host` | the lowercased hosts-file host the fetch asked |
| `answer` | `present` (HTTP 200) \| `absent` (HTTP 404), `CHECK`ed; any other status files no row (§11.2) |
| `blob` | the body as received, foreign key to `blobs`; set only for `present`, which the Rust enum `store::RobotsAnswer::{Present(BlobDigest), Absent}` holds and no `CHECK` repeats |

- One door, `Committer::record_robots(AttemptReceipt, host, RobotsAnswer, at)`: `record`'s checks
  (opened here, closed here, an abandoned attempt carries nothing, a replay must be the same row,
  else `StoreError::RobotsReplayDiffers` naming both rows), then the `Answered` and `Finalised` events, the blob's index row and the `robots` row in one
  transaction: `a_robots_row_and_its_attempt_events_commit_in_one_transaction` (mutation
  2026-11-15, the insert moved after `COMMIT`: red);
  `a_robots_replay_naming_another_host_or_answer_is_refused` (mutation 2026-11-15, the replay
  comparison skipped: red).
- Producer: `newsvault robots refresh` (§11.2). Reader: `store::RobotsLedger::newest_robots`,
  read-only, the newest answered fetch of a host by `closed_at` then `attempt_id`; called by
  `collect`'s robots check (§8 row R4, robots clause).

- `collapsed` is designed as its own outcome: a payload that shrank to a fraction of what the store
  already holds is a degraded fetch, not a quieter world, and a throttled success looks exactly like
  a quiet day.
- Not built: `OutcomeKind` has no such arm, so a collapse-shaped payload is filed `ok`
  (`decide_cannot_return_collapsed_until_a_floor_with_our_own_construction_exists`).
- Its floor waits for our own construction: `baseline_ratio` (`Baseline.ratio` in
  `news-provider/src/ledger.rs`) is written on every decided attempt, NULL when the trailing median
  is 0 (which includes no prior `ok` attempt), so the floor can be picked from real runs.

### 3.3 `RuleSet` — what `derive` is a function of

- A hashed, versioned file, the only place these choices exist. Rejected: constants in code, which cannot be diffed, tested or attributed (§2 `rule_version`).
- Design: no `rules/1` file and no reader exist (`rg '"rules/1"'` outside `docs/`, 2026-11-15; the one rules file in the tree is `edgar-press.rules/1`, extraction rules digested as `extract_rules_version`); `derive` has no code (§3.0).

| group | contents |
|---|---|
| `entity_rules` | corporate-suffix list (complete, no ellipsis), case policy per token count, minimum core length, and three gazetteers: generic name components, non-companies, places |
| `salience_rule` | six keys, each a literal, never an expression: `counting_unit` (**memo D-8's, open**), `window_sessions`, `baseline_sessions`, `order_statistic`, `floor` (counted in `counting_unit`, so **memo D-8's too**), `tie_rule`. **Salience** is how often a counterparty's **core** — the name token a match is made on — occurs in the window ending at T, in `counting_unit`; the core is salient when that count is at or above `floor` and at or above the `order_statistic` of its own baseline |
| `panel_rule` | `presence`: which (ticker, session) rows exist, and nothing else — all `derive` decides about the panel's shape. Winsorisation and market adjustment: the statistics tier's, below |

- Each gazetteer entry is a line in a file and therefore a diff.
- Motivating case: a hand-written gazetteer removed `Premier`, a real company and counterparty, and nothing recorded the error because the list lived inside a script.

**The file, what `rule_version` digests, and the two keys still blank.**

- One JSON object, keys sorted, LF endings, one trailing newline.
- `rule_version` is sha256 of those bytes as committed (`canon::FileDigest::of`), so a whitespace edit is a new version: wrong in the safe direction, and no second canonicalisation.
- Gazetteers and the calendar are their own files (§2), named by path and digest so one `rule_version` still covers the set.
- The two `null`s are memo D-8's and only memo D-8's; every other value is the example's own, the shape being what is contracted:

```json
{"schema": "rules/1",
 "entity_rules": {"suffixes": "suffixes.txt@sha256:3f2a…", "min_core_chars": 4, "case_policy": {"1": "exact", "2+": "fold"}},
 "salience_rule": {"counting_unit": null, "window_sessions": 1, "baseline_sessions": 60, "order_statistic": "q90", "floor": null, "tie_rule": "name_ascending_bytes"},
 "panel_rule": {"presence": "close_on_session_and_next"}}
```

**Where the winsorisation cuts and the market adjustment live.**

- Not in the rule set: both act on a return and §3.10.1's panel carries none.
- Today both are inline in `probes/panel_power.py`: the session cross-sectional mean subtracted (`:87`) and the 1st/99th percentile clip (`:88`–`:89`), beside its floors, alpha, draw counts and seed (`:52`–`:55`).
- Their home is that tier's own hashed params file; its digest — `params_version`, named here and nowhere else — goes beside every power number.

### 3.4 `Panel` — `derive` to `propose`, and to the statistics tier

- An Arrow file plus a manifest, and not a call: the panel outlives the process (§4).
- The data file is §3.10.1.
- **The manifest's field list is not here**: `panel-contract schema --format=json` names every field, its `need` (always required or not) and its `why`. Cost: a reader runs a binary. Rejected: a copy here, free to over-promise or under-report against the checker.

**Why the field set exists**, which no field list carries.

- The predecessor's power at the 0.05 sd floor, α = 0.05, by construction, measured 2026-11-11 (memo §1.8, §1.9):

| construction | power |
|---|---|
| independent-rows formula | 88.2% |
| measured through the predecessor's own randomization procedure | 27.4% |
| same, on a corrected archive | 10.9% |

- The formula answered a different question and nothing could tell.
- Hence `rows` is **counted**, never multiplied: the predecessor's panel was `len(universe) * len(sessions)`, a rectangle wearing a count's clothes.
- `exchangeable_units` carries `count`, `unit`, `definition` (`check_units_shape` in `panel-contract/src/lib.rs`); `definition` because the unit's name alone does not say what made two exchangeable.
- A required field is half the fix; the other half, a consumer that **refuses** a panel lacking it, does not exist: `panel-contract` requires the field (§8 row R8) while the statistics code reads an ad-hoc `assign.json` + `meta.json` and counts its own units (row R9).

### 3.5 `ModelRequest` / `ModelResponse` — the only exit to a model

- Design: `modelgate` has no code (§4).

| request | notes |
|---|---|
| `prompt_version` | hashed, so an answer can be attributed to the question |
| `state` | the text the model reads |
| `schema` | **text and choice only. The schema cannot express a number, a date or a threshold.** |
| `idempotency_key` | derived from (prompt_version, state digest, schema, draw index) — a retry never pays twice |
| `draw_index` | which of N draws this is |

| response | notes |
|---|---|
| `outcome` | four designed values, `ok` \| `empty` \| `failed` \| `collapsed` (§3.2 adds `interrupted`), for the coverage ledger's reason: a failed call and an empty one must not arrive as one answer; memo §1.5 is that defect costing 37 of 1,672 companies. R5 is satisfied by a handler that writes an empty answer |
| `value`, `span` | the answer and a verified character range in the state that supports it |
| `confidence` | the model's own, recorded and never thresholded without a measurement |
| `generation_id`, `model_build` | the dated build that answered, not the alias that was requested |
| `cost` | charged against a per-run cap that is a hard stop, not a warning |

Structural, not advisory:

- **A model is never asked for a number**: the schema has no numeric type, so a careless prompt cannot break the rule.
- **A bracket condition never enters a prompt**: filtering by market capitalisation, date or threshold happens afterwards in code, where it can be tested.

Draw voting:

- Lives here, not in callers: the instability is the model's, so the boundary takes N draws, records the vote and keeps the per-draw answers.
- Measured on the predecessor: proposing which names are in play disagreed from draw to draw; typing the relation between two named companies did not. So the two steps are different calls, metered separately.
- The two figures are not quoted: their construction sits in the predecessor's archive, which memo D-4 leaves behind, and a number whose construction a reader cannot reach is an anecdote. They belong here once re-measured on a real run.

### 3.6 `Brief` — `propose` to a person

| field | notes |
|---|---|
| `brief_id`, `panel_id`, `scorer_version`, `prompt_versions` | full provenance |
| `cutoff` | the as-of instant the proposal stands on |
| `candidates[]` | ranked; each carries the ticker, the counterparty that was in the news, the filing accession and the **character span** in it that disclosed the relationship, and the salience value at its session's T (§3.10.1) |
| `limitations[]` | carried into the document, not left in a memo |

**Fields that must be absent, enumerated so their absence is checkable:**

- no order, no side, no quantity, no position size, no entry price, no stop, no ladder.
- A brief carrying any of them is to be rejected by its own schema: design, no code (§8 row R12b).

### 3.7 `StreamMessage` — the private item stream (memo decision D-7)

- A derived surface on `vault`, never a window onto the outbox. Wire format §3.10.2; transport §6.

| field | notes |
|---|---|
| `seq` | the store's ordering authority, so a consumer can order by ours instead of trusting clocks |
| `item_id`, `valid_at`, `observed_at`, `valid_precision`, `source`, `provenance` | the consumer makes its own point-in-time cut |
| `valid_disputed`, `earliest_valid_at` | **R15.** A consumer cutting on a `valid_at` that looks undisputed makes exactly the R15 error, so the dispute travels with the time |
| `title`, `url`, `body_ref` | |
| `schema` | the item contract, versioned apart from the storage row |

- Plus a **heartbeat** (fields: §3.10.2), so silence is distinguishable from a dead stream, because a consumer reading our outage as "no news" turns our failure into theirs.
- No delivery guarantee: the archive is the source of truth and reconciliation is an HTTP read of it.
- No latency promise: it would need a freshness number the design does not yet have.

### 3.8 `Reading` — `ledger`, one-way

- `ledger` consumes briefs, prices and realised outcomes and produces readings.
- It holds no edge back upstream: a service that sees outcomes must not influence the service that produces proposals, or selection happens under the result.
- **Where that is enforced: nowhere yet, and nowhere here.**
  - No `ledger` service exists (§3.9's service count; `newsvault ledger` is §3.2's coverage-ledger reader), so a check here would pass unconditionally — §7's argument in the other direction.
  - The enforcement point is `ledger`'s own test, under the condition that nothing it produces is an input to `derive` or `propose`.

### 3.9 `RawBlob` and `Expiry` — the layer a retention number may name (decided 2026-11-12)

| layer | addressed by | may a number expire it |
|---|---|---|
| L0 item row (§3.1 minus the text) | `item_id` | **never** — it carries R1's completeness claim |
| L1 extracted body | `body_ref` | **not as a setting.** Matching runs over body text (R17), so an expired body cannot be re-matched under a new `rule_version`; a number here reopens memo D-2 instead of configuring it. |
| L2 payload as received | `raw_blob_ref` | **yes.** The cost is re-extraction, and memo §6b says an index timestamp is not an immutable archive, so a re-fetch may not recover it. |

**An expiry is its own row type, under the same `seq`, and not an `Attempt`.**
- Rejected: filing a deletion as an `Attempt` with `outcome=empty`. An `Attempt` is keyed by what a vendor was asked (§3.2), so it would count as a quiet vendor inside the reconstruction R16 exists for.
- `Expiry` carries `blob_digest`, `layer`, `bytes`, `items_affected`, `policy_version`, `decided_at`, `deleted_at` and `seq`.
- `policy_version` hashes a retention file as §3.3 hashes a rule file, so a later number is a setting.
- A consumer meets the absence at the item row's state, not at a failed blob fetch, so §5.1 gives it no code.
- Only `vault` may delete, and only at L2.
- Designed, not built (read 2026-11-15 at 697a6b0): no `Expiry` type or table; `news_provider::BodyState` has no `Expired` arm (`news-provider/src/provider.rs:109`); `items.body_blob` has no state column (`services/newsvault/src/store.rs:164`).

**Measured 2026-11-12 at 42322eb, and no gate proposed.**
- Two temporary fixtures in `lake.rs`, run and reverted:
  - a null `body` returns `body == ""` with `unusable_rows == 0`;
  - dropping the column fails the whole day as `LakeError::MissingColumn("body")`.
- The inherited layout has no third state, which is why L1 and L2 are blobs beside the row.
- Nothing can delete yet: `ls services` returns `newsvault` alone, no `fs::remove_*` call exists in `services/` outside `mod tests`, and none of the tree's three deletes an archive blob (2026-11-14 at d33cd55; the same three on 2026-11-15 at 697a6b0):
  - two inside `store.rs`'s `mod tests`, each deleting a blob its own test stored; one checks that an item row naming a file that is not there is refused (`an_item_row_naming_a_blob_that_is_not_on_disk_is_refused_rather_than_indexed`);
  - one in `probes/vendor_probe/src/main.rs`, deleting the probe's own write-check file.

**One row of a daily index is not dated by that index.**
- Measured on the same capture: the 2026-11-10 index carries four distinct `Date Filed` values — 20261102, 20261103, 20261105 and 20261110.
- So a late submission appears in a later day's index, whose day is not the row's filing date.
- The column is `YYYYMMDD`, not ISO.

**The file count this layout produces, which is the shape of the failure it has to avoid.**
- Two blobs per committed item, not one per attempt: the raw submission `.txt` (L2, `raw_blob_ref`) and the extracted body (L1, `body_ref`) are disjoint, under separate digests.
- Measured 2026-11-13 on window 2026-11-12:

  | count | value |
  |---|---|
  | distinct blobs | **916** |
  | distinct raw payloads | **458** |
  | distinct bodies | **458** |
  | attempts | **503** (1 index, ~502 document, 458 of them committed and ~44 lost to the transient failures R19's no-margin pace leaves) |
  | files on disk | 918, because two more are the daily index payload |

- Blobs far above attempts is the falsifier the earlier one-blob-per-attempt projection named; the two-blob count is the model.
- The per-day order of magnitude is the in-universe filing count, re-derived 2026-11-13 from the real `form.20261110.idx` (1,057,192 bytes, captured through the predecessor's own SEC client) against the 7,992 distinct CIKs of its `company_tickers.json` of 2026-05-29:

  | rows | count |
  |---|---|
  | filing rows | **5,429** |
  | of them 8-K or 8-K/A | **391** |
  | of those with a CIK in the roster | **347** — a floor, the roster being five months old |

- So about 916 blob files a run; at 252 session days about **231,000** a year, **3.5** files per directory across the two-level 65,536-directory fan-out (230,832 / 65,536).
- The predecessor of this shape reached 558,235 loose files holding 5,197 current values: content addressing dedups identical BYTES and its payload's last bar was still forming, so 98.7% of what it stored were exact copies.
- Here the immutable half is the filing document at a fixed accession URL, which dedups to zero new files on a re-fetch.
- The moving tail is the daily index: it grows through the day and mints one blob per fetch, bounded by runs per day, not by items.
- **Unmeasured:** the index re-fetch rate across a whole day.
- The observation that would now surprise this estimate: one filing document whose repeated fetches carry more than one digest.

**The ceiling: this settles the layer, not the number.**
- N4 still needs a count of days for L2 and a medium for what is kept past it; one number per provenance class (§3.1) is the next smallest form.
- N1 is decided (§10), so an L2 expiry demotes to the second copy rather than deleting — but only once that copy has been restored once, which §10 records as undone.
- The memo's §3 charter bars deleting "to another consumer's budget", which an L2 expiry under its own `policy_version` is not.

### 3.10 Wire formats — two of §3.0–3.8's nine contracts, decided 2026-11-12

**The trigger for writing a wire format down: a second consumer that exists in code.**
- With one consumer a format is that consumer's own serialisation; with two it is a contract, and the second is where an unstated assumption becomes a defect.
- Counted over §3.0–3.8:

  | contract | consumers |
  |---|---|
  | `Panel` | two — `propose`, and the statistics tier whose live half is `probes/panel_power.py`, reading an ad-hoc `assign.json` + `meta.json` where the panel belongs |
  | `Item` | three (§4's boundary table), none with code |
  | `StreamMessage` | none, so §3.10.2 is here on memo D-7's instruction that the shape go in from the start, not on the trigger |
  | `Snapshot` | one, `derive` |
  | the other five | wait |

**Nothing in §3.10 is enforced:** no validator exists for either format and nothing produces either artefact, so both subsections specify rather than check (2026-11-15 at 697a6b0: `panel.data/1` and `newsvault.stream/1` appear nowhere outside `docs/`).

#### 3.10.1 `panel.data/1` — the Arrow data file beside the manifest

- Arrow IPC **file** format, one schema, rows in ascending `session` then ascending `ticker` as raw UTF-8 bytes.
- That is a canonical row **order**, not a canonical file: `panel_id` digests the rows in canonical form (§2), the Arrow bytes being a writer's and a chunking's.

| column | Arrow type | null | unit, and who reads it |
|---|---|---|---|
| `ticker` | `utf8` | no | both consumers |
| `session`, `next_session` | `date32` | no | labels in the calendar named by `calendar_version`, never instants |
| `has_relation` | `bool` | **yes, and a null means unresolved** | the statistics tier's control arm, read from the relation map (§4), never from `derive`'s own work |
| `salient_counterparties` | `list<struct<name: utf8 not null, salience: int64 not null>>` | no; an untreated row carries an empty list | `propose` takes the counterparty, the tier takes treatment |

- `next_session` is stored rather than re-derived, because a half-day is §2's named way to get it silently wrong.
- `has_relation` is per row, not per ticker: constant across a ticker's rows today, and a point-in-time map makes it vary without a grain change.
- A null `has_relation` row is dropped or stratified, **never counted as a control**: an extraction that failed and a company with no relation were one value in the predecessor, which is how 37 of 1,672 companies sat in the control arm as eligible (memo §1.5).
- `salience` counts in the unit named by the `salience_unit` metadata key, at T, and **T is the close of `session`** in the calendar named by `calendar_version`.
- The manifest's single `cutoff` is the snapshot's visibility bound, not the treatment cut, so no row sees past its own session close; that forbids an intraday panel, which is the choice being made.
- The list is ordered by descending `salience`, ties by `name` ascending: unstated, it is a third way one row set digests twice.
- `salience` is an integer, never a float: it counts occurrences or documents and a float can lose one.
- No `present` or `treated` column: presence is the grain, a row existing only when both closes are there (R8 in the shape), and treatment is a non-empty list.
- Three `utf8` metadata keys:
  - `schema` = `panel.data/1`, so the manifest holds no second copy of the version;
  - `panel_id`, so a stray pairing is detectable;
  - `salience_unit` = `occurrences` or `documents`, so a memo D-8 flip shows in the data.
- **What never changes:** one row per (ticker, session) present, so the manifest's `rows` is this file's row count — the identity §8 row R8's ceiling asks a second reader for.

#### 3.10.2 `newsvault.stream/1` — the private item stream

- One JSON object per WebSocket text frame, never an array.
- Each frame carries `type` (`item` or `heartbeat`) and `schema` = `newsvault.stream/1`, on every frame and not only at connect, so a reconnecting consumer sees it. The key is `schema`, as `newsvault.day/1` (`services/newsvault/src/main.rs`) and `panel.manifest/1` (`panel_contract::SCHEMA`) use it.
- An item frame carries §3.7's field list and nothing of its own, instants being RFC 3339 UTC `Z`.
- A heartbeat carries `at`, `items_since` on this connection, `heartbeat_seconds`, and `seq`, the last the store committed.
- **What never changes:** `seq` is the commit order, strictly increasing, never re-used or re-ordered, with gaps allowed because memo D-7 promises no delivery — a gap is closed by the HTTP read, never read as an ordering.
- **A ceiling:** the frame carries `heartbeat_seconds` rather than naming one here, no measurement and no owner decision having produced one; it is a liveness interval, never N2's freshness target.
- `seq` is §2's number, not the per-invocation `sequence` of the owner's NDJSON convention, which no format here offers.
- A reader is to refuse an unknown `schema` value rather than read it optimistically, as `panel-contract` refuses one (`unknown_schema`).

---

## 4. Which boundary is a file, and which is a call

| boundary | form | why |
|---|---|---|
| vendor → `vault` | HTTP, behind **one owner for every vendor conversation**, not one client per vendor | a vendor conversation happens in exactly one place, so quota, retry and cost have one owner. Since 2026-11-13, when the first adapter shipped, the single `ureq` agent sits behind `news-provider`'s `Wire` and `providers/edgar-press` declares no HTTP dependency: only that shape makes "a fetch outside a ledger row does not compile" hold in every crate, not only in crates that own no agent (row R4). Per-vendor differences: the provider's `TransportProfile` (`news-provider/src/wire.rs`), call budget and bucket included (row R19) |
| `vault` → `derive` | **file**: snapshot manifest + Arrow | `derive` must re-run years later against the same bytes; a call cannot promise that |
| `derive` → statistics | **file**: panel manifest + Arrow | same, and it keeps the Python tier at arm's length |
| `propose` → `modelgate` | call, idempotent | it spends money per invocation and must meter |
| `modelgate` → `derive` | **file**: the relation map, hashed and versioned (§2) | `has_relation` comes from a model and `derive` reads no network (§1). Each edge carries its filing accession and span (§3.6), the model build that produced it, and a valid-from instant: a map that is not point-in-time dates a relation before its disclosure. Field list not written: `modelgate` has no code and §3.10's trigger has not fired |
| `propose` → person | **file**: immutable brief, hashed | the thing someone acted on must be recoverable byte-for-byte |
| `vault` → private stream | WebSocket, best-effort | a notification, never the transport of record |
| `vault` → `vault` consumers | HTTP read for point-in-time queries. **No code yet.** Instead four CLI commands (`newsvault schema`): `items`, `ledger` and `publish` REQUIRE `--at` since 2026-11-13, so a read with no cutoff does not parse; `day`'s `--at` is optional, and without it `day` returns the whole partition with no visibility test. The only HTTP client in `Cargo.lock` since 2026-11-13 is outbound: `ureq 3.5.2` under `news-provider`, behind `Wire` (row R4) | a point-in-time query is the only read a consumer should need, so the predicate has one place to be right |

- **The pattern:** anything a future reader must reproduce is a file; anything that only has to
  happen now is a call.

---

## 5. Failure semantics

- Scope: what each service does when its input is wrong, not when everything works.

| condition | behaviour | requirement |
|---|---|---|
| a required input file is missing | abort with a named error | **R6.** An empty fallback is forbidden: a missing dictionary once produced an audit result of `0`, indistinguishable from a clean pass. |
| a network call fails | the `Result` is handled, or the build is red in two of the four ways to discard one: `f();` fails `cargo check`, `let _ = f()` fails clippy, `let _named = f()` and `drop(f())` are silent under every lint in the floor (§8 row R5). Exhausted retries are an `Attempt` row with `outcome=failed` | **R5.** Uncaught exceptions once dropped 37 of 1,672 companies silently, and all of them then sat in a control arm as eligible. |
| a vendor returns far less than the store holds | `outcome=collapsed`, not a value (designed, not built: §3.2) | a throttled success looks like a quiet day |
| an item's observation precedes its publication | **stored** as `pit::datedness::Datedness::Undated`, visible at no cutoff, with its evidence: the raw blob, the observation, and the refused publication value in `items.valid_refused`. The lake reader still counts such a row unusable: it has no blob to keep the evidence in | a source bug, not an archive fact; discarding the row loses a real document, and `Undated` holds one. Measured 2026-11-12 over 40 partitions of the predecessor's lake, 13,074 rows: **0** in this state, so the corpus cannot say how often it happens |
| the same item arrives in several runs | first sighting wins, keyed by `story_id` (§2), never by `item_id`, which moves when a publication time or a title is corrected | **R3.** Keeping the last postdates our own knowledge, by up to 19h59m on the predecessor's store. |
| a panel lacks `exchangeable_units` | `panel-contract validate` calls the manifest not admissible. The statistics tier is *to* refuse to run and does not: it reads an ad-hoc assignment file, not the manifest | **R8** today, **R9** for the refusal |
| a brief carries an order-shaped field | rejected by schema | **R12b** |
| a count is taken over a partition's files | not a count. The partition key is the **publication day** (decision D3, §10), so a backfill appends a second segment into a months-old partition and no partition ever closes: measured 2026-11-13, a second run exporting one late item leaves two segment files in one `valid_date=` directory, both current. Every count is a `seq`-bounded query over the commit log; `ledger` reports `unpublished_seq_ranges`, so rows no segment carries are a number, not a silence | **R13**, and decision D3 |
| an event is lost | latency, not data: the archive is the source of truth and the periodic sweep closes the gap | memo §4 rule 2 |
| a consumer is silently dead | detected by lag and backlog depth, not by staleness; the threshold on the newest item's age is §10's N2 | memo §4 rule 3 |

### 5.1 The error-code registry (decided 2026-11-12)

**What a code is.**

- The `code` field of the error object a command writes to stderr before exiting non-zero, and
  nothing else.
- Never a second name for a condition another mechanism names: §3.2's outcome values, the counted
  rows of §5 rows 4 and 5, `panel-contract`'s `Finding.code`.
- `exit` and `retryable` per code: `newsvault schema --format=json`, key `error_codes`
  (`services/newsvault/src/main.rs`). That list is what a caller may branch on and a new command
  must implement.

**Which of §5's ten rows take a code.**

| §5 row | code | why |
|---|---|---|
| 1, missing input | yes: the input codes below | |
| 2, network call fails | yes, since 2026-11-13: the four `collect` codes below summarise a run | each failed call is still an `outcome=failed` attempt row (§3.2) |
| 3, collapse | no | the code does not exist (`news_provider::ledger::OutcomeKind` has no collapsed variant); a code would be §8 row R12a again |
| 6, refusal half | no | the statistics tier has no code; same reason |
| 7, brief | no | no `*.rs` file names a brief (grep 2026-11-14, again 2026-11-15); same reason |
| 4 and 5 | no | reachable and not errors; §3.9 owns a third |
| 8 | no | a counting rule |
| 9 and 10 | no | monitoring |

**What the binaries write today** (read from both `main.rs` files 2026-11-14, re-read 2026-11-15).

- Only `newsvault collect` and `newsvault robots refresh` write a `code` to stderr, through
  `emit_collect_error` and `emit_robots_error` (an object under `--format=json`), and only for
  `vendor_unreachable`; and `collect`, through `emit_robots_refusal`, for `robots_unverified` and
  `robots_refused` (an object under `--format=json`, `error: <text>` otherwise).
- The three exit-8 `collect` codes ride the stdout envelope as `exit.error_code`.
- Every other failure of either binary writes `error: <text>` or `usage error: <text>` with no
  `code`, so for those codes the exit status is the whole machine-readable answer.

| code | the condition, and who can emit it |
|---|---|
| `invalid_argument` | the caller's own mistake, which §5 has no row for. Every CLI. In `collect`: a flag its source requires or refuses, a second distinct `--source`, an unregistered source, an open-web `--at` off the clock's UTC day, `--window today` for `edgar_press` (§11.2), an unreadable `--window`, an open-web `--window` other than the run's UTC day (both `CollectRunError::WindowRefused`, on the dry-run and the driven path). Two stderr shapes for a caller matching text: `newsvault`'s own value checks and these eight `collect` checks write `usage error:` (`usage_exit` and `prefix_of` in `services/newsvault/src/main.rs`, over `CollectRunError::is_usage`); a clap parse failure in either binary writes `error:` (run 2026-11-15: `newsvault day --bogus`, `panel-contract validate --bogus`). Before 2026-11-15 the `collect` checks wrote `error:` (review that day: `error: edgar_press needs --calendar`). `every_collect_usage_error_is_written_as_a_usage_error`, one binary run per check; mutations 2026-11-15, the prefix always `error` and each of the six `is_usage` arms set to false: 7 red; the `WindowRefused` arm set to false / the dry-run refusal left as `PlanRefused`: each red; `of_driver`'s `Planning` arm removed: 0 of 171 `newsvault` tests red (2026-11-15), because the pre-flight refuses a window before `drive` plans it. Before 2026-11-15 both window refusals wrote `error:` (review that day: `error: target window garbage is neither a day nor a FROM..TO range`). Unguarded: another `newsvault` error that is the caller's mistake (`LedgerReadError::RangeInverted`) still writes `error:`. A driven (not `--dry-run`) `collect` refuses its window before `commit.sqlite3`, `LOCK` or a run envelope exist (`refuse_the_window` in `cmd_collect.rs`, after the `--at` check, before the store version check): `services/newsvault/tests/open_web_collect.rs::a_driven_collect_refuses_an_unreadable_or_off_day_window_before_writing_anything`, `--window garbage` and `--window 2026-11-13` at 2026-11-14 on an empty store; mutation 2026-11-15, `refuse_the_window` removed: red, all three paths left behind. Unguarded: only `WindowRefused` returns early, so edgar's no-lag `PlanRefused` and an unreadable calendar or listing still leave a run envelope with `finished_at` null; mutation 2026-11-15, the pre-flight returning every plan refusal: 0 of 171 `newsvault` tests red, so nothing pins the narrow choice |
| `input_absent` | a required input file is missing (row 1). `vault`, `panel-contract`. In `collect`, exit 4: an absent hosts file or `open-web-feed` rules file, and a hosts file admitting no host, which would plan zero requests (`an_open_web_run_with_an_absent_or_empty_hosts_file_or_an_absent_rules_file_exits_four`; mutations 2026-11-15, the arm removed: red; its `RulesAbsent` alternative removed: red, green before the rules case). An absent `edgar-press` rules file exits 2 |
| `day_absent` | narrower: the store is there, the day is not in it (row 1), kept apart for the reason the coverage ledger exists. `vault`. Measured 2026-11-12, two states reach it through one `LakeError::MissingDay`: an absent partition directory, and one holding no run file |
| `input_unparseable` | the file is present and is not what the contract says. `vault`, `panel-contract` |
| `input_schema_mismatch` | a required column is absent or has an unusable type; or the commit log's `user_version` is above the writer's, or is not the one `items` and `ledger` read (`StoreError::SchemaVersion`, §3.1); or a store below the current user_version whose columns differ from that version's shape (`StoreError::ShapeDiffers`) or whose migration step SQLite refused (`StoreError::MigrationFailed`), both leaving the store at its old user_version (§3.1). `vault` |
| `io_failed` | a read or write the filesystem refused for another reason. Every CLI |
| `roster_absent` | the in-universe roster is missing or names no CIK. `vault`'s `collect`, before the first request |
| `credential_contact_missing` | the User-Agent carries no contact address, read from the chosen source's own variable (§11.2). A startup refusal, never a vendor event in the coverage ledger |
| `lease_held` | another run holds the store's writer flock. `vault`'s `publish` and `collect` |
| `vendor_unreachable` | nothing committed and every failure transient. `vault`'s `collect` |
| `partial_collect` | something committed and something failed, abandoned or not asked for |
| `vendor_refused_all` | nothing committed and at least one PERMANENT failure. Exit 8, not 7: the reason is at `news_provider::Exit::code` |
| `commit_failed` | the store refused an attempt's rows, with that attempt named in the envelope |
| `robots_unverified` | exit 4. `vault`'s `collect`, dry run included, before the store version check, the envelope and the lease: an admitted hosts row whose host has no answered (200 or 404) robots fetch in the store, or whose newest one names a blob absent from disk, or whose pinned blob was absent when `open_provider` read the store (`CollectRunError::RobotsUnverified`). `details` `{host, pinned, fetched}`; `suggestions` one argv array, `newsvault robots refresh --source open_web_wires --hosts <path> --rules <path> --store <path>`: `a_robots_refusal_names_the_refresh_command_as_an_argv_suggestion` (mutation 2026-11-15, the refusal written as plain `error:` text: red) |
| `robots_refused` | exit 2. Same place, four conditions on an admitted row: (1) the newest answered robots fetch is not its pin (`CollectRunError::RobotsRefused`, `details` `{host, pinned, fetched}`, no suggestion: a refresh records the same change again and the fix is a person editing the hosts file; same test as above, mutation 2026-11-15, the refresh suggested again: red); (2) the pinned body cannot be read: not UTF-8, no `user-agent` line, a non-numeric Crawl-delay (`RobotsUnreadable`, `{host, pinned, why}`); (3) it disallows the index path with its query or the rule's `path_prefix` (`RobotsDisallowsPath`, `{host, path}`); (4) the provider's `per_host` is `None` or below max(Crawl-delay, 10 s) (`PaceBelowFloor`, `{host, per_host_seconds, floor_seconds}`); (5) the pinned blob's bytes on disk no longer hash to the pin (`RobotsBlobAltered`, `{host, pinned, found}`; `BlobStore::put` keeps an existing file, so a refresh repairs nothing). (2) to (5) suggest nothing, because a refresh cannot change a pinned body: `a_path_or_unreadable_robots_refusal_names_its_fact_and_suggests_no_refresh` covers (2) and (3), `a_pinned_robots_blob_changed_on_disk_is_refused_and_suggests_no_refresh` (5) (mutations 2026-11-15, the hash comparison in `BlobStore::read_verified` removed / the arm's suggestion switched on: each red); (4) is unreachable through the binary while the hand-off holds (§11.2), so its `main.rs` arm is untested |

- `input_unparseable`, `input_schema_mismatch` and `io_failed` exit `2` since 2026-11-13; `newsvault`
  returned `1` for them before. Decision D1 (§10), taken by default that day.
- No `newsvault` failure maps to `1` (`exit_code` in `services/newsvault/src/main.rs`); `1` is "the
  answer is no" and nothing else.
- One test pins it, the way the defect was measured: a 7-byte parquet at a day partition's path
  exits 2, and putting that arm back on 1 turns
  `a_day_whose_parquet_cannot_be_read_exits_two_and_never_one` red.
- `newsvault robots refresh` exits (`cmd_robots::RobotsExit`, over `news_provider::Tally::exit`
  with each answered row counted as committed):

| exit | when | stdout |
|---|---|---|
| 0 | every row answered (200 or 404) and every admitted pin matches | the `newsvault.robots/1` report |
| 1 | every row answered and an admitted row's pin differs (D1, "the answer is no"): `a_fetched_robots_digest_other_than_the_pin_exits_one_and_names_both` | the report |
| 8 | a row failed: `partial_collect` when another row answered, `vendor_refused_all` when none did and one failure is permanent | the report |
| 7 | every row failed transiently: `vendor_unreachable`; or `lease_held` | empty; the error object on stderr |
| 4 | `input_absent`: an absent hosts or rules file, a hosts file admitting no host | empty; `error:` |
| 2 | `invalid_argument` (`--source edgar_press`, written `usage error:`), an unparseable hosts or rules file, `credential_contact_missing`, a store of another version or shape, io | empty; `error:` or `usage error:` |

  - Which stream a refresh writes is `cmd_robots::Answer::emitted`, over a stub:
    `a_refresh_writes_its_report_on_stdout_for_exits_zero_one_and_eight_only` (mutation
    2026-11-15, stdout always chosen: red). The binary's exits 2 and 4, their empty stdout, the
    `usage error:` prefix and their keys in the schema's `exits` map:
    `a_refresh_refused_before_any_fetch_exits_two_or_four_with_an_empty_stdout` (mutations
    2026-11-15, `NoHostsFile` not a usage error / the absent-input arm mapped to 2 / exit 4 dropped
    from the schema: each red). Unguarded: the binary's exits 1, 7 and 8, which need a fetch; no
    binary test reaches the network, so `main.rs`'s two-arm match on `Emitted` is tested only
    through exit 0.

  - `--source edgar_press` is `invalid_argument` (`RobotsError::NoHostsFile`); the hosts file's
    own refusals are `collect`'s, through `OpenWebFeed::open`. A refresh emits neither robots code;
    `collect` does (above).
- The incomplete report at **8** carries its result on stdout and is not an error object:
  `ledger` writes the days it could read and names the run envelopes it could not.
- `Wire::send`'s refusals (reached by `newsvault collect` since 2026-11-13) are
  `news_provider::wire::SendFailure` variants: types, not codes, until `error-codes/` exists
  (row E1).

**One invariant over the exit codes, readable by a test: stdout is non-empty if and only if the exit
status is 0, 1 or 8.**

- D1's (§10) companion rule; 8 joined the set with `ledger`'s incomplete report.
- **One function since 2026-11-13**, `news_provider::Exit::carries_the_result_on_stdout`:
  - `collect` branches on it.
  - `stdout_carries_the_result_for_exactly_the_exits_zero_one_and_eight` asserts it over every arm
    of the exit table, not over the two a run usually takes; returning `true` unconditionally turns
    that test red.
  - `robots refresh` reaches it through `cmd_robots::RobotsExit::carries_the_result_on_stdout`,
    which answers 0 and 1 itself and delegates every failure to it (`Answer::emitted`, above).
  - `day`, `items`, `ledger` and `publish` hold the rule by hand.
- Measured 2026-11-12:
  - `panel-contract validate` on `{}` exits 1 with ten findings on stdout and nothing on stderr.
  - `newsvault day` over a 7-byte `.parquet` exited 1 with an empty stdout and
    `error: EOF: Parquet file too small` on stderr; D1 (§10) moved that case, and it exits 2 with an
    empty stdout today.
- **What is left of the ceiling:**
  - `1` still carries two sentences, now reading the same way: `{"1": "the answer is no"}` here and
    `{"1": "not admissible"}` there.
  - Nothing checks the iff clause across the two binaries: that needs a test over both schemas,
    which does not exist.

---

## 6. The event path

```
vendor  --poll/stream-->  vault: write  --same transaction-->  outbox
                                                                  |
                                                       cursor poll / stream
                                                                  v
                                                        derive: compute over a CLOSED window
```

Four rules from the memo; the contracts depend on them:

1. **An event triggers computation; it never accumulates it.**
   - `derive` stays a pure function over a closed window.
   - Incremental salience is rejected: it makes results depend on arrival order (§9).
2. **Write, then emit, in one transaction.**
   - The archive is the source of truth; the event says it grew.
3. **The periodic sweep stays** (every scheduled poll, at most 90 min apart, §11.2):
   - as a backstop;
   - as the thing that fills the coverage ledger even when no event fires.
4. **Salience is a function of a cutoff, not of a day**: `salient(core, T)`.
   - A rolling threshold computed from a partial day differs from the same threshold at the close,
     so every downstream claim names its `T`.

Transport, decided by the owner 2026-11-14 (no code yet; §11.5 step 8):
- An `outbox` table inside `commit.sqlite3`, written inside `Committer::record`'s transaction, so
  rule 2 is one SQLite transaction (an outbox in another database: §9).
- One row per closed attempt, zero-item attempts included: `event_seq`, `attempt_id`,
  `seq_lo`/`seq_hi` (a pointer into `items.seq`, never a copy), `items_new`. It is the replay log.
- The outbox's only reader is one poller inside `vault`: `newsvault events --after <cursor>`.
- The poller keeps its cursor outside `commit.sqlite3` and polls `event_seq > cursor` every 5 s.
- It wakes `derive` and feeds the stream (§3.7).
- No other service opens `commit.sqlite3`. `derive` still reads a snapshot file (§4).
- Nothing pushes: a late poll delays an event and never loses it (rule 2).
- At §11.5 step 8 the store switches to WAL, so a long-lived reader never blocks a commit.
- At the same step SQLite is linked `bundled`, replacing the no-`bundled` note in
  `services/newsvault/Cargo.toml` and adding a C compile to CI.
- SQLite versions seen:

| host | date | construction | version |
|---|---|---|---|
| Mac | 2026-11-13 | stated in the `services/newsvault/Cargo.toml` note until 2026-11-15; command not recorded | 3.54.0 |
| Mac | 2026-11-14 | `sqlite3 --version` | 3.54.0 |
| Mac | 2026-11-15 | `/usr/bin/sqlite3 --version` | 3.54.0 |
| VPS | 2026-11-14 | `sqlite3 --version` | 3.45.1 |

- WAL changes how N1's second copy is taken (§10, N1).
- A read-only open of a WAL store works: checked 2026-11-14 by a reviewer with Python `sqlite3` on
  SQLite 3.53.4, folder writable and read-only.
- The writer lease stays whole-life until the startup sweep can tell a live run from a dead one
  (§11.3): narrowed earlier, one run's sweep closes another live run's open attempts
  (`news_provider::collect::sweep`; `attempts_in_flight` has no run filter).
- Polling cost, measured 2026-11-14, Python `sqlite3` on SQLite 3.53.4, laptop, one connection,
  WAL file: 10,000 `SELECT … WHERE event_seq > ?` in 12.2 ms. Not measured through `rusqlite`.
- Reversed if: a second host must write the archive or read the outbox; more than 10 commits per
  second; or p95 latency from `observed_at` to the stream frame (§3.10.2) exceeds 60 s with the
  5 s poll as its larger part.

---

## 7. What is not reachable from here

- The order-placing path is in a different repository, holds the only credentials that matter, and
  accepts hand-written documents only.
- Nothing here holds a credential for it.

**Where the wall is enforced, and where it is not.**

- Not here: a check here for order-placing calls would pass unconditionally, because no such call
  exists to forbid; the predecessor's equivalent gate guards code that exists.
- If a mediating service is ever built between this repository and the executor, the enforcement
  belongs there, under the condition §8 row R12a states.
- Same shape in the other direction: no backward edge from `ledger`, enforced at `ledger` when it
  exists (§3.8).

**Lower latency makes this boundary *more* important, not less.**

- "We must act now" is exactly the argument that talks a person into automating a pick; the boundary
  has to hold at the moment it becomes inconvenient.

---

## 8. Where each invariant is enforced

- The maturity test: an invariant with no enforcement point is a wish, and the wishes are named
  rather than hidden.
- **The status column is the count.** No sentence anywhere repeats it as a number, because a
  repeated count is a count that goes stale.

**How to read the status column.**

- `design` means the enforcement point is specified and the code it names does not exist, so that
  row's `enforced by` cell says what *will* check the invariant, not what does.
- Where a row's clauses differ, its status splits with them.

| # | invariant | enforced by | status |
|---|---|---|---|
| R1 | archive complete, caps are views | blobs: `BlobStore::put` stages via `File::create_new`; `newsvault::store::tests::a_blob_whose_staging_path_already_exists_is_refused_rather_than_truncated`, `…::a_second_put_of_the_same_payload_keeps_the_standing_file_rather_than_rewriting_it`<br>segments: `Segment::create_new` is the only constructor; `newsvault::publish::tests::changing_the_one_segment_constructor_to_file_create_turns_the_write_gate_red`, `…::a_second_write_to_an_existing_published_path_fails_with_a_named_error`, `…::a_segment_is_renamed_into_place_only_after_its_bytes_and_its_directory_are_fsynced`, read-back `…::a_published_day_round_trips_through_the_strict_column_reader`<br>policy copy: `cmd_collect` stages via `File::create_new`; `services/newsvault/tests/collect.rs::a_staged_policy_copy_a_crash_left_behind_is_refused_rather_than_overwritten`<br>`items` never updated: §8 row R15 | **three of four creating calls enforced (blob, segment, policy copy), segments with fsync order**<br>unguarded: the fourth, `cmd_collect::write_envelope`, uses truncating `File::create` (envelope written twice per run)<br>unguarded: `File::create`, `OpenOptions::truncate`, `ArrowWriter` unsearched in the crate<br>unguarded: `blobs.rs` fsync order: 3 mutations (drop the file fsync; drop the directory fsync; rename before the file fsync) each leave `cargo test --locked -p newsvault --no-fail-fast` at exit 0, run 2026-11-15<br>unguarded: nothing forbids `UPDATE`/`DELETE` on `items`; the attempt row is updated in place by design<br>caps: design, no view builder; deletion only L2 blobs and `vault`, §3.9 |
| R2 | valid and observation time are distinct types | type system; `compile_fail` doc test in `pit/src/lib.rs`, run because of `--no-fail-fast`<br>positive control doc test with the same `use` line; layout asserted by `pit::tests::the_two_instants_still_cannot_be_compared_with_each_other_after_the_move`, `pit::tests::the_positive_control_for_the_compile_fail_block_imports_from_the_new_path`<br>not `compile_fail,E0308`: rustdoc reports `ok` on a different error code | **enforced, narrowly**: the two instants cannot be compared with each other<br>unguarded: `Visibility::valid()`, `observed()`, `Cutoff::inner()` are public, so a publication-only predicate compiles from any crate (demonstrated 2026-11-12)<br>fence annotations asserted exact: `compile_fail` on the block, a bare fence on the control; `compile_fail,ignore` on the block reds `pit::tests::the_two_instants_still_cannot_be_compared_with_each_other_after_the_move`, `ignore` on the control reds `pit::tests::the_positive_control_for_the_compile_fail_block_imports_from_the_new_path`; a decoy pair before them or a dropped closing fence reds `pit::tests::the_header_holds_exactly_the_compile_fail_block_and_its_control_both_closed`; mutation-tested 2026-11-15<br>unguarded: a `compile_fail` block that fails for another reason (a hidden `compile_error!`, a commented-out `use`) stays green<br>every member library keeps `doctest` and `test` on at `src/lib.rs`, and no committed `.cargo/config*` or workflow sets rustdoc flags: `repo_invariants::doctests::tests::every_member_library_in_this_workspace_runs_its_doc_tests` (mutation-tested 2026-11-15: `doctest = false`, behind `crate-type = ["rlib"]`, `test = false`, `path` elsewhere, `rustdocflags` in `.cargo/config.toml`)<br>unguarded: flags passed outside committed files (a shell's `RUSTDOCFLAGS`, `cargo test -- --skip`) |
| R3 | first sighting wins | store: `newsvault::store` `record` keys on `story_id` across the whole store, never updates a row; `newsvault::store::tests::the_same_story_in_three_commits_keeps_the_first_observation_and_writes_items_new_zero`, `…::an_earlier_observation_arriving_later_replaces_the_kept_one_and_is_counted` (mutation-tested 2026-11-13)<br>lake reader: keys on vendor `id` per partition; blank id unusable, `newsvault::lake::tests::a_blank_id_is_unusable_and_does_not_collapse_distinct_stories`<br>reader vs `probes/newsvault_parity.py`: committed output read by `services/newsvault/tests/parity.rs`; its one disagreement (ingest before a midnight-stamped publication, dropped by the probe) is now unusable here: `newsvault::lake::tests::a_row_fetched_before_its_own_day_stamp_is_unusable_and_not_day_dated`<br>provider key, open-web: `OpenWebFeed::item_key` is the one function behind `source_item_id` and `story_of`; a page's canonical link feeds `doc_id` only: `the_follow_up_story_equals_the_item_story_on_a_page_whose_canonical_link_differs` (synthetic fixture; mutation 2026-11-15, the item keyed on the canonical link: red), `www_and_rss_links_of_one_release_share_one_item_key` (mutations 2026-11-15, the rewrite removed from the rules file: red; the host's case kept by `host_and_path`: red, green before its `WWW.GlobeNewswire.com` case), `a_url_with_a_port_or_a_userinfo_has_no_item_key` (mutation 2026-11-15, `host_and_path`'s `HostKey` check deleted: red, green before the test), `a_host_two_rules_rewrite_to_two_hosts_is_refused` (§11.1)<br>index, open-web: the first row keying a story is followed up and a later row of the same index keying it again is counted in `filtered_out`, its `listed_date` dropped: `a_story_listed_twice_in_one_index_is_followed_up_once_and_the_repeat_counted` (mutation 2026-11-15, the seen-set removed: red) | store **enforced across the whole store, keyed on `story_id`**; lake reader **enforced at the partition grain**, keyed on the vendor id<br>unguarded: nothing reconciles `attempts.items_new` with `COUNT(items)`<br>lake reader derives no `story_id` (row R7); an id in k partitions is kept k times: 1,225 extra appearances of 501 ids (§10 Freshness, N2; §2 `story_id`)<br>open-web key **enforced** for GNW; unguarded: an edit to a GNW slug makes a second key, so one release becomes two stories (unmeasured)<br>unguarded: `item_key` keeps the path byte for byte, so percent-encoding case (`%C3%B1` / `%c3%b1`), a raw `ñ` and a trailing slash each key a second story (read from `host_and_path`, 2026-11-15, not run); latent while only the RSS link feeds `item_key`, normalised with a test before a second GNW surface feeds it |
| R4 | a **coverage ledger** (`Attempt`, §3.2) beside every store, **and no vendor call outside an open attempt row** | reader: `newsvault ledger --check` (`services/newsvault/tests/ledger.rs`), golden `services/newsvault/tests/fixtures/ledger/expected.json`<br>call: `Wire::send` consumes `SendPermit` from `AttemptSlot::take_permit`; `a_send_without_a_permit_does_not_compile` + positive control, fence annotations asserted exact; `repo_invariants::clients`<br>contact (accepted attack A3.7): `a_literal_contact_shaped_header_value_turns_a_named_test_red`<br>clock: `repo_invariants::clocks` over `news-provider/src` and `providers/*/src`, exempting `clock.rs`, `wire.rs`<br>no sink in a provider: `repo_invariants::sinks::tests::no_file_under_a_provider_crate_names_sink`, code lines of every `providers/*/src` file (mutation 2026-11-15, `use news_provider::Sink;` in a new file under `open-web-feed/src`: red)<br>sweep: `an_attempt_a_dead_run_left_open_is_closed_interrupted_by_the_next_run`<br>store: `Committer::record(AttemptReceipt)`, `compile_fail` + control in `store.rs`, annotations asserted exact; `a_receipt_closed_against_another_sink_is_refused_rather_than_filing_items_under_it`; FK under `PRAGMA foreign_keys = ON`<br>robots (§11.2): `collect`, dry run included, refuses an admitted hosts row unless the store's newest answered robots fetch of its host (`store::RobotsLedger::newest_robots`, read-only) is the row's pin and, for a digest, its blob is on disk: `services/newsvault/tests/open_web_collect.rs::an_admitted_row_without_a_ledgered_robots_fetch_is_refused` (no commit log; only a failed fetch; nothing written, nothing sent), `…::an_admitted_row_whose_newest_robots_fetch_differs_from_its_pin_is_refused`, `…::an_admitted_row_pinned_absent_is_admitted_only_by_a_ledgered_404`, `…::a_failed_robots_fetch_after_a_matching_one_leaves_the_host_admitted`, `…::a_candidate_row_needs_no_robots_fetch`, `…::a_pinned_robots_blob_missing_on_disk_is_unverified`, `…::a_robots_blob_written_after_the_provider_opened_is_unverified`, `…::a_dry_run_refuses_an_unverified_host_too`, `…::a_pinned_robots_blob_changed_on_disk_is_refused_and_suggests_no_refresh` (the pinned blob re-hashed when read), `…::a_second_admitted_host_without_a_ledgered_fetch_is_refused` (every admitted row; mutation 2026-11-15, the loop cut to its first row: red), `…::another_hosts_fetch_matching_the_pin_does_not_admit_a_host` (the lookup keyed by host; mutation 2026-11-15, both host predicates (`a.target_window = ?2` and `r.host = ?2`) replaced together by `?2 IS NOT NULL`: red, both tests; either alone: green, because the other still keys on the host), `…::an_open_web_collect_against_a_newer_store_is_a_version_refusal` (`RobotsLedger::open`'s version check, mutation 2026-11-15, removed: red); 7 mutations 2026-11-15 on the first build, as listed in §11.5 step 3, and the review round's beside each test above; the pinned body's paths and pace: §11.2, §8 row R19<br>held (§11.1): `--check` raises `rows_committed_zero_against_vendor_rows` only when `vendor_rows` − `held` > 0, its detail naming both; `services/newsvault/tests/open_web_collect.rs::a_quiet_day_whose_every_listed_story_is_held_passes_ledger_check` (run 1 commits GNW's 3 stories under 2026-11-14, run 2 on 2026-11-15 lists the same 3, all held: `vendor_rows` 3, `held` 3, rows 0, no finding, the binary exits 0; mutations 2026-11-15, the subtraction removed: red; the comparison made `>= 0`: red; `record_held` a no-op: red); `services/newsvault/tests/ledger.rs::check_still_flags_zero_rows_when_held_covers_only_part_of_the_vendor_rows` (59 vendor rows, 58 held: exit 1; mutation that day, any non-zero `held` suppressing the finding: red); `…::check_still_flags_zero_rows_when_an_unclosed_row_carries_held` (an open row holding 60 beside 59 vendor rows: `held` 0, exit 1; mutation 2026-11-15, `held` summed over every row: red, exit 0 with no finding); `open_web_collect.rs::two_all_held_polls_of_one_day_hold_every_vendor_row_of_both` (two all-held polls on 2026-11-15: `vendor_rows` 6, `held` 6, rows 0, exit 0; mutation 2026-11-15, `row.held += entry.held` made `=`: red, `held` 3 and a false finding); `ledger.rs::the_human_ledger_states_the_held_count_of_the_json_row` (mutation 2026-11-15, `held` dropped from `render_human`: red)<br>mutation-tested 2026-11-13 | call clause **enforced** within holes 1–2; store clause **enforced** within its two holes<br>robots clause **enforced** since 2026-11-15 for every admitted row, the whole run refused on the first failure (§11.3's per-host block is not built)<br>robots unguarded: no age bound on the fetch (no measured threshold; re-fetch is step 7); a redirect reaches a host with no ledgered robots fetch and no path check (R19 hole 9, until step 5); the read precedes the lease, so a refresh landing between them is unseen, and a refresh can only add a newer answer; a collect against a store with no robots table (user_version below 2) is `robots_unverified`, not a version refusal; the GNW pin vs the committed fixture is `open_web_feed::the_committed_hosts_file_pins_the_committed_robots_body`, not a `newsvault` test<br>held unguarded: `record_held` runs outside the commit transaction, so a crash between them leaves `held` NULL, read as 0: the finding is then a false positive, never a hidden one (the shape of R20 hole 5)<br>held unguarded: read for every row with `opened_at <= --at`, so a `held` written after the cutoff is counted; `vendor_rows` and `outcome` share this gap<br>held unguarded: `filtered_out` is not stored, so an all-held day whose index also filtered out a row (off-path, repeated) still flags<br>not built: `--check` asking-clock age<br>no caller: `Cost::Slots(0)`, `PermitAlreadyTaken`, `AnswerFromAnotherAttempt`<br>hole 1: privacy is per module: `SendPermit` literal in `ledger.rs` or a child of it, or `pub fn` in `wire.rs` calling `exchange`; `std::net` and unlisted clients pass `clients`<br>hole 2: `std::process::Command` running curl, unchecked<br>clock scan: fires on comments, misses renamed clocks; `services/` and other members unscanned<br>sink scan: cuts each line at its first `//`, so a `/* */` block hides a use and a `//` in a string cuts the line short; a sink reached through an alias without `Sink` in its name passes<br>contact: blob store unchecked; the written-contact test runs an `edgar_press` collection only, so an `open_web_wires` run's envelope and rows are unchecked<br>store hole: FK checks existence, not closure<br>store hole: second connection with `foreign_keys` off |
| R5 | failed calls must be handled | `cargo clippy --locked --all-targets -- -D warnings` over `[workspace.lints]`: `unused_must_use` (rustc), `clippy::let_underscore_must_use`. Never the type system. | **enforced against two of the four ways to discard a `Result`** (measured 2026-11-12, throwaway crate with this lint table)<br>unguarded: `let _named = f();` and `drop(f());` pass every gate; no lint closes `drop()`<br>unguarded: a crate/module `#![allow]` (none in the tree for these lints; all 8 present allow only `unwrap_used`/`expect_used`); in use: three item-level `#[allow(clippy::let_underscore_must_use, reason = …)]` on stderr writes, `panel-contract/src/main.rs` and `services/newsvault/src/main.rs` (two)<br>unguarded: doc tests, not covered by `--all-targets`<br>a local `macro_rules!` does not escape these two lints; it escapes row G1's |
| R6 | missing input aborts | `newsvault::lake::tests::a_missing_root_is_an_error`, `…::a_missing_day_is_an_error_not_an_empty_result`, `…::a_missing_or_undecodable_tickers_column_is_an_error_not_an_empty_list`<br>open-web: an absent hosts file and one admitting no host refuse the run at exit 4: `services/newsvault/tests/collect.rs::an_open_web_run_with_an_absent_or_empty_hosts_file_or_an_absent_rules_file_exits_four` | **partly**<br>unguarded: a null string cell reads as an empty string, e.g. null `body` → `body == ""` with `unusable_rows == 0` (§3.9)<br>a null `tickers` cell → empty list is intended (vendor tagged nothing) |
| R7 | units are types, identity is a content hash | `canon` (§2's one canonical form), 16 vectors in `canon/vectors/` (`ls`, 2026-11-15): `canon::digest::tests::every_committed_vector_reproduces_the_digest_it_states`; the `minute` vector reproduced by Python 3 `hashlib` 2026-11-15; minute against instant: `a_minute_and_the_instant_at_its_first_microsecond_digest_differently` (mutation 2026-11-15, minute rendered with the instant tag: both red)<br>producers: `item_id`, `story_id`, `doc_id` in `services/newsvault/src/cmd_collect.rs`, `story_id` also in `edgar-press`'s `Provider::story_of`; `attempt_id`, `filter_params`; `result_digest` in `newsvault ledger` (Python-computed values reproduced); file digests via `canon::FileDigest::of`<br>`items` unique over (`item_id`, `attempt_id`), not `item_id`: `newsvault::store::tests::an_a_then_b_then_a_revert_yields_three_items_two_blobs_and_three_attempts` (mutation-tested 2026-11-13) | identity **enforced as far as a producer exists**<br>units **absent**: nothing carries a basis point or standard deviation<br>unguarded: `canon::digest::snapshot_id` has no producer (grep 2026-11-15)<br>unguarded: no second reader of `items` digests; vectors are `canon`'s own<br>unguarded: the store is handed every digest and computes none, so nothing checks a stored `item_id` against the stored content |
| R8 | presence counted, never multiplied | `panel-contract`, six manifest rules: (a) identity and input fields present; (b) `exchangeable_units` carries the three §3.4 keys, one finding each; (c) cardinality product minus attrition equals `rows`; (d) `rows` equal to a product of stated cardinalities needs `zero_attrition_counted`; (e) unit count equal to `rows` needs `rows_are_exchangeable`; (f) unit count above `rows` is an error<br>seven fixtures incl. `1 672 × 47 = 78 584` rejected, `78 460` admitted<br>binary: `panel-contract/tests/every_committed_fixture_runs_through_the_binary.rs` | **enforced, within the ceiling** (each limit run 2026-11-12):<br>limit 1: Arrow file never opened; invented 248-row stage over `corrected_panel_78460` exits 0<br>limit 2: `rows` = product of 3 of 4 cardinalities, remainder in one stage, exits 0; §3.10.1 would close 1–2<br>limit 3: rule e exact equality only; 78 459 units over 78 460 rows passes; a ratio threshold would close it and needs its own construction first<br>limit 4: attestation and `definition` checks are length floors<br>unguarded: neither end of the file boundary runs it; `probes/panel_recount.py` / `probes/panel_power.py` use `assign.json` + `meta.json` (R9, needs §3.4 layout) |
| R9 | power from the registered procedure | none; `probes/panel_power.py` runs the registered permutation but reads `assign.json` + `meta.json`, not a manifest<br>`design_effect`, `within_unit_correlation`: `panel_contract::Need::DeferredToR9` | **no enforcement point yet**<br>missing: `probes/panel_power.py` reading a panel manifest and aborting when `exchangeable_units` is absent |
| R10 | provenance labelled, **and a mix disclosed** | labelling: `news_provider::Request` field set by `Provider::plan`; boundary = run's UTC date, placed by no measurement (§10, D5); non-nullable enum (§3.1); `providers/edgar-press/tests/edgar_press.rs::a_vendor_archive_window_is_refused_until_a_probed_lag_stands_behind_it` (mutation-tested 2026-11-13)<br>live only, open-web: `plan` refuses with `PlanRefusal::NotTheRunDay` any window whose days are not exactly the run's UTC day, before any request: `providers/open-web-feed/tests/open_web_feed.rs::a_target_other_than_the_run_day_is_refused_before_any_request` (mutation 2026-11-15, the day check removed: red); the driver resolves `--window today` to the UTC date of `--at` and refuses a fetch whose `--at` is off the clock's UTC day: `window_today_resolves_to_the_utc_date_of_at`, `an_open_web_run_whose_at_is_not_the_clocks_day_is_refused_before_anything_is_written` (§11.2); driver half: `services/newsvault/tests/open_web_collect.rs::an_open_web_run_for_yesterday_sends_nothing`, a 2026-11-13 window run on 2026-11-14 makes 0 sends through `cmd_collect::run` and a `--dry-run` binary exits 2 (mutation 2026-11-15, the day check removed: red); a driven run refuses that window before it writes a store file, the lock or a run envelope: `a_driven_collect_refuses_an_unreadable_or_off_day_window_before_writing_anything` (§5.1 `invalid_argument`)<br>run mix: `newsvault collect` envelope `provenance_counts` | labelling **enforced on each request and counted in the run envelope; no table column stores it**<br>open-web live only **enforced** in `plan`<br>disclosure **no enforcement point yet**<br>unguarded: `items` has no provenance column, so snapshot `provenance_counts` (§3.0) has no producer and a pooled live/archive read cannot be reconstructed<br>unguarded: a panel's mix has no manifest field<br>`bulk_dataset` has no producer<br>(`provenance`, `source`) waits on §9 |
| R11 | backfill lag is measured | type: `Visibility::from_backfill` takes a `MeasuredLag`; deserialisation runs the ordering check via `#[serde(try_from)]`<br>caller: `news_provider::provider::probed_lag`, called by `edgar-press::plan` before any `vendor_archive` request (D5, §10): `a_lag_with_no_probe_behind_it_is_refused_in_each_of_the_four_ways_it_can_be_empty`, `a_lag_that_cannot_date_an_item_is_refused_before_any_request`, `a_vendor_archive_window_is_refused_until_a_probed_lag_stands_behind_it`<br>producer: `probes/observation_lag/` measured 94,922 s for `edgar_press`/`vendor_archive` (max of 22 per-filing lags, 2026-11-10 and 2026-11-12, preregistered); `news_provider::lag_policy::LagPolicy` hashed file → `cmd_collect::open_provider`: `a_vendor_archive_window_collects_once_the_measured_lag_is_handed_to_the_provider`, `the_policy_that_dated_a_run_is_in_the_vault_under_its_own_digest`, `the_lag_is_looked_up_by_source_and_by_provenance_together`, `a_policy_stating_only_the_other_provenance_leaves_the_archive_window_refused`, `a_policy_naming_no_lag_is_refused_rather_than_read_as_no_lag`, `a_dry_run_dates_nothing_and_copies_no_policy_into_the_vault`, `a_staged_policy_copy_a_crash_left_behind_is_refused_rather_than_overwritten`, `the_run_envelope_names_the_policy_and_the_probe_that_dated_its_archive_windows`<br>dating: `EdgarPress::backfill_observation`, `pit::datedness::end_of_day`: `a_backfilled_item_observes_at_its_publication_plus_the_probed_lag_and_not_at_the_fetch`, `a_backfilled_day_dated_item_observes_at_the_end_of_its_own_day_plus_the_probed_lag`, `a_late_rows_backfilled_observation_is_dated_by_its_index_day_and_not_its_date_filed`, `a_rows_backfilled_observation_is_not_before_the_end_of_a_date_filed_later_than_its_index_day`, `an_archived_filing_with_no_readable_date_filed_places_no_observation`, `a_late_row_with_an_acceptance_instant_is_not_observed_before_the_end_of_its_index_day_plus_the_lag`, `an_on_time_row_with_an_acceptance_instant_observes_at_its_acceptance_plus_the_lag`, `an_on_time_row_accepted_before_its_index_day_is_not_observed_before_that_day_starts_plus_the_lag`, `an_acceptance_instant_row_with_no_readable_date_filed_is_bounded_by_its_index_days_end` (each mutation-tested 2026-11-15, §3.1), `an_archive_row_with_no_placed_observation_is_refused_rather_than_dated_by_the_fetch`, `a_live_row_that_placed_its_own_observation_is_refused`, `a_lag_past_the_last_representable_instant_is_refused_rather_than_panicking`, `an_archived_item_whose_observation_no_instant_can_hold_is_unusable_and_says_why`, `a_day_ending_at_an_ambiguous_midnight_ends_at_the_later_of_its_two_instants`, `a_day_whose_next_midnight_falls_in_a_gap_ends_where_the_gap_starts`<br>mutation-tested 2026-11-13 and 2026-11-14 | **enforced**: caller, policy path, dating<br>partial: the type requires a `MeasuredLag`, not the measurement: fields `pub`, no validating constructor, so `{probe_id: "", probes: 0, leaks: 0, seconds: 0}` is accepted; only the caller refuses it, in `probed_lag` not `LagPolicy::read` because a lag handed straight to `with_observation_lag` passes no policy file<br>open: minimum lag undecided; 1 s passes<br>instant arm: inverted pair stored `Undated` (`an_observation_preceding_its_publication_is_stored_undated_with_its_raw_blob`)<br>unguarded: day arm has no ordering check; `store::datedness` stores a `Published::On` row observed before its day (run 2026-11-14)<br>open: `ArchiveItemHasNoObservation` unreachable through `edgar-press`: `backfill_observation`'s `Ok(None)` arms are `Published::AtMinute` (since 2026-11-15; `publication` never states a minute, so unreachable), no lag (refused by `plan`), a zone other than `America/New_York` (the one `KNOWN_ZONES` admits), chrono's last date, or for an on-time instant row chrono's first date, a target window naming no day (refused by `plan`), and `Published::Unknown` (filed `NoParseableValidTime` by `document_of`)<br>**enforced** since 2026-11-15: a row with an acceptance instant whose `Date Filed` is not its index's day, or is unreadable, is bounded by the later of acceptance and the end of the index's day, plus the lag; a row whose `Date Filed` is its index's day by the later of acceptance and the START of that day, plus the lag (§3.1, four tests above). Reason, measured 2026-11-15 by a temporary test before the change: `Date Filed` 20261102 listed in the 2026-11-09 index, acceptance 2026-11-02T15:00Z, was observed 2026-11-02T17:00Z under the 7,200 s test lag of `probed()` (7 days before the listing index) and 2026-11-03T17:22:02Z under the probed 94,922 s (about 6 days before); the probed 94,922 s was measured on days with 0 off-day rows, so it does not cover late rows. Now 2026-11-10T06:59:59.999999Z under 7,200 s (the test) and 2026-11-11T07:22:01.999999Z under 94,922 s (arithmetic, not run)<br>reason for the start-of-day floor, from `probes/observation_lag/observation_lag_results.json` read 2026-11-15: the 22 probed lags come from the Tuesday 2026-11-10 and Thursday 2026-11-12 indexes, every one `filed_off_day: false`, the earliest acceptance 2026-11-10T00:39:40Z (19:39 New York the evening before), which set the 94,922 s maximum; no probed row was accepted earlier than that evening. Without the floor, a Friday 18:00 New York acceptance with `Date Filed` the Monday 2026-11-09 was observed 2026-11-07T01:00:00Z under 7,200 s (the test) and 2026-11-08T01:22:02Z under 94,922 s (temporary test, review 2026-11-15), before that index's day began at 2026-11-09T05:00:00Z. The floor also moves that probed 19:39 row: 2026-11-11T07:22:02Z instead of 2026-11-11T03:01:42Z (arithmetic, not run); a row accepted on its index's day in New York is unchanged<br>the floor bites on the real EDGAR calendar, measured 2026-11-15 read-only by two methods that agree (one: SQL over `items` and `attempts` plus `Date Filed` parsed from the daily-index blob `cb619738…fe09`; two, independent of SQLite: acceptance re-read from the raw filing blobs and `Date Filed` from the filing header `FILED AS OF DATE`) on scratch vault `store-20261112`, a session copy that is not kept (runs `adhoc-20261113T185402Z` and `adhoc-20261113T193234Z`, window 2026-11-12, `vendor_archive`, lag 94,922 s from the vault's policy copy `e76d544d55c163d821e8099c12241e4ee1fcd10b58d25f620ab03455ea3bb9cd`, `observed_at − valid_at` = 94,922 s on all 919 rows): 17 of 919 rows are observed earlier than the floor allows: 9 of 470 distinct filings, because both runs collected the same window (9 + 8 rows); the other 902 are unchanged<br>cause of the 17: accepted 2026-11-10 between 17:48:39 and 20:00:22 New York, after EDGAR's close, with 2026-11-11 Veterans Day, so `Date Filed` 20261112; stored `observed_at` 2026-11-12T01:10:41Z..03:22:24Z, before the index day began at 2026-11-12T05:00:00Z; the floor gives 2026-11-13T07:22:02Z, 100,778 to 108,681 s later<br>not observed on data, only tested: the same 919 rows hold 0 late rows, 0 day rows and 0 `Published::Unknown` rows, and every date falls after the 2026-11-01 DST end, so no DST path ran (2026-11-15)<br>the two run envelopes, written by an older build, carry no `observation_lag_seconds`; the current `cmd_collect` writes it<br>unguarded: rows stored before 2026-11-15 keep the old observation: `store` writes `observed_at` once into append-only `items`, nothing re-derives it from the raw blob, and first sighting is `MIN(observed_at)` per `story_id` (`store::first_observed`), so a re-collection's later value cannot displace a stored earlier one<br>no production rows found on 2026-11-15: Mac: the default vault `~/.bellwether/vault` absent, `BELLWETHER_VAULT` unset, and a search of the session scratchpads, `~/Developer`, `~/.alphalens`, `~/.local`, `~/Library/Application Support`, `/private/tmp` and the test temp directories found only scratch and test vaults; VPS: no `commit.sqlite3` and no `blobs/sha256` in any directory the `jacoren` user can read (120 system directories unreadable)<br>open: `leaks` is 0 by construction (lag = max), so `leak_rate()` is uninformative; the informative count is the probe's off-day rows, 0 of 384 and 0 of 549 on the two probed days<br>`bulk_dataset`: no producer |
| R12a | **no automatic derivation from a proposal to an order** | nothing here; the check belongs in the future mediator, whose input may not be a `brief_id` alone<br>formatting a human's decision into an order is allowed; deriving companies or size from a brief is not | **not this repository's gate**; mediator does not exist |
| R12b | the brief carries no order-shaped field | the brief schema, over the field list in §3.6 (one home: the list is the check) | design |
| R13 | five time columns, sequence is the authority | columns: `attempts.requested_at`, `received_at`; `items.committed_at`, `seq` (from `seq_reservations`), `observed_at`, one of `valid_at` / `valid_day`+`valid_tz` / `valid_minute` / `valid_refused`: `a_day_precision_publication_value_is_stored_as_a_day_and_never_as_a_fabricated_midnight`, `a_day_dated_item_is_invisible_until_its_own_calendar_day_has_closed`, `a_day_dated_row_reaches_the_segment_with_its_day_and_its_calendar`, `a_minute_precision_value_is_stored_as_a_minute_and_never_as_an_instant`, `a_minute_dated_row_reaches_the_segment_with_its_minute`, `a_row_holding_a_minute_and_an_instant_is_refused_by_the_schema` (mutations 2026-11-15: segment writes no minute / `valid_minute` dropped from the exactly-one CHECK: red), strict segment reader `a_segment_without_the_minute_column_is_refused_by_the_reader` (mutation 2026-11-15, absent column read as NULL: red; a segment written before 2026-11-15 is refused, and the R11 search of 2026-11-15 found no production vault), `an_item_offering_no_publication_value_is_refused_rather_than_stored_with_an_invented_one`, `a_stored_instant_keeps_the_microsecond_the_item_id_digests`; open-web end to end, `observed_at` equal to the stub's `received_at` and not its `requested_at`, and `valid_minute` set with `valid_at` NULL: `an_open_web_run_collects_a_globenewswire_day_end_to_end` (mutation 2026-11-15, observed at `requested_at`: red)<br>authority: `publish::read_day` keyed on `seq`: `the_ordering_helper_sorts_on_seq_and_leaves_no_row_behind`; `ledger` `unpublished_seq_ranges`: `an_unpublished_seq_range_is_reported_by_the_ledger_and_exported_by_the_next_run` | **partial**: four of five columns written, two read by a test<br>not built: `data_through` has no producer and no column<br>authority: one helper and one span reader; unguarded: no signature refuses a clock (`SegmentRow` carries four time columns)<br>unguarded: the `day` arm is visible 1 µs before its day closes: `pit::datedness::end_of_day` returns the day's last microsecond and both `Datedness::visible_at` and `cmd_items::visible_at` admit `cutoff >= ends` (read from the code 2026-11-15, not run); the `minute` arm was made exclusive that day (§3.1). Left for a separate change because `end_of_day` also dates `edgar-press` backfill observations and its `start_of_day` (§8 row R11)<br>no producer: `items.valid_source` `our_extraction` (`ValidSource::OurExtraction`) is written only by tests (`conformance.rs`, `cmd_collect.rs`, `store.rs`, each after `#[cfg(test)]`, grep 2026-11-15) until the CNBC host (§11.1) is built; the CHECK keeps the value so that host needs no CHECK rebuild migration (§8 row M1) |
| R14 | an attempt is recorded even when empty | `newsvault::store` `record` writes the `Finalised` row for every outcome: `an_attempt_with_zero_items_commits_and_writes_its_row` (mutation-tested 2026-11-13)<br>`outcome` NULL means opened and not closed, the ledger's in-flight count | **enforced for the empty fetch**<br>unchecked: `items_new = 0` on a non-empty fetch (R16) |
| R15 | disputed timestamps marked, never corrected | `record` writes `items.valid_disputed` and `items.earliest_valid_at` on the arriving row only; no stored row is updated<br>`a_disputed_publication_time_is_marked_on_the_arriving_row_with_the_earlier_twins_value` (mutation-tested 2026-11-13)<br>`the_first_row_is_byte_identical_after_the_second_arrives`: catches a write to a stored row only when it is conditional on a twin<br>minute rows within `valid_minute`: `a_re_stamped_minute_is_disputed_on_the_arriving_row_with_the_earlier_minute` (mutation 2026-11-15, minutes skipped: red)<br>rule: a stored twin carries a different publication value; no materiality threshold, no measurement places one<br>lake grain: `lake.rs` keeps the first sighting's publication time when another run of the same UTC day reports a different one, so a disagreement across two UTC days is not counted; counters `valid_time_conflict_events`, `items_with_conflicting_valid_time`; `a_publication_time_disagreement_is_counted_as_rows_and_as_items`, `a_replacing_row_counts_its_disagreement_too`<br>population: memo §5.1 | **enforced on the write side; detection counted in two units at the lake grain**<br>unguarded: no reader of `valid_disputed`, `earliest_valid_at` or `doc_id`<br>unguarded: twins of one story in two precision columns (instant, minute, day) are never compared; day rows are never disputed<br>unguarded since 2026-11-15: a story held under §11.1 is never fetched again, so its re-stamped publication time never reaches `record`; the index value lives only in the index attempt's raw blob (`a_second_live_poll_of_one_day_fetches_no_held_document`: the second poll sends the index alone) |
| R16 | the **coverage ledger** records poll events | `newsvault collect` writes one attempt row per HTTP call; `items_new` has one writer, `record` (first sightings per `story_id`, 0 written as 0)<br>`the_same_story_in_three_commits_keeps_the_first_observation_and_writes_items_new_zero`, `two_identical_collects_leave_the_right_events_one_blob_set_and_items_new_one_then_zero` (mutation-tested 2026-11-13)<br>`ledger --check` exits 1 when `vendor_rows` − `held` > 0 and committed items = 0 (§8 row R4): `check_exits_one_when_a_source_returns_zero_items_against_a_non_zero_vendor_row_count`, `a_cap_does_not_hide_a_check_finding`<br>held filter (§11.1), open-web live: the index is polled every run and a held story's document is not fetched again; `services/newsvault/tests/open_web_collect.rs`: `a_second_live_poll_of_one_day_fetches_no_held_document` (mutation 2026-11-15, the live arm handed an empty set: red), `a_permanently_failed_document_is_not_fetched_again` and `a_document_that_yielded_only_unusable_rows_is_not_fetched_again` (mutation that day, the held read keeping its items arm only: red); `open_web_feed::a_live_follow_up_whose_story_is_held_is_dropped_and_an_unheld_one_kept` (mutation that day, the filter removed: red) | **enforced for the poll event; zero-against-non-zero detected by the reader**<br>held filter: **enforced** for open-web live; edgar live polls every matched row, unchanged; the held read's `empty` arm has no real-path producer, since neither `classify` returns `Empty` for a `Document` (2026-11-15)<br>an all-held open-web day passes `ledger --check` since 2026-11-15 (row R4)<br>unreconciled: `items_new` against `COUNT(items)` when both are non-zero and disagree |
| R17 | matching survives a broad corpus, over article body | body: `edgar-press::EdgarPress::extract` → `ExtractedBody`; `the_extracted_body_is_article_text_and_never_page_html_or_a_script_payload` (mutation-tested 2026-11-13)<br>rules file hashed; `extract_rules_version` is not an `item_id` input: `one_byte_changed_in_the_rules_file_changes_the_extracted_body_and_therefore_the_item_id`, `a_rules_edit_that_leaves_the_bytes_identical_leaves_every_item_id_identical` | **body text enforced for two providers; broad corpus is design**<br>GNW: `<p>` text inside the rules file's `div.main-body-container` (§11.6): `the_extracted_body_is_article_text_and_never_page_html_on_a_globenewswire_page` over the trimmed real release 3403075, whose page holds a `<p>` and two `<script>`s outside the container (mutation 2026-11-15, container widened to `body` in the rules file: red); an absent container is `NoArticleBody`: `a_page_with_no_article_container_is_unusable_as_no_article_body`; a container holding only empty or whitespace `<p>` is `NoArticleBody`: `a_container_holding_only_empty_paragraphs_is_unusable_as_no_article_body` (mutation 2026-11-15, the empty-paragraph filter removed: red, green before the test); `script`/`style`/`noscript`/`template` text and paragraphs without a letter or digit, a non-UTF-8 page, two containers and a page past the depth bound: §11.1 (each mutation-tested 2026-11-15)<br>unguarded: corpus is 2 committed EDGAR fixtures and 1 real GNW page, median extracted body 212 chars against 14,039 real (n=103, §9)<br>gazetteer regression case is `derive`'s; `derive` has no code |
| R18 | two rule versions, one snapshot, a diff | a `derive diff` subcommand; its absence is the gap | design |
| R19 | **a vendor is called no faster than its stated budget, counted against the bucket the vendor counts it against** | `RequestBudget` (per-IP `min_interval`, optional `per_host`) in `TransportProfile`; `edgar-press`: 100 ms both = SEC's 10 req/s per IP (predecessor table, read 2026-11-13; unmeasured, no margin)<br>`Wire::send` (only socket, R4) waits host slot, then per-IP slot (so an IP slot is never held through a long host wait other hosts would queue behind), via `Pace::reserve` on `<store>/pace/` files; `Wire::new` builds its own `ThreadSleeper` (injecting constructor `pub(crate)`); not in-process: in the predecessor (`sec_rate_coordinator.py`), 3 processes on one IP drew a SEC 403, not measured here<br>an unparseable url skips the host wait: safe only because ureq 3.5.2 parses with the same `http::Uri` and fails before any socket (`news_provider::wire::tests::a_url_that_does_not_parse_skips_the_host_wait_and_fails_as_a_transport_error`); a ureq upgrade could change that<br>tests: `news_provider::wire::tests::two_wires_sharing_one_pace_file_serialise_against_each_other`, `news_provider::wire::tests::the_ip_slot_is_reserved_after_the_host_wait_and_not_before_it`, `news_provider::clock::testing::the_process_sleeper_actually_waits`, `news_provider::budget::tests::a_host_key_breaking_any_one_rule_is_refused`, `edgar_press::the_sec_profile_paces_both_our_address_and_the_sec_host_at_ten_a_second`, `open_web_feed::the_profile_paces_every_host_and_our_address_at_one_request_per_ten_seconds` (10 s both, §11.2)<br>admission, open-web: only `admitted` hosts-file rows are planned: `only_admitted_hosts_are_planned` (mutation 2026-11-15, the status filter removed from `plan`: red); a follow-up whose rewritten link leaves its index's host is filtered out: `a_link_off_the_path_filter_off_the_host_or_absent_is_filtered_out_and_counted`; an answer from a host no row admits, a `candidate` row's host included, is `PermanentFailure` and extracts nothing: `an_answer_from_a_host_no_row_admits_is_permanent` (mutation 2026-11-15, `admitted_row` matching any row: red, green before the candidate case; unreachable through `plan` today)<br>robots floor, open-web (§11.2): `collect`, dry run included, refuses an admitted row whose provider `per_host` is `None` or below max(its pinned robots Crawl-delay, 10 s), `cmd_collect::refuse_a_pace_below_the_floor`, after `open_provider` handed the delays to `OpenWebFeed::with_crawl_delays`: `services/newsvault/tests/open_web_collect.rs::a_hosts_row_faster_than_its_robots_crawl_delay_is_refused` (nasdaq's 30 s admitted at the slowed 30 s, refused at an unslowed 10 s); the 10 s is `cmd_collect::PACE_FLOOR`, apart from the provider's own; 4 mutations 2026-11-15, as in §11.5 step 3; mutation-tested: 7 mutations 2026-11-13, re-run 2026-11-15 with `cargo test --locked -p news-provider`, each turning a named test red (`Pace::reserve` returns zero; reserve without waiting; sleep the whole interval; one pace file per `Pace`; an unparseable pace value reset to `now`; reserve before the credential check; an empty `ThreadSleeper::sleep`); 15 mutations 2026-11-14, not re-run<br>retries: one `exchange.send` call site (grep 2026-11-13), structural, not a test; R21 | enforced for every call through `Wire::send`, retries included, per IP and per declared host, within the holes below<br>open-web admission **enforced** in `plan` and the index filter; driven by `newsvault collect` since 2026-11-15, no run against the live host yet<br>open-web robots floor **enforced** since 2026-11-15 for every admitted row of a `collect`; its paths: §8 row R4 and §11.1<br>hole 1: flock untested (hand-measured 2026-11-13: 2 processes, 20 slots, none twice)<br>hole 2: pace file per store, so two stores = two buckets<br>hole 3: no cap on a far-future reservation (a robots Crawl-delay of one year is accepted; owner decision pending, 2026-11-15)<br>a reservation `DateTime` cannot hold is `BudgetError::Unusable`, not a panic: `budget::tests::a_pace_too_long_to_add_is_refused_rather_than_panicking` and `…::a_per_ip_pace_too_long_to_add_is_refused_on_its_first_call` (Crawl-delay 9,000,000,000,000 s panicked at `budget.rs:202` before; mutations 2026-11-15, each unchecked add restored alone: its own test red)<br>unguarded: a reservation `DateTime` holds but past year 9999 is written as `+YYYYY-…`, which `prior` then refuses as `Unreadable` on every later call, so the bucket stays blocked (10,000-year interval, review 2026-11-15); closed by the owner's Crawl-delay cap<br>hole 4: no bounded lock acquire<br>hole 5: R4's `pub fn` in `wire.rs` reaches `exchange` unpaced<br>hole 6: same-host calls can be closer than `per_host` by the per-IP wait (900 ms at 1 s, 2026-11-14)<br>hole 7: one `per_host` per profile; `www.sec.gov` and `sec.gov` are two buckets; open-web: one admitted host's Crawl-delay above 10 s slows every host (§11.1 Pace)<br>hole 8: no test drives `edgar-press`'s profile through a real `Wire`<br>hole 9: redirect targets unpaced (ureq 3.5.2 follows up to 10)<br>hole 10: a host slot is spent when the per-IP file then cannot be read (pace-file error only; aborts the run)<br>open (owner): per-IP file keyed per source, not per address<br>design: per-credential scope not built |
| R20 | **a `vendor_archive` backfill converges: a re-run fetches only the uncovered accessions, and the reader reports the gap** | covered set, the `vendor_archive` arm's: `newsvault::store::Committer::covered_story_ids`, one target window, keyed on committed `items` only; `covered_story_ids_names_a_committed_story_and_not_an_ok_attempt_that_committed_nothing`<br>seam: `news_provider::collect::attempt_once`, `Archive::covered_story_ids`, `Archive::record_planned_coverage`<br>exclusion: `edgar-press::EdgarPress::follow_ups_to_issue`, over the trait's `Provider::story_of`; the covered set is its parameter, not `extract`'s, which stays pure over bytes and rules because the set is run state (an `edgar-press` `story_of` returning `None` turned `a_vendor_archive_follow_up_whose_story_is_covered_is_dropped_and_an_uncovered_one_is_kept` red, 2026-11-15); `a_vendor_archive_rerun_fetches_only_the_uncovered_accession`, `a_vendor_archive_follow_up_whose_story_is_covered_is_dropped_and_an_uncovered_one_is_kept`, `a_live_rerun_still_polls_every_matched_row_even_when_the_story_is_covered`, `a_live_follow_up_is_issued_even_when_its_story_is_already_covered`<br>held set, the `live_ingest` arm's (§11.1): `Committer::held_story_ids`, every target window, items plus closed `usable`/`empty`/`permanent_failure` document attempts; `a_story_held_under_yesterdays_window_is_not_fetched_today` (mutation 2026-11-15, the held read restricted to the newest window: red)<br>gap: `attempts.matched`, `newsvault::store::coverage_up_to`, `ledger --check` raises `coverage_incomplete` on any gap, no threshold; `a_vendor_archive_day_with_an_uncovered_accession_reports_coverage_incomplete`, `a_live_window_with_an_uncovered_follow_up_reports_no_coverage_gap`<br>all mutation-tested 2026-11-13 | **enforced for the `vendor_archive` arm, within these holes:**<br>hole 1: coverage is per store; a window read under two stores counts twice<br>hole 2: the denominator needs an index attempt recording `matched`; a provider that follows up without an index phase records none<br>hole 3: `coverage_up_to` compares a count, not the planned set; a roster change can mask an uncovered story<br>hole 4: `MAX(matched)` never decreases; a filing dropped from the roster leaves a permanent `coverage_incomplete`<br>hole 5: `record_planned_coverage` runs outside the commit transaction; its failure after the items commit leaves `matched` NULL and hides the window from `--check` until the next index run; its live twin, `record_held`, leaves `held` NULL (§8 row R4)<br>hole 6: follow-ups only from the `Index` phase; chained document follow-ups are dropped silently (`edgar-press` returns none today) |
| R21 | **a transient document-fetch failure is re-attempted within the run, bounded, and only a residual gap stands** | `news_provider::collect` re-queues a failed, non-`permanent` attempt (`Attempted::is_transient`) to the back of the queue, capped by `MAX_TRANSIENT_RETRIES` = 2 (a work cap, not fitted to the loss rate)<br>measured 2026-11-13, before retries: run 1 lost 42 of 502 document fetches (15 HTTP 503, 27 connection-level nulls), run 2 lost 43 of 502, a different subset, so the ~8% loss is transient<br>each retry is its own attempt row (R4); the exit counts only the final attempt, `per_source` counts rows<br>a transient failure holds nothing (§11.1), so the next poll fetches it again: `a_transiently_failed_document_is_fetched_again_by_the_next_poll` (mutation 2026-11-15, `transient_failure` added to the held verdicts: red)<br>not an injected backoff `Sleeper`: a second clock in the driver; retries pace through `Pace::reserve` (R19)<br>`services/newsvault/tests/collect.rs::a_transient_fetch_failure_is_retried_and_commits_the_item_within_one_run`, `::a_permanent_fetch_failure_is_attempted_once_and_never_retried`, `::a_request_that_fails_transiently_every_time_is_bounded_to_n_plus_one_attempts`; mutation-tested 2026-11-13/14<br>a body over the profile's `body_limit` is permanent in both providers: `Wire` maps ureq 3.5.2's `Error::BodyExceedsLimit` to `TransportKind::BodyOverLimit`; before 2026-11-15 it was a string like every other transport error and closed `TransientFailure` (review 2026-11-15, read from the code, not run against a vendor); `news_provider::wire::tests::a_body_over_the_limit_fails_as_its_own_kind_and_one_at_the_limit_is_read` over a loopback socket (`body_limit` bytes read, `body_limit` + 1 `BodyOverLimit`, a body cut short `Conversation`), `open_web_feed::a_body_over_the_limit_is_permanent`, `edgar_press::a_body_over_the_limit_is_permanent_and_a_lost_connection_transient`; 5 mutations 2026-11-15, all red<br>bot protection is permanent (§11.1), open-web: 403 and every other 4xx but 408 and 429 close `PermanentFailure`; `a_bot_protection_page_is_permanent_never_transient` over GNW's own Akamai 403 page (2026-11-14; mutation 2026-11-15, 403 classified transient: red)<br>a 408 is transient in both providers (owner decision 2026-11-15, §10), so it is retried within the run and never held: `open_web_feed::a_408_request_timeout_is_transient` and `edgar_press::a_408_request_timeout_closes_a_transient_failure`, each over an index and a document request; `services/newsvault/tests/open_web_collect.rs::a_document_answered_408_is_fetched_again_by_the_next_poll` (3 sends in the first poll, fetched again in the second); mutations 2026-11-15: the open-web 408 arm removed, the first and third red; the edgar 408 arm removed, the second red | **enforced** for every request the driver attempts<br>over-limit body permanent: **enforced** in both `classify`, whatever the status, a 408, 429 or 5xx included: `Wire` does not carry the status on that `TransportError`; no test drives a real provider's `body_limit` (2 MiB, 8 MiB) through `Wire`<br>408 transient: **enforced** in both providers' `classify`; `cmd_robots::verdict_of` keeps a robots 408 permanent (§10, step-3 table)<br>open-web bot protection permanent: **enforced** in `classify`; a challenge page served with 200 is `Usable` and becomes `NoArticleBody` at extraction, untested<br>unguarded: no bypass of the limiter only because there is one `exchange.send` call site, not a test; a second send path would be uncovered<br>unguarded: the cap is uniform per request<br>unguarded: a retry re-asks the identical `Request`; a vendor needing a different url or header is not helped |
| G1 | **a workspace member inherits the lint floor** | `repo_invariants::every_workspace_member_inherits_the_lint_floor` over the `cargo metadata` member set (not `workspace.members`: a `path`-dependency crate is a member, measured 2026-11-12); rejects `lints` inside `[package]`<br>floor contents: `repo_invariants::the_lint_floor_holds_what_the_committed_snapshot_holds` against `repo-invariants/lint-floor.snapshot`; a `warn`/`allow` entry is refused<br>`members` globs `providers/*`, `services/*` | **enforced** over 8 members (2026-11-15)<br>unguarded: a new top-level crate reached by no `members` entry and no `path` dependency is compiled by nothing and invisible to CI (reproduced 2026-11-12)<br>glob cost: a directory without `Cargo.toml` under either parent fails every cargo command (cargo 1.99.1, 2026-11-13) |
| U1 | **the panel names the roster it was built from** | collection side: `news_provider::Request.universe_version` inside `filter_params`; `attempts.universe_version` is `NOT NULL`; `news_provider::request::tests::two_runs_whose_rosters_differ_have_two_filter_params_digests`, `newsvault::store::tests::the_opened_row_carries_the_roster_digest_the_request_was_built_from`<br>absent, unparseable or empty roster: `edgar-press::Refusal` (§9)<br>panel side: nothing | **no enforcement point yet**: the panel side has none; the collection side has only a producer, a column and the two tests in the enforced-by cell<br>unguarded: `panel-contract schema` lists 13 manifest fields, none a roster or `relation_map_version` (measured 2026-11-12), so §2 names five `panel_id` inputs where the gate requires three<br>making them required rewrites seven fixtures and R8: a decision, not an omission |
| E1 | an error code is never renamed, and no code reaches stderr from outside the table | §5.1 stderr codes: nothing; design: workspace member `error-codes/` with `const CODES`, `codes.snapshot`, a snapshot test, and a constructor taking `&'static Code`<br>`panel-contract`'s 15 stdout `Finding` codes: renaming any one turns a named test red (`cargo test -p panel-contract --lib`, measured 2026-11-12) | **no enforcement point yet** for §5.1's codes<br>renaming the 15 `Finding` codes: **enforced**<br>unguarded: a new code needs no table entry and no review<br>unguarded: repurposing a code is not checkable |
| H1 | every difference between `derive`'s picks and the predecessor's has a named cause (memo §5.1, build step 6) | nothing: no cause list, diff artefact or test exists | **no enforcement point yet** |
| M1 | a store at the previous `user_version` opens, migrates and takes a row of every precision the current one stores (§11.1) | `Committer::open` → `migrate`: `newsvault::store::tests::a_store_at_the_previous_user_version_opens_migrates_and_takes_a_minute_row` (from 0); `a_store_at_user_version_one_opens_migrates_to_two_and_takes_a_robots_row` (from 1); carried from 1: `a_v1_store_keeps_every_v1_column_of_every_row` (one row in each of the five v1 tables, every v1 column compared by `quote()`); shape at 1: `a_v1_store_whose_columns_differ_is_refused_and_left_at_user_version_one` (`attempts.hosts_version INTEGER`, `published` dropped; both `Committer::open` and `refuse_an_unreadable_version`); `a_store_at_user_version_two_opens_migrates_to_three_and_takes_a_verdict` (from 2: a `Document` attempt's `verdict` and `story_id`, and a `held` count); carried from 2: `a_v2_store_keeps_every_v2_column_of_every_row` (the v1 fixture plus one `robots` row, every v2 column compared by `quote()`, the new `verdict`, `story_id` and `held` NULL); shape at 2: `a_v2_store_whose_columns_differ_is_refused_and_left_at_user_version_two` (`robots.host INTEGER`, `robots` dropped; both entry points, each matched to the exact `ShapeDiffers { found, columns }`, as at 1); pre-lease accept: the v0, v1 and v2 carried tests assert `refuse_an_unreadable_version` is `Ok` before `open` (mutations 2026-11-15, an `Ok(n) => Err(SchemaVersion)` arm added for n = 0, 1, 2 in turn: each red, the carried test at that version and, at 1 and 2, the shape test too); carried: `a_migrated_v0_store_keeps_every_v0_column_of_every_row_and_both_indexes` (v0 fixture: one instant, one day+tz and one refused row, `seq` 7..9, every nullable v0 column set, `valid_disputed` 1; all 15 v0 columns compared by `quote()` before and after, and the `items_by_story`/`items_by_doc` index names) | **enforced** from user_version 0 to 1: mutations 2026-11-15, the `items` rebuild replaced by the three `ADD COLUMN`s / every migration step skipped: red<br>**enforced** from 1 to 2 since 2026-11-15: mutations that day, `MIGRATIONS[1]` emptied: red; the shape check skipped at 1: red; a table missing at 1 skipped as at 0: red; `DELETE FROM published` added to `MIGRATIONS[1]`: the carried test red<br>**enforced** from 2 to 3 since 2026-11-15: mutations that day, `MIGRATIONS[2]` emptied: red; its `story_id` line alone removed: red; its `held` line alone removed: red; the shape check skipped at 2: red; `UPDATE attempts SET outcome = NULL` added to `MIGRATIONS[2]`: the carried test red. A copy of a test-made v2 store (9 attempts, 5 blobs, 4 items, 4 reservations, 1 robots row; no production store exists, R11 search 2026-11-15) migrated by an `edgar_press` collect over 2026-11-11, a day the fixture calendar closes, `planned_requests` 0 in the dry run first: user_version 3, every v2 column of all 23 rows identical by `quote()`, `verdict` and `story_id` NULL on all 9 attempts (2026-11-15, by hand, not committed; repeated that day on a fresh copy once `story_id` joined `MIGRATIONS[2]`, same result, and again once `held` joined it, over 2026-11-14: `verdict`, `story_id` and `held` NULL on all 9, every v2 column of the six tables identical, `ledger` reading `held` 0 for the copy's one open-web day)<br>carried columns and indexes **enforced** since 2026-11-15: mutations that day, the copy reduced to 11 columns without `seq`, `doc_id`, `earliest_valid_at`, `body_blob` / `doc_id` and `earliest_valid_at` dropped / both `CREATE INDEX` lines dropped: each green before the carried test, red after<br>a v0 store written by an older binary: **enforced** for a missing column since 2026-11-15 (`StoreError::ShapeDiffers { found: 0, .. }`): `a_v0_store_whose_attempts_predate_matched_is_refused_and_left_at_user_version_zero` (fixture: `attempts` as written before 067a0bd, 2026-11-13, 22 columns, no `matched`); mutation that day, the shape check skipped: red. Before it, review 2026-11-15 measured a scratch store of that shape (1,006 attempt rows) migrate to user_version 1 without `matched`, then `ledger` exit 2 on `no such column: a.matched`. Refused, not repaired: only scratch stores have that shape (R11 search, 2026-11-15)<br>a column whose name matches and whose type, NOT NULL, default or key differs: **enforced** since 2026-11-15: `a_v0_store_whose_column_differs_in_type_not_null_default_or_key_is_refused` (four fixtures: `attempts.slots TEXT`, `run_id` without NOT NULL, `outcome DEFAULT 'x'`, `attempt_id UNIQUE` for PRIMARY KEY); mutations that day, each of the four `PRAGMA table_info` fields read as a constant: each red<br>unguarded: at user_version 2, `robots`'s CHECK and FOREIGN KEY clauses are not compared: `columns_of` reads `pragma_table_info` only (review 2026-11-15, read from the code, not run)<br>unguarded: on `attempts`, `blobs`, `seq_reservations` and `published`, the v0 tables `MIGRATIONS[0]` does not rebuild, table-level UNIQUE, CHECK and FOREIGN KEY clauses, AUTOINCREMENT, WITHOUT ROWID and indexes are not compared (review 2026-11-15: `attempts` with `UNIQUE(run_id)` added reached user_version 1 with the clause kept)<br>unguarded: a v0 migration step SQLite refuses leaves `LOCK` and a run envelope under `collect` (§3.1) |

---

## 9. Alternatives considered

| alternative | why not |
|---|---|
| eleven services, one per bounded context | each deployable is another deploy, log, contract version and place something goes quiet; the predecessor has a recorded case where a boundary between components *removed* a standing alert instead of raising one. The eleven contexts are drawn so the seams exist later. |
| Kafka for the event bus | solves throughput we do not have (a few hundred items a day is one event every few minutes) and adds infrastructure a one-person system must operate. The planned `commit.sqlite3` outbox (§6, no code yet) is durable by construction, because it commits in the archive's own transaction, and it is the replay log. |
| a Postgres outbox with `LISTEN`/`NOTIFY` | rule 2 (§6) holds only inside one engine, so an outbox next to `commit.sqlite3` is a dual write, and keeping it atomic means moving the whole archive. The Postgres that "already runs in production" is the predecessor's container: no host port, holds `auth_user`, no automated backup, allowed to stop at memo step 7. `NOTIFY` is not queued for a disconnected listener, so a consumer polls by cursor anyway. Rejected 2026-11-14. |
| Common Crawl's CC-NEWS as the archive | probed and rejected: 7 of our own 287 items for one test day, 2 of our 17 publishers crawled at all, zero tier-1 US financial outlets in 72,000 records, 4.19 TB for ten months, and no index, so no URL lookup and no aimed range request. |
| **collecting the news ourselves** (publisher RSS, EDGAR, or crawling the open web) instead of buying it | **Chosen by the owner 2026-11-14 for the forward feed** (§11); history stays a one-time vendor purchase (`probes/vendor_probe/PREREGISTRATION.md`). Body-length evidence below. |
| a single news vendor for both the live stream and the archive | no vendor reviewed does both: the one with a 2014 archive filters on publication date only, and the one documenting an index-time filter has it behind a quoted Enterprise plan. So provenance labelling is necessary rather than avoidable. |
| incremental salience, updated per event | makes the result depend on arrival order, which destroys the determinism the panel contract is built on |
| the statistics tier as a separate repository | the panel handover is where the worst measured error happened, and a test can only span it from inside one tree |
| vendor dedup and vendor entity tags as ours | unauditable editorial decisions; recorded as data, never adopted (§3.1) |

**Collecting ourselves: the body-length evidence.**

- Pre-registered usability bar: median article body ≥ 2,000 characters, because R17 matches over
  body text (`probes/vendor_probe/PREREGISTRATION.md`, Q3).
- The predecessor's RSS path: median body **117** characters, short of the bar by more than an order
  of magnitude, so RSS alone is a snippet feed whatever it costs.
- Its EDGAR path clears the bar on either of the two figures the probes carry, and they disagree:
  `panel_recount.py` and memo §1.9 say 50,622 characters, `matcher_audit.py` says 14,039.
- 50,622 is not re-derivable: `panel_recount.py` dies on an uninstallable import (`probes/README.md`).
- 14,039 is re-derivable in one command through the compiled reader, no Python:
  `newsvault day 2026-07-01 --lake ~/thematic_news_lake --format json`, body lengths grouped by
  source, re-derived 2026-11-12. One day's median, therefore provisional:

  | source | n | median body chars |
  |---|---|---|
  | `edgar_press_release` | 103 | 14,039 |
  | `rss` | 103 | 120 |
  | `polygon` | 76 | 426 |
  | `perplexity` | 5 | 235 |

- "In-house" has to name its sources before it can be costed: EDGAR-only, RSS and crawling the open
  web have different bodies, volumes and costs.
- CC-NEWS (its row in the table above) is the cheap version of the third.
- So the forward feed fetches article pages, not RSS snippets: on 2026-11-14, 11 article pages over
  6 hosts gave 2,281 to 28,038 characters each, and 2 more were blocked (§11.6).

---

## 10. What this document leaves open

- Nothing of the shape is open; what is open is a choice taken by default, an assumption no measurement places, an unmeasured cost or a provisional number, with owner decisions and built comparators beside them.
- N1 and N2 are recorded below; beyond this section the memo's own list stays open.
- Decision ids: `D1`, `D3`, `D5` are this document's, set out below; `D-1` to `D-8` are the memo's §8 table and are written `memo D-n` here.
- Memo D-1 (which vendor for history) blocks only the one-time history purchase, memo §10 step 5 (backfill to 246 sessions); its state is memo §10 step 2.
- Memo D-1 does not block `edgar-press`'s `vendor_archive` backfill: it collected against live EDGAR on 2026-11-13 behind the probed lag (D5 below, §8 rows R11, R20).
- A purchased history still needs its own probed lag, because a lag is keyed by (source, provenance); today only `edgar-press`'s `plan` asks for one (§8 row R11).

**Three decisions taken by default on 2026-11-13.**

- The owner was asked twice, chose neither, and said to build anyway: each is the design's own recommendation, an assumption, not a settled fact, reversed where its fork bullet names (two functions for D1, one constant for D3, three places for D5).
- **D1, what `1` means.** `1` is **the answer is no**: `newsvault ledger --check` exits 1 on a coverage shortfall and `panel-contract validate` keeps its verdict there; `input_unparseable`, `input_schema_mismatch` and `io_failed` exit `2` (§5.1).
  - Callers: `git grep -n newsvault -- '*.yml' '*.py' '*.sh'` on 2026-11-13 returned two hits, a comment in `ci.yml` and a docstring in `probes/parity_baseline.py`, neither runs the binary; the one caller in the tree now is `probes/newsvault_parity.py`, reading `newsvault day`'s `0` (a result it compares) and `2` (a day that reader refused), never `1`, which `day` cannot return. Callers outside the tree: unknowable.
  - **The other fork is three functions:** `exit_code` in `services/newsvault/src/main.rs` decides every FAILURE exit; `news_provider::Exit` decides every exit a successful `collect` takes, including the stdout rule; `cmd_robots::RobotsExit` decides a successful `robots refresh`'s, its 1 included, and defers to `news_provider::Exit` for 7 and 8; nothing else branches on an exit status.
- **D3, the partition key.** The **publication day**, `items/valid_date=<D>/`, because the label is then a true statement about the rows under it; a day-dated row goes under the day it names, not under an instant nothing published. Its cost is §5's own row.
  - Cost of not having it, measured: 9 of 287 items in the predecessor's `session_date=2026-07-01` partition were published outside that UTC day, 3.14%.
  - Not a seq range: it can be declared complete, but its day-query scan cost is unmeasured.
  - **The fork is one constant:** `VALID_DATE` in `services/newsvault/src/store.rs`, `substr(COALESCE(i.valid_at, i.valid_day, i.valid_minute, i.valid_refused), 1, 10)` since 2026-11-15, which `covered_by_a_segment()`, `Committer::unpublished_rows` and `cmd_items` build from; `publish::partition_of` is the only place it becomes a path.
    - A `minute` row goes under its UTC day, the calendar its value is stated in.
    - `cmd_items` held its own copy of the expression until 2026-11-15; it now reads the constant.

- **D5, a backfill with no probe behind it.** Refused, not dated: a `vendor_archive` window is refused before its first request unless a `MeasuredLag` with a probe id, a non-zero probe count and a positive lag a chrono `TimeDelta` holds stands behind it (§8 row R11).
  - **The lag that stands behind EDGAR, measured 2026-11-13:** `94,922 s = 26.37 h`, the MAX of 22 per-filing lags over the two days the vendor's quarter listing named; each lag is the instant the vendor states it last wrote `form.<day>.idx` minus that filing's `<ACCEPTANCE-DATETIME>`; the run spent 27 requests of a registered 30. Registration, run log and the four refusals the run did not reach: `probes/observation_lag/PREREGISTRATION.md`; per-filing numbers: `observation_lag_results.json` beside it.
  - It falls in the registered band `24 h - 7 d`, so **the backfill is permitted only with the session shift written into the panel and no day column may mix backfilled and live items** - every backfilled item's membership moves by more than one session.
  - The max, not the registered p90 (`57,561 s`, inside 24 h): chosen and exercised offline before the first request, and the reason for the harder band; the registration's amendment dated 2026-11-13 carries the arithmetic.
  - `leaks` counts sampled filings above the scalar, so under a max `leak_rate()` is `0.0` by construction and says nothing about the vendor; the informative count is index rows whose `Date Filed` is not their own index's day, **0 of 384 and 0 of 549** on these two days.
  - **How the number reaches a run:**
    - `--lag-policy` names a hashed policy file holding one `MeasuredLag` per (source, provenance), keyed by both because one adapter read live and read earlier are two populations (R10); `news_provider::lag_policy::LagPolicy::read` is its only constructor; `cmd_collect::open_provider` hands the entry to `EdgarPress::with_observation_lag`; the committed policy is `news-provider/policy/observation-lag.json`.
    - The policy's bytes are archived before the plan (`cmd_collect::copy_policy_into_vault`, called from `cmd_collect::run`); the run envelope names policy, probe and lag (`cmd_collect::envelope_of`); not the item row: §3.1's `Item` has no observation-lag field (no column added: nothing would read it), so a stored item reaches the probe through its `attempt_id` and that run's envelope.
    - Measured 2026-11-13 against live EDGAR with the policy: window `2026-11-12`, run date `2026-11-13`, 1 index request planned, `vendor_archive` arm on all 5 attempts, 4 items committed from a 3-CIK roster out of 7,805 vendor rows, 5 requests spent; without `--lag-policy` it refuses at exit 2 with `no lag entry` before any request.
  - **Which instant a backfilled row carries: `valid + seconds`, with `valid` raised to the listing index day's start (an on-time instant row) or end (any other row), not the fetch clock (§3.1, §8 row R11).** Not the fetch clock: it RECORDS when we read, years late on a real backfill. `valid + seconds` is the COUNTERFACTUAL a point-in-time study wants, one number per source, loose by the spread of acceptance times inside a filing day; a date is bounded at the END of its own calendar day plus the lag.
    - That looseness, measured on the run above while rows were still dated by the fetch clock: the four items' `observed_at` `2026-11-13T17:44:5x Z` against `valid_at + 94,922 s` differ by **not one sign** - `-5.81 h`, `-5.82 h`, `+2.17 h`, `+2.16 h`; the two negatives are the max scalar loose for a row whose own lag was `5.85 h`.
  - **Two places decide it.** WHICH instant: `edgar-press`'s `EdgarPress::backfill_observation`, `Visibility::from_backfill`'s one production caller; it moves into `pit::datedness` beside `end_of_day` when a second `vendor_archive` provider exists, so two providers cannot date one `Published` value differently. WHETHER a row may carry a placed instant: `cmd_collect`'s `Committer::rows_of` stores one for `vendor_archive`, refuses an archive row with none and a `live_ingest` row with one.
  - **The other fork is three places:** `news_provider::provider::probed_lag` holds the six conditions; `edgar-press`'s `plan` is the one caller that asks; `LagPolicy::read` decides what a policy file may say.
- **The provenance boundary, which D5 needs and no measurement places.** A window whose last day is on or before the run's own UTC date is a live ingest and an earlier one a vendor archive; an assumption, not a figure: how long an EDGAR daily index keeps growing is unmeasured, so there is no quantile to put here.
  - **The one place to change it** is `edgar-press`'s `Provider::provenance`, also home of the fail-closed arm: a window nothing can read takes the archive arm, the one that refuses.
- **The backfill coverage convergence, a vertical slice 2026-11-13 (§8 row R20).** Three choices, each changeable in one place.

| choice | rule | **the one place to change it** |
|---|---|---|
| **Which arm converges** | a `vendor_archive` follow-up whose story the store already holds a committed item for is dropped; `edgar-press` always issues a `live_ingest` one (R16); so a re-run fetches only the uncovered accessions. `open-web-feed` is live only and drops a follow-up whose story is held (§11.1) | `edgar-press`'s `EdgarPress::follow_ups_to_issue`; a second provider with a different vendor-row identity maps a follow-up to its story in its own `Provider::story_of` and nowhere else |
| **The denominator** | `MAX(matched)` over a window's index attempts, because each run's index records the same in-universe count and `SUM` would multiply it by the number of runs | `newsvault::store`'s `coverage_up_to` |
| **The gate** | `newsvault ledger --check` raises `coverage_incomplete` whenever `covered < planned`, on a gap of ANY size - no percentage, no tolerance, because a threshold chosen after seeing a result is forbidden here | the `coverage_incomplete` push in `cmd_ledger`'s `read` |

- **The transient-retry bound, taken by default 2026-11-13 (§8 rows R19, R21).** A document fetch that fails transiently (HTTP 503, or a transport error other than a body over the profile's `body_limit`) is re-attempted within the run, bounded by `MAX_TRANSIENT_RETRIES` = 2, a MECHANISM cap on the work spent per request whose only job is to stop one persistently failing accession stalling the run; what still fails is a coverage gap R20 converges or the next run fills.
  - Not a figure read off the ~8% transient loss measured that day: deriving a retry count from the observed rate is choosing a threshold after seeing the result.
  - **The one place to change it** is the `MAX_TRANSIENT_RETRIES` constant in `news_provider::collect` (re-exported from the crate root); the rate it is spaced at is R19's, not this knob's.
- **The per-host pace, taken by default 2026-11-14 (§8 row R19).** Three choices; the first two change in `news_provider`.
  - **The key** is the request url's host, lowercased, not the source and not the registrable domain, so two sources asking one host share a bucket and `sec.gov` beside `www.sec.gov` would not; changed in `Wire::wait_out_the_host` and `HostKey::new`, the file name in `BudgetScope::file_name`.
  - `HostKey::new` refuses a trailing dot since 2026-11-15, because `host.` and `host` are one host and would be two buckets and two hosts-file rows: `a_host_key_breaking_any_one_rule_is_refused` (mutation 2026-11-15, the check removed: red). No committed url or hosts row ends in a dot (grep 2026-11-15).
  - **The order** is host wait first, then the per-IP reservation, so a slow host never holds an IP slot other hosts queue behind; its price: two calls to one host can leave closer than `per_host` by the per-IP wait owed after the host wait (§8 row R19, hole 6); changed in `Wire::send`.
  - **The SEC declaration**, 100 ms per host for `www.sec.gov`, the same interval as its per-IP pace, lives in `edgar-press`'s `transport()`; setting it to `None` returns the run to the per-IP pace alone.
- **The run's own key, taken by default 2026-11-13.** A run id has a schedule-slot form and an ad-hoc form (`run_id`, `ADHOC`, `RUN_ID_FORMAT` in `services/newsvault/src/cmd_collect.rs`); `run_date` is the UTC date of the run's own instant for BOTH, because no schedule registry exists to take a slot's date from (accepted attack A1.7 asked for the slot's date where there is one).
  - **The one place to change it** is `run_id` in `services/newsvault/src/cmd_collect.rs`.

**The calendar, the listing and the roster are FILES the caller names, taken by default
2026-11-13; robots.txt is a fetched vendor payload, since 2026-11-15.**

- Not a fetched quarter listing: as a hashed input file it keeps a collection reproducible from the files alone and leaves `EdgarPress::listing_url` with no production caller. **The one place to change it** is `cmd_collect::inputs`.
- Required for `edgar_press` and refused for `open_web_wires`, which takes a hosts file instead (§11.2's flag table); which source takes which is `cmd_collect::open_provider`.
- robots.txt is fetched, under attempt rows of its own: `attempts.source` `robots` (`cmd_robots::ROBOTS_SOURCE`), run envelopes of kind `robots_refresh`, phase `Index`, no third `RequestPhase`. A caller-named hashed file, the hosts file, pins its digest (§11.1). The fetch is `newsvault robots refresh` (§11.2).
  - Rejected: a third `RequestPhase`, which forces `classify` and `extract` arms in `edgar-press` and `open-web-feed` that no request reaches. Rejected: source `open_web_wires`, whose `ok` rows with `vendor_rows` 0 would pull down `ok_vendor_rows`' trailing median and appear in that source's ledger rows.
  - The ledger counts only `collect` envelopes in `runs_observed`, `runs_expected`, `runs_unfinished` and the per-source day rows, and takes every kind for the orphan check; the asking clock skips `source = 'robots'`. So a robots 403 is no `holds_a_failure` finding and a robots run fills no missing collect run: `a_robots_refresh_run_is_not_counted_against_expected_runs`, `a_robots_refresh_runs_attempts_are_not_orphans`, `a_robots_fetch_moves_neither_the_asking_clock_nor_a_ledger_check_finding` (mutations 2026-11-15, every envelope counted / `known_runs` from `collect` only / the asking-clock filter removed: each red). Rejected: no envelope, or one in a subfolder: the robots attempts would be orphans and `ledger` would exit 8.
  - `kind` is optional inside `newsvault.run/1`, absent read as `collect` and never written for it, so every earlier envelope and the ledger golden file are unchanged. **The one place to change it** is `cmd_ledger::RunKind`.
  - `no_registered_source_is_named_robots`: a registered source of that name would vanish from the asking clock.

**The vendor's timeouts and body limit, the first an assumption and the second now measured.**

- Timeouts: one unmeasured value, `VENDOR_PATIENCE` in `providers/edgar-press/src/lib.rs`.
- Body limit: 8 MiB, against full daily indexes of **1,057,192 B** / 5,429 filing rows (2026-11-10) and **1,520,327 B** / 7,805 rows (2026-11-12), read as received by `probes/observation_lag/probe.py` on 2026-11-13; the vendor's quarter listing states them as `'1033 KB'` and `'1485 KB'`, rounded to KB, so it is not a source of a byte figure; real rows are 191 to 194 bytes, not the 110 bytes the committed fixture's header carries. **The one place to change them** is `edgar-press`'s `Provider::transport`.
- An answer over the body limit closes `PermanentFailure` and is not retried (§8 row R21).

**The archive's second copy: the VPS, with the provider's own backup behind it (N1, decided
2026-11-12 by the owner).**

- The second copy does not exist yet: no vault on the VPS on 2026-11-15, and no newsvault process, timer or cron entry there (§8 row R11).
- Open, not blocking the system's shape: nobody has restored the backup and nobody has timed a restore; one timed drill, recorded once, closes both (a backup nobody has restored is not yet a backup).
- Once the store runs in WAL (§6), copy `commit.sqlite3` with `.backup` or `VACUUM INTO`, because a file copy loses commits still in `-wal`.

**What the drill compares, and what the 156-of-156 does not say.**

- (1) `probes/parity_baseline.py` hashes (sha256) the sorted distinct **vendor ids** (lake `id`, §3.1's `source_item_id`), **not** `item_id` (§2, produced only in the new archive), so it shows the id **set** came back and would match with every title, body and url different.
- (2) It agreed on **156 of 156** days, re-derived 2026-11-12, over a corpus with none of the shapes the two readers can disagree on (across 779 run files and 243,226 rows: zero null ids, empty-string ids, null publication or ingest times and inverted rows; no file missing `tickers`), so `unusable_rows` and §5's rows 4 and 5 have never fired on real data and the number says the lake is clean, not that two readers encode one rule.

**The comparator exists since 2026-11-13**, which closes the third thing that was not true of it and makes the first two measurable rather than merely admitted.

- `newsvault day` emits a `content_digest` beside the `id_digest` (`lake::Day::content_digest`), so a matching id set with a different title no longer passes; `probes/newsvault_parity.py` computes both in pandas, compares every reported field day by day and exits 1 on any difference, under a decision rule registered before it ran (`probes/newsvault_parity/PREREGISTRATION.md`, which holds its run log).
- Committed corpus, 9 partitions and 11 run files, one partition per shape the lake lacks: null id; real empty-string id; null publication time; null ingest time; inverted row at both precisions; re-stamped repeat; repeat arriving with an earlier observation; day-dated value beside an instant one; run file missing a column. Its first run disagreed on exactly one of them and the reader changed (row R3).
- **CI runs no Python**, so the gate is the probe's committed output: `services/newsvault/tests/parity.rs` is the only reader of `expected.json`.
- Not covered: the corpus is synthetic rather than a sample; both readers read the same parquet bytes through two arrow bindings; the new archive's own rows have no second reader.

**The risk the owner accepted, stated rather than implied.**

- Both copies sit in one provider and one account: a disk, a kernel or a bad deployment is covered; a billing, suspension or credential loss is not. A copy at a second provider was the alternative and was not taken.

**Freshness: 2 hours inside the US session, 6 hours outside it (N2, measured 2026-11-12).**

- The alert is the age of the newest item (§5's "a consumer is silently dead" row), measured over the predecessor's lake (156 partitions, 779 run files, 243,226 raw rows, publication times 2026-05-19 to 2026-11-10) as gaps between consecutive publication instants over **61,282** distinct items.
- **61,282** is the union of distinct `id` values across the archive, because a gap is a property of the archive, not of a partition; memo §5.1's **62,507** is the per-partition count summed, double-counting 1,225 repeats of 501 ids (row R3's grain); both re-derived 2026-11-12 in one pass.

| window | p50 | p90 | p99 | p99.9 | worst |
|---|---|---|---|---|---|
| 13:00–20:00 UTC, the US session | 1.0 min | 10.1 min | 26.7 min | 50.3 min | **1.2 h** |
| outside it | 0.3 min | 13.0 min | 45.0 min | 105.1 min | 332.6 h |

**What would have fired is not what that table says.**

- A gap's quantile is bucketed by the hour it **starts**, while the alert asks whether the newest item's age exceeds the threshold at an instant inside the window, so a gap from 12:10 to 15:00 UTC is an out-of-session gap and an in-session alert. `probes/news_gap.py` prints both, measured 2026-11-12:

| episode | gap | 2 h in session | 6 h outside |
|---|---|---|---|
| 2026-05-19 → 06-02 | 332.6 h | fires | fires |
| 06-02 → 06-03 | 16.8 h | fires | fires |
| 06-03 12:10 → 15:00 | **2.8 h** | fires | does not fire |
| 06-05 → 06-08 | 93.2 h | fires | fires |

- All four start outside the session, which is why gap-start bucketing reports zero in session; the control reproduces that zero. **The threshold choice survives**: three of the four are outages the 6-hour rule catches anyway, so 2 hours adds one alert in 175 days, 2026-06-03's.

**What makes those two numbers wrong.**

- They measure the predecessor's **retrieval** coverage, not the world's news, so a different vendor moves them; provisional, re-measured against `open-web-feed` (§11) and the first real vendor.
- The publication gap transfers where an observation gap would not, because the predecessor's `ingested_at` spacing is its scheduler, not the news; the record already holds outages: 161 of the 175 days hold an item, 14 hold none, and the 332.6-hour gap is one.

**Defaults taken by the builder on 2026-11-15, open-web step 2 (§11.5), not by the owner.**

- Construction: the review of 2026-11-15 listed the step-2 lines that state a choice; each was checked against the 2026-11-14 owner lines of §11.1 and §11.2, and those the owner lines already decide are left out (`universe_version` as the digest of zero bytes is the owner's "empty for the POC"; `Minute` precision is the owner's "Decided" bullet). The reason behind each default was not recorded when it was taken.

| default | where it is changed |
|---|---|
| a 429, a 5xx and a transport error are transient, except a body over `BODY_LIMIT` (2 MiB), which is permanent; every other non-200 status but 408 is permanent (the owner decided bot protection, permanent, and on 2026-11-15 a 408, transient) | `OpenWebFeed::classify`, `providers/open-web-feed/src/lib.rs` |
| edgar-press: a 408 is transient, the owner's 2026-11-15 rule above; an S3 403 `AccessDenied` stays permanent because its arm is matched first | `EdgarPress::classify`, `providers/edgar-press/src/lib.rs` |
| a `pubDate` carrying seconds is `NoParseableValidTime`, never truncated to its minute | `minute_of`, same file |
| a minute is visible from its close (the next minute's start) and only once observed; stored `Undated` only when observed before the minute starts (§3.1) | `Datedness::of_minute` and `Datedness::visible_at`, `pit/src/datedness.rs` |
| `item_key` is the rewritten, lowercased host plus the path, without scheme, query or fragment | `OpenWebFeed::item_key` and `host_and_path`, `providers/open-web-feed/src/lib.rs` |
| a relative canonical href is resolved against the page's url; another host is kept (§11.1) | `absolute`, same file |
| credential name `open_web_contact`, read from `BELLWETHER_FEED_CONTACT` | `CONTACT_CREDENTIAL` in that file; `Registered::contact_variable` in `cmd_collect.rs` |
| a second distinct `--source` is refused (§11.2) | `Registered::chosen`, `cmd_collect.rs` |
| an `EveryUtcDay` run whose `--at` is not the clock's UTC day is refused (§11.2) | `cmd_collect::run` |
| the page depth bound, 112 = 14 (the untrimmed GNW article, measured 2026-11-15) × 8; the margin is the builder's (§11.1) | `DEEPEST_PAGE`, `providers/open-web-feed/src/parse.rs` |
| a page with two containers, or not UTF-8, is unusable rather than read from its first container or decoded lossily (review remedy, 2026-11-15) | `parse::page`, same file |

**Defaults taken by the builder on 2026-11-15, open-web step 3 (§11.5), not by the owner.**

| default | rejected, and why | where it is changed |
|---|---|---|
| a robots attempt is `source` `robots`, `target_window` the lowercased host, phase `Index`, `live_ingest`, `universe_version` the digest of zero bytes, `hosts_version` the hosts file's, through the open-web `TransportProfile` and `open_web_wires` pace files | the robots bullet above | `cmd_robots::request_for`; the `Wire` in `main.rs` |
| the link from fetch to bytes is the `robots` table (§3.2) | an `attempts.raw_blob` column, which moves `blobs_indexed` for every run and the ledger golden file | `MIGRATIONS[1]`, `Committer::record_robots` |
| the pin is one hosts-row field `robots`: 64 lowercase hex of the body as received after content decoding, or `absent`; required on admitted rows, optional on candidates; schema `open-web-feed.hosts/2` | a state plus a digest, which can disagree; `null`, which cannot tell "forgotten" from "404" | `RobotsPin::read` and `read_hosts`, `providers/open-web-feed/src/hosts.rs` |
| 200 present (ok); 404 absent (empty), no digest; any other 4xx (403, 410) failed permanent; 429, a 5xx and a transport error failed transient; no robots row for a failure; one attempt per row, no retry, no startup sweep | RFC 9309's "4xx means allow all", which admits `www.globenewswire.com`, whose robots.txt answered an Akamai 403 (§11.2) | `cmd_robots::verdict_of` |
| a refresh fetches every row, admitted and candidate, writes attempt rows, `robots` rows, blobs and an envelope, and never the hosts file; a pin that differs exits 1 with both digests | rewriting the hosts file, which re-admits a host whose robots changed without a person looking | `cmd_robots::run`, `exit_of` |
| CLI `newsvault robots refresh --source open_web_wires --hosts --rules [--store] [--dry-run] [--non-interactive] [--format]`, run id `robots-<YYYYmmddTHHMMSSZ>` from the clock, no `--at` | | `RobotsAction` in `main.rs`; `cmd_robots::run` |
| robots.txt is read by our own RFC 9309 subset: the `*` groups only, merged, the largest Crawl-delay; a non-UTF-8 body, a 200 with no `user-agent` and a non-numeric Crawl-delay refused (§11.1) | a crate, which adds a dependency for four rules; a group naming our token, which code cannot know (the token is the contact variable's) | `newsvault::robots::Robots::parse` |
| the paths checked are each admitted row's index path with its query and its rule's `path_prefix`; documents are not checked per request | a per-request check, which needs the parsed robots inside the provider, not only the delay | `OpenWebFeed::admitted_paths`; `refuse_what_the_pinned_robots_forbids` in `cmd_collect.rs` |
| the floor compares the provider's one `per_host` with max(Crawl-delay, 10 s) per admitted host, `None` refused; the provider sets `per_host` to the largest admitted delay above 10 s, so every host is slowed to the slowest | refusing any host above 10 s, which leaves the hand-off no reader; an `interval_s` hosts-row field, a second number that must equal the profile's | `refuse_a_pace_below_the_floor` and `PACE_FLOOR` in `cmd_collect.rs`; `OpenWebFeed::with_crawl_delays` |
| the pinned blob is read in `open_provider`, by digest and without the commit log, before the provider is boxed, and re-hashed (`store::blob_bytes`); a blob absent then is `robots_unverified` at the check, one whose bytes changed `robots_refused` | reading it after the `Wire` exists, which paces the run at a profile the check never saw | `admitted_robots` in `cmd_collect.rs` |

**Defaults taken by the builder on 2026-11-15, open-web step 4 (§11.5), not by the owner.**

| default | rejected, and why | where it is changed |
|---|---|---|
| permanence is stored in a third new column, `attempts.verdict`, the four `Verdict::name` labels `CHECK`ed, NULL when abandoned or pre-v3; §11.1 names only `story_id` and `held` | a new outcome label (`failed_permanent`), which changes §3.2's outcome vocabulary and every ledger count keyed on `failed`; a boolean `permanent`, which cannot tell `Usable` from `Empty` | `MIGRATIONS[2]`; `Verdict::name`, `news-provider/src/ledger.rs` |
| `Finalised` carries the `Verdict` inside `Outcome`, a private field set by `Outcome::decide`, None from `Outcome::abandoned` | a separate `verdict` field on `Finalised` passed to `AttemptSlot::close`: two values, kind and verdict, that can disagree | `Outcome`, `news-provider/src/ledger.rs` |
| `story_id` rides on `Opened`, through `AttemptSlot::open`'s `story: Option<&Digest>`, computed by the driver from `Provider::story_of` | on `Finalised`, where a swept attempt names nothing; a `Request` field, a second source of truth beside `story_of` that changes every `Request` literal; a second storyless constructor the driver could call by mistake with every test green | `attempt_once`, `news-provider/src/collect.rs` |
| `story_id` is written for `Document` requests only, NULL on index and robots attempts | `story_of` on every request: open-web's `item_key` can key the index url, and that index attempt would then hold a story | `attempt_once` |
| `held` is an INTEGER on an index attempt, `matched - issued`, written whatever the count; NULL on document, robots and pre-v3 rows | a per-document flag: an all-held day has no document attempt to carry it, so `ledger --check` could subtract nothing | `Archive::record_held`, `Committer::record_held` |
| `held` is written on `live_ingest` index attempts only; edgar live writes 0, `vendor_archive` rows stay NULL | counting the covered stories edgar's archive arm drops: an all-covered archive re-run would lose today's `rows_committed_zero_against_vendor_rows` finding, a change to edgar §11 does not ask for | `attempt_once` |
| `held` is its own `UPDATE` after the commit, as `record_planned_coverage` is; a crash before it leaves NULL, read as 0, a loud false finding (§8 row R4) | `held` on `Finalised` inside the commit transaction: needs the follow-up selection moved before `slot.close`, which reorders edgar's archive writes (`matched`, §8 row R20 hole 5) | `attempt_once`; `Committer::record_held` |
| two sets: the live arm reads `held_story_ids(source)`, every window; the `vendor_archive` arm keeps `covered_story_ids(source, window)`, items only | one held set for both arms: edgar's archive re-run would stop re-fetching a permanently failed accession and would skip across windows, a change to §8 row R20's convergence | `attempt_once` |
| edgar live still issues every follow-up; only open-web drops held stories | edgar live skipping held stories, which `a_live_rerun_still_polls_every_matched_row_even_when_the_story_is_covered` and `a_live_follow_up_is_issued_even_when_its_story_is_already_covered` forbid; §11 scopes the held filter to the feed | `EdgarPress::follow_ups_to_issue`; `OpenWebFeed::follow_ups_to_issue` |
| a follow-up whose `story_of` is None is never held, always issued | dropping it: a skip nothing justifies | `OpenWebFeed::follow_ups_to_issue` |
| the held set is read once per live index attempt, after that index's commit, with no cutoff (the driver holds the lease) | an in-run seen set in the driver: new driver state §11 does not ask for; its cost, a story listed by two index attempts fetched twice, is §11.1's unguarded bullet | `attempt_once` |
| pre-v3 rows are not backfilled: `verdict`, `story_id`, `held` stay NULL | backfilling `verdict` from `outcome`: `failed` names neither failure kind, so half the column would be invented | `MIGRATIONS[2]` |
| the ledger finding is `rows_committed == 0 AND vendor_rows - held > 0`; `filtered_out` is not subtracted | a `filtered_out` column, a fourth new column §11 does not name; revisit when a real all-held day flags | `findings_for`, `services/newsvault/src/cmd_ledger.rs` |
| `ledger`'s source row gains `held`, printed by the human renderer; schema stays `newsvault.ledger/1`; the collect envelope `newsvault.collect/2` is unchanged | a ledger schema bump, which an added field does not need; `held` in the collect envelope, whose reader the ledger already is, and whose busy-time fields are step 7 | `SourceRow`, `render_human`, `cmd_ledger.rs` |
| the held read keeps `empty`, as §11.1's table states, though no provider closes a `Document` `Empty` (§8 row R16) | dropping `empty`, which contradicts the table | `Committer::held_story_ids` |
| no index on `attempts(source, story_id)` | an index before a measurement shows the scan matters | `MIGRATIONS[2]` |
| one schema version for the step: user_version 3, `MIGRATIONS[2]` grown across its commits | a version per commit: three migrations in one PR and versions no released build wrote | `MIGRATIONS`, `SCHEMA_VERSION` |

---

## 11. `open-web-feed` — our own forward news feed (decided 2026-11-14; §11.5 steps 1 to 4 are built; `newsvault collect` drives it)

- Every line in §11.1 and §11.2 is an owner decision of 2026-11-14, except the `ValidSource` cells of §11.1's valid-time table and the bullet below that table, both decided 2026-11-15, and the sub-bullets marked `Built 2026-11-15`, which record what was built; the choices among them the owner did not make are listed in §10's 2026-11-15 defaults table.

| term | meaning |
|---|---|
| **POC** | proof of concept: the first build |
| **Target** | the shape the crate grows into; nothing in it is built or gated |
| **Held** | the store already has an answer for a story, so the feed does not fetch it again |
| **Admitted** | a hosts-file row with status `admitted`: its robots.txt allows the paths used and the host passed a probe |
| `Index` request | a dated news sitemap or RSS feed |
| `Document` request | one article page |
| GNW / PRN / CNW / BI / Fool | GlobeNewswire / PR Newswire / Canada Newswire (`www.newswire.ca`) / Business Insider / The Motley Fool |
| Fed | the Federal Reserve press feed (`www.federalreserve.gov/feeds/press_all.xml`) |

### 11.1 Fixed in both layers

- Crate `providers/open-web-feed/`, top-level `[lints] workspace = true`; no clock and no HTTP client in the crate.
  - Built 2026-11-15: `OpenWebFeed` implements `Provider` for `open_web_wires`, offline over committed fixtures; `newsvault collect --source open_web_wires` drives it through the same `Wire`, store and envelope as `edgar_press` (§11.2). No run against the live host yet.
  - Parsers: RSS through `roxmltree` 0.21.2, pages through `scraper` 0.29.0 (html5ever); the dependency graph grew from 161 to 192 packages (manifest comment, 2026-11-15). `serde` 1.0.229 is read directly for the duplicate-key check and the count stays 192 (same command, 2026-11-15).
  - `OpenWebFeed::open(hosts, rules, contact)` refuses before any request, one `Refusal` arm each, one test each: absent or unparseable file, a field the reader does not know, a host named twice (case-insensitive), a host `HostKey` refuses, a row naming a rule the rules file lacks, an index url on another host, zero admitted rows (R6), a contact with no address, one host rewritten to two hosts by two rules (`a_host_two_rules_rewrite_to_two_hosts_is_refused`; mutation 2026-11-15, the refusal disabled: red, green before the test).
    - Added 2026-11-15 (review: each loaded and then yielded nothing usable), each red under the mutation that removes its check, run once:

| refusal | test |
|---|---|
| a key named twice in one JSON object of either file (`serde_json::Value` keeps the last, so `"status":"candidate","status":"admitted"` was admitted) | `a_key_named_twice_in_one_object_of_either_file_is_refused` |
| a rewrite source or target `HostKey` refuses | `a_rewrite_to_a_host_no_url_can_name_is_refused` |
| one rule rewriting one host twice in two cases (both lowercase to one key; the last was kept) | `a_host_one_rule_rewrites_twice_in_two_cases_is_refused` |
| a `path_prefix` not starting with `/` | `a_path_filter_not_starting_with_a_slash_is_refused` |
| a `valid_format` that does not print 2026-11-14T19:07Z as text `minute_of` reads back as that minute, or prints second 31 differently from second 0 | `a_valid_format_that_does_not_read_back_a_minute_is_refused` (fixture: `%S` added; two mutations, the whole check and the seconds half: both red); `a_valid_format_whose_minute_does_not_parse_back_is_refused` (fixture `%d %b %H:%M GMT`, no year, so only the read-back half refuses it; mutation 2026-11-15, `is_some_and` → `is_none_or`: red, green with clippy clean before the test) |

  - Hosts file `hosts/open_web_wires.hosts.json`, schema `open-web-feed.hosts/2`: one row, GNW (`rss.globenewswire.com`, index `https://rss.globenewswire.com/rssfeed/`) admitted, `robots` pinned to `eb8bb698171dd56c139c224c9217070adf9947f6ed6cd02336eba55aa4155bfe`: the sha256 of the 2026-11-14 fetch of its robots.txt (4,354 B, HTTP 200, `date` 19:21:07 GMT, decompressed by `curl --compressed`), committed byte for byte as `tests/fixtures/gnw/robots_rss_globenewswire_com.txt` (`the_committed_hosts_file_pins_the_committed_robots_body`). Unknown: whether the live bytes are still identical, and whether `ureq`'s decoding gives curl's bytes; a first live refresh that differs exits 1 and the pin is edited by hand.
    - An admitted row with no pin, and a pin that is neither 64 lowercase hex nor `absent`, are `HostsUnparseable`: `an_admitted_hosts_row_without_a_robots_pin_is_refused`, `a_robots_pin_that_is_neither_64_lowercase_hex_nor_absent_is_refused` (mutations 2026-11-15, the admitted check removed / any text read as a digest: each red).
    - `collect` reads the pin since 2026-11-15: an admitted row is refused unless the store's newest answered robots fetch of its host is the pin (§8 row R4, robots clause). No live refresh has run, so a live store refuses GNW until the owner runs one.
  - Rules file `rules/open-web-feed.rules.json`: per rule the `pubDate` field and its exact minute format, the path filter `/news-release/`, the `www.` → `rss.` rewrite, the title and canonical picks, the container and paragraph selectors; its digest is `extract_rules_version`.
  - Classify: index 200 with rows `Usable`, with none `Empty`, not RSS `PermanentFailure`; document 200 `Usable` (extraction decides `NoArticleBody`, `PageNotUtf8`, `PageTooDeep`, `ArticleContainerAmbiguous`); 408, 429, 5xx and transport errors transient, except a body over `BODY_LIMIT` (2 MiB), permanent (§8 row R21); every other status permanent.
  - Precision ceiling `Minute`; a `pubDate` with seconds is `NoParseableValidTime`, never an instant. So is a `pubDate` the rule's format does not print back byte for byte: chrono's `%Y` read `Sat, 14 Nov 26 19:00 GMT` as the year 26, and `%H` read `9:05` (review 2026-11-15); `a_globenewswire_pubdate_is_a_minute_and_never_an_instant` (mutation 2026-11-15, the round trip removed: red). Unknown: how GNW spells a day before the 10th; the 20 items of 2026-11-14 are all day 14. Today `Sun, 01 Nov 2026 19:00 GMT` is a minute and `Sun, 1 Nov 2026 19:00 GMT` is `NoParseableValidTime` (chrono reads both; only `01` prints back): `a_single_digit_day_is_a_minute_only_when_zero_padded` (mutations 2026-11-15, the round trip removed / the reprint under `%-d`: each red). The rule changes only after a real GNW `pubDate` on days 1-9 is recorded.
  - Index rows: only an element with no namespace is an RSS field; `has_tag_name(&str)` compared the local name alone, so a `dc:pubDate` or `atom:link` placed first was read instead (`a_namespaced_element_named_like_an_rss_field_is_never_read_as_that_field`; mutation 2026-11-15, the namespace check removed: red). A link whose path holds a `.` or `..` segment is filtered out (`/news-release/../rssfeed/` passed the prefix; `a_link_off_the_path_filter_off_the_host_or_absent_is_filtered_out_and_counted`, mutation 2026-11-15: red; the `.` arm alone removed: red since 2026-11-15, green before `/news-release/./x.html` was added); percent-encoded dots are not decoded. A second row keying a story an earlier row of the same index keyed is counted in `filtered_out`, not followed up (§8 row R3).
  - Body: the text of the one container's `paragraph` descendants, skipping `script`, `style`, `noscript` and `template` text; a paragraph counts only if it holds a letter or a digit (U+200B alone passed before, review 2026-11-15). `paragraph_text_skips_script_style_and_noscript_and_needs_a_letter_or_a_digit` (mutations 2026-11-15, the skip removed: red; the letter-or-digit test replaced by non-empty: red).
  - A page whose bytes are not UTF-8 is `PageNotUtf8`; it was decoded lossily and its `meta charset` is never read (`a_page_that_is_not_utf8_is_unusable_and_never_decoded_lossily`). A page matching the container selector more than once is `ArticleContainerAmbiguous`; only the first was read (`a_page_holding_the_article_container_twice_is_unusable_as_ambiguous`). Each red under its mutation, 2026-11-15. The untrimmed release 3403075 holds one `div.main-body-container`, as the trimmed fixture does (measured 2026-11-15 with `scraper` on the 2026-11-14 probe body, 71,657 B, HTTP 200; that body is not kept in the repository).
  - Depth bound: `PageTooDeep` past 112, read by `depth::nesting_depth` from the source tags before html5ever parses. html5ever is superlinear in nesting: 2,000 / 4,000 / 8,000 nested `<div>` parsed in 0.011 / 0.027 / 0.070 s (release, 2026-11-15), about ×2.5 per doubling; a second run that day: 0.009 / 0.024 / 0.068 s.

| quantity, 2026-11-15, release build, one run each | value |
|---|---|
| `nesting_depth` of `article_3403075.html` (trimmed); html5ever's element depth of it | 7; 7 |
| untrimmed release 3403075, the 2026-11-14 probe body (71,657 B, not kept in the repository): `nesting_depth`; html5ever's element depth (debug test build) | 14; 14 |
| bound: 14 × 8, so 8 times the untrimmed page's depth | 112 |
| a 2 MiB page of closed 112-deep `<div>` blocks (2,096,864 B): scan / parse | 0.007 s / 0.061 s |
| a 2 MiB page of 419,430 open `<div>`: scan, before the parse is refused | 0.008 s |

    - The scan's first version was itself quadratic: the same 2 MiB page took 346.6 s, because each tag searched the open stack for `svg`/`math`; each entry now carries that count.
    - The count is meant never to fall below html5ever's depth less the implied `html` and `body`: an end tag closes its element only when it is on top or below implied-end elements (`p`, `li`, ...) alone, and is otherwise ignored, as html5ever ignores an end tag naming no open element; raw text is skipped except inside `svg`/`math`; a comment ends where html5ever ends it (`<!-->`, `--!>`); a `>` inside a quoted attribute does not end a tag. `depth::tests::each_rule_of_the_scan_counts_its_own_row`, 17 rows; 14 mutations 2026-11-15, one per rule: 14 red; the stray-end-tag rule's mutation (the implied-end elements popped although the name is not open): red on its 2 rows.
    - Before that rule (review 2026-11-15, release build, one run each): `<rb><rt></x>` repeated 16,000 times (192,000 B) counted 2 while html5ever held 32,002 and parsed in 1.78 s; `<ul><li></x>` × 300 counted 301, html5ever 602. Both now count 600 for 300 rounds.
    - `a_page_nested_past_the_bound_is_unusable_and_one_at_the_bound_is_read` (mutations 2026-11-15, the check removed: red; `>=` for `>`: red).
    - Unguarded: html5ever's reconstruction of active formatting elements adds nodes no source tag opens. Probe 2026-11-15: 800 rounds of `<p><b x=i>t</p>` (13,490 B) built 322,004 nodes, depth 804, in 0.154 s; the scan counted 1,600 and refuses it, but no general rule bounds that reconstruction.
    - Sloppy markup (`<span><b>x</span>`) counts deeper than html5ever's tree, so a real page could be refused. The `PageTooDeep` share per host cannot be read: no `UnusableReason` is stored, only the count `unusable_rows` in the run envelope (grep of `newsvault`'s store and envelope, 2026-11-15).
  - `universe_version` is the digest of zero bytes. A follow-up whose story is held is dropped (`OpenWebFeed::follow_ups_to_issue`); one with no `story_of` is issued: `open_web_feed::a_follow_up_with_no_story_is_issued_and_a_held_one_dropped`, a port-bearing url built by hand beside a held one (mutation 2026-11-15, `is_none_or` made `is_some_and`: red). `index_rows_of` cannot emit such a follow-up: every url it builds is `https://{host}{path}` from a pair `rewritten` already accepted. User-Agent by credential name `open_web_contact`; `cmd_collect::Registered::credential` names the same credential for each provider: `every_registered_credential_is_the_one_its_providers_user_agent_names` (mutation 2026-11-15, the open-web entry naming edgar's credential: red, green before the test).
  - Conformance: `the_open_web_feed_breaks_no_claim_of_the_seam` (harness over the fixtures, plus every produced item at exactly the ceiling: mutation 2026-11-15, ceiling `Instant`: red).
- Live only: `plan` refuses any target that is not the run's UTC day, before any request; no `vendor_archive` request can exist.
- Every UTC day is collected, weekends included. Investor-relations pages are never fetched. Observation time is `Wire`'s `received_at`: `an_open_web_run_collects_a_globenewswire_day_end_to_end`, whose stub receives one second after it sends (mutation 2026-11-15, `collect` observing at `requested_at`: red; green before the stub told the two apart). The store refuses an observation time set by the provider: `Committer::rows_of` in `cmd_collect.rs`, `StoreError::LiveItemPlacedItsOwnObservation`, `a_live_row_that_placed_its_own_observation_is_refused`.

| host | valid time |
|---|---|
| PRN, GNW, Fed | RSS `pubDate`, `ValidSource::VendorPublished` |
| CNW | sitemap `news:publication_date`, a full instant with offset, `ValidSource::VendorPublished` |
| CNBC | the page's `article:published_time`, `ValidSource::OurExtraction`; never its sitemap (§11.6) |
| Yahoo | its syndication time field, `ValidSource::VendorPublished`; the field is named in its host PR |
| Fool, BI | the sitemap's publication field, `ValidSource::VendorPublished`; the field is named in their host PR, as Yahoo's is |
| never | `lastmod`, `dateModified`, fetch time, a date folded into midnight |

- `ValidSource::PageMetadata` stays unbuilt.
- **`ValidSource` per host, decided 2026-11-15 by the owner.** Both arms exist (`provider.rs:41-46`); `CollectedItem.valid_source` is required (`provider.rs:147`); no item from CNW, Yahoo, Fool or BI is built yet, so no code changed.
  - `VendorPublished`: a value read from a structured feed field the publisher states (RSS `pubDate`, sitemap `news:publication_date`, a sitemap's publication field, Yahoo's syndication time field).
  - `lastmod` stays excluded (the `never` row), so only a sitemap's publication field is `VendorPublished`, and a sitemap with no publication field gives no valid time.
  - `OurExtraction`: a value read out of an article page under a hashed rule (CNBC's `article:published_time`).
  - Rejected: `OurExtraction` for sitemaps, because the arm would then mean "we parsed XML" and no longer separate "the publisher stated it" from "we inferred it from the page".
- Bot protection (HTTP 403, a challenge page, a web application firewall, DataDome) is a `PermanentFailure`, never retried in another form.
- robots.txt is respected; a robots.txt 404 is recorded as "no robots" and is not digest-pinned. Sites' terms of use are not a criterion (owner, 2026-11-14).
  - Built 2026-11-15: `newsvault::robots::Robots`, our own RFC 9309 subset, no new dependency: only the `user-agent: *` groups, merged; `Allow`/`Disallow` by longest pattern, `Allow` winning a tie, `*` any run of characters, a final `$` the end; the largest `Crawl-delay` of those groups, in decimal seconds; a line ends at CR, LF or CRLF (RFC 9309 §2.2), a leading UTF-8 byte order mark is dropped, a `#` cuts the rest of a line, an empty `Allow`/`Disallow` value is skipped. Refused: a body that is not UTF-8, a 200 naming no `user-agent` (an HTML page at the robots path), a `Crawl-delay` that is not digits with an optional fraction. Unknown lines are ignored.
  - Tests in `services/newsvault/src/robots.rs`, over the 2026-11-14 18:40:10 GMT probe bodies of `www.nasdaq.com` (`*` Crawl-delay 30, bingbot 10), `www.accessnewswire.com` (5) and `www.prnewswire.com` (none), copied byte for byte to `services/newsvault/tests/fixtures/robots/`, the GNW fixture and ten synthetic files: `the_star_group_is_read_and_a_named_bot_group_is_not`, `a_named_group_after_the_star_group_adds_no_star_rule`, `a_cr_only_or_bom_led_body_keeps_its_star_rules_and_crawl_delay` (before 2026-11-15's fix, review that day: each body parsed with no `*` rule and no delay), `a_trailing_comment_is_cut_from_a_rule_and_a_crawl_delay`, `an_empty_disallow_disallows_nothing`, `allow_wins_a_tie_and_the_longest_match_decides`, `wildcard_and_end_anchor_match_as_rfc_9309_states` (GNW's `*/news-release/*.html?pdf=1` blocks a release's PDF form, not `/news-release/`), `a_crawl_delay_that_is_not_a_number_is_refused`, `a_robots_200_with_no_user_agent_line_is_refused`, `the_gnw_robots_allows_the_rssfeed_index_and_the_news_release_prefix`. Mutations 2026-11-15 (review round), each run once with `cargo test --locked --no-fail-fast -p newsvault --lib robots`: `lines()` restored; the BOM strip, the group reset, the comment cut and the empty-value skip each removed: 5 of 5 red, each its named test only.
  - Unguarded: percent-encoding is not normalised; a group naming our own product token is never read, because the token is `BELLWETHER_FEED_CONTACT`'s, not code's; document urls are not checked per request, so a Disallow narrower than the path prefix (`/news-release/2026/`) passes admission and its documents are fetched.
- `item_key(url)` is the one function behind `source_item_id`; a page's canonical link feeds `doc_id` only.
  - Built 2026-11-15, default taken without the owner: one rule for an href with a scheme and a scheme-relative one (`//` gets `https:`): kept as written when it names any authority, port, userinfo and a host `HostKey` refuses included; with none (`https://`, `https:///x`) the page's url. A root-relative href is resolved against the page's url; a path-relative or non-web href, or none, leaves the page's url. Another host is kept, because `doc_id` is cross-source.
  - Rejected: the href as written, which digested the bare path `/news-release/...` (review probe 2026-11-15). Rejected: `host_and_path`'s check on either form, which replaced a `:443` canonical by the page's url, so one link spelled `https://h:443/x` and `//h:443/x` gave two `doc_id` values (reviews 2026-11-15).
  - `a_relative_canonical_link_is_resolved_against_the_page_url_before_it_reaches_doc_id`, 9 hrefs; mutations 2026-11-15: href passed as written: red; `//` sent through `host_and_path`: red; an empty authority kept: red (both green before their cases).
  - GNW's `doc_id` is not cross-source: the real page's canonical names the requested host `rss.globenewswire.com`, its `og:url` names `www.globenewswire.com` (`tests/fixtures/gnw/article_3403075.html` lines 20 and 21). No code reads `doc_id` (2026-11-15); reversing the rewrite or preferring `og:url` is picked when a second source citing GNW is built.
- Pace: `interval = max(Crawl-delay, 10 s)`; a host may be slowed, never sped up. Busy time = summed paced waits plus request times across hosts; worst case ≤ 50% of a day.
  - Built 2026-11-15: `OpenWebFeed::with_crawl_delays` takes the Crawl-delay per host that `newsvault` parsed from each pinned body; `transport()` sets `per_host` = max(10 s, the largest ADMITTED host's Crawl-delay), per IP stays 10 s. One `per_host` serves every host (§8 row R19 hole 7), so one slow host slows all of them: `a_crawl_delay_above_ten_seconds_slows_every_host_and_one_below_speeds_none_up` (30 s gives 30 s; 5 s gives 10 s; a candidate's 60 s gives 10 s).
  - Hole 7's price: the 50% worst case is no longer bounded by a 10 s interval, because one admitted host's Crawl-delay multiplies every host's paced wait. Probe material of 2026-11-14 (`probe/robots/`, `probe2/`): Crawl-delay above 10 s only at `www.nasdaq.com` (30 s), which is not planned; `www.accessnewswire.com` and `www.newswire.com` state 5 s; GNW, PRN and Yahoo state none; CNW's and Fed's robots.txt were not fetched.
- Hosts file: one hashed file per SourceId under `providers/open-web-feed/hosts/`; its digest is `hosts_version` (§2), stored in its own `attempts` column and in the run envelope. Only admitted rows are planned.
  - Built 2026-11-15: `collect --hosts`; the envelope field is the stdout envelope's (`newsvault.collect/2`), not the `runs/` file's (`newsvault.run/1`), which names no input digest.
- `universe_version` = digest of this provider's ticker roster file (empty for the POC). Path filters and the Yahoo key list live in the rules file, so `extract_rules_version` covers them.
- Held is keyed on (SourceId, `story_id`) over every target window, never the run's window alone.

| story state | held? |
|---|---|
| has a committed item | yes |
| `Document` attempt closed `Usable`, `Empty`, `PermanentFailure`, or with only unusable rows | yes (an `Empty` or unusable page: re-fetching returns the same page; the raw blob, §3.9, allows re-extraction). Unguarded: an unusable reason taken from the index row rather than the page (`NoParseableValidTime` from `listed_date`, `BlankSourceItemId`) is held the same way, though neither reason holds for it: the valid time is not in the page blob and `listed_date` is stored nowhere, so a later listing or a changed pubDate rule never reaches the story again (review 2026-11-15, two-poll driver test; owner decision pending) |
| `TransientFailure`, interrupted by the startup sweep, in flight | no |

- `Usable`, `Empty`, `PermanentFailure`, `TransientFailure` are `news_provider::ledger::Verdict` arms; §3.2's `OutcomeKind` maps both failures to `failed` (`Outcome::decide`, `news-provider/src/ledger.rs`). `Finalised` carries the `Verdict` inside `Outcome` (`Outcome::verdict`, None from `Outcome::abandoned`), stored as `attempts.verdict` (§3.2), since 2026-11-15.
  - `Outcome` derives `PartialEq`, so receipts compared in tests compare verdicts too; a replay with the same items and another verdict is still accepted, because the replay rule compares items only (`StoreError::ReplayWithDifferentItems`).
- The held read: `Committer::held_story_ids(source)`, behind `Archive::held_story_ids`, since 2026-11-15: committed `items` joined to their attempt's `source`, UNION closed `attempts` of that `source` with a non-NULL `story_id` and `verdict` in (`usable`, `empty`, `permanent_failure`); no `target_window`, no cutoff (the driver holds the lease, as for `covered_story_ids`).
  - `held_story_ids_names_each_state_the_held_table_holds_and_no_other`: one row per table state (an item whose attempt names no story, usable with no item, empty, permanent, transient, swept, in flight); mutations 2026-11-15: the items arm dropped: red; the verdict arm dropped: red; `transient_failure` admitted: red.
  - `held_story_ids_spans_every_target_window_and_one_source_only`: each arm holds one story under yesterday's window only, beside a story under today's; mutations 2026-11-15: either arm filtered to the latest window: red; either arm's `source` predicate dropped: red.
  - unguarded: `closed_at IS NOT NULL` is redundant with the `verdict` test, since an in-flight row has NULL `verdict`; dropping it stayed green (2026-11-15).
  - only-unusable rows: such a `Document` closes `Usable` and commits no item, so the verdict arm holds it (the "usable with no item" row).
  - `empty`: neither provider's `classify` returns `Empty` for a `Document` (2026-11-15); that arm is reached by the store test alone.
  - the real-path caller, since 2026-11-15: `news_provider::collect::attempt_once` hands a live index's follow-ups the held set; the `vendor_archive` arm keeps `covered_story_ids` (one window, items only), so edgar's archive convergence (R20) is unchanged. `EdgarPress::follow_ups_to_issue` ignores the set for `live_ingest`, so edgar live still issues every follow-up; it costs one held read per live index attempt.
  - unguarded: within one run, a story listed by two index attempts is fetched twice; each index reads the set before the other's documents run. Unreachable while GNW is the only admitted host (2026-11-15).
  - pre-v3 rows carry no `story_id` or `verdict`: a document that failed permanently or yielded only unusable rows before the upgrade is fetched once more; a committed story is still held through `items`.
- New columns `attempts.story_id` and `attempts.held` (§3.2, both built 2026-11-15); `ledger --check` subtracts `held` (§8 row R4). First sighting is unchanged: a skip writes no item. So R15 cannot see a re-stamped time of a held story; the index value survives only in the attempt's raw blob.
- Schema versioned with `PRAGMA user_version`, read at `Committer::open`: a lower version is migrated by explicit `ALTER TABLE` (a table rebuild for a `CHECK` change); an unknown higher version is refused. Built 2026-11-15 at user_version 1 (§3.1); step 2 grew that one migration rather than adding a version per commit; step 3 added user_version 2, the `robots` table; step 4 added user_version 3: `attempts.verdict`, `attempts.story_id` and `attempts.held`, one migration grown across the step.

### 11.2 POC

- One SourceId, `open_web_wires`; `open_web_outlets` arrives with its first outlet (Target). A second SourceId in one invocation needs a run envelope with a list of `hosts_version` values.
  - Built 2026-11-15: a second distinct `--source` is `invalid_argument` (`cmd_collect::Registered::chosen`); `two_sources_in_one_invocation_are_refused` (mutation 2026-11-15, first source accepted: red).
- Hosts, in order: GNW, then PRN, CNW, Fed, Yahoo (wire keys only). One PR per host, as each robots fetch lands. CNW and Fed are admitted only after a body probe.
- GNW article URLs are rewritten from `www.` to `rss.` (both GNW's own): `www.globenewswire.com` returned Akamai 403 on robots.txt twice; its article pages were never tested.
- Yahoo: only provider keys of wires not fetched directly (`business-wire.com`, `accesswire.ca`), matched exactly, never as substrings. Not PRN, GNW, CNW keys: each release would be stored twice. Deferred: Zero Hedge, Newswire.com, Business Wire direct.
- Robots: a refresh command fetches robots.txt through `Wire` under an attempt row; the hosts file pins the robots digest. How a robots attempt is labelled and kept out of the expected-run count: §10's "calendar, listing and roster are FILES" paragraph.
  - Built 2026-11-15: `newsvault robots refresh` (`services/newsvault/src/cmd_robots.rs`), one request `https://<host>/robots.txt` per hosts-file row under an `AttemptSlot`, through the collector's `Wire`, credential and pace files; the body of a 200 is a blob linked by the `robots` table (§3.2); its exits are §5.1's; `newsvault schema` path `["robots", "refresh"]`, checked against the parser by `every_option_the_robots_refresh_parser_takes_is_in_the_machine_readable_schema`.
  - Tests over a stub `Exchange` and a fixed clock: `a_robots_refresh_writes_one_attempt_per_hosts_row_under_source_robots`; `a_robots_404_is_recorded_as_absent_and_pins_no_digest` (mutation 2026-11-15, 404 a failure: red); `a_robots_403_5xx_or_transport_error_is_a_failed_attempt_with_no_robots_row`, 429 included (mutations 2026-11-15, 403 absent / 429 permanent: each red); `a_fetched_robots_digest_other_than_the_pin_exits_one_and_names_both` (mutation 2026-11-15, `matches_pin` always true: red); `a_candidate_whose_fetch_differs_from_its_pin_exits_zero` (mutation 2026-11-15, `row.admitted` dropped from `exit_of`: red); `a_robots_refresh_dry_run_plans_one_url_per_row_and_writes_nothing`, through the library and the binary.
  - Unguarded: `Wire` still follows redirects (R19 hole 9 until step 5), so a redirected robots.txt is stored under the host that was asked.
  - Unguarded: a refresh that never runs, or dies, is invisible to the ledger, which counts only `collect` envelopes; no timer runs it (step 7).
  - No run against a live host yet.
- The robots and floor checks run in `newsvault` (only the driver reads the store); it hands the provider the Crawl-delay parsed from the pinned robots blob.
  - Built 2026-11-15: `cmd_collect::open_provider` reads each admitted row's pinned blob by digest (`store::blob_bytes`, no commit log), parses it and hands the delays to the concrete `OpenWebFeed` before it is boxed, so the `Wire` in `main.rs` and the floor check read one profile. After the pin check (§8 row R4), `refuse_what_the_pinned_robots_forbids` refuses the row's paths (`OpenWebFeed::admitted_paths`: the index url's path with its query, then the rule's `path_prefix`) and the floor (§8 row R19); dry run included; codes §5.1.
  - Tests in `services/newsvault/tests/open_web_collect.rs`: `an_admitted_row_whose_index_or_path_prefix_robots_disallows_is_refused`, `a_hosts_row_faster_than_its_robots_crawl_delay_is_refused`, `a_pinned_robots_body_that_does_not_parse_is_refused`, `a_robots_blob_written_after_the_provider_opened_is_unverified`, `a_path_or_unreadable_robots_refusal_names_its_fact_and_suggests_no_refresh`; in `open-web-feed`: `the_admitted_paths_are_each_admitted_index_with_its_query_and_its_rule_prefix`.
  - Unguarded: `newsvault robots refresh` paces its own fetch with the provider opened without delays, so at 10 s per host.
- Redirects refused: `TransportProfile.redirects = Refuse` sets `max_redirects(0)`; a 3xx is a `PermanentFailure`, which closes R19 hole 9 by refusal.
- One interval for all hosts: every admitted host is at 10 s. R19 hole 7 stays open.
  - Built 2026-11-15 as one interval, max(10 s, the largest admitted Crawl-delay), not 10 s (§11.1 Pace).
- Decided: `ValidPrecision::Minute` for GNW's minute-resolution `pubDate`. Built 2026-11-15; producer: `open-web-feed`'s document extraction: `pit::datedness::Minute` and `Published::AtMinute`, `canon::Stamp::Minute` with `canon/vectors/item_id_minute_precision.json`, `items.valid_minute` in the rebuilt table (§3.1, §8 row M1). Driven by `newsvault collect --source open_web_wires` end to end: `an_open_web_run_collects_a_globenewswire_day_end_to_end` (§11.5 step 2). `items.valid_raw`, `valid_field`, `valid_source` stay nullable after the rebuild: rows committed at user_version 0 have no evidence (§3.1).

| window (UTC) | poll every | run stops issuing requests when busy time would pass |
|---|---|---|
| 11:00–22:00 (pre-market through after-close releases) | 30 min | 15 min |
| outside | 90 min | 45 min |

- Both poll periods are the owner's choice, not measured; the weekday probe checks each against its index window. GNW's Saturday window allows at most 118 min (3.93 h × 0.5).
- The stop deadline (half the poll period) is what holds busy time ≤ 50% of a day. Unissued follow-ups count as `deferred` and are retried at the next poll.
- Follow-up cap: 150 per invocation, the owner's choice, not measured. Worst case per run: (150 + index requests) × (1 + `MAX_TRANSIENT_RETRIES` = 2) × 10 s.
- Conformance: exempt by a provider declaration, a new trait method without a default; `edgar-press` declares itself session-bound, so its check stays.
  - Built 2026-11-15: `Provider::cadence()` returns `Cadence::{SessionBound, EveryUtcDay}`; `news_provider::collect` refuses a calendar or listing the cadence does not take, and a missing one it needs, with `PlanRefusal::InputsDoNotMatchCadence` before the startup sweep reads the archive; `--dry-run` refuses the same way, untested. No CLI input reaches either refusal: `cmd_collect::open_provider` refuses the same mismatch first, from the flags, before the store is read (below). `OpenWebFeed::plan` runs the same check itself, so a future driver that skips both still gets the refusal: `a_calendar_or_a_listing_handed_to_plan_is_refused` (review 2026-11-15: the check removed, every test stayed green before this test; mutation that day: red).
  - Tests, each turned red by one mutation 2026-11-15: `edgar_press_declares_itself_session_bound` (declaration set to `EveryUtcDay`); `an_every_utc_day_provider_is_not_held_to_the_listed_session_check` (check run unconditionally); `a_session_bound_provider_that_asks_for_a_closed_day_is_still_named` (check never run); `a_session_bound_run_with_no_calendar_is_refused_before_any_request` (driver check removed: the archive is read first).
- `collect` takes `--calendar` and `--listing` per provider; the open-web run takes neither. Accounting is per (day, host).
  - Built 2026-11-15 in `cmd_collect::open_provider` over the table `cmd_collect::Registered` (source, credential name, contact variable, own files, cadence):

| flag | `edgar_press` | `open_web_wires` | decided by |
|---|---|---|---|
| `--calendar`, `--listing` | required | refused | `Registered::cadence`, equal to the opened provider's `Cadence` (`every_registered_cadence_is_the_one_its_provider_declares`; mutation 2026-11-15, the open-web entry `SessionBound`: red) |
| `--roster` | required | refused | the table |
| `--hosts` | refused | required | the table |
| `--lag-policy` | optional | refused (live only) | `open_provider` |
| `--rules` | required: the chosen provider's file | same | clap |
| contact variable | `BELLWETHER_SEC_CONTACT` | `BELLWETHER_FEED_CONTACT` | the table |

  - A missing required flag or a refused one is `invalid_argument`, exit 2, before any input file, store read, write or send: `a_flag_error_beside_an_absent_input_file_is_still_a_usage_error` (mutation 2026-11-15, the calendar/listing checks after the provider's `open`: red; review 2026-11-15 measured exit 4 there before the move); `an_open_web_run_refuses_a_calendar_or_listing_flag`, `an_edgar_run_without_a_calendar_is_a_usage_error` (mutation 2026-11-15, the cadence check dropped: both red; the roster/hosts check dropped: the second red; the lag-policy check dropped: the first red); `an_open_web_run_reads_its_contact_from_its_own_variable` (mutation 2026-11-15, both entries on the SEC variable: red).
  - `--window today` is the UTC date of `--at` (`cmd_collect::target_of`); the envelope's `window` is the resolved day: `window_today_resolves_to_the_utc_date_of_at`, run under `TZ=Pacific/Kiritimati` with `--at` 23:30Z (mutation 2026-11-15, `chrono::Local`'s date: red).
  - `--window today` for a `SessionBound` source (`edgar_press`) is `invalid_argument`, in `open_provider`, dry run included: a UTC date is not a New York session day. Before 2026-11-15 it was accepted: review that day measured `--at 2026-11-11T02:00:00Z --window today --dry-run` plan `2026-11-11`, exit 0. `an_edgar_run_refuses_window_today` (mutation 2026-11-15, the check removed: red, and the `today for edgar` row of `every_collect_usage_error_is_written_as_a_usage_error`: red).
  - An `EveryUtcDay` run (not a dry run) whose `--at` is not the clock's UTC day is `invalid_argument`, before the envelope is written: `an_open_web_run_whose_at_is_not_the_clocks_day_is_refused_before_anything_is_written` (mutation 2026-11-15, check removed: red). A dry run may name another day.
  - A dry run plans one index request per admitted host and writes nothing: `an_open_web_dry_run_plans_one_index_request_per_admitted_host_and_writes_nothing`.
  - Unguarded: the `required_for` / `refused_for` lists in `newsvault schema` are written by hand beside the table; `every_option_the_collect_parser_takes_is_in_the_machine_readable_schema` checks the names, not those lists.
  - Built 2026-11-15 in `plan_accounts_for_every_day`: a (day, host) pair is asked at most once; each day the window names has at least one request or exactly one `NotPlanned` row, never both; a request whose url host `HostKey` refuses is a breach. Host = `news_provider::budget::host_of`, the rule `Wire`'s host pace also uses.
    - Exemption: a `NotPlanned` row naming the whole target window accounts for it only when it is the plan's one row and the plan has no request; otherwise every clause above runs. `a_one_day_target_asked_twice_and_also_not_planned_is_named_and_its_row_alone_is_not` (one-day target `2026-11-14`, the open-web shape; mutation 2026-11-15, the exemption for any whole-window row restored: red; a review probe that day showed that exemption hid a day asked twice and also not planned).
  - Tests: `two_hosts_on_one_day_account_once_each` (red when keyed by day only, 2026-11-15); `each_claim_of_the_harness_is_raised_by_a_provider_that_breaks_only_that_claim` gained four arms (one pair asked twice, an unkeyable host, a day both asked and not planned, a day not planned twice), each red under the mutation that drops its clause, 2026-11-15.
- Fixtures: trimmed real pages from the 2026-11-14 probes, a few paragraphs each, plus one synthetic page whose canonical link differs from its URL.
  - Built 2026-11-15 in `providers/open-web-feed/tests/fixtures/gnw/`: the 19:22:19Z RSS cut to its first 3 of 20 items; release 3403075 (fetched 19:36:55Z from `rss.`) cut to its head, the source `<p>`, two `<script>`s, the first 3 container paragraphs and one tag; the Akamai 403 from `www.` robots.txt; seven synthetic files (one index item, a page whose canonical link differs, a page whose canonical link is root-relative, a page with no container, a container holding only empty paragraphs, a container whose paragraphs hold script, style and noscript text and zero-width spaces, a page with two containers). Release 3403075 is the only real article captured; GNW pages on `www.` were never fetched.
- Scheduler: a VPS timer outside the crate.

### 11.3 Target shape

| item | target |
|---|---|
| sources | every candidate that passes robots and a weekday probe, incl. Business Wire if stable, Zero Hedge, Newswire.com (measured zone, date-only valid time) |
| `open_web_outlets` | Fool, CNBC, BI, Yahoo outlet syndication; Fool's earnings-call transcripts are a separate item kind, field named when built |
| per-host intervals | R19 hole 7 closed (`HostPaces::Listed`) |
| redirects | followed as their own paced, ledgered requests; hole 9 stays closed; needs `RawResponse` headers and `Document`-phase follow-ups |
| robots | re-fetched on a schedule; a changed robots digest blocks the host until the hosts file is updated. Built 2026-11-15 without the schedule and wider than a host: `robots_refused` refuses the whole run (§5.1) |
| collectors | per SourceId in parallel; needs a startup sweep that tells live runs from dead ones; then the lease covers only `Committer::reserve` through COMMIT (§6) |
| follow-up cap, run length | set from measured weekday busy time; busy seconds printed in every envelope |
| `NoArticleBody` share per host | a ledger reading; its threshold is fixed before its first measurement |
| outbox poller | wakes `derive` and feeds the private stream (memo D-7) |

### 11.4 Gates planned (each leaves this table for the §8 row named when it bites; none of the rows below exists, 2026-11-15)

| gate | turns red when | §8 row |
|---|---|---|
| a Yahoo row outside the key list is filtered out | exact match becomes substring match | R3 |
| (c) a redirect is a permanent failure, not followed | `max_redirects(0)` is removed | R19 |
| the extracted body is article text, per further host | the selector widens to `<body>` | R17 |
| a rolled-back `record` leaves no outbox row | the insert moves outside the transaction | new row O1 |
| a zero-item attempt writes one outbox row | the insert is skipped when rows are empty | R14 |
| CNBC valid time is the page meta (Target) | the sitemap value is used | R13 |

- Unguarded in the POC: no reader of the stored raw valid-time fields (§3.1); items lost between polls (an index whose oldest item is newer than the previous poll's newest).

### 11.5 Build order (each PR passes the gate alone)

1. Crate skeleton with `[lints] workspace = true`, so row G1 covers it. Built with no dependency and no item; each mutation of the new crate turned the named tests red, 2026-11-15:
   - `[lints]` stanza deleted: `every_workspace_member_inherits_the_lint_floor` (G1)
   - `ureq` added: `only_the_seam_crate_declares_an_http_client` and `the_seam_crate_declares_the_only_client_this_workspace_holds` (R4)
   - a `Utc::now` read added: `no_provider_and_no_part_of_the_seam_outside_wire_and_clock_reads_a_clock` (R4)
2. GNW end to end, the largest PR (touches `pit`, `canon`, `news-provider`, `newsvault` and the new crate): hosts loader, provider table in `cmd_collect` (replacing the single `EdgarPress`); index and document parsers, `item_key`, `Provider::story_of`, `NoArticleBody`; schema versioning, `items` valid-time columns, `hosts_version` column and envelope field; `Minute` precision; conformance declaration and per-(day, host) accounting in `plan_accounts_for_every_day`; per-provider `--calendar`/`--listing`, `--window today`; an end-to-end test.
   - Built 2026-11-15: schema versioning at user_version 1, `items.valid_raw`/`valid_field`/`valid_source`, `attempts.hosts_version` (§3.1, §3.2); `Minute` precision through `pit`, `canon`, `news-provider` and the store, with the `items` rebuild (§3.1, §8 row M1); the conformance declaration and per-(day, host) accounting (§11.2); `Provider::story_of` on the trait, read by `edgar-press`'s `follow_ups_to_issue` (§8 row R20); `UnusableReason::NoArticleBody` ("the article container is absent or yields zero paragraph text"); the `open-web-feed` provider with its hosts and rules loaders, GNW index and document parsers, `item_key`, `NotTheRunDay` and committed fixtures (§11.1), the producer of a non-`None` `Request.hosts_version`, `Published::AtMinute`, `NoArticleBody` and `Cadence::EveryUtcDay`; the R4 no-sink scan (§8). The provider table in `cmd_collect`, per-provider `--calendar`/`--listing`/`--hosts`, `--window today` and the envelope field (§11.2); `the_run_envelope_names_the_hosts_file_it_was_allowed_to_use` drives `cmd_collect::run` over the synthetic index and page (1 item committed).
   - End to end, `services/newsvault/tests/open_web_collect.rs::an_open_web_run_collects_a_globenewswire_day_end_to_end`: `cmd_collect::run` over `rssfeed_3_items.xml`, a stub `Exchange` and a fixed clock at 2026-11-14T19:00:30Z, a Saturday, so the weekend day is planned.

| what the run asserts | value |
|---|---|
| attempts, all `live_ingest` | 4: 1 index + 3 documents |
| index `vendor_rows` / `filtered_out` | 3 / 0 |
| documents: committed / unusable (`synthetic_no_article_container.html`, `NoArticleBody`) | 2 / 1 |
| committed row | `valid_minute` set, `valid_at` NULL, `valid_raw` the fixture's `pubDate` text, `valid_field` `pubDate`, `valid_source` `vendor_published`, `observed_at` the stub's `received_at` (19:00:29Z; `requested_at` 19:00:28Z, the clock 19:00:30Z), `story_id` from `item_key`, `doc_id` from the page's canonical link |
| `attempts.hosts_version`, envelope `hosts_version` | the hosts file's digest; envelope `calendar_version` null |
| `items --at` 19:00:45Z / 19:01:00Z | 1 / 2 rows: the 19:00 release is invisible until its minute has closed |

   - Only release 3403075 is a real page; the other two links are answered with the synthetic pages (§11.2 Fixtures).
   - The clock sits inside the 19:00 minute, not after every `pubDate`: at a later clock every observation postdates every minute's end, and `items --at` could not show the minute rule.
   - R10 driver half: `an_open_web_run_for_yesterday_sends_nothing` (§8 row R10).
   - Every mutation named for step 2 in this document was re-run on the final tree, 2026-11-15, one at a time, with `cargo test --locked -p <crate> --no-fail-fast -- <test>`: 49, the table below (5 named rows and 44 others), each turning its named test red. That run counted 50; no 50th mutation is named anywhere, so it is not counted (review 2026-11-15). 2 needed a second, corrected mutation (`cmd_items`'s first version called a private `pit` method and did not build; the segment reader's anchor matched 3 functions).

| mutation | named test red |
|---|---|
| `rows_of` writes an empty `valid_raw` | `an_open_web_run_collects_a_globenewswire_day_end_to_end`, `an_items_row_keeps_the_raw_publication_value_its_field_and_its_source` |
| `document_of` labels the minute `Published::At` | `an_open_web_run_collects_a_globenewswire_day_end_to_end` |
| `plan`'s run-day check removed | `an_open_web_run_for_yesterday_sends_nothing`; `a_target_other_than_the_run_day_is_refused_before_any_request` |
| `cmd_items` minute visible from its start | `a_minute_dated_item_is_invisible_until_its_minute_has_closed`, `an_open_web_run_collects_a_globenewswire_day_end_to_end` |
| envelope `hosts_version` null | `the_run_envelope_names_the_hosts_file_it_was_allowed_to_use`, `an_open_web_run_collects_a_globenewswire_day_end_to_end` |
| the other 44, as named in §3.1, §3.2, §8 rows R3, R4, R7, R13, R15, R17, R19, R21, M1 and §11.1, §11.2 | each its own named test |
| review round 2026-11-15: 18 mutations, each run once its new test was written, as named beside those tests in §2, §3.1, §5.1, §8 rows R3, R13, R17, R19, M1 and §11.1, §11.2 | each its own named test |
| second review round 2026-11-15, migration: 4 mutations, each run once, as named in §3.1 and §8 row M1 | 3 red, each its own named test; 1 green, enforced since the third round (§8 row M1) |
| second review round 2026-11-15, minute close: 3 mutations, each run once, as named in §3.1's `minute` bullets | 3 red, each its own named test |
| second review round 2026-11-15, open-web identity: 4 mutations, each run once, as named in §8 row R3 and §11.1 | 4 red, each its own named test |
| second review round 2026-11-15, over-limit body: 5 mutations, each run once, as named in §8 row R21 (`BodyExceedsLimit` mapped to `Conversation`; every other read error mapped to `BodyOverLimit`; `of_kind` dropping its kind; the over-limit arm removed from each `classify`) | 5 red, each its own named test |
| second review round 2026-11-15, collect usage errors: 9 mutations, 10 red results (the `today` mutation reds 2 named tests), each run once, as named in §5.1's `invalid_argument` row and §11.2 (the `today` refusal removed, checked against 2 tests; the prefix always `error`; each of the six `is_usage` arms false; `OpenWebFeed::plan`'s cadence check removed) | 10 red, each its own named test |
| second review round 2026-11-15, open-web parser: 31 mutations, each run once, as named in §10's per-host pace key, §8 row R3 and §11.1's refusal table, `pubDate`, index, body and depth bullets | 31 red, each its own named test |
| third review round 2026-11-15, store: 7 mutations, each run once, as named in §3.1 and §8 row M1 | 7 red, each its own named test |
| third review round 2026-11-15, open-web parser: 5 mutations, each run once, as named in §11.1's refusal table, `pubDate`, index and depth bullets | 6 red results (the round-trip mutation reds 2 named tests) |
| third review round 2026-11-15, collect usage errors: 3 mutations, each run once, as named in §5.1's `invalid_argument` row | 3 red, each its own named test |

   - Holes step 2 leaves open:
     - GNW's admission rested on the 2026-11-14 robots fetch made outside the ledger; since step 3 a collect requires a ledgered one (§8 row R4).
     - ureq 3.5.2 still follows redirects, up to 10, unpaced (§8 row R19 hole 9) until step 5.
     - no run against the live host yet.
3. Robots refresh command; `newsvault` checks the pinned digest and the floor.
   - Built 2026-11-15: the refresh command, the `robots` table at user_version 2, the hosts-file pin (§11.1, §11.2), the envelope `kind` (§10).
   - Built 2026-11-15: the collect-side digest check, `refuse_an_unverified_robots` in `cmd_collect.rs`, in `run` after the window pre-flight and in the dry-run `planned` after the plan, so usage errors still come first (§8 row R4, robots clause; codes §5.1). Every open-web collect test seeds its store through one helper, `services/newsvault/tests/seed/mod.rs`, which runs `cmd_robots::run` over a stub answering the committed GNW body.
   - Mutations 2026-11-15, each run once against its named test in `services/newsvault/tests/open_web_collect.rs`, 7 of 7 red: the check returning before the lookup; the reader ordered oldest first; an `absent` pin skipped without the lookup; a missing blob skipped; the dry-run call removed; the reader taking a failed fetch as an answer; the binary writing the refusal as plain text.
   - Built 2026-11-15: the robots parser, the path and floor checks, the Crawl-delay hand-off (§11.1 robots and Pace, §11.2, §8 row R19).
   - Mutations 2026-11-15 on the final tree, one at a time, each file copied aside and restored, `cargo test --locked -p <crate> --no-fail-fast -- <test>`: 19 of 19 red, each its named test only.

| mutation | named test red |
|---|---|
| the first group read instead of `*` | `the_star_group_is_read_and_a_named_bot_group_is_not` |
| `Disallow` wins a tie; the tie-break dropped (the later rule wins) | `allow_wins_a_tie_and_the_longest_match_decides` (2) |
| a wildcard's last piece matched only where the rest starts; a final `$` ignored | `wildcard_and_end_anchor_match_as_rfc_9309_states` (2) |
| a Crawl-delay fraction not checked | `a_crawl_delay_that_is_not_a_number_is_refused` |
| a body naming no `user-agent` accepted | `a_robots_200_with_no_user_agent_line_is_refused` |
| the path check removed | `an_admitted_row_whose_index_or_path_prefix_robots_disallows_is_refused` |
| the floor check removed; `max(cd, 10 s)` replaced by `cd`; `>=` replaced by `>`; the hand-off in `open_provider` removed | `a_hosts_row_faster_than_its_robots_crawl_delay_is_refused` (4) |
| an unreadable body read as no robots | `a_pinned_robots_body_that_does_not_parse_is_refused` |
| a blob missing at open read as no robots | `a_robots_blob_written_after_the_provider_opened_is_unverified` |
| the path arm dropped from `main.rs`'s `robots_refusal` | `a_path_or_unreadable_robots_refusal_names_its_fact_and_suggests_no_refresh` |
| the delays ignored; `min` for `max`; a candidate's delay counted | `a_crawl_delay_above_ten_seconds_slows_every_host_and_one_below_speeds_none_up` (3) |
| the index url's query dropped | `the_admitted_paths_are_each_admitted_index_with_its_query_and_its_rule_prefix` |

4. Held filter: `Finalised` carries the `Verdict`, `attempts.story_id`, `attempts.held`, live covered set, ledger check. Amends R16 and R20.
   - Built 2026-11-15: the `Verdict` on `Finalised` and `attempts.verdict`; `attempts.story_id` and the held read; the live arm's held set and the open-web filter; `attempts.held` and the `ledger --check` subtraction (§11.1, §8 rows R4, R16, R20, R21).
   - Every mutation named for step 4 in this document was re-run on the final tree, 2026-11-15, one at a time, each file copied aside and restored, `cargo test --locked -p <crate> --no-fail-fast -- <test>`: 26 mutations, 30 red results of 30, each its named test (some mutations checked against 2 or 3 tests).
   - Review round, 2026-11-15, same procedure, not in the table below: 8 mutations, each red on the test named beside it in §3.2 `held`, §8 rows R4 and M1, and §11.1.

| mutation | named test red |
|---|---|
| `apply` writes NULL `verdict` | `every_verdict_reaches_the_verdict_column_by_its_own_name_and_an_abandoned_attempt_none` |
| `transient_failure` removed from the `verdict` CHECK | `every_verdict_name_is_one_the_verdict_check_admits` |
| `decide` stores `Usable` | `a_finalised_outcome_carries_its_verdict_and_an_abandoned_one_none` |
| `commit_opened` binds NULL `story_id` | `the_opened_row_carries_the_story_the_driver_named_and_an_index_row_none` |
| the driver passes no story; the driver names a story for the index too | `a_document_attempt_opens_under_the_story_its_provider_names` (2) |
| `record_held` a no-op | `an_all_held_index_attempt_records_its_held_count`, `a_quiet_day_whose_every_listed_story_is_held_passes_ledger_check` |
| `held` written on the `vendor_archive` arm too | `a_vendor_archive_rerun_fetches_only_the_uncovered_accession` |
| the `held` subtraction removed; the comparison `>= 0` | `a_quiet_day_whose_every_listed_story_is_held_passes_ledger_check` (2) |
| any non-zero `held` suppresses the finding | `check_still_flags_zero_rows_when_held_covers_only_part_of_the_vendor_rows` |
| the live arm handed an empty set | `a_second_live_poll_of_one_day_fetches_no_held_document` |
| the held read keeps its items arm only | `a_permanently_failed_document_is_not_fetched_again`, `a_document_that_yielded_only_unusable_rows_is_not_fetched_again`, `held_story_ids_names_each_state_the_held_table_holds_and_no_other` |
| the open-web filter keeps every follow-up | `a_live_follow_up_whose_story_is_held_is_dropped_and_an_unheld_one_kept` |
| both arms of the held read restricted to the newest `target_window` | `a_story_held_under_yesterdays_window_is_not_fetched_today` |
| `transient_failure` admitted to the held read | `a_transiently_failed_document_is_fetched_again_by_the_next_poll`, `held_story_ids_names_each_state_the_held_table_holds_and_no_other` |
| the held read's items arm dropped | `held_story_ids_names_each_state_the_held_table_holds_and_no_other` |
| each arm alone restricted to the newest window; each arm's `source` predicate dropped | `held_story_ids_spans_every_target_window_and_one_source_only` (4) |
| `MIGRATIONS[2]` emptied; its `story_id` line removed; its `held` line removed | `a_store_at_user_version_two_opens_migrates_to_three_and_takes_a_verdict` (3) |
| the shape check skipped at 2 | `a_v2_store_whose_columns_differ_is_refused_and_left_at_user_version_two` |
| `UPDATE attempts SET outcome = NULL` added to `MIGRATIONS[2]` | `a_v2_store_keeps_every_v2_column_of_every_row` |

   - Not re-run: the M1 hand migration of a test-made v2 store copy (§8 row M1), and `closed_at IS NOT NULL` in the held read, which no test guards (§11.1).
5. Redirect refusal and 3xx classification, with a loopback stub.
6. One PR per further host.
7. VPS timer, follow-up cap with the run deadline, busy seconds in the envelope.
8. Outbox, WAL, `bundled`, and the §6 poller as `newsvault events --after <cursor>`.

### 11.6 Evidence, 2026-11-14 (Saturday)

- Method: `curl -sS --compressed -A 'research-feed-probe/0.1' -o <body> -D <hdr> --max-time 30 <url>`; body = whitespace-collapsed `<p>` text in the host's container, or JSON-LD `articleBody`, parsed offline with Python's `html.parser`; 13 article fetches: 11 reached, 2 blocked.
- Spacing ≥ 5 s between requests to one host, ≤ 3 per host per probe run: below §11.1's 10 s floor, and Crawl-delay was not read before every fetch.

| host | request start (UTC) | status | body chars | rule |
|---|---|---|---|---|
| prnewswire.com (2 fetches) | 19:36:58; 19:37:18 | 200 | 9,463; 19,337 | `<p>` in `<article>` |
| rss.globenewswire.com | 19:36:54 | 200 | 5,117 | `<p>` in `div.main-body-container` |
| cnbc.com (2 fetches) | 19:37:19; 19:37:49 | 200 | 6,891; 4,910 | `<p>` in `div.ArticleBody-articleBody` |
| fool.com (article; transcript) | 19:37:51; 19:37:59 | 200 | 4,467; 28,038 | `<p>` in `<main>` |
| businessinsider.com (2 fetches) | 19:37:51; 19:37:59 | 200 | 2,281; 3,719 | JSON-LD `articleBody` |
| finance.yahoo.com (GNW release; Fool copy) | 19:37:52; 19:38:00 | 200 | 5,395; 5,174 | `<p>` in `<article>` |
| benzinga.com; marketbeat.com | 19:36:59; 19:37:49 | 403 | 0 | Cloudflare block page; Cloudflare JS challenge |

- CNBC sitemap vs page, 2 of 2 checked: Berkshire story sitemap `09:02:39-04:00` (13:02:39Z), page `article:published_time` 14:02:39Z, a wrong offset (EDT in EST season); FAA story sitemap `13:32:47-04:00` (17:32:47Z), page `datePublished` 09:26:23Z, `dateModified` 18:32:47Z, so the sitemap holds the modification time, with the wrong offset.
- Yahoo sitemap, 19:20:57Z: 1,000 rows over 18.27 h; provider keys from `image:loc` (`media.zenfs.com/en/<key>/`): `cnwgroup.com` 37, `prnewswire.com` 36, `globenewswire.com` 26, `business-wire.com` 6, `accesswire.ca` 6.
- GNW RSS, 19:22:19Z: 20 items over 3.93 h.

