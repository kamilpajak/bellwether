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
| `extract_version` | since 2026-11-16, sha256 in the canonical form below over the rules file's sha256 (`extract_rules_version`), then each file of `providers/open-web-feed/src/` by name and sha256, ordered by name over raw UTF-8 bytes (`canon::digest::extract_version`, `OpenWebFeed::extract_version`); the files are compiled in by `include_str!` (`version::SOURCES`). Carried by `Request.extract_version` through `AttemptEvent::Opened` into `attempts.extract_version` and the collect envelope's `extract_version`; NULL for `edgar-press` and for robots attempts. Tests and mutations: §8 row R17 | P5 (owner decision 2026-11-16, §11.1): `extract_rules_version` alone did not move when `parse.rs` changed a body, and the body is an `item_id` input. Rejected: inside `filter_params`, which names what the vendor is asked: `requests_that_differ_only_in_extract_version_have_one_filter_params_digest` |
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
- **The eight `canon::Domain` digests.** The variant's name is the form's first field
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
  | `ExtractVersion` | `extract_version` (above), since 2026-11-16 | `canon::digest::extract_version` |

  - Seven have a production caller; `SnapshotId` has none (§8 row R7).
    - `extract_version`: `OpenWebFeed::open`, since 2026-11-16.
    - `item_id`, `story_id`, `doc_id`: `Committer::rows_of`, called by `impl Archive for Committer`
      (`services/newsvault/src/cmd_collect.rs`); `story_id` also `edgar-press`'s `Provider::story_of`.
    - `attempt_id`, `filter_params`: `news-provider`'s `AttemptSlot::open`.
    - `result_digest`: `newsvault ledger`, over the `item_id`s each closed attempt committed
      (§3.2; `drift` in `services/newsvault/src/cmd_ledger.rs`).
  - Each of the eight has a committed vector under `canon/vectors/` (`extract_version.json`
    computed 2026-11-16 by Python's `hashlib` and `unicodedata`), which is what makes two
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
- Stored today: `items` in `services/newsvault/src/store.rs` at `PRAGMA user_version` 7: the
  frozen v0 DDL `SCHEMA_V0` plus `MIGRATIONS[0]`, which rebuilds `items` (SQLite's table-rebuild
  procedure: foreign keys off around one transaction, `PRAGMA foreign_key_check` before commit),
  plus `MIGRATIONS[1]`, which creates `robots` (§3.2), plus `MIGRATIONS[2]`, which adds
  `attempts.verdict`, `attempts.story_id` and `attempts.held`, plus `MIGRATIONS[3]`, which adds
  `attempts.listing_unusable`, plus `MIGRATIONS[4]`, which adds `attempts.extract_version` (§3.2,
  2026-11-16), plus `MIGRATIONS[5]`, which creates `payloads` (§3.9, 2026-11-16), plus
  `MIGRATIONS[6]`, which creates `relisted` (§8 row R15, 2026-11-16), plus `MIGRATIONS[7]`,
  which adds `attempts.relisted_compared` (§3.2, decision x, 2026-11-16); exported as
  `publish::SegmentRow` with `source`
  joined from `attempts`; `requested_at` and `received_at` live on `attempts`, reached through
  `attempt_id`.
- Schema versioning (§11.1), since 2026-11-15:
  - a store is brought to user_version 8 in one transaction by `Committer::open`: `SCHEMA_V0`
    (`CREATE TABLE IF NOT EXISTS`) creates each v0 table the store lacks, then each
    `MIGRATIONS[i]` from the version it holds.
  - a store at user_version `n` below 8 is compared first with the columns of `SCHEMA_V0` replayed
    with `MIGRATIONS[..n]` in memory (`PRAGMA table_info`: name, type, NOT NULL, default, primary
    key); a difference is refused with `StoreError::ShapeDiffers { found, columns }` naming each
    `table.column`, exit 2, store left at user_version `n`, no repair step: §8 row M1.
  - at user_version 0 a table the store lacks is skipped, because `SCHEMA_V0` creates it whole; at
    1, 2, 3, 4, 5, 6 and 7 every column of a missing table is named.
  - a migration step SQLite refuses is `StoreError::MigrationFailed { from, step, cause }`, exit
    2, store left at user_version `from`; `cause` is SQLite's message without the SQL batch
    (review 2026-11-15 measured 2,040 bytes of stderr, most of it the batch, before):
    `a_failed_migration_step_is_named_without_its_sql_and_leaves_user_version_zero` (mutations
    2026-11-15, the step's error left unwrapped / the SQL kept in `cause`: each red).
  - a failure inside `SCHEMA_V0` is `step: MigrationStep::CreatingTheV0Tables`, written "creating
    the v0 tables the store lacks"; review 2026-11-15 read "the step to user_version 0" for it:
    `a_failure_creating_the_v0_tables_is_named_as_that_step` (mutations 2026-11-15, labelled
    `ToUserVersion(0)` / left unwrapped: each red).
  - a user_version above 7 is refused (`StoreError::SchemaVersion`):
    `a_store_at_an_unknown_higher_user_version_is_refused` (mutation 2026-11-15, refusal removed:
    red).
  - `collect` refuses that store before its policy copy, run envelope and lease, so the store
    is left byte-identical (`store::refuse_an_unreadable_version`):
    `a_run_against_a_store_at_a_newer_user_version_leaves_the_store_unchanged` (mutation
    2026-11-15, the check removed: red; review 2026-11-15 measured a `runs/` file and a `LOCK`
    left behind before it).
  - the same check refuses a store below 7 whose columns differ from its version's shape,
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
  - `items` and `ledger` open read-only, cannot migrate, and refuse every user_version but 7 with
    `StoreError::SchemaVersion`, exit 2: `a_reader_refuses_a_store_at_another_user_version`
    (mutation 2026-11-15, the check removed from either reader: red).
  - so a store below 7 is unreadable by `items` and `ledger` until a writer (`collect`, `publish`)
    opens it.
  - `store::RobotsLedger::open` (§8 row R4, robots clause) accepts any user_version up to 7, so
    `collect` reads a v2 store's `robots` table before the same run migrates it.
- No column today: `source_item_id`, `provenance` (§8 row R10), `data_through` (§8 row R13),
  `title`, `url`, `lang`,
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
| `body_ref` | digest, plus the blob's length | **R17.** Article text, never page HTML, held as a blob beside the row (§3.9). No `expired` state: nothing expires L1 or L2 (§3.9), and `news_provider::BodyState` has no `Expired` arm (`news-provider/src/provider.rs:109-111`) |
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
  - producer: `open-web-feed`'s document extraction, from GNW's and, since 2026-11-16, PRN's
    `pubDate` under the rules file's exact minute formats
    (`a_globenewswire_pubdate_is_a_minute_and_never_an_instant`,
    `a_pr_newswire_pubdate_is_a_minute_and_never_an_instant`), driven by
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
- Stored as `attempts` in `services/newsvault/src/store.rs` (user_version 8, §3.1), written from
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
| `listing_unusable` | unusable rows of a usable answer whose reason the index row decided (`news_provider::DecidedBy::IndexRow` on `UnusableRow`, set by the provider; counted by `attempt_once` into `Counts`, carried by `Finalised` inside `Outcome`, so it lands in the `UPDATE` that writes `verdict`, inside the commit transaction); 0 for any other answer. A usable `Document` with any is not held (§11.1). Names no reason. NULL for an abandoned attempt and for every row closed before user_version 4, not backfilled: `every_listing_unusable_count_reaches_its_column_and_an_abandoned_attempt_none` (mutation 2026-11-15, `apply` binding NULL: red). Read by `Committer::held_story_ids`, and by `ledger`'s source-row `listing_unusable`, summed over rows with non-NULL `vendor_rows` (§8 row R4); the collect envelope does not carry it (2026-11-15) |
| `relisted_compared` | since 2026-11-16 (decision x), held stories a `live_ingest` index attempt compared with the minute of their kept item, equal or not (§8 row R15); written by `Committer::record_relisted` in the transaction of its `relisted` rows, 0 included. NULL wherever `held` is NULL and on every row before user_version 8, not backfilled. A kept item with no `valid_minute` is not counted. Read by `ledger`'s source-row `relisted_compared`, the sum over every attempt of the source and day (§8 row R4). Mutations 2026-11-16, each red: the `UPDATE` skipped when 0 (`an_index_with_nothing_held_records_zero_compared`: NULL before, 0 after an empty call); the `UPDATE` run after the commit (`a_refused_comparison_count_keeps_no_relisted_row`: a temp trigger refuses the count, no `relisted` row kept); a refused index commit recording an empty comparison (`open_web_collect.rs::a_live_index_whose_commit_was_refused_records_no_relisted`: NULL); a kept row counted whatever its minute (`a_listing_of_a_story_whose_kept_item_has_no_minute_is_not_recorded`: 0); the ledger's `SUM` made `MAX`, and its per-source `+=` made a maximum (`ledger.rs::a_relisted_comparison_count_is_summed_for_the_source_and_day_of_its_index_attempt`: 3 + 5 in one group, 4 in another, 12) |
| `extract_version` | since 2026-11-16, the extraction version the answer is read under (§2), from `Request.extract_version` via `Opened`, written before the socket; outside `filter_params`. Every `open-web-feed` index and document attempt carries it; NULL for `edgar-press`, for robots attempts and for every row opened before user_version 5, not backfilled. NULL on robots rows: `a_robots_refresh_writes_one_attempt_per_hosts_row_under_source_robots` (mutation 2026-11-16, `request_for` given a version: red). NULL on edgar rows and on the edgar envelope: `two_identical_collects_leave_the_right_events_one_blob_set_and_items_new_one_then_zero` (mutations 2026-11-16, both edgar `Request` literals given a version: red; the envelope given a value when `Opened` has none: red; before, the robots and edgar mutations together left all 597 tests green). Written on all 19 attempt rows of `an_open_web_run_collects_a_gnw_a_prn_a_cnw_a_fed_and_a_yahoo_day_end_to_end`, equal to the envelope's `extract_version` (mutation 2026-11-16, `commit_opened` binding NULL: red, with `a_store_at_user_version_four_opens_migrates_to_five_and_takes_an_extract_version`). No reader on a real path: `ledger` does not read it (2026-11-16) |
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
| `blob` | the body as received, foreign key to `blobs`, for a `present` row closed before user_version 6; NULL on every row from user_version 6, whose body is named by its `payloads` row alone (§3.9) |

- One home for a robots body (builder default 2026-11-16, §10): the `payloads` row from user_version 6, `robots.blob` before it; readers take `COALESCE(robots.blob, payloads.raw_blob)` for a `present` row (`store::ROBOTS_BODY`). The `answer` column decides `present` or `absent`, because a 404's body is a payload too.
  - `a_robots_body_is_read_from_robots_blob_before_user_version_six_and_from_payloads_after`: a hand-written pre-6 row, a row written by `record_robots` (its `robots.blob` NULL) and a `present` row naming neither, `StoreError::RobotsBodyUnlinked` (mutations 2026-11-16: `r.blob` alone: red; `p.raw_blob` alone: red; `record_robots` writing `robots.blob` again: red).
  - a v5 store, read by `collect`'s check before its lease migrates it, has no `payloads` and is read through `robots.blob` alone: `a_v5_store_s_robots_row_is_read_without_the_payloads_table` (mutation 2026-11-16, `payloads` assumed present: red).
- One door, `Committer::record_robots(AttemptReceipt, host, RobotsAnswer, &Payload, at)`: `record`'s checks
  (opened here, closed here, an abandoned attempt carries nothing, a replay must be the same row,
  else `StoreError::RobotsReplayDiffers` naming both rows, and the same payload row, else `StoreError::ReplayWithDifferentPayload`, a comparison no robots test reaches, unguarded), then the `Answered` and `Finalised` events, the blob's index row, the `payloads` row and the `robots` row in one
  transaction: `a_robots_row_and_its_attempt_events_commit_in_one_transaction` (mutation
  2026-11-15, the insert moved after `COMMIT`: red);
  `a_robots_replay_naming_another_host_or_answer_is_refused` (mutation 2026-11-15, the replay
  comparison skipped: red). A `Present` digest other than the payload's is `StoreError::RobotsAnswerIsNotItsPayload`, before any write: `a_robots_answer_naming_another_blob_than_its_payload_is_refused` (mutation 2026-11-16, the check skipped: red).
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

### 3.9 `RawBlob` — the payload layers, none of which a retention number expires (decided 2026-11-12; L2 amended 2026-11-16)

| layer | addressed by | may a number expire it |
|---|---|---|
| L0 item row (§3.1 minus the text) | `item_id` | **never** — it carries R1's completeness claim |
| L1 extracted body | `body_ref` | **not as a setting.** Matching runs over body text (R17), so an expired body cannot be re-matched under a new `rule_version`; a number here reopens memo D-2 instead of configuring it. |
| L2 payload as received | `raw_blob_ref` | **never** (owner decision 2026-11-16). Rejected: an expiry after a count of days, because a re-fetch does not recover the payload: memo §6b says an index timestamp is not an immutable archive, and a PRN, CNW or Fed article page returns other bytes on every fetch, a GNW page across about 70 min (below). |

**How L2 is kept: owner decisions of 2026-11-16; P9, P10 and the over-limit rule built 2026-11-16, P11 and compression not.**

| decision | rule | state |
|---|---|---|
| P9, headers | response headers kept beside the blob, outside its digest; allowlist `Content-Type`, `Last-Modified`, `ETag`, `Date`, `Cache-Control`, `Vary`, plus the original `Content-Encoding`, plus `Retry-After` since 2026-11-16 (§8 row R19, server slow-down) | built: `news_provider::Headers` on `RawResponse` and on a `TransportError` that read a head (`wire::KEPT_HEADERS`); stored as `payloads.headers` (table below); `only_the_allowlisted_headers_are_kept_lowercase_with_every_repeat` (mutation 2026-11-16, every header kept: red); `Location` stays out (§11.2); `Retry-After` reaches the payload when a payload row exists, that is when a body, whole or cut at the limit, was read; a slow-down answer whose body read failed changes the pace and stores no `Retry-After`: `a_429_s_retry_after_is_kept_in_the_payload_headers` (mutation 2026-11-16, `retry-after` replaced in `KEPT_HEADERS`: red, with 4 other `wire` tests) |
| P9, the original `Content-Encoding` | as above | built: ureq 3.5.2 deletes `Content-Encoding` and `Content-Length` when its `gzip` feature decodes (`run.rs`, `is_decompressing`), so that feature is off and `Wire::exchange` decodes `gzip` itself (§10, P9 defaults); `a_gzip_body_is_decoded_to_the_digest_ureq_gave_it_and_keeps_its_content_encoding` (below); mutation 2026-11-16, `content-encoding` off the allowlist: red |
| P10, link | a per-attempt link table: attempt → url, phase, `listed_date`, raw digest, headers, truncated flag; every blob `put_blob` writes gets a `blobs` row | built: `payloads` (table below), written by `Committer::record` and `Committer::record_robots` in the transaction that closes the attempt; every provider through `news_provider::collect`'s `Archive::commit`, edgar included, and `newsvault robots refresh` |
| P11, re-extraction | a separate derived layer that copies `observed_at`, `story_id` and `seq` from the stored item and never recomputes them | no such layer and no re-extraction command |
| over-limit body | stored truncated at the limit, with the truncated flag set | built: a body over `body_limit` is still `TransportKind::BodyOverLimit`, so the verdict stays `permanent_failure` and nothing is extracted; the `TransportError` carries the first `body_limit` bytes (`cut_body`), which `Payload::stored` hands to `put_blob` with `truncated` 1 |
| compression | deferred | `BlobStore::put` writes the body as `Wire` decoded it (gzip only), uncompressed (`services/newsvault/src/blobs.rs:56-72`) |

**`payloads`: one row per attempt that got an HTTP answer, an `Ok` response or a body cut at `body_limit` (user_version 6, `MIGRATIONS[5]`).**

| column | notes |
|---|---|
| `attempt_id` | primary key and foreign key to `attempts` |
| `url`, `phase`, `listed_date` | the `Request`'s, as asked; `phase` `index` \| `document`, `CHECK`ed; `listed_date` NULL on an index and a robots attempt<br>`url` is the url the body came from: `Wire` follows no redirect for any provider (owner decision v, 2026-11-16; §8 row R19); `an_edgar_document_answered_with_a_redirect_is_a_permanent_failure_and_its_hop_never_sent` asserts it for edgar, the open-web redirect tests below for open-web |
| `raw_blob` | the body as received, after gzip decoding, cut at the limit when `truncated`; foreign key to `blobs` |
| `headers` | `Headers::to_json`: a JSON array of `[name, value]` arrays, no whitespace; names lowercase, in `http` 1.x `HeaderMap` order (names by first appearance, each name's values as received, repeats kept); a value's bytes read as ISO-8859-1, so every byte round-trips |
| `truncated` | 1 when the body was over `body_limit`, `CHECK`ed 0 or 1 |

- No row for an attempt with no answer (a transport error with no body) and none for an attempt closed before user_version 6, not backfilled.
- Tests, each mutation run 2026-11-16 once:
  - `news_provider::wire::tests::a_gzip_body_is_decoded_to_the_digest_ureq_gave_it_and_keeps_its_content_encoding`: a loopback server sends a 104-byte gzip stream with `Content-Encoding: gzip`; the body's sha256 (`a2c552e1…f096`, 97 bytes) and the request head equal what a temporary test measured through the same server at `e2ed6d4`, ureq's `gzip` feature on. Mutations: the explicit `accept_encoding` dropped: red (the head loses `accept-encoding: gzip`); decoding skipped: red.
  - `…::a_coding_ureq_did_not_decode_is_stored_as_received` (`GZIP` in capitals, which ureq's exact match does not decode); `…::a_gzip_body_over_the_limit_once_decoded_is_cut_at_the_limit_decoded` (the limit counts decoded bytes, as ureq's `BodyReader::new` orders it).
  - `…::a_body_over_the_limit_keeps_its_first_limit_bytes_and_its_headers`. Mutations: the cut body dropped: red, with the two driven tests below; the over-limit probe never firing: red, with both.
  - `services/newsvault/tests/open_web_collect.rs::every_body_an_open_web_run_stores_is_indexed_and_linked_to_its_attempt`: the GNW day with release 1 answered 403, after a robots seed; the blob files on disk equal the `blobs` rows, and the index, the unusable page and the 403 page each have their row (mutations: `link` a no-op: red; the payload left out of the indexed blobs: red; `collect` storing only usable bodies: red). `tests/collect.rs::every_body_an_edgar_run_stores_is_indexed_and_linked_to_its_attempt`: the same over an edgar good day.
  - `open_web_collect.rs::a_redirect_whose_body_is_over_the_limit_is_fetched_again_by_the_next_poll` and `robots_refresh.rs::a_robots_redirect_whose_body_is_over_the_limit_keeps_its_status`: through the real open-web `Wire`, a 301 body of `BODY_LIMIT` + 1 bytes is stored as 2,097,152 bytes, `truncated` 1, verdict `permanent_failure` (mutation: `truncated` never set: both red).
  - `store::tests::a_payload_row_and_its_attempt_events_commit_in_one_transaction` (mutation: `link` moved after `COMMIT`: red); `…::a_replay_naming_another_payload_is_refused_and_the_same_one_is_standing` (mutation: the comparison skipped: red).
  - `…::a_payload_row_breaking_any_constraint_of_its_table_is_refused`: four rows by hand, each breaking one of the two `CHECK`s or two foreign keys, beside a control row that is taken (mutations: each of the four clauses removed alone: red). `…::an_abandoned_attempt_may_not_carry_a_payload`: refused as `ItemsUnderAbandonedAttempt`, no row written (mutation: the payload half of `admits`' `carries` dropped: red). No real path closes an abandoned attempt with a payload.
  - Both driven over-limit tests also assert the cut body's link row keeps the loopback 301's `Content-Type` (mutation: a cut body's headers emptied in `Payload::stored`: both red, green before the assertion). `wire::tests::a_gzip_body_of_two_members_is_decoded_to_both` (mutation: `MultiGzDecoder` swapped for `GzDecoder`, ureq 3.5.2's being `MultiGzDecoder`: red; green before the test, review 2026-11-16).
- Reader on a real path: `newsvault ledger`'s source row `bodies_truncated`, a `SUM` of `payloads.truncated` per source and day (§8 row R4). No reader of `url`, `phase`, `listed_date` or `headers` outside tests (2026-11-16).
  - `bodies_truncated` reads 0 for an attempt closed before user_version 6, which stored no over-limit body, so "not recorded" and "none cut" read the same; kept and not backfilled (owner decision u, 2026-11-16).
- Cost, accepted by the owner: the edgar good-day fixture's `blobs_indexed` is 5, was 4 (the index payload); the ledger golden file gains `bodies_truncated` on every source row.
- Unguarded: a commit the store refuses (`commit_failed`) leaves the raw blob and the bodies `rows_of` already wrote as files no row names; so does a crash between `put_blob` and the commit. Nothing counts them (read from the code 2026-11-16).
- Unguarded: a body stored before user_version 6 that no item row names (an index payload, an error page, an `Empty` or unusable page) stays a file no row names after the migration: `MIGRATIONS[5]` only creates `payloads`, and a backfill would have to invent the url and headers never recorded (read from the code 2026-11-16).
- The collect envelope's `blobs_written` and `bytes_written` count a body cut at the limit since 2026-11-16; before, an `Err` answer counted 0 and no blob (`open_web_collect.rs::a_body_cut_at_the_limit_counts_in_the_envelope_s_blobs_and_bytes_written`; mutation 2026-11-16, the cut body left out of both: red).
- Compression measured 2026-11-16, Python 3.14.7 `gzip` level 9 per page, summed, and `compression.zstd` level 19 over all pages of a set concatenated in file-name order:

  | set | pages | bytes as stored | gzip -9 per page | zstd -19 one stream |
  |---|---|---|---|---|
  | PRN and GNW article pages, fetched 2026-11-16 09:42–09:45Z | 34 (19 PRN, 15 GNW) | 5,798,997 | 729,054 (7.95×) | 170,713 (33.97×) |
  | Yahoo article pages, fetched 2026-11-16 10:52–10:53Z (§11.6) | 5 | 4,869,607 | 737,423 (6.60×) | not measured |

**Measured 2026-11-12 at 42322eb, and no gate proposed.**
- Two temporary fixtures in `lake.rs`, run and reverted:
  - a null `body` returns `body == ""` with `unusable_rows == 0`;
  - dropping the column fails the whole day as `LakeError::MissingColumn("body")`.
- The inherited layout has no third state, which is why L1 and L2 are blobs beside the row.
- Nothing deletes a blob: `ls services` returns `newsvault` alone, and no `fs::remove_*` call outside a test deletes an archive blob (three calls on 2026-11-14 at d33cd55 and on 2026-11-15 at 697a6b0; four by `git grep -n "remove_file\|remove_dir" -- '*.rs'` on 2026-11-16 at `c2975cf`):
  - two inside `store.rs`'s `mod tests`, each deleting a blob its own test stored; one checks that an item row naming a file that is not there is refused (`an_item_row_naming_a_blob_that_is_not_on_disk_is_refused_rather_than_indexed`);
  - one in `services/newsvault/tests/open_web_collect.rs`, deleting the robots blob its own test seeded (`a_pinned_robots_blob_missing_on_disk_is_unverified`);
  - one in `probes/vendor_probe/src/main.rs`, deleting the probe's own write-check file.
- No deletion path for a takedown, for now (owner decision i, 2026-11-16).

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

**A re-fetched PRN, CNW or Fed article page is a new blob; its extracted text is not, where measured.**
- Measured 2026-11-16 (probe7): each URL fetched three times: r0 the morning copy (PRN and GNW 09:42Z, CNW, Fed and BI 08:05Z), r1 at 10:51:54–10:52:16Z, r2 about 30 s after r1. Bodies compared as decoded, by sha256 and line diff:

  | host | URLs | body bytes, r1 vs r2 | body bytes, r0 vs r1 | after replacing Cloudflare values | extracted text, r0 = r1 = r2 |
  |---|---|---|---|---|---|
  | PRN | 3 | differ, 3 of 3 | differ, 3 of 3 | identical, 3 of 3 | 3 of 3, `<p>` in `section.release-body` |
  | CNW | 2 | differ, 2 of 2 | differ, 2 of 2 | identical, 2 of 2 | 2 of 2, `<p>` in `section.release-body` |
  | Fed | 2 | differ, 2 of 2 | differ, 2 of 2 | identical, 2 of 2 | 2 of 2 by `OpenWebFeed::extract` under the `fed` rule, 2026-11-17 (§11.1's Fed coverage table) |
  | GNW | 3 | identical, 3 of 3 | differ outside `div.main-body-container` only: the related-releases carousel (`card-explore-carousel`) and `__RequestVerificationToken` | — | 3 of 3, `<p>` in `div.main-body-container` |
  | BI | 1 | identical | identical | — | not measured: no BI rule |

  - Cloudflare values replaced: `data-cfemail` attributes, `/cdn-cgi/l/email-protection#…` links, the `r` and `t` values of `__CF$cv$params`, and `jsd/main.js?ray=`.
  - Extracted text: whitespace-collapsed `<p>` text by Python's `html.parser`, re-run 2026-11-16 for this table; not `OpenWebFeed::extract`.
- So on PRN, CNW and Fed every fetch of a page mints one L2 blob; on GNW two fetches 30 s apart gave one blob and two about 70 min apart gave two. The held filter (§11.1) is what keeps a story to one document fetch.

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
| vendor → `vault` | HTTP, behind **one owner for every vendor conversation**, not one client per vendor | a vendor conversation happens in exactly one place, so quota, retry and cost have one owner. Since 2026-11-13, when the first adapter shipped, the single `ureq` agent sits behind `news-provider`'s `Wire` and `providers/edgar-press` declares no HTTP dependency: only that shape makes "a fetch outside a ledger row does not compile" hold in every crate, not only in crates that own no agent (row R4). Per-vendor differences: the provider's `TransportProfile` (`news-provider/src/wire.rs`), call budget, bucket and redirect policy included (row R19) |
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
| 4 and 5 | no | reachable and not errors |
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
| `robots_refused` | exit 2. Same place, six conditions on an admitted row: (1) the newest answered robots fetch is not its pin (`CollectRunError::RobotsRefused`, `details` `{host, pinned, fetched}`, no suggestion: a refresh records the same change again and the fix is a person editing the hosts file; same test as above, mutation 2026-11-15, the refresh suggested again: red); (2) the pinned body cannot be read: an HTML page, a non-numeric `*` Crawl-delay (`RobotsUnreadable`, `{host, pinned, why}`; §10 robots rows); (3) it disallows the index path with its query or the rule's `path_prefix` (`RobotsDisallowsPath`, `{host, path}`); (4) the provider's `per_host` is `None` or below 10 s (`PaceBelowFloor`, `{host, per_host_seconds, floor_seconds}`); (5) the pinned `*` Crawl-delay is above that `per_host` (`CrawlDelayOverInterval`, `{host, crawl_delay_seconds, interval_seconds}`, owner decision 1); (6) the pinned blob's bytes on disk no longer hash to the pin (`RobotsBlobAltered`, `{host, pinned, found}`; `BlobStore::put` keeps an existing file, so a refresh repairs nothing). (2) to (6) suggest nothing, because a refresh cannot change a pinned body: `a_path_or_unreadable_robots_refusal_names_its_fact_and_suggests_no_refresh` covers (2), (3) and (5) (mutation 2026-11-16, the (5) arm dropped from `robots_refusal`: red), `a_pinned_robots_blob_changed_on_disk_is_refused_and_suggests_no_refresh` (6) (mutations 2026-11-15, the hash comparison in `BlobStore::read_verified` removed / the arm's suggestion switched on: each red); (4) is unreachable through the binary, because open-web's `per_host` is always 10 s, so its `main.rs` arm is untested |

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
| 0 | every row answered (200 or 404) and every admitted pin matches | the `newsvault.robots/3` report |
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
| R1 | archive complete, caps are views | blobs: `BlobStore::put` stages via `File::create_new`; `newsvault::store::tests::a_blob_whose_staging_path_already_exists_is_refused_rather_than_truncated`, `…::a_second_put_of_the_same_payload_keeps_the_standing_file_rather_than_rewriting_it`<br>segments: `Segment::create_new` is the only constructor; `newsvault::publish::tests::changing_the_one_segment_constructor_to_file_create_turns_the_write_gate_red`, `…::a_second_write_to_an_existing_published_path_fails_with_a_named_error`, `…::a_segment_is_renamed_into_place_only_after_its_bytes_and_its_directory_are_fsynced`, read-back `…::a_published_day_round_trips_through_the_strict_column_reader`<br>policy copy: `cmd_collect` stages via `File::create_new`; `services/newsvault/tests/collect.rs::a_staged_policy_copy_a_crash_left_behind_is_refused_rather_than_overwritten`<br>`items` never updated: §8 row R15 | **three of four creating calls enforced (blob, segment, policy copy), segments with fsync order**<br>unguarded: the fourth, `cmd_collect::write_envelope`, uses truncating `File::create` (envelope written twice per run)<br>unguarded: `File::create`, `OpenOptions::truncate`, `ArrowWriter` unsearched in the crate<br>unguarded: `blobs.rs` fsync order: 3 mutations (drop the file fsync; drop the directory fsync; rename before the file fsync) each leave `cargo test --locked -p newsvault --no-fail-fast` at exit 0, run 2026-11-15<br>unguarded: nothing forbids `UPDATE`/`DELETE` on `items`; the attempt row is updated in place by design<br>caps: design, no view builder; nothing deletes a blob, L2 included (§3.9); a takedown deletion path is an open owner question (2026-11-16) |
| R2 | valid and observation time are distinct types | type system; `compile_fail` doc test in `pit/src/lib.rs`, run because of `--no-fail-fast`<br>positive control doc test with the same `use` line; layout asserted by `pit::tests::the_two_instants_still_cannot_be_compared_with_each_other_after_the_move`, `pit::tests::the_positive_control_for_the_compile_fail_block_imports_from_the_new_path`<br>not `compile_fail,E0308`: rustdoc reports `ok` on a different error code | **enforced, narrowly**: the two instants cannot be compared with each other<br>unguarded: `Visibility::valid()`, `observed()`, `Cutoff::inner()` are public, so a publication-only predicate compiles from any crate (demonstrated 2026-11-12)<br>fence annotations asserted exact: `compile_fail` on the block, a bare fence on the control; `compile_fail,ignore` on the block reds `pit::tests::the_two_instants_still_cannot_be_compared_with_each_other_after_the_move`, `ignore` on the control reds `pit::tests::the_positive_control_for_the_compile_fail_block_imports_from_the_new_path`; a decoy pair before them or a dropped closing fence reds `pit::tests::the_header_holds_exactly_the_compile_fail_block_and_its_control_both_closed`; mutation-tested 2026-11-15<br>unguarded: a `compile_fail` block that fails for another reason (a hidden `compile_error!`, a commented-out `use`) stays green<br>every member library keeps `doctest` and `test` on at `src/lib.rs`, and no committed `.cargo/config*` or workflow sets rustdoc flags: `repo_invariants::doctests::tests::every_member_library_in_this_workspace_runs_its_doc_tests` (mutation-tested 2026-11-15: `doctest = false`, behind `crate-type = ["rlib"]`, `test = false`, `path` elsewhere, `rustdocflags` in `.cargo/config.toml`)<br>unguarded: flags passed outside committed files (a shell's `RUSTDOCFLAGS`, `cargo test -- --skip`) |
| R3 | first sighting wins | store: `newsvault::store` `record` keys on `story_id` across the whole store, never updates a row; `newsvault::store::tests::the_same_story_in_three_commits_keeps_the_first_observation_and_writes_items_new_zero`, `…::an_earlier_observation_arriving_later_replaces_the_kept_one_and_is_counted` (mutation-tested 2026-11-13)<br>lake reader: keys on vendor `id` per partition; blank id unusable, `newsvault::lake::tests::a_blank_id_is_unusable_and_does_not_collapse_distinct_stories`<br>reader vs `probes/newsvault_parity.py`: committed output read by `services/newsvault/tests/parity.rs`; its one disagreement (ingest before a midnight-stamped publication, dropped by the probe) is now unusable here: `newsvault::lake::tests::a_row_fetched_before_its_own_day_stamp_is_unusable_and_not_day_dated`<br>provider key, open-web: `OpenWebFeed::item_key` is the one function behind `source_item_id` and `story_of`; a page's canonical link feeds `doc_id` only: `the_follow_up_story_equals_the_item_story_on_a_page_whose_canonical_link_differs` (synthetic fixture; mutation 2026-11-15, the item keyed on the canonical link: red), `www_and_rss_links_of_one_release_share_one_item_key` (mutations 2026-11-15, the rewrite removed from the rules file: red; the host's case kept by `host_and_path`: red, green before its `WWW.GlobeNewswire.com` case), `a_url_with_a_port_or_a_userinfo_has_no_item_key` (mutation 2026-11-15, `host_and_path`'s `HostKey` check deleted: red, green before the test), `a_host_two_rules_rewrite_to_two_hosts_is_refused` (§11.1); PRN, no rewrite, so the key is `www.prnewswire.com` plus the path: `the_real_pr_newswire_article_carries_its_title_its_canonical_link_and_its_www_item_key` (mutation 2026-11-16, a rewrite `www.prnewswire.com` → `prnewswire.com`: red); CNW the same with `www.newswire.ca`: `the_real_canada_newswire_article_carries_its_title_its_canonical_link_and_its_item_key` (mutation 2026-11-17, a rewrite `www.newswire.ca` → `newswire.ca`: red); the Fed the same with `www.federalreserve.gov`: `the_real_federal_reserve_release_carries_its_title_its_own_url_and_its_item_key` (mutation 2026-11-17, a rewrite `www.federalreserve.gov` → `federalreserve.gov`: red); no real Fed page names a canonical link (0 of 9), so the canonical pick is tested on the real release with one added to its head: `a_federal_reserve_page_naming_a_canonical_link_is_stored_under_it` (mutations 2026-11-17, the pick `meta[property="og:url"]` and `link[rel="x"]`: each red); Yahoo, since 2026-11-17: the key is the Yahoo link's own host and path, never the wire release it copies: `the_real_yahoo_reprint_carries_its_title_its_canonical_link_and_its_item_key` (mutation 2026-11-17, a rewrite `finance.yahoo.com` → `yahoo.com`: red)<br>provider key filter, Yahoo (§11.2, since 2026-11-17): a row is followed only when the `image:loc` segment after `https://media.zenfs.com/en/` equals `business-wire.com` or `accesswire.ca`, so a PRN, GNW or CNW reprint is never a second story: `a_provider_key_is_matched_whole_and_never_as_a_substring` (mutations 2026-11-17, the match by `contains` either way and by `eq_ignore_ascii_case`: each red), `the_real_yahoo_sitemap_follows_only_its_business_wire_and_accesswire_rows` (mutations 2026-11-17, the filter removed, a row with no key followed, `prnewswire.com` moved to `follow`: each red); unguarded: a wire release that Yahoo files under another key, or under none, is not followed, and one filed under a followed key is stored though it may also be listed directly; neither was observed (§11.2)<br>index, open-web: the first row keying a story is followed up and a later row of the same index keying it again is counted in `filtered_out`, its `listed_date` dropped, so row R15 never compares it: `a_story_listed_twice_in_one_index_is_followed_up_once_and_the_repeat_counted` (mutation 2026-11-15, the seen-set removed: red) | store **enforced across the whole store, keyed on `story_id`**; lake reader **enforced at the partition grain**, keyed on the vendor id<br>unguarded: nothing reconciles `attempts.items_new` with `COUNT(items)`<br>lake reader derives no `story_id` (row R7); an id in k partitions is kept k times: 1,225 extra appearances of 501 ids (§10 Freshness, N2; §2 `story_id`)<br>open-web key **enforced** for GNW, PRN, CNW, the Fed and Yahoo; unguarded: an edit to a GNW slug makes a second key, so one release becomes two stories (unmeasured)<br>unguarded, PRN: a retitled release changes its slug and so its key, while the 9-digit id at the path's end stays; not observable on 2026-11-16, because the 2026-11-14 and 2026-11-16 feeds share no release id<br>unguarded, PRN: the opposite shape, 4 releases listed twice under two ids with the same title, `description`, `dc:contributor` and slug, 8 of 40 items (feeds of 2026-11-14 and 2026-11-16, construction in §11.1); `story_id` and `doc_id` do not collapse them; whether a pair is one release is unknown<br>unguarded, PRN: a correction notice is listed under a release link (2026-11-14 feed item 2, fixture `rss_3_items.xml` item 1, `/C O R R E C T I O N - WeaveGrid/`, link id 302913655), so it keys that release's story: when the release is already held the correction is never fetched, and its listed minute is a `relisted` row when it differs from the kept item's (row R15, owner decision 10, 2026-11-16); when not, the page at that link is stored under the release's key with the correction's `pubDate`, 2026-11-14T17:48Z, while the item's own `description` dates the release 20-Oct-2026, so the stored minute is about 25 days late, never early; that page was never fetched, so what it holds is unknown<br>unguarded: `item_key` keeps the path byte for byte, so percent-encoding case (`%C3%B1` / `%c3%b1`), a raw `ñ` and a trailing slash each key a second story (read from `host_and_path`, 2026-11-15, not run); latent while only the RSS link feeds `item_key`, normalised with a test before a second GNW surface feeds it |
| R4 | a **coverage ledger** (`Attempt`, §3.2) beside every store, **and no vendor call outside an open attempt row** | reader: `newsvault ledger --check` (`services/newsvault/tests/ledger.rs`), golden `services/newsvault/tests/fixtures/ledger/expected.json`<br>call: `Wire::send` consumes `SendPermit` from `AttemptSlot::take_permit`; `a_send_without_a_permit_does_not_compile` + positive control, fence annotations asserted exact; `repo_invariants::clients`<br>contact (accepted attack A3.7): `a_literal_contact_shaped_header_value_turns_a_named_test_red`<br>clock: `repo_invariants::clocks` over `news-provider/src` and `providers/*/src`, exempting `clock.rs`, `wire.rs`<br>no sink in a provider: `repo_invariants::sinks::tests::no_file_under_a_provider_crate_names_sink`, code lines of every `providers/*/src` file (mutation 2026-11-15, `use news_provider::Sink;` in a new file under `open-web-feed/src`: red)<br>sweep: `an_attempt_a_dead_run_left_open_is_closed_interrupted_by_the_next_run`<br>store: `Committer::record(AttemptReceipt)`, `compile_fail` + control in `store.rs`, annotations asserted exact; `a_receipt_closed_against_another_sink_is_refused_rather_than_filing_items_under_it`; FK under `PRAGMA foreign_keys = ON`<br>robots (§11.2): `collect`, dry run included, refuses an admitted hosts row unless the store's newest answered robots fetch of its host (`store::RobotsLedger::newest_robots`, read-only) is the row's pin and, for a digest, its blob is on disk: `services/newsvault/tests/open_web_collect.rs::an_admitted_row_without_a_ledgered_robots_fetch_is_refused` (no commit log; only a failed fetch; nothing written, nothing sent), `…::an_admitted_row_whose_newest_robots_fetch_differs_from_its_pin_is_refused`, `…::an_admitted_row_pinned_absent_is_admitted_only_by_a_ledgered_404`, `…::a_failed_robots_fetch_after_a_matching_one_leaves_the_host_admitted`, `…::a_candidate_row_needs_no_robots_fetch`, `…::a_pinned_robots_blob_missing_on_disk_is_unverified`, `…::a_robots_blob_written_after_the_provider_opened_is_unverified`, `…::a_dry_run_refuses_an_unverified_host_too`, `…::a_pinned_robots_blob_changed_on_disk_is_refused_and_suggests_no_refresh` (the pinned blob re-hashed when read), `…::a_second_admitted_host_without_a_ledgered_fetch_is_refused` (every admitted row; mutation 2026-11-15, the loop cut to its first row: red), `…::another_hosts_fetch_matching_the_pin_does_not_admit_a_host` (the lookup keyed by host; mutation 2026-11-15, both host predicates (`a.target_window = ?2` and `r.host = ?2`) replaced together by `?2 IS NOT NULL`: red, both tests; either alone: green, because the other still keys on the host), `…::an_open_web_collect_against_a_newer_store_is_a_version_refusal` (`RobotsLedger::open`'s version check, mutation 2026-11-15, removed: red); 7 mutations 2026-11-15 on the first build, as listed in §11.5 step 3, and the review round's beside each test above; the pinned body's paths and pace: §11.2, §8 row R19<br>held (§11.1): `--check` raises `rows_committed_zero_against_vendor_rows` only when `vendor_rows` − `held` > 0, its detail naming both; `services/newsvault/tests/open_web_collect.rs::a_quiet_day_whose_every_listed_story_is_held_passes_ledger_check` (run 1 commits GNW's 3 stories under 2026-11-14, run 2 on 2026-11-15 lists the same 3, all held: `vendor_rows` 3, `held` 3, rows 0, no finding, the binary exits 0; mutations 2026-11-15, the subtraction removed: red; the comparison made `>= 0`: red; `record_held` a no-op: red); `services/newsvault/tests/ledger.rs::check_still_flags_zero_rows_when_held_covers_only_part_of_the_vendor_rows` (59 vendor rows, 58 held: exit 1; mutation that day, any non-zero `held` suppressing the finding: red); `…::check_still_flags_zero_rows_when_an_unclosed_row_carries_held` (an open row holding 60 beside 59 vendor rows: `held` 0, exit 1; mutation 2026-11-15, `held` summed over every row: red, exit 0 with no finding); `open_web_collect.rs::two_all_held_polls_of_one_day_hold_every_vendor_row_of_both` (two all-held polls on 2026-11-15: `vendor_rows` 6, `held` 6, rows 0, exit 0; mutation 2026-11-15, `row.held += entry.held` made `=`: red, `held` 3 and a false finding); `ledger.rs::the_human_ledger_states_the_held_count_of_the_json_row` (mutation 2026-11-15, `held` dropped from `render_human`: red)<br>refetch cost (§11.1): the source row's `listing_unusable`, a `SUM` beside `held` with the same `vendor_rows IS NOT NULL` guard, one per fetch of such a story, the first fetch included; not subtracted before the finding; `open_web_collect.rs::the_ledger_counts_every_fetch_of_a_story_whose_listing_stayed_unparseable` (release 0 listed with seconds on 2026-11-14 and by both polls of 2026-11-15: `vendor_rows` 8, `held` 4, `listing_unusable` 2, rows 0, finding `rows_committed_zero_against_vendor_rows` "8 vendor rows, 4 held, and no item row committed"; mutations 2026-11-15, the `SUM` made constant 0: red; `+=` made `=`: red); `ledger.rs::the_human_ledger_states_the_listing_unusable_count_of_the_json_row` (mutation that day, the field printed as 0 by `render_human`: red); golden `expected.json` carries it (0)<br>bodies stored cut at the limit (§3.9): the source row's `bodies_truncated`, a `SUM` of `payloads.truncated`; `ledger.rs::a_body_stored_cut_at_the_limit_is_counted_for_its_source_and_day` (the fixture's one truncated row, edgar 2026-07-03: 1; polygon that day and edgar 2026-07-01: 0; the human line: 1; mutations 2026-11-16, the `SUM` made 0: red, with the golden test; the field dropped from `render_human`: red); golden `expected.json` carries it on every source row since 2026-11-16; 0 for an attempt closed before user_version 6, kept (owner decision u, §3.9)<br>re-listed held stories (row R15): the source row's `relisted_disputed`, a count of `relisted` rows on the day's attempts; `ledger.rs::a_relisted_story_is_counted_for_the_source_and_day_of_its_index_attempt` (one hand-written row on edgar's 2026-07-03 index attempt: 1; polygon that day and edgar 2026-07-01: 0; the human line: 1; mutations 2026-11-16, the `SUM` made 0: red, with `open_web_collect.rs::a_held_story_listed_under_another_minute_is_recorded_without_a_fetch_and_counted`; the field dropped from `render_human`: red); golden `expected.json` carries it on every source row since 2026-11-16; no `--check` finding; 0 for an attempt closed before user_version 7; a story re-listed by k polls counts k; 0 does not mean no dispute, because a listing R15 does not compare (its unguarded cell) writes nothing<br>held stories compared (decision x, 2026-11-16): the source row's `relisted_compared`, a `SUM` of `attempts.relisted_compared`, so on an attempt from user_version 8 `relisted_disputed` 0 with `relisted_compared` 0 reads as "nothing compared", not "no dispute"; an attempt closed before user_version 8 keeps NULL, summed as 0, so on such a day `relisted_compared` can be below `relisted_disputed` and 0/0 means "not counted" (shown 2026-11-16 by a temporary print in `a_relisted_story_is_counted_for_the_source_and_day_of_its_index_attempt`: `relisted_disputed 1 relisted_compared 0`); no `--check` finding for it, which every store that ran user_version 7 would raise; `ledger.rs::a_relisted_comparison_count_is_summed_for_the_source_and_day_of_its_index_attempt` (3 hand-written on edgar's 2026-07-03 index attempt: 3, with `relisted_disputed` 0; polygon that day and edgar 2026-07-01: 0; the human line: 3; mutations 2026-11-16, the `SUM` made 0: red; the field dropped from `render_human`: red); producer on the real collect path: `open_web_collect.rs::a_held_story_listed_under_another_minute_is_recorded_without_a_fetch_and_counted` (3 held, 2 compared: release 0 differs, release 1 equals, release 2's page left no item; mutation 2026-11-16, `compared` counting differences only: red); golden `expected.json` carries it (0) on every source row since 2026-11-16; 0 for an attempt closed before user_version 8<br>mutation-tested 2026-11-13 | call clause **enforced** within holes 1–2; store clause **enforced** within its two holes<br>robots clause **enforced** since 2026-11-15 for every admitted row, the whole run refused on the first failure (§11.3's per-host block is not built)<br>robots pins vs the committed bodies, GNW, PRN and CNW, and the Fed's `absent` pin answered by a 404: `open_web_feed::the_committed_hosts_file_pins_the_committed_robots_body` and `newsvault`'s `robots_refresh.rs::the_committed_hosts_file_refreshed_with_its_committed_bodies_matches_every_pin` (mutations 2026-11-16 and 2026-11-17, one hex digit of the PRN pin, then of the CNW pin, changed: both red each time, §11.5 step 6)<br>robots unguarded: no age bound on the fetch (no measured threshold; re-fetch is step 7); the read precedes the lease, so a refresh landing between them is unseen, and a refresh can only add a newer answer; a collect against a store with no robots table (user_version below 2) is `robots_unverified`, not a version refusal<br>held unguarded: `record_held` runs outside the commit transaction, so a crash between them leaves `held` NULL, read as 0: the finding is then a false positive, never a hidden one (the shape of R20 hole 5)<br>held unguarded: read for every row with `opened_at <= --at`, so a `held` written after the cutoff is counted; `vendor_rows`, `outcome` and `bodies_truncated` (its `payloads` row is written at close) share this gap<br>held cost (owner decisions 2026-11-15): a document permanently failed with a 3xx, or usable with an unusable row the index row decided (`attempts.listing_unusable` > 0), is fetched again by every poll, one paced request (10 s) each, and on an otherwise all-held day its listed row leaves `vendor_rows` − `held` > 0 with 0 rows, so `--check` raises `rows_committed_zero_against_vendor_rows` (exit 1); accepted, because a listed story we cannot store is a real gap; measured 2026-11-15 by `the_ledger_counts_every_fetch_of_a_story_whose_listing_stayed_unparseable`; the 3xx refetch is derivable from `transport_status` and counted by no ledger or envelope field, unguarded; the listing one is the source row's `listing_unusable`<br>listing cost unguarded: the `vendor_rows IS NOT NULL` guard on `listing_unusable`; dropping it stayed green (2026-11-15), because the count is written only by `Finalised`, which also writes `vendor_rows`<br>held unguarded: `filtered_out` is not stored, so an all-held day whose index also filtered out a row (off-path, repeated) still flags<br>not built: `--check` asking-clock age<br>no caller: `Cost::Slots(0)`, `PermitAlreadyTaken`, `AnswerFromAnotherAttempt`<br>hole 1: privacy is per module: `SendPermit` literal in `ledger.rs` or a child of it, or `pub fn` in `wire.rs` calling `exchange`; `std::net` and unlisted clients pass `clients`<br>hole 2: `std::process::Command` running curl, unchecked<br>clock scan: fires on comments, misses renamed clocks; `services/` and other members unscanned<br>sink scan: cuts each line at its first `//`, so a `/* */` block hides a use and a `//` in a string cuts the line short; a sink reached through an alias without `Sink` in its name passes<br>contact: blob store unchecked; the written-contact test runs an `edgar_press` collection only, so an `open_web_wires` run's envelope and rows are unchecked<br>store hole: FK checks existence, not closure<br>store hole: second connection with `foreign_keys` off |
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
| R15 | disputed timestamps marked, never corrected | `record` writes `items.valid_disputed` and `items.earliest_valid_at` on the arriving row only; no stored row is updated<br>`a_disputed_publication_time_is_marked_on_the_arriving_row_with_the_earlier_twins_value` (mutation-tested 2026-11-13)<br>`the_first_row_is_byte_identical_after_the_second_arrives`: catches a write to a stored row only when it is conditional on a twin<br>minute rows within `valid_minute`: `a_re_stamped_minute_is_disputed_on_the_arriving_row_with_the_earlier_minute` (mutation 2026-11-15, minutes skipped: red)<br>re-listed held story (owner decision 10, 2026-11-16, every open-web host): a live index attempt that does not issue a held story records, after its commit, one `relisted` row (attempt, story, listed text, listed minute, stored minute) when the minute it lists (`Provider::listed_publication`) differs from the `valid_minute` of the item first sighting kept; the page is not fetched and no stored row is updated; producer `news_provider::collect::attempt_once` → `Committer::record_relisted`; reader `newsvault ledger`'s `relisted_disputed`, and of the comparisons made, equal or not, `relisted_compared` (decision x, 2026-11-16; row R4); `newsvault::store::tests::a_held_story_listed_under_another_minute_is_recorded_on_the_index_attempt_only`, `…::a_listing_equal_to_the_kept_minute_or_with_nothing_to_compare_is_not_recorded`, `…::a_listing_is_compared_with_the_item_first_sighting_kept`, `…::an_earlier_listing_is_compared_with_an_earlier_observation_committed_later`, `…::a_listing_of_a_story_whose_kept_item_has_no_minute_is_not_recorded`, `…::a_relisted_row_breaking_a_constraint_of_its_table_is_refused`; driven: `open_web_collect.rs::a_held_story_listed_under_another_minute_is_recorded_without_a_fetch_and_counted` (GNW release 0 kept at 19:00, re-listed at 19:05 by a second poll: the index sent alone, the `stored_items` columns unchanged (not `valid_disputed` or `earliest_valid_at`; the store test's `row_text` covers every column), one row on the second index attempt, ledger 1), `…::a_live_index_whose_commit_was_refused_records_no_relisted`; mutations 2026-11-16 after review, each red: `ORDER BY observed_at, seq` made `ORDER BY seq` (`an_earlier_listing_…`); `!=` made `<` (`an_earlier_listing_…`); the kept read restricted to `valid_minute IS NOT NULL` (`…_kept_item_has_no_minute_…`); `record_relisted` moved out of the refused-commit guard (`…_refused_records_no_relisted`); each was green before its test; mutations 2026-11-16, each red: the driver's call removed (driven); `Unknown` handed for every listing (driven); open-web's `listed_publication` returning `Unknown` (driven, `a_globenewswire_pubdate_is_a_minute_and_never_an_instant`, `a_pr_newswire_pubdate_is_a_minute_and_never_an_instant`); every listing inserted (`…_or_with_nothing_to_compare_is_not_recorded`, `…_first_sighting_kept`, driven); the kept item read newest first (`…_first_sighting_kept`); the `CHECK` removed (`…_breaking_a_constraint_of_its_table_is_refused`)<br>rule: a stored twin carries a different publication value; no materiality threshold, no measurement places one<br>lake grain: `lake.rs` keeps the first sighting's publication time when another run of the same UTC day reports a different one, so a disagreement across two UTC days is not counted; counters `valid_time_conflict_events`, `items_with_conflicting_valid_time`; `a_publication_time_disagreement_is_counted_as_rows_and_as_items`, `a_replacing_row_counts_its_disagreement_too`<br>population: memo §5.1 | **enforced on the write side; detection counted in two units at the lake grain**<br>re-listed held story **enforced** for a listed minute since 2026-11-16, counted by `ledger`<br>unguarded: no reader of `valid_disputed`, `earliest_valid_at` or `doc_id`<br>unguarded: twins of one story in two precision columns (instant, minute, day) are never compared; day rows are never disputed<br>unguarded since 2026-11-15, narrowed 2026-11-16: a story held under §11.1 is never fetched again (`a_second_live_poll_of_one_day_fetches_no_held_document`: the second poll sends the index alone); its re-listed minute is a `relisted` row (above), but a held story with no committed item (a permanent failure, a page-decided unusable row), a kept item with no `valid_minute` (`valid_refused`) and a listing that does not parse are not compared, and their listed text lives only in the index attempt's raw blob, linked by its `payloads` row from user_version 6 (§3.9)<br>unguarded: a story listed twice in one index is compared by its first row only (row R3 drops a later row's `listed_date`), so a re-listing after the original in one feed writes no row and one before it does; probe 2026-11-16, GNW release 0's `<item>` doubled with pubDate 19:05 after vs before the original, second poll: 0 rows vs 1 (probe not kept; the PRN 302913655 notice was listed without its original)<br>unguarded: every poll that lists a re-listed story again writes another row, one per index attempt; a crash between `record_held` and `record_relisted` leaves the held count without its rows (read from the code 2026-11-16). Narrowed that day: a story whose listed time did not parse is not held, so a corrected listing reaches `record` (`a_document_whose_listed_time_did_not_parse_is_fetched_again_and_committed_once_the_listing_parses`); a story held with a committed item still is not re-read |
| R16 | the **coverage ledger** records poll events | `newsvault collect` writes one attempt row per HTTP call; `items_new` has one writer, `record` (first sightings per `story_id`, 0 written as 0)<br>`the_same_story_in_three_commits_keeps_the_first_observation_and_writes_items_new_zero`, `two_identical_collects_leave_the_right_events_one_blob_set_and_items_new_one_then_zero` (mutation-tested 2026-11-13)<br>`ledger --check` exits 1 when `vendor_rows` − `held` > 0 and committed items = 0 (§8 row R4): `check_exits_one_when_a_source_returns_zero_items_against_a_non_zero_vendor_row_count`, `a_cap_does_not_hide_a_check_finding`<br>held filter (§11.1), open-web live: the index is polled every run and a held story's document is not fetched again; `services/newsvault/tests/open_web_collect.rs`: `a_second_live_poll_of_one_day_fetches_no_held_document` (mutation 2026-11-15, the live arm handed an empty set: red), `a_permanently_failed_document_is_not_fetched_again` and `a_document_that_yielded_only_unusable_rows_is_not_fetched_again` (`NoArticleBody`, the page-decided control; mutation that day, the held read keeping its items arm only: red); a usable document with an unusable row the index row decided is not held (owner decision 2026-11-15): `a_document_whose_listed_time_did_not_parse_is_fetched_again_and_committed_once_the_listing_parses`, `a_story_whose_listing_stays_unparseable_is_fetched_by_every_poll` (mutations that day: `NoParseableValidTime` marked `Page` in open-web, the driver passing `listing_unusable` 0, the exclusion dropped, `apply` binding NULL: each red, both tests); a permanent failure answered with a 3xx is not held (owner decision 2026-11-15): `a_document_answered_with_a_redirect_is_fetched_again_by_the_next_poll` (mutation that day, the 3xx exclusion dropped: red; the 301 from a loopback server through the real open-web `Wire`, row R21); `open_web_feed::a_live_follow_up_whose_story_is_held_is_dropped_and_an_unheld_one_kept` (mutation that day, the filter removed: red) | **enforced for the poll event; zero-against-non-zero detected by the reader**<br>held filter: **enforced** for open-web live; edgar live polls every matched row, unchanged; the held read's `empty` arm has no real-path producer, since neither `classify` returns `Empty` for a `Document` (2026-11-15)<br>an all-held open-web day passes `ledger --check` since 2026-11-15 (row R4)<br>unreconciled: `items_new` against `COUNT(items)` when both are non-zero and disagree<br>unguarded: an item an index lists and drops between two polls leaves no row, so the ledger cannot count it and no reader detects it; the PRN and GNW RSS indexes left stretches no poll covered at both periods, 2026-11-16 (§11.2's period table), items lost in them not counted; option C's Yahoo cross-check (§11.2, built 2026-11-17) names, per run, Yahoo rows under a PRN, GNW or CNW key whose title no stored direct index listed, in the collect envelope only: `a_yahoo_row_under_a_wire_key_no_direct_index_listed_is_named_in_the_run_envelope`; it is a title match, and the ledger does not read it |
| R17 | matching survives a broad corpus, over article body | body: `edgar-press::EdgarPress::extract` → `ExtractedBody`; `the_extracted_body_is_article_text_and_never_page_html_or_a_script_payload` (mutation-tested 2026-11-13)<br>rules file hashed; `extract_rules_version` is not an `item_id` input: `one_byte_changed_in_the_rules_file_changes_the_extracted_body_and_therefore_the_item_id`, `a_rules_edit_that_leaves_the_bytes_identical_leaves_every_item_id_identical` | **body text enforced for two providers; broad corpus is design**<br>GNW: `p, li, figcaption` text inside the rules file's `div.main-body-container` (§11.6; the selector's history: §11.1): `the_extracted_body_is_article_text_and_never_page_html_on_a_globenewswire_page` over the trimmed real release 3403075, whose page holds a `<p>` and two `<script>`s outside the container (mutation 2026-11-15, container widened to `body` in the rules file: red); an absent container is `NoArticleBody`: `a_page_with_no_article_container_is_unusable_as_no_article_body`; a container holding only empty or whitespace `<p>` is `NoArticleBody`: `a_container_holding_only_empty_paragraphs_is_unusable_as_no_article_body` (mutation 2026-11-15, the empty-paragraph filter removed: red, green before the test)<br>PRN, since 2026-11-16: `p, li, figcaption` text inside `section.release-body` (§11.1), the 3 summary `<li>` of 302939553 first: `the_extracted_body_is_article_text_and_never_page_html_on_a_pr_newswire_page` over the trimmed real release 302939553, whose page holds the dateline and `Share this article` `<p>` inside `<article>` but outside the section, an ld+json `<script>`, and the real page's `section.nav-main` with one `<p>` (mutations 2026-11-16, the container `body`: red; the container `article`: red; the container `section`: red, green before `nav-main` was kept); a page with no `section.release-body` is `NoArticleBody`, decided by the page: `a_pr_newswire_page_with_no_release_body_is_unusable_as_no_article_body` (mutation 2026-11-16, the container `article`: red); a `br` inside a paragraph is a space: `a_line_break_inside_a_paragraph_is_read_as_a_space` (mutation 2026-11-16, the `br` arm removed: red)<br>CNW, since 2026-11-17: the same `section.release-body` and paragraph selector: `the_extracted_body_is_article_text_and_never_page_html_on_a_canada_newswire_page` over trimmed real 805423040 (mutations 2026-11-17, the container `body`, `article` and `section`: each red), `a_canada_newswire_page_with_no_release_body_is_unusable_as_no_article_body` (mutations 2026-11-17, the container `article` and `div.release-content`: each red); coverage 0.988–0.9995 over 7 URLs (§11.1)<br>Fed, since 2026-11-17: container `div#article > div.col-md-8:not(.heading)` and the same paragraph selector: `the_extracted_body_is_article_text_and_never_page_html_on_a_federal_reserve_page` over trimmed real bcreg20261113a and orders20261109a (mutations 2026-11-17, the container `body`, `div#article`, `div#article > div.col-md-8` and `div.col-md-8:not(.heading)`: each red), `a_federal_reserve_page_with_no_article_block_is_unusable_as_no_article_body` (mutations 2026-11-17, the container `body` and `div.col-md-8:not(.heading)`: each red); coverage 0.911–0.991 over 5 URLs (§11.1); Yahoo, since 2026-11-17: container `div[data-testid="article-body"]` and the same paragraph selector, with no Yahoo skip rule: `the_extracted_body_is_article_text_and_never_page_html_on_a_yahoo_page` over trimmed real `business-wire.com` and `prnewswire.com` reprints (mutations 2026-11-17, the container `body` and `div.bodyItems-wrapper`: each red), `a_yahoo_page_with_no_article_body_is_unusable_as_no_article_body` (mutations 2026-11-17, the container `article`, `body` and `div.body`: each red); `Story Continues` in 0 of 16 bodies (§11.1); unguarded, Yahoo: `article` and `div.body` give the fixture's body unchanged, so only the synthetic page tells them from the committed container; unguarded, Fed: `figcaption` in the paragraph selector, because no Fed fixture holds one (0 of 5 files, `grep -c`, 2026-11-17), so dropping it turns only the golden test red<br>`script`/`style`/`noscript`/`template` text and paragraphs without a letter or digit, a non-UTF-8 page, two containers and a page past the depth bound: §11.1 (each mutation-tested 2026-11-15)<br>unguarded: corpus is 2 committed EDGAR fixtures (median extracted body 212 chars, stated 2026-11-14), 3 trimmed real GNW pages (1,050 chars; 504 and 312 for 3403177 and 3403225, each cut to its contact block for r, Python `len` 2026-11-16) and 3 trimmed real PRN pages (2,092, 646 and 1,643 chars; 1,491 for 302939553 before its `<li>` were read), the 6 by `OpenWebFeed::extract` on 2026-11-16, and 2 trimmed real CNW pages (1,480 and 2,039 chars, `parse::page` under the committed rule, 2026-11-17), against 14,039 real (n=103, §9)<br>one shared path for every host (P6), each rule of §11.1's Body table a named test there: `a_list_item_holding_a_paragraph_or_a_nested_list_is_one_line_read_once`, `a_cloudflare_placeholder_and_an_address_never_reach_the_body_and_a_mailto_name_does`, `a_table_is_read_one_line_per_row_and_a_nested_table_inside_its_outer_cell`, `a_globenewswire_page_reads_lists_tables_and_addresses_through_the_same_path`, `a_figure_caption_is_one_line_in_document_order_on_a_pr_newswire_page`, `an_address_in_link_text_or_plain_text_leaves_a_globenewswire_body_and_the_contact_stays` (mutations 2026-11-16 in §11.1)<br>email addresses removed from body text on every host, names, phone numbers and labels kept (owner decision r, 2026-11-16, an exception to P3; §11.1): the r test above, `parse::tests::every_address_the_regex_matches_is_removed_and_nothing_around_it`, `parse::tests::a_handle_a_profile_url_and_a_bare_at_sign_are_kept`; 0 addresses left in 57 page files of 39 URLs (§11.1); unguarded: an address the regex does not match, such as `name [at] example.com` (none in those 57 files, 2026-11-16); letters joined to an address with no space, as when inline elements meet, are removed with it (`Media Contact<a href="mailto:…">jane@example.com</a>` reads `Media`; pinned, not prevented, by `letters_joined_to_an_address_with_no_space_are_removed_with_it`; 0 of the 20 matches in 58 page files joined, 2026-11-16, §11.1)<br>**extraction code versioned since 2026-11-16** (P5, §11.1): `extract_version` (§2) digests the rules file and every file of `providers/open-web-feed/src/`; `version::tests::every_source_file_is_in_the_extraction_version` compares `SOURCES` with the directory, hidden entries left out (mutation 2026-11-16, `depth.rs` dropped from `SOURCES`: red; a `.DS_Store` in `src/` was red before the filter, green after); `version::tests::every_listed_source_is_the_file_it_names` (mutation 2026-11-16, `("parse.rs", include_str!("lib.rs"))`: red; before, the golden test alone); `version::tests::one_byte_of_extraction_code_or_rules_changes_the_extraction_version`; golden `every_golden_page_extracts_to_its_committed_body_under_the_committed_version` over 20 fixture pages (7 GNW, 7 PRN, 2 CNW, 2 Fed, 2 Yahoo, 2026-11-17; `tests/fixtures/golden_bodies.json` holds each body's sha256 and the `extract_version` that produced them) stays red after any edit to the crate's `src/` or rules file until that file is rewritten by hand (mutations 2026-11-16, one doc comment in `parse.rs` reworded: red, the golden test alone; PRN's paragraph `p` alone: red; each Body rule of §11.1 removed: red; either host's paragraph without `figcaption`: red)<br>unguarded: `edgar-press`'s code is not versioned (its `extract_rules_version` is the rules digest alone and its attempts carry NULL); code outside `providers/open-web-feed/src/` that shapes a body is not in the digest: `news-provider`'s `ExtractedBody`, and `scraper` 0.29.0 and html5ever as `Cargo.lock` pins them (read 2026-11-16, not run)<br>line ends of the digested files: §8 row G2<br>gazetteer regression case is `derive`'s; `derive` has no code |
| R18 | two rule versions, one snapshot, a diff | a `derive diff` subcommand; its absence is the gap | design |
| R19 | **a vendor is called no faster than its stated budget, counted against the bucket the vendor counts it against** | `RequestBudget` (optional per-IP `min_interval`, optional `per_host`) in `TransportProfile`; `edgar-press`: 100 ms both = SEC's 10 req/s per IP (predecessor table, read 2026-11-13; unmeasured, no margin); `open-web-feed`: 10 s `per_host` and no per-IP pace, so each host is its own queue (owner decision 1, 2026-11-17, §10)<br>a profile with neither pace is `BudgetError::NoPace`, refused before any clock read or wait: `news_provider::wire::tests::a_profile_stating_no_pace_at_all_is_refused_before_any_wait` (mutation 2026-11-17, the check removed: red)<br>`Wire::send` (only socket, R4) waits host slot, then per-IP slot when the profile states one (so an IP slot is never held through a long host wait other hosts would queue behind), via `Pace::reserve` on `<store>/pace/` files; `Wire::new` builds its own `ThreadSleeper` (injecting constructor `pub(crate)`); not in-process: in the predecessor (`sec_rate_coordinator.py`), 3 processes on one IP drew a SEC 403, not measured here<br>an unparseable url skips the host wait: safe only because ureq 3.5.2 parses with the same `http::Uri` and fails before any socket (`news_provider::wire::tests::a_url_that_does_not_parse_skips_the_host_wait_and_fails_as_a_transport_error`); a ureq upgrade could change that<br>tests: `news_provider::wire::tests::two_wires_sharing_one_pace_file_serialise_against_each_other`, `news_provider::wire::tests::the_ip_slot_is_reserved_after_the_host_wait_and_not_before_it`, `news_provider::clock::testing::the_process_sleeper_actually_waits`, `news_provider::budget::tests::a_host_key_breaking_any_one_rule_is_refused`, `edgar_press::the_sec_profile_paces_both_our_address_and_the_sec_host_at_ten_a_second`, `open_web_feed::the_profile_paces_each_host_at_one_request_per_ten_seconds_and_no_shared_ip_queue` (10 s per host, `min_interval` `None`, §11.2)<br>per-host queues (owner decision 1, 2026-11-17): over two loopback hosts (`127.0.0.1`, `localhost`) and `WaitedClock`, each red under the named mutation 2026-11-17 (`cargo test --locked -p <crate> --no-fail-fast`): `news_provider::wire::tests::a_hold_on_one_host_leaves_another_host_owing_only_its_own_floor` (a 120 s `Retry-After` on A; B owes 0 s, then its full 10 s), `…::a_hosts_only_profile_writes_one_pace_file_per_host_and_no_per_ip_file`, `…::two_sends_to_one_host_under_a_hosts_only_profile_are_still_its_floor_apart` (all three red under a per-IP pace built from `per_host` when `min_interval` is `None`, and under the host wait skipped when no per-IP pace exists); `…::a_profile_with_a_per_ip_pace_still_spaces_two_hosts_by_it` (edgar's shape; the per-IP slot reserved and never slept: red, with 21 other `wire` tests); `open_web_feed::the_profile_paces_each_host_at_one_request_per_ten_seconds_and_no_shared_ip_queue` (open-web `min_interval` `Some(INTERVAL)`: red, with the golden-body test, whose `extract_version` hashes the crate source); `edgar_press::the_sec_profile_paces_both_our_address_and_the_sec_host_at_ten_a_second` (edgar `min_interval` `None`: red)<br>send order, round-robin by host (§10, 2026-11-17): over three loopback hosts (`127.0.0.1`, `localhost`, `127.1`), `news_provider::wire::tests::round_robin_by_host_overlaps_the_host_floors_through_a_real_wire` waits 30 s in all (40 s under the mutation keying every request to one lane; 70 s under a turn that does not move past the lane served, mutation 2026-11-17); `services/newsvault/tests/open_web_collect.rs::a_five_host_run_asks_its_hosts_round_robin` (GNW, PRN, CNW, Fed and Yahoo indexes, then one release of each per turn, Yahoo's two first to run out; red under both mutations and under lanes sorted by host name, re-run 2026-11-17 on the five-host name); `…::a_hold_on_one_host_delays_the_other_hosts_turns_under_round_robin` waits 150 s (130 s under the one-lane mutation; §10 table); both `wire` tests and `collect` re-queue through one `HostQueue::requeue`, and the loop around it in the `wire` tests is a copy of `collect`'s, so a change to `collect`'s own loop is caught only by the `newsvault` tests (mutation 2026-11-17, retries held in a queue of `collect`'s own until every lane is empty: red only `a_retried_index_keeps_its_host_turn_while_the_other_hosts_releases_are_served`); page urls differ from their index's, so a lane per url instead of per host is red in both `wire` tests (mutation 2026-11-17); no host is asked faster: the order changes, `Wire::send` does not<br>admission, open-web: only `admitted` hosts-file rows are planned: `only_admitted_hosts_are_planned` (mutation 2026-11-15, the status filter removed from `plan`: red); a follow-up whose rewritten link leaves its index's host is filtered out: `a_link_off_the_path_filter_off_the_host_or_absent_is_filtered_out_and_counted`; an answer from a host no row admits, a `candidate` row's host included, is `PermanentFailure` and extracts nothing: `an_answer_from_a_host_no_row_admits_is_permanent` (mutation 2026-11-15, `admitted_row` matching any row: red, green before the candidate case; unreachable through `plan` today); the committed file plans GNW's index, then PRN's (§11.2's order): `the_committed_hosts_file_plans_gnw_then_prn_then_cnw_then_the_fed_then_yahoo` (mutations 2026-11-16, the PRN row `candidate`: red; the two rows swapped: red)<br>robots floor, open-web (§11.2): `collect`, dry run included, refuses an admitted row whose provider `per_host` is `None` or below 10 s, or whose pinned `*` Crawl-delay is above that `per_host` (owner decision 1, 2026-11-16; `CrawlDelayOverInterval`), `cmd_collect::refuse_a_pace_below_the_floor` and `crawl_delay_over_interval`: `services/newsvault/tests/open_web_collect.rs::a_hosts_row_whose_robots_crawl_delay_is_above_the_shared_interval_is_refused` (nasdaq's 30 s and the B2 body's 30 s refused, a 10 s delay admitted, `per_host` 10 s in all three); the 10 s is `cmd_collect::PACE_FLOOR`, apart from the provider's own `INTERVAL`; mutations 2026-11-16 as in §11.5 step 3; mutation-tested: 7 mutations 2026-11-13, re-run 2026-11-15 with `cargo test --locked -p news-provider`, each turning a named test red (`Pace::reserve` returns zero; reserve without waiting; sleep the whole interval; one pace file per `Pace`; an unparseable pace value reset to `now`; reserve before the credential check; an empty `ThreadSleeper::sleep`); 15 mutations 2026-11-14, not re-run<br>redirects (§11.2, §11.5 step 5; owner decision v, 2026-11-16, §10): `Wire::with_sleeper` sets ureq 3.5.2's `max_redirects` to `MAX_REDIRECTS` = 0 for every profile, with no `TransportProfile` field; at 0 ureq returns the 3xx as the answer, never `TooManyRedirects` (`config.rs` doc of `max_redirects`); over a loopback server answering `/page` 301 `Location: /elsewhere`: `news_provider::wire::tests::a_redirect_is_answered_as_its_3xx_and_its_target_never_requested` (301, its body, server saw `/page` only, one pace wait); `…::every_redirect_is_answered_as_itself` (300, 301, 302, 303, 304, 307, 308, one request each); `open_web_feed::a_redirect_is_permanent_never_transient` and `edgar_press::a_redirect_is_permanent_never_transient` (the same seven, index and document; mutations: 303 and 304 mapped to `TransientFailure` in open-web, 2026-11-15: red; a `300..=399 => TransientFailure` arm in edgar's `classify`, 2026-11-16: red); edgar through its real `Wire`: `services/newsvault/tests/collect.rs::an_edgar_document_answered_with_a_redirect_is_a_permanent_failure_and_its_hop_never_sent` (Apple's filing relayed to a loopback 301: the hop never sent, `transport_status` 301, `permanent_failure`, `payloads.url` the url asked, exit 8; red under edgar's former `FollowUpTo(10)`, 2026-11-16: two hops, status 200, no 301 row, exit 0); mutation 2026-11-16, `MAX_REDIRECTS` 1: red, the three `wire` redirect tests, the edgar test, `a_document_answered_with_a_redirect_is_fetched_again_by_the_next_poll` and its over-limit twin, `a_robots_redirect_is_a_permanent_failure_with_no_robots_row_and_its_target_never_fetched` and its over-limit twin<br>server slow-down (owner-approved 2026-11-16, §10 server back-off): `Wire::send` reads every answer it gets; a 429 or any 5xx (`asks_to_slow_down`; every 5xx since 2026-11-16, recommendation #3, 503 alone before) with one usable `Retry-After` (delta-seconds or any RFC 9110 HTTP-date; a past date is a wait of 0) holds that host's next slot until that instant (`Pace::reserve_not_before`); the hold is reserved as that host's grant in `pace/<host>.per-host`, so later runs and other processes on the store wait for it too (hole 11); one with no usable value (absent, repeated, negative, unparseable, past `u64`, past year 9999, or a weekday off the calendar, which chrono 0.4.45 refuses) doubles that host's `per_host` for the rest of the `Wire`'s run (10 s, 20 s, 40 s); other hosts keep their pace; tests over a loopback server and `WaitedClock`, each red under the named mutation 2026-11-16: `news_provider::wire::tests::a_429_with_retry_after_seconds_holds_that_host_until_then_and_no_other` (hold ignored in `wait_out_the_host`; floor ignored in `Pace`; one host's hold read for every host; 429 dropped from `asks_to_slow_down`), `…::a_503_with_an_http_date_holds_that_host_until_that_date` (503 dropped; `SERVER_ERRORS` 504..=599), `…::a_429_without_retry_after_doubles_the_host_interval_at_each_answer` (no doubling), `…::a_doubled_interval_slows_only_the_host_that_answered` (every host given the most doublings of any host, review 2026-11-16: green before this test), `…::an_unparseable_or_negative_retry_after_is_absent_so_the_interval_doubles` (an unparseable value read as 0; a repeated header's first value taken), `…::a_retry_after_date_in_the_past_holds_nothing_and_doubles_nothing` (a usable value doubling too), `…::a_500_with_a_retry_after_holds_that_host_until_then` (`SERVER_ERRORS` 503..=503, 501..=599 or 504..=599), `…::every_5xx_without_retry_after_doubles_the_host_interval` (500, 502 and 599 doubled once, 499 and 600 not; red under 503..=503, 501..=599, 500..=598, 499..=599, 500..=600 and every status slowing), `…::a_403_is_no_slow_down_answer_even_with_a_retry_after` (403 added to `asks_to_slow_down`; every status slowing; the whole workspace was green under the 403 mutation before this test), `…::a_503_whose_body_is_over_the_limit_still_holds_its_host` (a failure's head status ignored in `send`; green before this test), `…::a_retry_after_past_year_9999_leaves_the_host_pace_file_readable` (the year bound removed; also red: the unusable-values test), `…::a_retry_after_in_each_http_date_form_is_read_as_one_instant` (a past date kept as is)<br>the production `Wire`s are built by `cmd_collect::wire_for` and `cmd_robots::wire_for`, which `main.rs` and the loopback tests both call (since 2026-11-15); unguarded: a `main.rs` that built its own `Wire` again (structural, not a test)<br>retries: one `exchange.send` call site (grep 2026-11-13), structural, not a test; R21 | enforced for every call through `Wire::send`, retries included, per IP where the profile states it (edgar) and per declared host, within the holes below<br>open-web admission **enforced** in `plan` and the index filter; driven by `newsvault collect` since 2026-11-15, no run against any live host yet<br>open-web robots floor **enforced** since 2026-11-15 for every admitted row of a `collect`; its paths: §8 row R4 and §11.1<br>hole 1: flock untested (hand-measured 2026-11-13: 2 processes, 20 slots, none twice)<br>hole 2: pace file per store, so two stores = two buckets<br>hole 3: no cap on a far-future reservation in `Pace`; since 2026-11-16 no robots Crawl-delay reaches a profile (one above 10 s is refused, owner decision 1), so only a profile constant could set one<br>a reservation `DateTime` cannot hold is `BudgetError::Unusable`, not a panic: `budget::tests::a_pace_too_long_to_add_is_refused_rather_than_panicking` and `…::a_per_ip_pace_too_long_to_add_is_refused_on_its_first_call` (Crawl-delay 9,000,000,000,000 s panicked at `budget.rs:202` before; mutations 2026-11-15, each unchecked add restored alone: its own test red)<br>unguarded: a reservation `DateTime` holds but past year 9999 is written as `+YYYYY-…`, which `prior` then refuses as `Unreadable` on every later call, so the bucket stays blocked (10,000-year interval, review 2026-11-15); unreachable from robots since 2026-11-16 (hole 3); from `Retry-After`, a hold past year 9999 is read as absent since 2026-11-16 (`a_retry_after_past_year_9999_leaves_the_host_pace_file_readable`; before it, `Retry-After: 8000000000000` wrote `+255536-10-08T03:13:20.000000Z` and the next `Wire` on the store got `Unreadable`, loopback probe 2026-11-16); still unguarded: a hold within one host interval of 9999-12-31T23:59:59, whose next grant is advanced past it<br>hole 4: no bounded lock acquire<br>hole 5: R4's `pub fn` in `wire.rs` reaches `exchange` unpaced<br>hole 6: same-host calls can be closer than `per_host` by the per-IP wait (900 ms at 1 s, 2026-11-14); a profile with a per-IP pace only (edgar), since open-web states none (2026-11-17)<br>hole 7: one `per_host` per profile; `www.sec.gov` and `sec.gov` are two buckets; open-web: a host whose Crawl-delay is above the profile's one 10 s `per_host` is refused, not paced, until per-host intervals exist (§11.3; owner decision 1, 2026-11-16)<br>hole 8: one test drives `edgar-press`'s profile through a real `Wire`, `an_edgar_document_answered_with_a_redirect_is_a_permanent_failure_and_its_hop_never_sent` (2026-11-16); none asserts its pace or its `body_limit`<br>hole 9: **closed** for every provider by refusing: open-web 2026-11-15, edgar 2026-11-16 (owner decision v); no hop is sent, so none is unpaced or unledgered<br>hole 10: a host slot is spent when the per-IP file then cannot be read (pace-file error only; aborts the run); edgar only since 2026-11-17<br>server slow-down **enforced** for every call through `Wire::send` whose profile states `per_host`, which both providers do<br>hole 11: the doubling, and a hold until its first reservation, live in the `Wire`'s memory; a second process sees neither the doubling nor a hold this run never reserved (it asks that host nothing more); a reserved hold is the host file's grant, so every later run waits until it plus `per_host`, a run killed during the wait included (loopback probe 2026-11-16: `Retry-After: 315360000`, file `2036-11-07T13:00:00.000000Z`, the next `Wire` waited 315,360,001 s); `newsvault robots refresh` uses the same profile and `pace/<host>.per-host` files (`cmd_robots::wire_for`), so a `collect` on that store waits for a refresh's grant: `newsvault`'s `a_robots_refresh_and_a_collect_on_one_store_reserve_one_pace_file_per_host` (mutations 2026-11-17, either `wire_for` given another store directory, or open-web `min_interval` `Some(INTERVAL)`: red); a refresh asks each host once, so a robots.txt 429 or 5xx hold or doubling is never reserved and does not reach `collect` (read from the code 2026-11-17: `Wire::heed` writes only the `Wire`'s memory; `cmd_robots::run` sends one request per hosts row, no retry); the only release is deleting `pace/<host>.per-host`<br>hole 12: no cap on a slow-down: a `Retry-After` up to year 9999 is waited out in full inside `Wire::send`, and every other host queues behind it; round-robin by host (§10, 2026-11-17) asks the held host's retry on that host's next turn, so the other hosts' remaining requests wait the hold out too, where block order asked them first (150 s against 130 s, §10 table); stopping the host and recording the rest as not looked needs a run deadline, and no code holds one (§11.2's stop deadline is §11.5 step 7, which owner condition A, 2026-11-16, requires before the first live run); consequences, unguarded:<br>• the waiting run holds the store lease, so every other `collect` or `robots refresh` on that store exits 7 (`lease_held`) for the whole wait<br>• a run killed during the wait leaves its attempt open, and the next run's sweep closes it `interrupted`, the label of a call killed mid-socket, though no call was made<br>• a value naming year 9999 is waited out, one naming year 10000 is read as absent and only doubles: the bound is the pace file's format, not a policy<br>• the doubling has no ceiling and lasts the run, recovered host included: edgar's 100 ms after 13 slow-down answers with no `Retry-After` is 819.2 s; the factor saturates at `u32::MAX`, 2^32 − 1, from 32 answers, so 100 ms reaches 429,496,729.5 s (about 13.6 years) and 10 s about 1,361 years, both within `TimeDelta` and `DateTime` (arithmetic and a probe, 2026-11-16)<br>hole 13: a profile whose `per_host` is `None` ignores slow-down answers; no provider's is<br>open (owner): per-IP file keyed per source, not per address; edgar only since 2026-11-17<br>design: per-credential scope not built |
| R20 | **a `vendor_archive` backfill converges: a re-run fetches only the uncovered accessions, and the reader reports the gap** | covered set, the `vendor_archive` arm's: `newsvault::store::Committer::covered_story_ids`, one target window, keyed on committed `items` only; `covered_story_ids_names_a_committed_story_and_not_an_ok_attempt_that_committed_nothing`<br>seam: `news_provider::collect::attempt_once`, `Archive::covered_story_ids`, `Archive::record_planned_coverage`<br>exclusion: `edgar-press::EdgarPress::follow_ups_to_issue`, over the trait's `Provider::story_of`; the covered set is its parameter, not `extract`'s, which stays pure over bytes and rules because the set is run state (an `edgar-press` `story_of` returning `None` turned `a_vendor_archive_follow_up_whose_story_is_covered_is_dropped_and_an_uncovered_one_is_kept` red, 2026-11-15); `a_vendor_archive_rerun_fetches_only_the_uncovered_accession`, `a_vendor_archive_follow_up_whose_story_is_covered_is_dropped_and_an_uncovered_one_is_kept`, `a_live_rerun_still_polls_every_matched_row_even_when_the_story_is_covered`, `a_live_follow_up_is_issued_even_when_its_story_is_already_covered`<br>held set, the `live_ingest` arm's (§11.1): `Committer::held_story_ids`, every target window, items plus closed `usable`/`empty`/`permanent_failure` document attempts, less a `permanent_failure` with a 3xx status and a `usable` one with `listing_unusable` > 0 (§11.1); `a_story_held_under_yesterdays_window_is_not_fetched_today` (mutation 2026-11-15, the held read restricted to the newest window: red)<br>gap: `attempts.matched`, `newsvault::store::coverage_up_to`, `ledger --check` raises `coverage_incomplete` on any gap, no threshold; `a_vendor_archive_day_with_an_uncovered_accession_reports_coverage_incomplete`, `a_live_window_with_an_uncovered_follow_up_reports_no_coverage_gap`<br>all mutation-tested 2026-11-13 | **enforced for the `vendor_archive` arm, within these holes:**<br>hole 1: coverage is per store; a window read under two stores counts twice<br>hole 2: the denominator needs an index attempt recording `matched`; a provider that follows up without an index phase records none<br>hole 3: `coverage_up_to` compares a count, not the planned set; a roster change can mask an uncovered story<br>hole 4: `MAX(matched)` never decreases; a filing dropped from the roster leaves a permanent `coverage_incomplete`<br>hole 5: `record_planned_coverage` runs outside the commit transaction; its failure after the items commit leaves `matched` NULL and hides the window from `--check` until the next index run; its live twin, `record_held`, leaves `held` NULL (§8 row R4)<br>hole 6: follow-ups only from the `Index` phase; chained document follow-ups are dropped silently (`edgar-press` returns none today) |
| R21 | **a transient document-fetch failure is re-attempted within the run, bounded, and only a residual gap stands** | `news_provider::collect` re-queues a failed, non-`permanent` attempt (`Attempted::is_transient`) to the back of its host's lane (`HostQueue`, round-robin by host since 2026-11-17, §10), capped by `MAX_TRANSIENT_RETRIES` = 2 (a work cap, not fitted to the loss rate)<br>retry order under round-robin, each red under the mutation 2026-11-17 keying every request to one lane: `news_provider::collect::tests::a_retry_goes_to_the_back_of_its_host_lane_and_stops_at_the_cap`, `services/newsvault/tests/open_web_collect.rs::a_retried_release_waits_behind_the_other_hosts_and_stops_at_the_cap` (GNW's first release answering 503 three times: asked after every queued PRN and CNW release, 3 times in all); the retry's own wait on its host's floor: `news_provider::wire::tests::round_robin_by_host_overlaps_the_host_floors_through_a_real_wire` (a 408 retried after 10 s)<br>back of its OWN host's lane, not of the whole queue, on lanes of unequal length: `news_provider::collect::tests::a_retry_on_a_short_lane_is_served_in_its_host_turn_while_a_longer_lane_drains` (GNW 1 page failing, PRN 3 pages) and `services/newsvault/tests/open_web_collect.rs::a_retried_index_keeps_its_host_turn_while_the_other_hosts_releases_are_served` (PRN's index answering 503 three times); mutations 2026-11-17, each on the `news-provider` and `newsvault` suites: retries served only after every lane is empty, inside `HostQueue`: red, both tests and `wire`'s hold test; the same inside `collect`'s loop: red, the `newsvault` test only (on c972ae7, before these tests, it left all 419 tests of both suites green, review 2026-11-17); a retry to the FRONT of its lane: red, `a_retry_goes_to_the_back_of_its_host_lane_and_stops_at_the_cap`, `one_host_is_one_lane_and_keeps_the_fifo_order`, `an_edgar_run_on_its_one_host_keeps_the_fifo_order_retries_last` (Apple, the first filing, failing; green before 2026-11-17 with Microsoft, the last, failing) and `a_retried_release_waits_behind_the_other_hosts_and_stops_at_the_cap`<br>measured 2026-11-13, before retries: run 1 lost 42 of 502 document fetches (15 HTTP 503, 27 connection-level nulls), run 2 lost 43 of 502, a different subset, so the ~8% loss is transient<br>each retry is its own attempt row (R4); the exit counts only the final attempt, `per_source` counts rows<br>a transient failure, and a permanent one answered with a 3xx (owner decision 2026-11-15), hold nothing (§11.1), so the next poll fetches it again: `a_transiently_failed_document_is_fetched_again_by_the_next_poll` (mutation 2026-11-15, `transient_failure` added to the held verdicts: red), `a_document_answered_with_a_redirect_is_fetched_again_by_the_next_poll` (sent once in the first poll, not retried; mutation 2026-11-15, the held read's 3xx exclusion dropped: red); release 0 goes through the real open-web `Wire` (no redirect followed, row R19) to a loopback server answering `301 Location: /elsewhere`, every other url to the stub; the attempt row stores `transport_status` 301, verdict `permanent_failure`; the server sees release 0's path once per poll and `/elsewhere` never (mutation 2026-11-15, open-web set to `FollowUpTo(1)`: red, `/elsewhere` fetched, the attempt closed `usable` with status 200 and the story held; 2026-11-16, `MAX_REDIRECTS` 1: red)<br>a refetch poll of a day whose other stories are all held commits nothing and closes one `permanent_failure`, so it exits 8 `vendor_refused_all` (`Tally::exit`) although the index answered 200: that test and its over-limit twin assert exits `partial_collect` then `vendor_refused_all` (2026-11-15); such a day exits 8 on every poll while the 3xx lasts; whether a 3xx-only permanent failure should count toward `vendor_refused_all` is open (owner)<br>not an injected backoff `Sleeper`: a second clock in the driver; retries pace through `Pace::reserve` (R19), and since 2026-11-16 after a 429 or a 5xx through R19's server slow-down, which changes the spacing and not `MAX_TRANSIENT_RETRIES`<br>`services/newsvault/tests/collect.rs::a_transient_fetch_failure_is_retried_and_commits_the_item_within_one_run`, `::a_permanent_fetch_failure_is_attempted_once_and_never_retried`, `::a_request_that_fails_transiently_every_time_is_bounded_to_n_plus_one_attempts`; mutation-tested 2026-11-13/14<br>a body over the profile's `body_limit` is permanent in both providers: `Wire` maps ureq 3.5.2's `Error::BodyExceedsLimit` to `TransportKind::BodyOverLimit`; before 2026-11-15 it was a string like every other transport error and closed `TransientFailure` (review 2026-11-15, read from the code, not run against a vendor); `news_provider::wire::tests::a_body_over_the_limit_fails_as_its_own_kind_and_one_at_the_limit_is_read` over a loopback socket (`body_limit` bytes read, `body_limit` + 1 `BodyOverLimit`, a body cut short `Conversation`), `open_web_feed::a_body_over_the_limit_is_permanent`, `edgar_press::a_body_over_the_limit_is_permanent_and_a_lost_connection_transient`; 5 mutations 2026-11-15, all red<br>since 2026-11-16 the over-limit body's first `body_limit` bytes are stored and linked `truncated` (§3.9), the verdict unchanged<br>bot protection is permanent (§11.1), open-web: 403 and every other 4xx but 408 and 429 close `PermanentFailure`; `a_bot_protection_page_is_permanent_never_transient` over GNW's own Akamai 403 page (2026-11-14; mutation 2026-11-15, 403 classified transient: red)<br>a 408 is transient in both providers (owner decision 2026-11-15, §10), so it is retried within the run and never held: `open_web_feed::a_408_request_timeout_is_transient` and `edgar_press::a_408_request_timeout_closes_a_transient_failure`, each over an index and a document request; `services/newsvault/tests/open_web_collect.rs::a_document_answered_408_is_fetched_again_by_the_next_poll` (3 sends in the first poll, fetched again in the second); mutations 2026-11-15: the open-web 408 arm removed, the first and third red; the edgar 408 arm removed, the second red | **enforced** for every request the driver attempts<br>over-limit body permanent: **enforced** in both `classify`, whatever the status, a 408, 429 or 5xx included: `classify` reads the kind first; since 2026-11-15 that `TransportError` carries the head's status, stored as `transport_status` (§11.1)<br>open-web's `body_limit` (2 MiB) is driven through `Wire` by `a_redirect_whose_body_is_over_the_limit_is_fetched_again_by_the_next_poll` and `a_robots_redirect_whose_body_is_over_the_limit_keeps_its_status` (2026-11-15); edgar's (8 MiB) by no test<br>408 transient: **enforced** in both providers' `classify`; `cmd_robots::verdict_of` keeps a robots 408 permanent (§10, decisions 6 and 7: `a_robots_408_stays_a_permanent_failure_with_no_robots_row`)<br>open-web bot protection permanent: **enforced** in `classify`; a challenge page served with 200 is `Usable` and becomes `NoArticleBody` at extraction, untested<br>unguarded: no bypass of the limiter only because there is one `exchange.send` call site, not a test; a second send path would be uncovered<br>unguarded: the cap is uniform per request<br>unguarded: a retry re-asks the identical `Request`; a vendor needing a different url or header is not helped |
| G1 | **a workspace member inherits the lint floor** | `repo_invariants::every_workspace_member_inherits_the_lint_floor` over the `cargo metadata` member set (not `workspace.members`: a `path`-dependency crate is a member, measured 2026-11-12); rejects `lints` inside `[package]`<br>floor contents: `repo_invariants::the_lint_floor_holds_what_the_committed_snapshot_holds` against `repo-invariants/lint-floor.snapshot`; a `warn`/`allow` entry is refused<br>`members` globs `providers/*`, `services/*` | **enforced** over 8 members (2026-11-15)<br>unguarded: a new top-level crate reached by no `members` entry and no `path` dependency is compiled by nothing and invisible to CI (reproduced 2026-11-12)<br>glob cost: a directory without `Cargo.toml` under either parent fails every cargo command (cargo 1.99.1, 2026-11-13) |
| G2 | **no file outside a `-text` or `binary` path of `.gitattributes` holds `\r`, so no file whose bytes feed a version digest does** | `repo_invariants::line_ends::tests::no_hashed_input_in_this_tree_holds_a_carriage_return`: every non-hidden file outside `target/`; the exemptions are read from `.gitattributes`, the last matching `text` line deciding, as in git; a pattern with syntax the check does not read fails by name: `…::a_pattern_this_check_cannot_read_fails_by_name`<br>`HASHED_INPUTS` names the digest each path feeds, for the message only: `providers/open-web-feed/src/`, every file (`extract_version`, through `include_str!`); `providers/open-web-feed/rules/open-web-feed.rules.json` (`extract_rules_version`, `extract_version`); `providers/open-web-feed/hosts/` (`hosts_version`); `providers/edgar-press/rules/edgar-press.rules.json` (edgar's `extract_rules_version`); `news-provider/policy/observation-lag.json` (the `LagPolicy` version)<br>a listed path absent or an empty directory fails: `…::a_listed_path_that_is_absent_or_an_empty_directory_fails_rather_than_passes`<br>`.gitattributes`: `* text=auto eol=lf`; `-text` on `providers/*/tests/fixtures/**`, `services/*/tests/fixtures/**` and `probes/observation_lag/quarter_index.*.json`; `binary` on `*.parquet` | **enforced on the working-tree bytes of the whole tree** (mutations 2026-11-16, one at a time, each file converted to CRLF: the hosts file, `src/depth.rs`, both rules files, the lag policy; each red; the golden extraction test is red too for `src/` and the open-web rules file, so G2 is the only red test for the hosts file, the edgar rules file and the lag policy)<br>a digested file left out of `HASHED_INPUTS` is still checked (mutation 2026-11-16, the hosts entry removed and the hosts file converted to CRLF: all green on the fixed-list gate of `e3fab92`, red now with `feeds no listed digest`): `…::a_crlf_file_outside_every_listed_input_fails`<br>an exemption line is guarded where it covers a file holding `\r` (mutations 2026-11-16, each line deleted alone, red over): `providers/*/tests/fixtures/**`, `gnw/robots_rss_globenewswire_com.txt` (125 `\r`); `services/*/tests/fixtures/**`, `robots/synthetic_cr_line_ends.txt` (7); `*.parquet`, 1 parity parquet (2): `…::a_crlf_fixture_fails_once_its_raw_line_is_gone`<br>measured 2026-11-16 (research run, scratch clone of main `e2ed6d4` with `core.autocrlf=true` and no attributes): 201 files checked out CRLF and 6 tests red, among them the golden extraction test and both robots pins; rustc 1.99.1 compiles a CRLF `.rs` file as it compiles LF, while `include_str!` keeps the `\r`<br>`git add --renormalize .` under the committed `.gitattributes`, `*.parquet binary` included, changes 0 tracked files (2026-11-16); with `* text=auto eol=lf` alone it changes 1, `gnw/robots_rss_globenewswire_com.txt`, whose CRLF bytes are the GNW pin `eb8bb698…`<br>unguarded: the `quarter_index.*.json` `-text` line; the capture holds no `\r`, so deleting the line turns nothing red (2026-11-16)<br>unguarded on CI: a CRLF edit is committed as LF, so the gate is red only in the working tree holding the edit; a lone `\r` is committed as is, git then treats the file as `-text`, and the gate is red on CI too (2026-11-16, `git add` then `git ls-files --eol`: `i/lf w/crlf` for the CRLF lag policy, `i/-text w/-text` for the hosts file with one lone `\r`)<br>a CRLF copy of a tracked file is not listed by `git status`, and `git checkout -- <file>` leaves its bytes; deleting the file first restores LF (2026-11-16, the lag policy)<br>cost: untracked files are scanned too, so an untracked CRLF file outside `target/` and hidden directories turns the test red locally; the main checkout held 0 such files (2026-11-16, `find` plus `grep -c $'\r'`: 3 files hold `\r`, all 3 exempt)<br>unguarded: the deploy host; a file copied there by scp or rsync passes neither git nor this test, and how the VPS receives files is unknown (§10)<br>no edgar roster is committed: the roster is a run-time argument, and its test fixtures are `-text` |
| U1 | **the panel names the roster it was built from** | collection side: `news_provider::Request.universe_version` inside `filter_params`; `attempts.universe_version` is `NOT NULL`; `news_provider::request::tests::two_runs_whose_rosters_differ_have_two_filter_params_digests`, `newsvault::store::tests::the_opened_row_carries_the_roster_digest_the_request_was_built_from`<br>absent, unparseable or empty roster: `edgar-press::Refusal` (§9)<br>panel side: nothing | **no enforcement point yet**: the panel side has none; the collection side has only a producer, a column and the two tests in the enforced-by cell<br>unguarded: `panel-contract schema` lists 13 manifest fields, none a roster or `relation_map_version` (measured 2026-11-12), so §2 names five `panel_id` inputs where the gate requires three<br>making them required rewrites seven fixtures and R8: a decision, not an omission |
| E1 | an error code is never renamed, and no code reaches stderr from outside the table | §5.1 stderr codes: nothing; design: workspace member `error-codes/` with `const CODES`, `codes.snapshot`, a snapshot test, and a constructor taking `&'static Code`<br>`panel-contract`'s 15 stdout `Finding` codes: renaming any one turns a named test red (`cargo test -p panel-contract --lib`, measured 2026-11-12) | **no enforcement point yet** for §5.1's codes<br>renaming the 15 `Finding` codes: **enforced**<br>unguarded: a new code needs no table entry and no review<br>unguarded: repurposing a code is not checkable |
| H1 | every difference between `derive`'s picks and the predecessor's has a named cause (memo §5.1, build step 6) | nothing: no cause list, diff artefact or test exists | **no enforcement point yet** |
| M1 | a store at the previous `user_version` opens, migrates and takes a row of every precision the current one stores (§11.1) | `Committer::open` → `migrate`: `newsvault::store::tests::a_store_at_the_previous_user_version_opens_migrates_and_takes_a_minute_row` (from 0); `a_store_at_user_version_one_opens_migrates_to_two_and_takes_a_robots_row` (from 1); carried from 1: `a_v1_store_keeps_every_v1_column_of_every_row` (one row in each of the five v1 tables, every v1 column compared by `quote()`); shape at 1: `a_v1_store_whose_columns_differ_is_refused_and_left_at_user_version_one` (`attempts.hosts_version INTEGER`, `published` dropped; both `Committer::open` and `refuse_an_unreadable_version`); `a_store_at_user_version_two_opens_migrates_to_three_and_takes_a_verdict` (from 2: a `Document` attempt's `verdict` and `story_id`, and a `held` count); `a_store_at_user_version_three_opens_migrates_to_four_and_takes_a_listing_count` (from 3); `a_store_at_user_version_four_opens_migrates_to_five_and_takes_an_extract_version` (from 4, and the new column's `table_info` shape: `TEXT`, nullable, no default, not a key); `a_store_at_user_version_five_opens_migrates_to_six_and_takes_a_payload_row` (from 5, and the new table's `table_info` shape); carried from 5: `a_v5_store_keeps_every_v5_column_of_every_row` (the v4 fixture plus `extract_version` set, every v5 column compared by `quote()`, no `payloads` row); shape at 5: `a_v5_store_whose_columns_differ_is_refused_and_left_at_user_version_five` (`attempts.extract_version BLOB`, `attempts.listing_unusable` dropped; both entry points, exact `ShapeDiffers`); carried from 4: `a_v4_store_keeps_every_v4_column_of_every_row` (the v3 fixture plus `listing_unusable` set, every v4 column compared by `quote()`, the new `extract_version` NULL); shape at 4: `a_v4_store_whose_columns_differ_is_refused_and_left_at_user_version_four` (`attempts.listing_unusable TEXT`, `attempts.held` dropped; both entry points, exact `ShapeDiffers`); carried from 3: `a_v3_store_keeps_every_v3_column_of_every_row` (the v2 fixture plus `verdict`, `story_id` and `held` set, every v3 column compared by `quote()`, the new `listing_unusable` NULL); shape at 3: `a_v3_store_whose_columns_differ_is_refused_and_left_at_user_version_three` (`attempts.held TEXT`, `attempts.story_id` dropped; both entry points, exact `ShapeDiffers`); carried from 2: `a_v2_store_keeps_every_v2_column_of_every_row` (the v1 fixture plus one `robots` row, every v2 column compared by `quote()`, the new `verdict`, `story_id` and `held` NULL); shape at 2: `a_v2_store_whose_columns_differ_is_refused_and_left_at_user_version_two` (`robots.host INTEGER`, `robots` dropped; both entry points, each matched to the exact `ShapeDiffers { found, columns }`, as at 1); pre-lease accept: the v0, v1, v2, v3, v4 and v5 carried tests assert `refuse_an_unreadable_version` is `Ok` before `open` (mutations 2026-11-15, an `Ok(n) => Err(SchemaVersion)` arm added for n = 0, 1, 2 in turn, and for 3 that day: each red, the carried test at that version and, at 1, 2 and 3, the shape test too); carried: `a_migrated_v0_store_keeps_every_v0_column_of_every_row_and_both_indexes` (v0 fixture: one instant, one day+tz and one refused row, `seq` 7..9, every nullable v0 column set, `valid_disputed` 1; all 15 v0 columns compared by `quote()` before and after, and the `items_by_story`/`items_by_doc` index names) | **enforced** from user_version 0 to 1: mutations 2026-11-15, the `items` rebuild replaced by the three `ADD COLUMN`s / every migration step skipped: red<br>**enforced** from 1 to 2 since 2026-11-15: mutations that day, `MIGRATIONS[1]` emptied: red; the shape check skipped at 1: red; a table missing at 1 skipped as at 0: red; `DELETE FROM published` added to `MIGRATIONS[1]`: the carried test red<br>**enforced** from 2 to 3 since 2026-11-15: mutations that day, `MIGRATIONS[2]` emptied: red; its `story_id` line alone removed: red; its `held` line alone removed: red; the shape check skipped at 2: red; `UPDATE attempts SET outcome = NULL` added to `MIGRATIONS[2]`: the carried test red. A copy of a test-made v2 store (9 attempts, 5 blobs, 4 items, 4 reservations, 1 robots row; no production store exists, R11 search 2026-11-15) migrated by an `edgar_press` collect over 2026-11-11, a day the fixture calendar closes, `planned_requests` 0 in the dry run first: user_version 3, every v2 column of all 23 rows identical by `quote()`, `verdict` and `story_id` NULL on all 9 attempts (2026-11-15, by hand, not committed; repeated that day on a fresh copy once `story_id` joined `MIGRATIONS[2]`, same result, and again once `held` joined it, over 2026-11-14: `verdict`, `story_id` and `held` NULL on all 9, every v2 column of the six tables identical, `ledger` reading `held` 0 for the copy's one open-web day)<br>**enforced** from 3 to 4 since 2026-11-15: mutations that day, `MIGRATIONS[3]` emptied: red (re-measured with its `ALTER` line removed, `cargo test --locked --no-fail-fast` at d2f8a78, 2026-11-15: 143 other tests red, all in `newsvault`, 21 of them in `tests/ledger.rs`; at 7b3cba6, the same day: 418 passed, 145 red in all (144 at d2f8a78), the extra one `a_robots_408_stays_a_permanent_failure_with_no_robots_row`); the shape check skipped at 3: red; `UPDATE attempts SET verdict = NULL` added to `MIGRATIONS[3]`: the carried test red. A copy of a test-made v3 store (6 attempts, 5 blobs, 2 items, 2 reservations, 1 robots row: `a_second_live_poll_of_one_day_fetches_no_held_document` built at 63df8bd with its store kept) migrated by an `edgar_press` collect over 2026-11-11, `planned_requests` 0 in the dry run first, the dry run leaving user_version 3: user_version 4, every v3 column of all 16 rows identical by `quote()`, `listing_unusable` NULL on all 6 attempts (2026-11-15, by hand, not committed). Not an `open_web_wires` collect: it would plan a live GNW fetch<br>**enforced** from 4 to 5 since 2026-11-16: mutations that day, the `ALTER` line of `MIGRATIONS[4]` removed: red (the from-4 and carried-4 tests among the `newsvault` lib failures); the shape check skipped at 4 (`older < MIGRATIONS.len() - 1`): red, the shape-at-4 test alone; `BLOB` for `TEXT` in `MIGRATIONS[4]`: red, the from-4 test alone (green before its shape assert: the shape check compares a store with a replay of `MIGRATIONS`, so the newest step matches itself; review 2026-11-16). No store copy was migrated by hand for this step (2026-11-16)<br>**enforced** from 5 to 6 since 2026-11-16: mutations that day, `MIGRATIONS[5]` creating another table name: red (103 `newsvault` tests, the from-5 test among them); the shape check skipped at 5 in `refuse_an_unreadable_version` (`older < MIGRATIONS.len() - 1`): red, the shape-at-5 test alone; `TEXT` for `INTEGER` on `payloads.truncated`: red, the from-5 test among 11 (the shape check replays the newest step against itself, as at 4). No store copy was migrated by hand for this step (2026-11-16)<br>**enforced** from 6 to 7 since 2026-11-16: `a_store_at_user_version_six_opens_migrates_to_seven_and_takes_a_relisted_row`, `a_v6_store_keeps_every_v6_column_of_every_row` (one `payloads` row added to the v5 fixture), `a_v6_store_whose_columns_differ_is_refused_and_left_at_user_version_six` (`payloads.headers` dropped; `payloads` dropped, all 7 columns named); mutations that day: `MIGRATIONS[6]` creating another table name: red, the from-6 test, the `relisted` constraint test and `ledger.rs`'s relisted test among others; the shape check skipped at 6 (`older < MIGRATIONS.len() - 1`): red, the shape-at-6 test alone; `DELETE FROM payloads` added to `MIGRATIONS[6]`: red, the carried-6 test alone; `BLOB` for `TEXT` on `relisted.listed_raw`: red, the from-6 test alone (its shape assert). No store copy was migrated by hand for this step (2026-11-16)<br>**enforced** from 7 to 8 since 2026-11-16 (decision x): `a_store_at_user_version_seven_opens_migrates_to_eight_and_takes_a_compared_count` (and the new column's `table_info` shape: `INTEGER`, nullable, no default, not a key), `a_v7_store_keeps_every_v7_column_of_every_row` (one `relisted` row added to the v6 fixture; the new column NULL), `a_v7_store_whose_columns_differ_is_refused_and_left_at_user_version_seven` (`relisted.listed_raw` dropped; `relisted` dropped, all 5 columns named); mutations that day, each alone over `cargo test --locked --no-fail-fast -p newsvault`: the `ALTER` line of `MIGRATIONS[7]` replaced by `SELECT 1`: red (78 `newsvault` tests, the from-7 test among them); the shape check skipped at 7 (`older < MIGRATIONS.len() - 1`): red, the shape-at-7 test alone; `TEXT` for `INTEGER`: red, the from-7 test and 3 other `record_relisted` tests (a `TEXT` column hands the count back as text); `DELETE FROM relisted` added to `MIGRATIONS[7]`: red, the carried-7 test alone. No store copy was migrated by hand for this step (2026-11-16)<br>unguarded: `payloads`'s and `relisted`'s CHECK and FOREIGN KEY clauses are not compared, as for `robots` (below)<br>carried columns and indexes **enforced** since 2026-11-15: mutations that day, the copy reduced to 11 columns without `seq`, `doc_id`, `earliest_valid_at`, `body_blob` / `doc_id` and `earliest_valid_at` dropped / both `CREATE INDEX` lines dropped: each green before the carried test, red after<br>a v0 store written by an older binary: **enforced** for a missing column since 2026-11-15 (`StoreError::ShapeDiffers { found: 0, .. }`): `a_v0_store_whose_attempts_predate_matched_is_refused_and_left_at_user_version_zero` (fixture: `attempts` as written before 067a0bd, 2026-11-13, 22 columns, no `matched`); mutation that day, the shape check skipped: red. Before it, review 2026-11-15 measured a scratch store of that shape (1,006 attempt rows) migrate to user_version 1 without `matched`, then `ledger` exit 2 on `no such column: a.matched`. Refused, not repaired: only scratch stores have that shape (R11 search, 2026-11-15)<br>a column whose name matches and whose type, NOT NULL, default or key differs: **enforced** since 2026-11-15: `a_v0_store_whose_column_differs_in_type_not_null_default_or_key_is_refused` (four fixtures: `attempts.slots TEXT`, `run_id` without NOT NULL, `outcome DEFAULT 'x'`, `attempt_id UNIQUE` for PRIMARY KEY); mutations that day, each of the four `PRAGMA table_info` fields read as a constant: each red<br>unguarded: at user_version 2 and 3, `robots`'s CHECK and FOREIGN KEY clauses are not compared, nor at 3 `attempts.verdict`'s CHECK: `columns_of` reads `pragma_table_info` only (review 2026-11-15, read from the code, not run)<br>unguarded: on `attempts`, `blobs`, `seq_reservations` and `published`, the v0 tables `MIGRATIONS[0]` does not rebuild, table-level UNIQUE, CHECK and FOREIGN KEY clauses, AUTOINCREMENT, WITHOUT ROWID and indexes are not compared (review 2026-11-15: `attempts` with `UNIQUE(run_id)` added reached user_version 1 with the clause kept)<br>unguarded: a v0 migration step SQLite refuses leaves `LOCK` and a run envelope under `collect` (§3.1) |

---

## 9. Alternatives considered

| alternative | why not |
|---|---|
| eleven services, one per bounded context | each deployable is another deploy, log, contract version and place something goes quiet; the predecessor has a recorded case where a boundary between components *removed* a standing alert instead of raising one. The eleven contexts are drawn so the seams exist later. |
| Kafka for the event bus | solves throughput we do not have (a few hundred items a day is one event every few minutes) and adds infrastructure a one-person system must operate. The planned `commit.sqlite3` outbox (§6, no code yet) is durable by construction, because it commits in the archive's own transaction, and it is the replay log. |
| a Postgres outbox with `LISTEN`/`NOTIFY` | rule 2 (§6) holds only inside one engine, so an outbox next to `commit.sqlite3` is a dual write, and keeping it atomic means moving the whole archive. The Postgres that "already runs in production" is the predecessor's container: no host port, holds `auth_user`, no automated backup, allowed to stop at memo step 7. `NOTIFY` is not queued for a disconnected listener, so a consumer polls by cursor anyway. Rejected 2026-11-14. |
| Common Crawl's CC-NEWS as the archive | probed and rejected: 7 of our own 287 items for one test day, 2 of our 17 publishers crawled at all, zero tier-1 US financial outlets in 72,000 records, 4.19 TB for ten months, and no index, so no URL lookup and no aimed range request. |
| **collecting the news ourselves** (publisher RSS, EDGAR, or crawling the open web) instead of buying it | **Chosen by the owner 2026-11-14 for the forward feed** (§11); history is a one-time vendor purchase (`probes/vendor_probe/PREREGISTRATION.md`) or the wires' own sitemaps, chosen after their depth is measured (owner decision 2026-11-16, §10). Body-length evidence below. |
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
- Decision ids: `D1`, `D3`, `D5` are this document's, set out below; `S7-D1` to `S7-D11` are the owner's step-7 decisions of 2026-11-17 (§11.2, §11.5 step 7); `PG-D1` to `PG-D9` are the owner's decisions of 2026-11-17 after the PRN/GNW research ("tak, zgadzam sie z rekomendacjami"); `D-1` to `D-8` are the memo's §8 table and are written `memo D-n` here.
- Memo D-1 (which vendor for history) blocks only the one-time history purchase, memo §10 step 5 (backfill to 246 sessions); its state is memo §10 step 2.
- The source of the 12-month history, a vendor purchase or the wires' own sitemaps, is chosen only after the depth of those sitemaps is measured (owner decision 2026-11-16); unmeasured: how far back the PRN, GNW and Business Wire sitemaps go.
- How PRN and GNW are read so that no stretch between polls goes uncovered: decided 2026-11-17 by the owner, not built; the plan is §11.2's completeness table. Their 20-item RSS indexes left uncovered stretches at both periods (§11.2's period table, measured 2026-11-16). Still open: the GNW walk's own follow-up cap (S7-D6, §11.5 step 7), whether a GNW id answered 410 is asked again under another slug, and when a PRN monthly archive is published and whether it is complete.
  - Also open: whether a GNW id stays non-public for long after a higher id is listed (an embargo); probe10 bounds it at 20.6%, not at zero (§11.6).
  - Also open: the PRN October 2026 archive was 2.27 MB gzipped (research 2026-11-17), above open-web's `BODY_LIMIT` of 2 MiB (§11.1), so today it would be a `PermanentFailure` stored cut; and whether its `<lastmod>` is the publication time was not checked.
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
- **Per-host queues, owner decision 1 of 2026-11-17** ("tak, zgadzam sie z rekomendacjami"; §8 row R19). Option (a): open-web's `RequestBudget::min_interval` is `None`, so its per-IP pace is gone and each admitted host is its own queue.
  - Unchanged: the 10 s `per_host` floor, the Crawl-delay refusal (owner decision 1, 2026-11-16), `Retry-After` holds and 5xx doubling, each per host as before.
  - Unchanged: edgar's 100 ms per-IP pace, because SEC counts its limit per IP.
  - Own queue means own slots, not concurrency: a host's next slot depends only on `pace/<host>.per-host`, its own hold and its own doubling. One process still sends one request at a time, so a wait on host A still delays, in wall time, every request behind it; host B's next request owes only B's own floor (`a_hold_on_one_host_leaves_another_host_owing_only_its_own_floor`). The stop deadline (§11.2) is per run.
  - The 50% busy bound (§11.1 Pace) is read per host; the run's wall time is set by the send order (next bullet).
  - PRN and CNW, owner decision of 2026-11-17 ("zgadzam sie z Twoja rekomendacja"): each keeps its own per-host queue; no per-vendor bucket. Both are Cision and cross-list releases (§11.5 step 6), so our combined rate to the two can be 2 requests per 10 s, against at most 1 under the shared queue. `dig +short` 2026-11-17: PRN (`www.prnewswire.com`) 104.18.36.40, 172.64.151.216; CNW (`www.newswire.ca`) 104.16.44.11, 104.16.45.11; no shared limit is known, and none is ruled out.
- **Round-robin by host, owner decision of 2026-11-17** ("zgadzam sie z Twoja rekomendacja", after owner decision 1; §8 rows R19, R21). `collect` keeps one FIFO lane per url host (`news_provider::collect::HostQueue`) and serves the lanes in turn, one request each, so one host's wait also runs down the other hosts' floors.
  - Unit: the url host by `budget::host_of`, the key `Wire` paces by; a url it cannot key goes to one extra lane, and `Wire` refuses or fails it as before.
  - Tie-break: lanes in the order their host first entered the queue; a turn starts at the lane after the last one served. The plan's order decides it, so the order, and every attempt's ordinal and `attempt_id`, is a function of the plan and the answers (`a_replayed_five_host_run_gives_every_attempt_the_same_id_and_url`).
  - Follow-ups and retries go to the back of their own host's lane, so a follow-up still leaves after its index and a retry still waits its host's pace (§8 row R21).
  - One host is one lane (a lane per url instead: red, 8 tests, both `wire` tests of §8 row R19 among them, mutation 2026-11-17), so one host is a plain FIFO (`one_host_is_one_lane_and_keeps_the_fifo_order`). EDGAR's order (`www.sec.gov` only) is unchanged (`an_edgar_run_on_its_one_host_keeps_the_fifo_order_retries_last`, which checks the order only: one-request lanes served in turn give the same FIFO). `--dry-run` prints counts and no order, so its output is unchanged.
  - Still one request at a time: concurrency was not chosen; between SourceIds it stays §11.3's target.

    | send order, 10 s floor, no request time | round-robin by host | block order (before) | shared queue | construction |
    |---|---|---|---|---|
    | three loopback hosts, 3 indexes, 2 pages each, A's first page answered 408 and retried | 30 s | 40 s | 90 s | round-robin: `round_robin_by_host_overlaps_the_host_floors_through_a_real_wire`, `WaitedClock` waits summed, 2026-11-17; block order: the same test with one lane for every host (mutation 2026-11-17); shared: arithmetic, 9 waits of 10 s |
    | two loopback hosts, 8 requests | 30 s | 50 s | 70 s | probe test 2026-11-17, not kept (PR #38 review) |
    | two loopback hosts: A's index and 1 page answered 408 three times; B's index and 3 pages | 30 s | 40 s | not computed | probe test over `driven_through_a_wire` 2026-11-17, not kept; block order: one lane for every host |
    | two loopback hosts: A's index and 1 page answered 503 `Retry-After: 120`, then 200; B's index and 4 pages | 150 s | 130 s | not computed | round-robin: `a_hold_on_one_host_delays_the_other_hosts_turns_under_round_robin`, waits 0, 0, 10, 0, 120, 0, 10, 10 s, 2026-11-17; block order: the same test with one lane for every host, waits 0, 0, 10, 0, 10, 10, 10, 90 s (mutation 2026-11-17); B's last 3 pages leave about 120 s later |
    | refetch worst-case poll: 4 indexes + 20 GNW + 20 PRN + 100 CNW + 20 Fed follow-ups | 1,000 s | 1,570 s | 1,630 s | arithmetic 2026-11-17, not measured: round-robin is CNW's own 100 waits; block order is GNW's 20 waits, then PRN's 19, CNW's 99 and the Fed's 19; 36.3% shorter than block order (27.5% with three hosts, 1,000 s against 1,380 s) |

  - The run's busy time, summed over hosts, does not depend on the send order: the refetch worst case is 6,200 + 6,200 + 31,000 + 6,200 (the Fed) = 49,600 s a day; its wall time is about the busiest host's waits, 31 × 1,000 s = 31,000 s, 35.88% of 86,400 s, unchanged by the Fed (arithmetic 2026-11-17, request times not counted).
  - Deferred by the owner until the first live data (2026-11-17 ~16:10Z, "zgoda" to the step-7 critique, §11.5 step 7), earliest-due-first: picks the host whose slot is due soonest, so it reads each host's pace file and the clock before every pick, a second reader of the pace beside `Wire::send`; of the table above, only the `Retry-After` row could be shorter under it. That is its one gain, not built: it would serve other hosts while one host is under a hold, which round-robin waits out on that host's turn (§8 row R19, hole 12).
  - Not built, open (owner): retries in a lane of their own, served after every first attempt. Measured 2026-11-17 by the mutation that does it inside `HostQueue`, on the table's rows: 30 s on the three-host row (as round-robin), 40 s on the 408-three-times row (as block order), 130 s on the `Retry-After` row (as block order). It would change §8 row R21's "back of its host's lane" and its tests.
  - Rejected, lanes sorted by host name: also deterministic, but the order would no longer follow the hosts file's order (§11.2: GNW, then PRN, CNW and the Fed).
  - Rejected, concurrent sends: not chosen by the owner (2026-11-17).
  - **The one place to change it** is `HostQueue` in `news-provider/src/collect.rs`; keying every request to one lane restores the block order, and turns `the_queue_serves_three_hosts_round_robin_in_the_order_each_host_first_appeared`, `a_retry_goes_to_the_back_of_its_host_lane_and_stops_at_the_cap`, `a_retry_on_a_short_lane_is_served_in_its_host_turn_while_a_longer_lane_drains`, `round_robin_by_host_overlaps_the_host_floors_through_a_real_wire`, `a_hold_on_one_host_delays_the_other_hosts_turns_under_round_robin`, `a_five_host_run_asks_its_hosts_round_robin`, `a_retried_release_waits_behind_the_other_hosts_and_stops_at_the_cap` and `a_retried_index_keeps_its_host_turn_while_the_other_hosts_releases_are_served` red (mutation 2026-11-17). The re-queue order is `HostQueue::requeue`, which `collect` and the order tests share.
  - `newsvault robots refresh` builds the same profile (`cmd_robots::wire_for`), so its robots.txt fetches to different hosts now leave back to back; same-host spacing is unchanged.
  - Rejected, the shared queue it replaces: the admitted hosts summed in one 10 s queue charged every host for the other hosts' requests; read per host, the refetch worst case put 43,400 s a day, 50.23% of 86,400 s, on each host's bucket with GNW, PRN and CNW, above the bound, and 49,600 s, 57.41%, with the Fed (§11.1's cost-bound construction, every host in one column).
  - Rejected, (b) the per-IP bucket keyed by host: a `<host>.per-ip` file holding the same 10 s as `<host>.per-host`, reserved after that host's own wait, can add at most the time between the previous call's two reservations (not measured), and is a second file per host that a slow-down does not double.
  - Rejected, (c) one SourceId per host: §11.2 refuses a second SourceId per invocation until the run envelope lists `hosts_version` values, so it needs a run envelope, ledger rows and a hosts file per source for the same pacing.
  - **The one place to change it** is `min_interval` in `OpenWebFeed::transport` (`providers/open-web-feed/src/lib.rs`): `Some(INTERVAL)` restores the shared queue; the change also turns `every_golden_page_extracts_to_its_committed_body_under_the_committed_version` red until `extract_version` in `providers/open-web-feed/tests/fixtures/golden_bodies.json` is refreshed, because that version hashes the crate source (2026-11-17).
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

**The archive's copies: the VPS store is the first copy, with a backup pulled to the Mac and the provider's own backup behind it (N1, decided 2026-11-12 by the owner; amended 2026-11-17, S7-D10 of §11.5 step 7, owner: "zgoda").**

- S7-D10 runs `collect` on the VPS (§11.5 step 7), so the store it writes there is the first copy, and the copy pulled from it to the Mac is the second.
- Neither exists yet: no vault on the VPS on 2026-11-15, and no newsvault process, timer or cron entry there (§8 row R11); no pull to the Mac is built.
- Open, not blocking the system's shape: nobody has restored a backup and nobody has timed a restore; one timed drill, recorded once, closes both (a backup nobody has restored is not yet a backup).
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

- The VPS store and the provider's backup sit in one provider and one account: a disk, a kernel or a bad deployment is covered; a billing, suspension or credential loss is covered only by the Mac copy, which is not built. A copy at a second provider was the alternative and was not taken.

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
| a 429, a 5xx and a transport error are transient, except a body over `BODY_LIMIT` (2 MiB), which is permanent; every other non-200 status is permanent (the owner decided only bot protection, permanent; a 408 is owner decision 7, below) | `OpenWebFeed::classify`, `providers/open-web-feed/src/lib.rs` |
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
| a robots attempt is `source` `robots`, `target_window` the lowercased host, phase `Index`, `live_ingest`, `universe_version` the digest of zero bytes, `hosts_version` the hosts file's, through the open-web `TransportProfile` and `open_web_wires` pace files | the robots bullet above | `cmd_robots::request_for`; `cmd_robots::wire_for` |
| a robots fetch's answer is the `robots` table (§3.2); its bytes are linked by `payloads` from user_version 6 (§3.9), by `robots.blob` before | an `attempts.raw_blob` column, which moves `blobs_indexed` for every run and the ledger golden file; P10 (2026-11-16) took that cost with a table of its own | `MIGRATIONS[1]`, `MIGRATIONS[5]`, `Committer::record_robots` |
| the pin is one hosts-row field `robots`: 64 lowercase hex of the body as received after content decoding, or `absent`; required on admitted rows, optional on candidates; schema `open-web-feed.hosts/2` | a state plus a digest, which can disagree; `null`, which cannot tell "forgotten" from "404" | `RobotsPin::read` and `read_hosts`, `providers/open-web-feed/src/hosts.rs` |
| 200 present (ok); 404 absent (empty), no digest; any other 4xx (403, 408, 410) and any 3xx, not followed (§11.2), failed permanent; 429, a 5xx and a transport error failed transient; no robots row for a failure; one attempt per row, no retry, no startup sweep | RFC 9309's "4xx means allow all", which admits `www.globenewswire.com`, whose robots.txt answered an Akamai 403 (§11.2) | `cmd_robots::verdict_of` |
| a refresh fetches every row, admitted and candidate, writes attempt rows, `robots` rows, blobs and an envelope, and never the hosts file; a pin that differs exits 1 with both digests | rewriting the hosts file, which re-admits a host whose robots changed without a person looking | `cmd_robots::run`, `exit_of` |
| CLI `newsvault robots refresh --source open_web_wires --hosts --rules [--store] [--dry-run] [--non-interactive] [--format]`, run id `robots-<YYYYmmddTHHMMSSZ>` from the clock, no `--at` | | `RobotsAction` in `main.rs`; `cmd_robots::run` |
| robots.txt is read by our own RFC 9309 subset: the `*` groups only, merged, the largest Crawl-delay; a non-numeric `*` Crawl-delay refused (§11.1); the body read as bytes, its rule bytes never decoded (row "key and line spellings", (e)) | a crate, which adds a dependency for four rules; a group naming our token, which code cannot know (the token is the contact variable's) | `newsvault::robots::Robots::parse` |
| grouping, owner decision 2 (B2, 2026-11-16): only `Allow`/`Disallow` ends a `user-agent` run; a Crawl-delay or unknown line between two `user-agent` lines leaves them one group (both gated, §11.5 step 3 mutations), so `User-agent: *` / `Crawl-delay: 30` / `User-agent: bingbot` / `Disallow: /prywatne/` gives `*` both the 30 s and the `Disallow`. RFC 9309 §2.2's grammar (`group = startgroupline *(startgroupline / emptyline) *(rule / emptyline)`, `rule` only `allow`/`disallow`) and §2.2.4 ("Parsing of other records MUST NOT interfere with the parsing of explicitly defined records"; "a Sitemaps record MUST NOT terminate a group"), read 2026-11-16 at rfc-editor.org; Google's `robots.cc` (github.com/google/robotstxt, master, read 2026-11-16) sets `seen_separator_` only in `HandleAllow`/`HandleDisallow` and has no Crawl-delay key | ending the run at a Crawl-delay (built 2026-11-15): a named group after `*` with only a delay between them was read as its own group, so its rules were not `*`'s; E, the stricter of both readings: two parsers to keep in step for a case no probed host has | `Robots::parse`, `Group` in `robots.rs` |
| key and line spellings, recommendation #1 (owner, 2026-11-16: "zgadzam sie na warunek A, na rekomendacje #1, #2 i #3"; rule "stosujmy standard google + crawl-delay"): read as Google's `robots.cc` (github.com/google/robotstxt at 50a0dac) reads them, reimplemented, not copied (Apache-2.0); a key matches by case-insensitive prefix: `user-agent`, `useragent`, `user agent` (lines 219-224), `allow`, no typo (230), `disallow`, `dissallow`, `dissalow`, `disalow`, `diasllow`, `disallaw` (235-243), so `Disallowed:` is a `Disallow`; a line with no colon splits at its first run of spaces or tabs when exactly two words remain (323-340), so `Disallow /x` is read and `User-agent * bot` is not; `*` followed by a space, tab or vertical tab and more names every agent (625); absl's ASCII whitespace, vertical tab included, is stripped and no other, so `*` plus U+00A0 names no agent (296); since 2026-11-17 (owner: "ok, zgadzam sie z rekomendacjami", after being shown that robots.cc is Google's open-sourced production parser) also: (a) an `Allow` that does not match, whose text from its last `/` starts with `/index.htm`, allows `<text up to that slash>/$` at that pattern's length (655-668); it is a prefix test, so `index.htmlx` counts, and it works through `*`; (b) a pattern's bytes at or above 0x80 become `%XX` and its `%xx` gets uppercase hex (167-213; robots.cc escapes the value of every key but user-agent and sitemap, 284-291, and ours matches only `Allow`/`Disallow` values); the escaped length is the priority (`MatchAllow`, `MatchDisallow`, 690-698), a tie goes to `Allow`; a path is matched as given, never escaped or normalised, and holds no `#`, which robots.cc cuts from a url (146-148) and the loaders refuse (§11.1); (c) a line keeps its first 16,663 bytes, counted after the BOM and before its line end, and is still read (`kMaxLineLen` 2083 × 8 = 16,664 with the last byte for the terminator, 418-447); (d) a line ends at its first NUL byte, whether the NUL sits in a key, before the colon or in a value, because each line is read as a C string (`strchr`, `StripAsciiWhitespace`, 295-305); (e) since 2026-11-17 (owner: "w punkcie 3 tak", answering "match Google for robots.txt bodies that are not valid UTF-8"), a body is read as bytes and its rule bytes are never decoded: a pattern byte at or above 0x80 is escaped as in (b); a key or user-agent byte is compared as it is, so `*` followed by 0xA0 names no agent; each leading byte that continues the BOM `EF BB BF` is skipped, so `EF BB` alone is skipped too (434-441); the HTML check (3b) reads bytes too (row "HTML"); a non-ASCII Crawl-delay value is no number (decision 1), its error text holding the value's bytes escaped (`5\xe4`); parity, 2026-11-17: 96 rows (body × path; the decision-4 cases, pctsem p1-p14, i1-i11, l1-l10, n1-n11) through robots.cc 50a0dac compiled with clang++ and through `Robots::parse` at 6319f1e and at c355655: 35 rows differed before, 0 after, besides 6 rows of 3 refused bodies; a random sweep of 3,000 bodies × 8 paths (seed 17, 24,000 rows) differs in 0 rows outside refusals after and in 146 before; parity for (e), 2026-11-17, robots.cc 50a0dac compiled with clang++ against `Robots::parse` at main 21989d7 and at 3e6d723: 908 cases, 14,688 rows (body × path), differ in 58 rows before (56 rows of 8 non-UTF-8 bodies refused) and in 2 after, both a path holding `#`, which the loaders refuse (§11.1); a random sweep, 3,000 bodies × about 12 paths, keys, user-agents, values and a leading BOM drawn with non-UTF-8 bytes: seed 17, 2,757 bodies not UTF-8, 36,152 rows, 33,241 refused before and 372 after (HTML or Crawl-delay), 0 other rows differ; seed 18, 2,743 not UTF-8, 36,158 rows, 362 refused after, 0 differ; differences kept: Crawl-delay is read (decision 1), its line whole (the Crawl-delay row), an HTML body is refused (3b) | for (b), allowing a path only when both our raw reading and Google's escaped one allow it, and for (c) and (d), refusing the body (recommended 2026-11-17): rejected by the owner for Google's reading; for (e), refusing the body, `NotUtf8` and `refused_not_utf8` (built 2026-11-15): `Disallow: /\xe4` gave no answer where Google disallows `/%E4`; keeping (a) to (d) as differences (built 2026-11-16): `Disallow: /ä` allowed `/%C3%A4` and `Disallow: /x\0y` allowed `/x` where Google disallows both | an exact key (built 2026-11-15): `User agent: *` or `Disalow: /x` gave no group or no rule where Google obeys them | `USER_AGENT_KEYS`, `DISALLOW_KEYS`, `Key::of`, `directive`, `names_every_agent` in `robots.rs`; `every_key_spelling_google_accepts_is_read`, `a_key_spelling_google_does_not_accept_is_not_read`, `a_key_and_value_split_by_whitespace_alone_is_read`, `only_ascii_whitespace_is_stripped`; (a) to (d): `index_directory`, `escape_pattern`, `PathRule::priority`, `read_line`, `MAX_LINE_BYTES`; `an_index_page_allow_also_allows_its_directory_as_google_reads_it`, `a_pattern_is_escaped_as_googles_parser_escapes_it`, `a_long_line_is_cut_where_googles_parser_cuts_it`, `a_line_ends_at_its_first_nul_as_googles_parser_reads_it`, `the_bodies_google_reads_and_ours_refuses_are_the_named_exceptions`; 96 verdicts written as literals from the compiled parser: the 90 parity rows outside the refused bodies and 6 added by the 2026-11-17 review; (e): `skip_byte_order_mark`, `starts_with_markup`; `a_body_that_is_not_utf8_is_read_as_bytes_as_googles_parser_reads_it`, `an_html_body_that_is_not_utf8_or_starts_with_unicode_space_is_refused`, 34 verdicts written as literals from the compiled parser, 2026-11-17 |
| the Crawl-delay key: `robots.cc` names none (lines 217-252), and Google's spec page says "other fields such as `crawl-delay` aren't supported" (read 2026-11-16); ours matches `crawl-delay` whole, case aside; the colon-less split is the line's, so `Crawl-delay 20` is read; its line is read whole, never cut at a NUL or at 16,663 bytes as a rule line is since 2026-11-17, because decision 1 keeps our reading (review 2026-11-17: under the cut, `Crawl-delay: 5\0x` read as 5 s, where 6319f1e refused it); unguarded: a misspelt key (`crawldelay`, `Crawl-delays`) is an unknown line, so the host is paced at the 10 s floor | prefix or typo forms: no standard names them, so the list would be our invention | `CRAWL_DELAY_KEY`, `Key::of`, `crawl_delay_value`; `only_the_crawl_delay_key_itself_is_read`, `a_crawl_delay_line_is_read_whole_and_not_cut_as_googles_parser_cuts_a_rule_line` (its 4 verdicts also those of 6319f1e, run 2026-11-17) |
| empty body, owner decision 3a (2026-11-16): a 200 that is empty or only comments and blank lines is no rule, every path allowed, the 10 s floor | refusing it as no `user-agent` (built 2026-11-15): RFC 9309 has no group to require, and the host said nothing | `Reading::NoRulesEmpty` |
| HTML, owner decision 3b: a 200 is refused, `RobotsError::HtmlPage`, when its first byte that is not ASCII whitespace, NUL or at or above 0x80 is `<` (since 2026-11-17: the skip steps over any BOM, UTF-8 BOM or UTF-16 `FF FE`/`FE FF`, UTF-8 or Latin-1 whitespace and a UTF-16 unit's zero half, so a UTF-16 or Latin-1 soft-404 page is refused too), kept by the owner after recommendation #2; Google differs: "Google will try to parse the content and extract rules, and ignore everything else" (developers.google.com/crawling/docs/robots-txt/robots-txt-spec, read 2026-11-16), so a page holding no robots key is no rules to Google | reading an HTML page as no rules: a soft-404 page at the robots path would admit everything; the body's leading valid UTF-8 only (built 2026-11-17 in 3e6d723): `FF FE` + UTF-16LE, `FE FF` + UTF-16BE, UTF-16BE with no BOM and `A0` + a UTF-8 page were each read as no rules, every path allowed (review, 2026-11-17) | `Robots::parse`, `starts_with_markup`; `an_html_body_is_refused`, `an_html_body_that_is_not_utf8_or_starts_with_unicode_space_is_refused` |
| text with no `user-agent` line, recommendation #2 (owner, 2026-11-16: "zgadzam sie na warunek A, na rekomendacje #1, #2 i #3"): no group, so no rule, every path allowed, the 10 s floor, verdict `no_rules_no_group`; a `Sitemap:`-only body, orphan lines alone, any other text; RFC 9309 §2.2.1: "or no groups are present at all, no rules apply" (read 2026-11-16 at rfc-editor.org); Google's `robots.cc` (github.com/google/robotstxt at 50a0dac) drops every `Allow`/`Disallow` while no agent was seen | refusing it, `NoUserAgentLine` and `refused_no_user_agent` (built 2026-11-16): the host got no answer for a body RFC 9309 and Google both read as allowing everything | `Reading::NoRulesNoGroup`; `text_with_no_user_agent_line_reads_as_no_rules` |
| orphan lines, owner decision 2026-11-16 ("stosujmy standard google + crawl-delay", Google's standard; it replaces decision 3c): every `Allow`, `Disallow` and Crawl-delay before the first `user-agent` line is ignored, and the verdict is `orphan_rules_ignored`; an orphan line with an empty value carries no rule and leaves the verdict `rules`; RFC 9309 §2.2.2: "SHOULD ignore 'disallow' and 'allow' rules that are not in any group"; Google's `robots.cc` (github.com/google/robotstxt at 50a0dac, near lines 640 and 673, as shown to the owner 2026-11-16): `HandleAllow`/`HandleDisallow` return while `!seen_any_agent()`, and Crawl-delay is an unknown key; a Crawl-delay inside a `*` group is read as before (decision 1) | decision 3c, an orphan `Disallow` and Crawl-delay applied to `*` (built 2026-11-16): rejected by the owner to follow Google's standard; an orphan `Allow` applied to `*` too (built 2026-11-16): `Allow: /news/` before `User-agent: *` / `Disallow: /` allowed `/news/x` at 79a79b8, and with `Crawl-delay: 5` before the `Allow` at 41fcbf9; refusing the body: a host gets no answer for lines both readings ignore | `Reading::OrphanRulesIgnored`, `Robots::parse` |
| `robots refresh` prints each answered row's `parse_verdict` (owner decision 3d): `rules`, `no_rules_empty`, `no_rules_no_group`, `orphan_rules_ignored`, `no_rules_absent` (a 404), `crawl_delay_over_interval`, `refused_html`, `refused_crawl_delay`; precedence: a `refused_*` verdict, then `crawl_delay_over_interval`, then the reading (`rules`, `no_rules_empty`, `no_rules_no_group`, `orphan_rules_ignored`), so orphan lines before a `*` Crawl-delay above the interval give `crawl_delay_over_interval`; a Crawl-delay equal to the interval is not over it; `null` for a failed fetch and a dry run; the JSON row and the human line both; each gated (§11.5 step 3 mutations); listed under `output_fields` in `newsvault schema`; schema `newsvault.robots/3` since 2026-11-17, because the owner decision of that day (row "key and line spellings", (e)) removed `refused_not_utf8`, a value `newsvault.robots/2` released (PR #35); `/2` since 2026-11-16, because recommendation #2 removed `refused_no_user_agent`, a value `/1` released (PR #33) | a verdict that changes the refresh exit: the exit answers the pin question (decision D1), and `collect` already refuses the same bodies; keeping the major and listing the removed value with no producer: an enum value nothing reaches (CLAUDE.md), and `a_refresh_reports_the_parse_verdict_of_each_answered_row` requires the listed set to equal the produced one; keeping the major and dropping the value: a removal within a major, which the CLI rule forbids | `cmd_robots::parse_verdict_of`, `parse_verdicts`, `render_human` |
| the paths checked are each admitted row's index path with its query and its rule's `path_prefix`; documents are not checked per request; a per-document check is a hard gate before the first live run (owner decision 2026-11-17, §11.5 step 7), not built | a per-request check, which needs the parsed robots inside the provider, not only the delay | `OpenWebFeed::admitted_paths`; `refuse_what_the_pinned_robots_forbids` in `cmd_collect.rs` |
| owner decision 1 (B, 2026-11-16): while one `per_host` serves every host (§8 row R19 hole 7), an admitted host whose `*` Crawl-delay is above it, the provider's `INTERVAL` of 10 s, is refused (`CrawlDelayOverInterval`, `robots_refused`); a `per_host` that is `None` or below `PACE_FLOOR` (10 s) is refused too; such a host waits for per-host intervals (§11.3) | slowing every host to the slowest (built 2026-11-15, `OpenWebFeed::with_crawl_delays`, removed): one host's 30 s tripled every host's paced wait; an `interval_s` hosts-row field, a second number that must equal the profile's; a cap above 10 s (the 870 s alternative): needs per-host intervals and the step-7 deadline first | `refuse_a_pace_below_the_floor`, `crawl_delay_over_interval` and `PACE_FLOOR` in `cmd_collect.rs`; `INTERVAL` in `providers/open-web-feed/src/lib.rs` |
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

**Owner decisions of 2026-11-15 on the held read (§11.1): 6, a listing-derived unusable row is not held; 7, a 408 is transient and a 3xx is not held. Defaults the builder took inside them.**

- Decision 7's 408 rule, both providers: `OpenWebFeed::classify` (`providers/open-web-feed/src/lib.rs`) and `EdgarPress::classify` (`providers/edgar-press/src/lib.rs`) close a 408 `TransientFailure`, so it is retried within the run and never held (§8 row R21).
- edgar's S3 403 `AccessDenied` arm is matched before the arm that joins 408 to 403 and 429, so it stays permanent.

| default | rejected, and why | where it is changed |
|---|---|---|
| a robots.txt 3xx is refused, not followed: `Wire`'s `MAX_REDIRECTS` 0 (§8 row R19), closed `PermanentFailure` by `verdict_of`, `transport_status` 301, no robots row, exit 8; `a_robots_redirect_is_a_permanent_failure_with_no_robots_row_and_its_target_never_fetched` (3 mutations 2026-11-15, §11.2) | RFC 9309 §2.3.1.2, a crawler SHOULD follow at least five redirects for robots.txt: each hop would be unpaced and unledgered (R19 hole 9), and the body stored under the host asked; a 3xx read as `absent`: admits a host whose robots.txt was never read | `cmd_robots::wire_for` (`feed.transport()`), which `main.rs` calls; `cmd_robots::verdict_of` |
| robots keeps a 408 permanent: no robots row, exit 8; `a_robots_408_stays_a_permanent_failure_with_no_robots_row` (mutation 2026-11-15, 408 mapped to transient in `verdict_of`: red) | a transient robots 408: decision 7 names the providers only, and robots has no in-run retry | `cmd_robots::verdict_of` |
| the 3xx rule keys on the stored `transport_status`: `NOT (verdict = 'permanent_failure' AND COALESCE(transport_status, 0) BETWEEN 300 AND 399)`, the range a named constant, the same for every source; no schema change, `transport_status` is a v0 column | a new `Verdict` arm `Redirect`, which changes the verdict `CHECK` and vocabulary and needs a migration; an open-web-only predicate, a second held definition, while edgar is unaffected anyway (§11.1) | `REDIRECTS` and `Committer::held_story_ids`, `services/newsvault/src/store.rs` |
| which input decided an unusable reason is a field `decided_by: DecidedBy { IndexRow, Page }` on `news_provider::UnusableRow`, set where each provider decides the reason | a method on `UnusableReason`: `NoParseableValidTime` is `IndexRow` in open-web and `Page` in edgar; a `Provider` method `refetchable(reason)`: a second mapping that can drift from `document_of`'s checks; two `NoParseableValidTime` variants: changes the reason vocabulary and still cannot name edgar's two-source case | `news-provider/src/provider.rs`; each provider's `document_of` |
| edgar marks every reason `Page`, one literal line; behaviour unchanged, because edgar live ignores the held set and its archive arm reads `covered_story_ids` | `IndexRow` for edgar's `NoParseableValidTime`, which fires only when the page's acceptance field AND the listed Date Filed both fail; the owner scoped edgar to the 408 change | `providers/edgar-press/src/lib.rs`, `document_of` |
| stored as one count, `attempts.listing_unusable` (§3.2), carried in `Counts` inside `Outcome` on `Finalised`, so it is written in the `UPDATE` and transaction that write `verdict`; its own user_version 4 | an `UPDATE` after the commit, as `record_held`: a crash between the two leaves NULL, read as held, a silent hole; reason names as text: a list per attempt, beyond this decision; a boolean: cannot count the cost; folding the 3xx rule into the same version: `transport_status` is a v0 column | `MIGRATIONS[3]` and `apply`, `services/newsvault/src/store.rs` |
| `Outcome::decide(verdict, Counts { vendor_rows, listing_unusable }, baseline)`, every caller naming both counts | a builder `.with_listing_unusable(n)`: the driver could skip it with every non-driver test green, and `Outcome` keeps two constructors and nothing else | `news-provider/src/ledger.rs` |
| a NULL count (a row closed before user_version 4, an abandoned row) reads as 0 through `COALESCE`, so a v3-era usable document with no item stays held | NULL meaning not held: refetches every v3 usable-no-item document once, the page-decided ones included; no production store exists (R11 search, 2026-11-15) | `Committer::held_story_ids` |
| open-web's precedence is kept: `BlankSourceItemId`, then `NoParseableValidTime`, then the page's reason, so a bad listing beside a bad page reports `IndexRow` and is fetched again | the page's reason first: holds a story a later listing could still fix | `OpenWebFeed::document_of` |
| one index-row row in an attempt is enough to not hold it | every row index-decided: a rule no producer reaches, since each provider yields at most one row per `Document` | `Committer::held_story_ids` (`> 0`) |
| the listing refetch cost is `ledger`'s source-row `listing_unusable`, a `SUM` beside `held`, printed by the human renderer; schema stays `newsvault.ledger/1`; the collect envelope `newsvault.collect/2` is unchanged; not subtracted before `rows_committed_zero_against_vendor_rows` | a field per attempt in the collect envelope: a second home for one count, whose reader the ledger already is; subtracting it in `findings_for`: hides a listed story we cannot store | `SourceRow`, `grouped_attempts`, `render_human`, `cmd_ledger.rs` |

**Defaults taken by the builder on 2026-11-16, open-web step 6 (PRN, §11.5), not by the owner.**

| default | rejected, and why | where it is changed |
|---|---|---|
| PRN's body container is `section.release-body` | `<article>`, §11.6's probe rule: it adds the dateline and `Share this article` `<p>` on each probe page (§11.1) | `prn.body.container`, `providers/open-web-feed/rules/open-web-feed.rules.json` |
| a table row's empty cells are dropped before the ` \| ` join, and a row with no letter or digit is no line (P2 build, 2026-11-16) | keeping empty cells: not recorded | `rows`, `providers/open-web-feed/src/parse.rs` |
| a table's `caption` is one line before its rows (P2 build) | not reading it: drops text, against P3 | `rows`, same file |
| a `table` is read by rows before the paragraph selector is tried | the selector first: a `table` match would be one line | `blocks`, same file |
| a table inside a paragraph match (an `li`) ends that line, is read by rows, and the text after it starts a new line (review 2026-11-16) | the table as part of the `li`'s line: `Listed: Cost 4 after`, cells run together, against P2. 0 of 61 real pages hold a table inside an open `li` or `p` (34 research and 27 probe7 pages, a tag-stack scan with Python's `html.parser`, 2026-11-16) | `blocks`, `prose`, same file |
| block edges are a space inside a line for the 33 names of `BLOCKS`; every other element is inline and adds none | a space at every element edge: splits `<b>Bold</b>tail` into two words | `BLOCKS`, same file |
| `extract_version` is NULL on robots attempts and on `edgar-press` attempts (P5 build) | edgar's rules digest in the column: a value that does not cover code, under a name that says it does | `cmd_robots::request_for`, `EdgarPress` |
| the golden file holds `extract_version` beside the body digests, so a comment-only edit under `src/` is red until the file is rewritten (P5 build) | body digests alone: a code change that keeps all 11 fixture bodies would be versioned in the store but red nowhere | `providers/open-web-feed/tests/fixtures/golden_bodies.json` |
| a `CanonError` from `extract_version` is `Refusal::RulesUnparseable`; unreachable, since names and digests hold no separator | a new `Refusal` arm: an arm no input reaches | `OpenWebFeed::open` |
| `valid_format` `%a, %d %b %Y %H:%M:00 +0000`, `:00` and `+0000` literal | `%S`: the loader's `reads_a_minute` refuses a format printing seconds (§11.1); an offset field: not recorded. Cost of the literals: a `pubDate` with another second or offset is `NoParseableValidTime` and fetched every poll, 0 of 40 observed (§11.1) | `prn.valid_format`, same file |
| `path_prefix` `/news-releases/` (plural) | GNW's `/news-release/`: filters out all 40 real links (§11.1) | `prn.path_prefix`, same file |
| no host rewrite; `item_key` keeps `www.prnewswire.com` | a rewrite to another host: the canonical link and `og:url` of both probe pages equal the RSS link (§11.1) | `prn.rewrite_hosts`, same file |
| the GNW-only `newsvault` tests read `gnw_admitted.json`, a byte copy of the GNW-only hosts file | not recorded | `services/newsvault/tests/collect.rs`, `open_web_collect.rs`, `robots_refresh.rs` |
| a `br` element inside a paragraph is read as a space (review remedy, 2026-11-16) | no whitespace: fuses a bold heading and the next word into one, and the body is an `item_id` input (§11.1) | `LINE_BREAK` and `prose`, `providers/open-web-feed/src/parse.rs` |

**Defaults taken by the builder on 2026-11-17, open-web step 6 (CNW, §11.5), not by the owner.**

- Counts below: the 2026-11-16 weekday probe's 26 polls of each CNW index (§11.6), parsed with Python's ElementTree on 2026-11-17 unless the row names `OpenWebFeed`.

| default | rejected, and why | where it is changed |
|---|---|---|
| CNW's index is `https://www.newswire.ca/sitemap-news.xml?page=1`, the URL the probe polled, read as a news sitemap (`index` `news_sitemap`); this applies §11.1's valid-time row for CNW (`news:publication_date`), so the builder chose the URL and the reader, not the field | the RSS feed `https://www.newswire.ca/rss/news-releases-list.rss`, which reuses the RSS reader and PRN's `pubDate` format with no `src/` change: it contradicts §11.1's CNW row, and its margin is smaller (table below) | `cnw.index`, `providers/open-web-feed/rules/open-web-feed.rules.json`; the CNW row, `providers/open-web-feed/hosts/open_web_wires.hosts.json` |
| a rule's `index` field is optional and absent means `rss`, so the `gnw` and `prn` rules are byte-unchanged; another value is `RulesUnparseable` | required on every rule: rewrites the GNW and PRN rules and the 9 earlier rules fixtures for no new reading | `IndexFormat`, `parse_rule`, `providers/open-web-feed/src/rules.rs` |
| a `news_sitemap` rule's `valid_field` is `news:<name>`, read from `url/news:news/<name>`; every element matched by namespace URI and local name | the prefix as written: a feed may bind `news` to another URI, the RSS defect of 2026-11-15 (§11.1) | `sitemap_rows`, `is_in`, `providers/open-web-feed/src/parse.rs` |
| `valid_format` `%Y-%m-%dT%H:%M:00%:z`: `:00` literal, the offset read and the minute stored in UTC; `minute_of` reprints the value at its own offset, so `-04:00` (daylight time) reads as well as `-05:00` | a literal `-05:00`, as PRN's `+0000`: every value after the switch to daylight time would be `NoParseableValidTime`, and every November value read as UTC, 5 h early; the loader accepts such a format, because `reads_a_minute` prints its probe minute at offset 0 only (mutation 2026-11-17: the file loads, and only `a_canada_newswire_publication_date_is_a_minute_at_its_own_offset_and_never_an_instant` and the golden test go red); `Z` accepted too: 0 of 2,600 values use it, so it is refused and fetched every poll | `cnw.valid_format`; `minute_of`, `providers/open-web-feed/src/lib.rs`; `printed`, `rules.rs` |
| `path_prefix` `/news-releases/`, no host rewrite; `item_key` keeps `www.newswire.ca` | a rewrite: canonical link and `og:url` equal the sitemap `loc` on all 11 page files (7 URLs) | `cnw.path_prefix`, `cnw.rewrite_hosts` |
| body container `section.release-body`, PRN's (decision P6: only the container differs; here it does not) | `article`: adds the dateline and `Share this article`, 2 lines on each of the 11 files; `div.release-content`: the same body on 11 of 11 files, rejected only to keep one selector for one Cision template | `cnw.body.container` |
| title `meta[property="og:title"]` `content`, canonical `link[rel="canonical"]` `href`, as PRN | not recorded | `cnw.title`, `cnw.canonical` |
| fixtures: urls 1-3 of the p0800 sitemap; releases 805423040 and 823884942 trimmed; one contact name, phone digits and both Cloudflare-encoded addresses replaced by `Jane Example`, `00-…` and `example.com` ones; 823884942's two paragraphs naming drivers left out | the untrimmed pages: commit a person's name, number and address | `providers/open-web-feed/tests/fixtures/cnw/` |

| quantity, 26 polls each | news sitemap | RSS |
|---|---|---|
| rows read; distinct keys | 2,600; 246 | 520; 166 |
| keys also in the other index | 166 of 246 | 166 of 166 |
| keys only in this index dated 2026-11-16 (UTC) | 0 of 80 | 0 of 0 |
| value shape | `YYYY-MM-DDTHH:MM:00-05:00`, 2,600 of 2,600 | `Ddd, DD Mon YYYY HH:MM:00 +0000`, 520 of 520 |
| precision; instant on the 166 shared keys | minute; equal on 166 of 166 | minute; equal on 166 of 166 |
| language field | `news:language` `en`, 2,600 of 2,600 | `dc:language` `en`, 520 of 520 |
| window h, min / median / max (§11.2) | 7.1 / 11.375 / 60.95 | 0.867 / 2.225 / 16.183 |
| fewest of the previous poll's items still present (§11.2) | 86 of 100 | 5 of 20 |
| keys whose listed date changed between polls; `loc` changed | 3 of 246 (848800830, 836571051, 808938924); 0 | the same 3 of 166; 0 |
| p0800 body, gunzipped | 89,550 B | 32,797 B |
| read by `OpenWebFeed` under the committed rule (temporary test, 2026-11-17) | 2,600 rows, 2,600 minutes, 2,600 under `/news-releases/` | not read |

**Defaults taken by the builder on 2026-11-16, P9, P10 and the over-limit rule (§3.9), not by the owner.**

| default | rejected, and why | where it is changed |
|---|---|---|
| ureq's `gzip` feature off; `Wire::exchange` decodes a `Content-Encoding` of exactly `gzip` with `flate2::read::MultiGzDecoder` (ureq 3.5.2's own decoder), counts `body_limit` in decoded bytes and states `accept_encoding("gzip")`; `flate2` 1.1.11 is a direct dependency, already in `Cargo.lock` through ureq | a ureq middleware: it runs outside ureq's `run`, after the header is deleted; asking for no encoding: changes what the vendor is asked, and a server may compress anyway | `news-provider/Cargo.toml`; `Wire::exchange`, `news-provider/src/wire.rs` |
| a header value is its bytes read as ISO-8859-1; names lowercase in `HeaderMap` order, repeats kept | UTF-8 with replacement: loses a byte that is not UTF-8; the order as sent on the wire: ureq 3.5.2 hands over an `http` 1.x `HeaderMap`, which groups a name's values | `Headers::kept_from`, same file |
| headers stored as one TEXT column, a JSON array of `[name, value]` arrays | a JSON object: loses a repeated name and the order; a child table per header: a second table for one row's fact | `Headers::to_json`; `payloads.headers` |
| P10's link is its own table, `payloads`, one row per answered attempt | `attempts` columns: NULL on every attempt with no answer, and the headers belong to the answer, not to the attempt | `MIGRATIONS[5]`, `services/newsvault/src/store.rs` |
| the link reaches the store through `Archive::commit`, whose `raw_blob: Option<&str>` became `payload: Option<&Payload>`; `Payload::stored` is the one rule for `collect` and robots refresh | a field on the `Answered` event: the ledger never holds the blob digest; a second store door the driver could skip | `news-provider/src/collect.rs` |
| a robots body's one home is its `payloads` row; `robots.blob` is NULL from user_version 6 and kept on older rows, read through `COALESCE` | rebuilding `robots` without `blob` and backfilling `payloads`: the url and headers of a pre-6 fetch were never recorded, so the backfill would invent them; writing both: two homes for one digest | `ROBOTS_BODY`, `Committer::record_robots` |
| robots refresh stores and links every body, a 404's and a 403's included; only a 200 is pinned | storing a 200's body only: a 403 page is the evidence of bot protection (§11.2) | `cmd_robots::fetch` |
| a replay with another payload row is `StoreError::ReplayWithDifferentPayload` | ignoring the payload on replay: a second claim accepted silently, the defect `ReplayWithDifferentItems` was added for | `Committer::record` |
| the reader is `ledger`'s source-row `bodies_truncated`; schema stays `newsvault.ledger/1` | a collect-envelope field: the envelope is the driver's own count, not a read of the store | `SourceRow`, `grouped_attempts`, `render_human`, `cmd_ledger.rs` |

**Server back-off (§8 row R19): the owner approved the proposal on 2026-11-16; these choices inside it are the builder's.**

| default | rejected, and why | where it is changed |
|---|---|---|
| slow-down answers are 429 and every 5xx (500..=599), recommendation #3 (owner, 2026-11-16: "zgadzam sie na warunek A, na rekomendacje #1, #2 i #3"), in every provider, read in `Wire::send`; Google: "`5xx` and `429` server errors prompt Google's crawlers to temporarily slow down with crawling" (developers.google.com/crawling/docs/troubleshooting/http-status-codes, read 2026-11-16); that page names no `Retry-After`, so honouring it on any 5xx is RFC 9110 §10.2.3's rule, not Google's | 429 and 503 only (built 2026-11-16, the two the approved proposal names): a 500, 502 or 504 kept the normal pace and only R21's retry; a slow-down rule per provider: two copies of one rule | `TOO_MANY_REQUESTS`, `SERVER_ERRORS`, `asks_to_slow_down` in `news-provider/src/wire.rs` |
| edgar's 403 is no slow-down answer; it stays `TransientFailure` and retried at the 100 ms pace | every 403: the same status is SEC's S3 `AccessDenied` for a closed or unpublished day, so a weekend window would double SEC's interval; edgar's own body test in the wire: a provider rule inside the shared transport. Unmeasured: whether SEC's traffic 403 carries a `Retry-After`; Google, the same page: "Don't use `401` and `403` status codes for limiting the crawl rate" | `asks_to_slow_down`; `EdgarPress::classify` |
| the doubling, and a hold until its first reservation, live in the `Wire`'s memory for its run, keyed by `HostKey`; a reserved hold is the host file's grant and outlives the run (§8 row R19 hole 11) | a third pace file per host: one more locked file and write per answer, and a write failing after the socket would abort the run with the answered attempt left open; the doubling is per run by the proposal | `Wire::backoff`, `Wire::heed` |
| a `Retry-After` hold is a floor on the host reservation, `Pace::reserve_not_before`, read with the same clock read | a separate sleep before the reservation: one more clock read per send, and the host file would record a grant earlier than the call | `news-provider/src/budget.rs` |
| a usable `Retry-After` sets the hold and does not double | doubling on every slow-down answer: the server already named its wait | `Wire::heed` |
| unusable `Retry-After`: absent, two or more values, not all digits and no HTTP-date, past `u64`, past year 9999 (`LAST_STAMPED_YEAR`, the last year the pace file reads back); read as absent, so the interval doubles | the first of two values: RFC 9110 defines one value; past `u64` as "forever": that is item 3's stop, unbuilt (§8 row R19 hole 12); a hold past year 9999 kept: stamped `+YYYYY-…`, it made the host pace file `Unreadable` for every later run (review 2026-11-16) | `retry_after`, `news-provider/src/wire.rs` |
| HTTP-date in all three RFC 9110 §5.6.7 forms via chrono 0.4.45 formats; the RFC 850 two-digit year is chrono's `%y` (69–99 → 19xx); a day name that does not match the date is refused by chrono, so the value is absent and the interval doubles (`Mon, 10 Nov 2026 …`: refused; `Tue, …`: read, probe 2026-11-16) | IMF-fixdate alone: §5.6.7 says a recipient MUST accept all three; the `httpdate` crate: a new dependency for three format strings; ignoring the day name: three more formats for a server error not yet seen | `HTTP_DATE_FORMATS`, same file |
| no stop after a long wait, no "not looked" record (proposal item 3); owner condition A (2026-11-16) makes building it part of §11.5 step 7, before the first live run; its deadline (S7-D3), `deferred` record (S7-D4) and hold ceiling (S7-D7) are owner decisions of 2026-11-17, not built (§11.5 step 7) | a cap from §11.2's stop deadline (S7-D3) in this step: no code holds a run deadline before §11.5 step 7 | §8 row R19 hole 12; §11.5 step 7 |

**Defaults taken by the builder on 2026-11-17, open-web step 6 (Fed, §11.5), not by the owner.**

- Counts below: the 2026-11-16 weekday probe's 26 polls of `press_all.xml` (§11.6), one body in all 26 (14,607 B, sha256 `e3409b426930…`), parsed with Python's ElementTree on 2026-11-17, and the 9 Fed page files of that day (§11.1's Fed coverage table), unless the row names `OpenWebFeed`.

| default | rejected, and why | where it is changed |
|---|---|---|
| index `https://www.federalreserve.gov/feeds/press_all.xml`, the URL the probe polled, read as RSS (`index` absent): 20 items, `guid` equal to `link` on 20 of 20, `language` `en` | not recorded | the Fed row, `providers/open-web-feed/hosts/open_web_wires.hosts.json` |
| `valid_format` `%a, %-d %b %Y %H:%M:00 GMT`: the day unpadded, `:00` and `GMT` literal; observed: 20 of 20 `GMT` and second `00`, 9 of 20 on a day before the 10th, each unpadded; 20 of 20 read as minutes by `OpenWebFeed` (temporary test, 2026-11-17) | `%d`, GNW's and PRN's: refuses all 9 single-digit days as `NoParseableValidTime`, each then fetched by every poll while listed; `%S`: the loader refuses a format printing seconds (§11.1); a zero-padded day kept beside the unpadded one: never observed, and the round trip admits one spelling per format | `fed.valid_format`, `providers/open-web-feed/rules/open-web-feed.rules.json` |
| `path_prefix` `/newsevents/pressreleases/`, no host rewrite; `item_key` keeps `www.federalreserve.gov` | a rewrite: the 20 links and `og:url` on 9 of 9 files name `www.federalreserve.gov` | `fed.path_prefix`, `fed.rewrite_hosts` |
| body container `div#article > div.col-md-8:not(.heading)`, the release column; it matches once on 9 of 9 files | `div#article`, the probe's candidate: adds the date, the release time and `Share` on 5 of 5 URLs, and on bcreg20261113a the 4 issuing agencies and the Related Information panel's title of another release (§11.1); `div#article > div.col-md-8`: the heading block matches too, so every page is `ArticleContainerAmbiguous`; `div.col-md-8:not(.heading)`: the attachments panel inside the column matches too on 8 of 9 files, ambiguous; an element deeper than the column: none holds the paragraphs and the attachment links together | `fed.body.container` |
| paragraph `p, li, figcaption` (P6, decision s) | a Fed selector: not needed, the container alone keeps the heading, the right-hand column and the nav out (§11.1) | `fed.body.paragraph` |
| title `meta[property="og:title"]` `content`, canonical `link[rel="canonical"]` `href`, as every rule; no Fed page names a canonical link (0 of 9), so `doc_id` is the requested url | `meta[property="og:url"]`: equal to the requested url on 9 of 9, so the same `doc_id` on the data; kept out to keep one canonical pick for every rule (mutation 2026-11-17: red on `a_federal_reserve_page_naming_a_canonical_link_is_stored_under_it`, a synthetic canonical in the real release's head) | `fed.title`, `fed.canonical` |
| robots pin `absent`: the robots path answered 404 on 2026-11-14 and 2026-11-16, the same 81,196 B HTML page; §11.1 reads a 404 as no robots, so no `Disallow`, no Crawl-delay and `per_host` 10 s | the 404 page's digest: pins an HTML body nothing parses, where §10's pin row keeps `absent` for a 404 | the Fed row's `robots` |
| fixtures: feed items 1, 3 and 5 byte for byte; pages bcreg20261113a and orders20261109a cut, the 4 media contacts' names and direct numbers replaced by `Jane Example`-style names and `000-000-000n`; the Cloudflare-encoded `media@frb.gov` and the press line `202-452-2955` kept: the Fed's press office, not a person | enforcement20261112a: its release text names the person the action is against; the untrimmed bcreg20261113a: 4 spokespeople's names and direct numbers | `providers/open-web-feed/tests/fixtures/fed/` |

**Defaults taken by the builder on 2026-11-17, open-web step 6 (Yahoo, §11.5), not by the owner.**

- Counts below: the 26 polls of `news-sitemap.xml` of 2026-11-16 (§11.6), parsed with Python's ElementTree on 2026-11-17, and the 16 Yahoo page files of that day (§11.1's Yahoo table), unless the row names `OpenWebFeed`.

| default | rejected, and why | where it is changed |
|---|---|---|
| index `https://finance.yahoo.com/news-sitemap.xml`, the URL the probe polled, read as `news_sitemap`; 1,000 rows a poll | not recorded | the Yahoo row, `providers/open-web-feed/hosts/open_web_wires.hosts.json` |
| the key is the segment after `https://media.zenfs.com/en/` in the row's first `image:loc`, up to the next `/`; the mark occurs once in each of 21,345 images that hold it, and in none of 351 | the key read anywhere in the url, or the last `/en/` segment: no row needs it, and the mark is the probe's construction (§11.6); `image:caption`: free text, not a key | `yahoo.provider_keys.image_key_after`, `providers/open-web-feed/rules/open-web-feed.rules.json` |
| the key filter is a rule field (`provider_keys`) read by the shared index path, so it is in the rules digest and `extract_version` | a Yahoo branch in code: a per-host code path, against P6's one path | `yahoo.provider_keys` |
| `path_prefix` `/`: followed rows sit under 9 first segments (`/technology/`, `/healthcare/`, `/markets/`, …); robots is checked on `/` (§11.5 step 6's holes) | a list of prefixes: the rules schema holds one; one section prefix: drops the other 8 (mutation `/markets/`, red) | `yahoo.path_prefix` |
| `valid_format` `%Y-%m-%dT%H:%M:00Z`: 2,017 of 2,017 followed rows `Z` with second `00`; 12,247 of 23,983 other rows carry a second | `%:z`: no row spells an offset; `%S`: the loader refuses a format printing seconds (§11.1) | `yahoo.valid_format` |
| body container `div[data-testid="article-body"]`, the research's (§11.6); one match on 16 of 16 files | `article`: holds the paid-release disclaimer and the cover `figure` around the body (Laverock's untrimmed page, read 2026-11-17), equal on the trimmed fixture; `div.body`: also matches the synthetic page's other `div` (mutation, red); `div.bodyItems-wrapper`: drops every line after `Story Continues` (mutation, red) | `yahoo.body.container` |
| no Yahoo skip rule: `Story Continues` and the ad slots yield no paragraph (§11.1); Yahoo's logo caption and Cision lines are kept | a selector excluding `figcaption` on Yahoo: a second paragraph rule, against P6 and decision s | `yahoo.body.paragraph` |
| the cross-check runs inside `newsvault collect` after the run, and its result is the envelope's `cross_check` | a `crosscheck` table: a migration for a reading the owner asked to store no row of; a separate reader command: a second CLI surface over the same stored blobs, with no consumer yet | `cmd_collect::cross_check_of` |
| matching by normalised title, §11.6's construction | the release id: absent from the Yahoo sitemap, and from GNW reprint pages (§11.6); the minute: Yahoo mostly copies it (the `Yahoo` valid-time row, §11.1), but releases share minutes (12 GNW notices in 10:46–10:47Z, §11.6), so a minute alone merges distinct releases | `crosscheck::normalised`, `CrossChecker::check` |
| compared from `watched_from`, the first direct index answer the store holds; direct answers read from `DIRECT_LEAD` (2 h) before the earliest `cross_check` minute of this Yahoo answer (`CrossChecker::read_from`); 2 h covers the largest lead measured, 72 min (§11.2), and was set after seeing it; the owner replaced it with 90 min on 2026-11-17, not built (§11.2) | every stored direct answer: grows with every run (about 124 index blobs a day at §11.2's periods); no lead: 2 PRN urls (3 row-polls) named unlisted although PRN's RSS had listed them up to 36 min before Yahoo's minute (review 2026-11-17) | `crosscheck::DIRECT_LEAD`, `cmd_collect::read_answers` |
| a row is compared only when every admitted direct index has an answer that parses, received at or after the row's minute closed; otherwise it is `not_looked` | per key, only the index that lists that wire: the rules hold no key-to-index map (the owner decided one on 2026-11-17, not built, §11.2); any one index answering: a CNW 503 run named 3 CNW-keyed rows unlisted (review 2026-11-17) | `CrossChecker::check` |
| a store failure while reading is `cross_check.error` (`blob_altered`, `blob_absent`, `store_unreadable`) with `result` null, and the run still writes `finished_at` | failing the run: the items are already committed, and one altered blob failed every run while it stayed inside the read window, each left unfinished in the ledger (review 2026-11-17) | `cmd_collect::cross_check_of` |
| the keyed index's last attempt of the run is the one checked | the first: a retried index's first answer is the failure it was retried for (mutation, red) | same |
| fixtures: 8 sitemap rows byte for byte, one per key class; the Laverock and Horizon pages cut to head, disclaimer, `h1` and the article body whole; contacts' names and addresses replaced (`Jane Example`, `Jane Doe`, `example.com`); the `accesswire.ca` row answered with a synthetic page | an untrimmed page: about 1 MB each, 45% of it `<script>` (§11.6) | `providers/open-web-feed/tests/fixtures/yahoo/` |

**Owner decisions of 2026-11-16, second batch; each stands where its fact is.**

| decision | where it stands |
|---|---|
| i, no takedown deletion path for now | §3.9 |
| j, Business Insider not added for now | §11.3, `open_web_outlets` |
| u, `bodies_truncated` 0 before user_version 6, kept | §3.9, §8 row R4 |
| v, edgar refuses redirects | §3.9 `payloads.url`, §8 row R19 |
| 10, a held story re-listed under another publication value is an R15 dispute, for every open-web host | §8 row R15 |
| r, every email address is removed from body text on every host; names, phone numbers and labels stay; an exception to P3, and the `mailto:` skip dropped | §11.1's extraction table, §8 row R17 |
| s, `figcaption` is read on PRN and GNW | §11.1's extraction table |
| t, `.gitattributes` stores text as LF and fixtures byte for byte, and a gate refuses a `\r` in every file it does not mark `-text` or `binary` | §8 row G2 |

**Defaults taken by the builder on 2026-11-16, decisions v and 10, not by the owner.**

| default | rejected, and why | where it is changed |
|---|---|---|
| decision v removes the `Redirects` knob: `Wire` sets ureq's `max_redirects` to `MAX_REDIRECTS` = 0 for every profile | `Redirects::FollowUpTo(u32)` kept with no provider producing it: a branch nothing reaches; a one-variant `Redirects`: a profile test that cannot go red | `MAX_REDIRECTS`, `news-provider/src/wire.rs` |
| edgar refused with no SEC redirect measured: every url edgar asks is `https://www.sec.gov/Archives/...`, and 491 of 491 rows of the real 2026-11-10 index name `edgar/data/<cik>/<accession>.txt` (read 2026-11-16, no network) | waiting for a live measurement: a 3xx is a permanent failure with `transport_status` 3xx, held by nothing, so a redirecting path shows as `holds_a_failure` and exit 8, plus `coverage_incomplete` for a vendor_archive document (only that arm plans coverage), not as a silent hole | `MAX_REDIRECTS` |
| the re-listed value is its own table, `relisted` (user_version 7), one row per (index attempt, story) | an `items` row: needs the page the held filter does not fetch, and a listing is not a sighting; an `attempts` column: one index lists many stories; `valid_disputed` on the stored row: R15 forbids updating it | `MIGRATIONS[6]`, `services/newsvault/src/store.rs` |
| compared with the minute of the item first sighting kept (`ORDER BY observed_at, seq`), as row R3 keeps it | every twin's value, as `dispute` compares an arriving item: a row would have to name which of several stored values it disagrees with | `Committer::record_relisted` |
| minutes only: a listing that is not a minute, a story with no item and a kept item with no `valid_minute` are not compared | instants and days: no provider lists one for a held story (edgar live issues every follow-up, and lists a day); day rows are never disputed (row R15) | `Committer::record_relisted` |
| the listed value is `Provider::listed_publication`, the rule `extract` dates the item by (open-web's `document_of`, edgar's `Date Filed` fallback), a required trait method | parsing `listed_date` in the store: a second parse that can drift; comparing `listed_date` with `items.valid_raw` as text: a host whose valid time comes from the page would compare two fields | `Provider::listed_publication`, `news-provider/src/provider.rs` |
| a row only for a disagreement; how many were compared, equal or not, is one count on the index attempt, `attempts.relisted_compared` (user_version 8, decision x, 2026-11-16), written in the same transaction (gated, §3.2 `relisted_compared` row) | a row per comparison: `relisted`'s `CHECK (listed_minute <> stored_minute)` needs a table rebuild, and an equal listing adds a row on every poll; `attempts.held` alone: it counts held stories, compared or not | `Committer::record_relisted`, `MIGRATIONS[7]` |
| a story listed twice in one index is compared by its first row only, the row `index_rows_of` follows up (review 2026-11-16, §8 row R15) | carrying the dropped repeats' `listed_date` out in `Extracted`: a held story would be compared with a minute the not-held path never stores, and `Extracted` is shared by every provider | `index_rows_of`, `providers/open-web-feed/src/lib.rs` |
| written after the index commit beside `record_held`, live arm only, not when the commit was refused (gated, §3.2 `relisted_compared` row) | inside the commit transaction: needs the follow-up selection moved before `slot.close`, the reason `held` is outside it (step 4 table) | `attempt_once`, `news-provider/src/collect.rs` |
| reader: `ledger`'s source-row `relisted_disputed` and, since 2026-11-16, `relisted_compared` (a sum, gated in the §3.2 row), both printed by the human renderer; no `--check` finding; schema stays `newsvault.ledger/1`, an added field | a finding: R15 marks and never corrects, and a re-listing is no coverage gap | `SourceRow`, `grouped_attempts`, `render_human`, `cmd_ledger.rs` |

**Defaults taken by the builder on 2026-11-16, decisions s and t, not by the owner.**

| default | rejected, and why | where it is changed |
|---|---|---|
| GNW's selector names `figcaption` though 0 of its 15 research pages hold one; GNW's P6 fixture `gnw/synthetic_lists_tables_and_addresses.html` holds one caption, so the GNW half has a test | `p, li` on GNW alone: P6's "only the container differs" stops being true | `gnw.body.paragraph`, `providers/open-web-feed/rules/open-web-feed.rules.json` |
| the PRN fixture keeps 2 of release 302939742's 4 figures, the 2 whose captions name no person | all 4: commits a person's name with no test gain | `providers/open-web-feed/tests/fixtures/prn/article_302939742.html` |
| `-text` also on `probes/observation_lag/quarter_index.*.json`, a SEC listing capture | leaving it to `text=auto`: a capture holding `\r` would be normalised | `.gitattributes` |
| `*.parquet binary`, though git already treats the parity parquets as binary (`git ls-files --eol`: `-text`) | no line: G2 reads its exemptions from `.gitattributes`, and 1 parquet holds 2 `\r` bytes | `.gitattributes` |
| G2 reads bytes | a `git check-attr` test (the research pick for `* -text`): shows that a line exists, not that a file is LF | `repo-invariants/src/line_ends.rs` |
| G2 counts a lone `\r` too | CRLF pairs only: a lone `\r` moves a digest the same way | same file |
| G2 scans every non-hidden file outside `target/`, as `version.rs` skips hidden entries, and reads its exemptions from `.gitattributes` | a fixed list of digested paths (PR review 2026-11-16): the hosts entry removed and the hosts file converted to CRLF left all 425 tests of 3 crates green; a hard-coded exemption list: a second copy of `.gitattributes` | same file |

---

## 11. `open-web-feed` — our own forward news feed (decided 2026-11-14; §11.5 steps 1 to 5 are built and step 6 has admitted PRN, CNW, the Fed and Yahoo's wire keys; `newsvault collect` drives it)

- §11.1 and §11.2 hold owner decisions beside builder records. A statement's content decides its group, never `git blame`: blame dates a line by its last edit, and `fea3734` (2026-11-15, "compress §10 and §11") rewrote lines the owner decided on 2026-11-14.
  - Owner decisions of 2026-11-14: the statements of §11.1 and §11.2 as they stand in `50c1705`, wherever they now stand and however reworded. `50c1705` is the last commit dated 2026-11-14 (author date, +0100) that changed §11.1 or §11.2, and its §11 opens "Every line in §11.1 and §11.2 is an owner decision of 2026-11-14". Check: `git show 50c1705:docs/design/architecture.md`. Two examples: "Both poll periods are owner's choices, not measured" and "Follow-up cap: 150 per invocation" (§11.2), as `50c1705` words them, which blame dates 2026-11-16 and 2026-11-15.
  - Later owner decisions, each dated where it stands: the valid-time cells of the CNW, Yahoo and Fool/BI rows of §11.1's table as `d0b414e` and `800a17a` rewrote them (the `ValidSource` and the "named in their host PR" wording), and the bullet below that table with its sub-bullets (2026-11-15, `d0b414e` and `800a17a`); decisions 6 and 7 (§10, 2026-11-15) wherever §11.1 or §11.2 applies them: the held table's two `owner decision 2026-11-15` rows, the two held-read sub-bullets marked `owner decision, §10`, the 408 of the Classify bullet, §11.2's "and is not held" in the redirects bullet, and the `user_version` 4 bullet that names decision 6.
  - Owner decisions of 2026-11-16, each marked `owner decision 2026-11-16` where it stands: §11.1's extraction table (decisions 8 and 9, P1, P2, P3, P6; `mailto:` dropped by r) and P5 bullet, both built 2026-11-16, §11.2's Yahoo sub-bullets and §11.3's `open_web_outlets` row; outside §11, §3.9's L2 row and its storage table, §9's history cell and §10's history bullet. The second batch (i, j, u, v, 10, r, s, t) is §10's table of that name.
  - Owner decisions of 2026-11-17, each marked `owner decision 2026-11-17` or by its id where it stands: `PG-D1` to `PG-D7` and `PG-D9` after the PRN/GNW research (§10's per-host queues; §11.2's completeness table; §11.6's probe10; step 6's CNW holes), round-robin (§10), robots decision 4 on robots paths (§11.1), and the ~16:10Z batch: the Yahoo lead and key→index map (§11.2), `S7-D1` (§11.2), `S7-D2` to `S7-D11` and the per-document robots gate (§11.5 step 7), N1 (§10).
  - Builder records: every other line, and what a line adds to an owner statement (a test, a mutation, a file or line reference, a count, a measured value). `Built` and `Measured` mark a record where present; a line without them is not thereby an owner decision.
  - The choices among the records that the owner did not make: §10's tables of builder defaults, 2026-11-15 (steps 2, 3 and 4, and inside decisions 6 and 7), 2026-11-16 (step 6 PRN, and decisions s and t) and 2026-11-17 (step 6 CNW, step 6 Fed, step 6 Yahoo).

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
| Yahoo | Yahoo Finance's news sitemap (`finance.yahoo.com/news-sitemap.xml`) and its article pages |
| provider key | the path segment after `https://media.zenfs.com/en/` in a Yahoo sitemap row's `image:loc`: the source Yahoo files the row under (§11.2) |
| keyed / direct index | Yahoo's index, read under a key filter; and each other admitted index (§11.2's cross-check) |

### 11.1 Fixed in both layers

- Crate `providers/open-web-feed/`, top-level `[lints] workspace = true`; no clock and no HTTP client in the crate.
  - Built 2026-11-15: `OpenWebFeed` implements `Provider` for `open_web_wires`, offline over committed fixtures; `newsvault collect --source open_web_wires` drives it through the same `Wire`, store and envelope as `edgar_press` (§11.2). No run against any live host yet.
  - Parsers: RSS through `roxmltree` 0.21.2, pages through `scraper` 0.29.0 (html5ever); the dependency graph grew from 161 to 192 packages (manifest comment, 2026-11-15). `serde` 1.0.229 is read directly for the duplicate-key check and the count stays 192 (same command, 2026-11-15).
  - `OpenWebFeed::open(hosts, rules, contact)` refuses before any request, one `Refusal` arm each, one test each: absent or unparseable file, a field the reader does not know, a host named twice (case-insensitive), a host `HostKey` refuses, a row naming a rule the rules file lacks, an index url on another host, zero admitted rows (R6), a contact with no address, one host rewritten to two hosts by two rules (`a_host_two_rules_rewrite_to_two_hosts_is_refused`; mutation 2026-11-15, the refusal disabled: red, green before the test).
    - Built 2026-11-17, owner decision 4: an index target (`IndexTargetNotPlain`) or a rule's `path_prefix` (`PathPrefixNotPlain`) holding `%`, a non-ASCII byte or a control byte is refused, because robots.txt is matched against the path as written and such a byte gives it a second spelling; so is a `path_prefix` holding `#`, added by the 2026-11-17 review: robots.cc cuts a url at `#`, so `Disallow: /c$` disallowed `/c#x` there and allowed it here; an index target never holds one, because `host_and_target` cuts it; `an_index_target_holding_a_percent_a_non_ascii_or_a_control_byte_is_refused`, `a_path_filter_holding_a_percent_a_non_ascii_or_a_control_byte_is_refused`.
    - 0 of the 8 configured paths refused (4 index targets, 4 `path_prefix` values, ASCII with no `%` and no `#`, read 2026-11-17).
    - Added 2026-11-15 (review: each loaded and then yielded nothing usable), each red under the mutation that removes its check, run once:

      | refusal | test |
      |---|---|
      | a key named twice in one JSON object of either file (`serde_json::Value` keeps the last, so `"status":"candidate","status":"admitted"` was admitted) | `a_key_named_twice_in_one_object_of_either_file_is_refused` |
      | a rewrite source or target `HostKey` refuses | `a_rewrite_to_a_host_no_url_can_name_is_refused` |
      | one rule rewriting one host twice in two cases (both lowercase to one key; the last was kept) | `a_host_one_rule_rewrites_twice_in_two_cases_is_refused` |
      | a `path_prefix` not starting with `/` | `a_path_filter_not_starting_with_a_slash_is_refused` |
      | a `valid_format` that does not print 2026-11-14T19:07Z as text `minute_of` reads back as that minute, or prints second 31 differently from second 0 | `a_valid_format_that_does_not_read_back_a_minute_is_refused` (fixture: `%S` added; two mutations, the whole check and the seconds half: both red); `a_valid_format_whose_minute_does_not_parse_back_is_refused` (fixture `%d %b %H:%M GMT`, no year, so only the read-back half refuses it; mutation 2026-11-15, `is_some_and` → `is_none_or`: red, green with clippy clean before the test) |

  - Hosts file `hosts/open_web_wires.hosts.json`, schema `open-web-feed.hosts/2`: five rows, all admitted, in §11.2's order; each `robots` pin is the sha256 of one fetch of the host's robots.txt, committed byte for byte under `tests/fixtures/`, or `absent` where that fetch was a 404:

    | row | host; index | `robots` pin | fetch the pin is the sha256 of | committed as |
    |---|---|---|---|---|
    | GNW | `rss.globenewswire.com`; `https://rss.globenewswire.com/rssfeed/` | `eb8bb698171dd56c139c224c9217070adf9947f6ed6cd02336eba55aa4155bfe` | 2026-11-14, HTTP 200, `date` 19:21:07 GMT, 4,354 B decompressed by `curl --compressed` | `gnw/robots_rss_globenewswire_com.txt` |
    | PRN, Built 2026-11-16 | `www.prnewswire.com`; `https://www.prnewswire.com/rss/news-releases-list.rss` | `906828bd8cf3ae9fd8bfd1475ae1abe7db4cfda54a2ddbf88dcb80cd55f522b2` | 2026-11-16, HTTP 200, `date` 08:04:29 GMT, 427 B gzip on the wire, 3,457 B gunzipped by the weekday probe; byte-identical to the 2026-11-14 18:40:10 GMT probe body | `prn/robots_www_prnewswire_com.txt` |
    | CNW, Built 2026-11-17 | `www.newswire.ca`; `https://www.newswire.ca/sitemap-news.xml?page=1` | `ab9873af0f8c94c48918cd75345a8fc609665602dfcd96aa1e574881a34fb2a0` | 2026-11-16, HTTP 200, `date` 08:04:29 GMT, 129 B gzip on the wire, 251 B gunzipped by the weekday probe; one `Disallow` (`/templates/get-bitly-url.txt`) and four `Sitemap` lines, two of them `/fr/` | `cnw/robots_www_newswire_ca.txt` |
    | Fed, Built 2026-11-17 | `www.federalreserve.gov`; `https://www.federalreserve.gov/feeds/press_all.xml` | `absent` | none: HTTP 404 on 2026-11-14 (`date` 19:36:51 GMT, curl) and on 2026-11-16 (`date` 08:04:29 GMT, the weekday probe), the same 81,196 B HTML page both times (`Federal Reserve Board - Page not Found`, sha256 `0efee485ab8d…`, decompressed) | `fed/robots_404_www_federalreserve_gov.html`, the 2026-11-16 page cut to its head (156 B); pinned by nothing, never parsed |
    | Yahoo, Built 2026-11-17 | `finance.yahoo.com`; `https://finance.yahoo.com/news-sitemap.xml` | `a01715ceceb6faead3058d083abc97bdec68c7e4c6f522f3211dd0819ce38eb2` | 2026-11-16, HTTP 200, fetched 08:04:29.807Z by the weekday probe, 2,539 B; its `*` group: 32 `Disallow`, 7 `Allow`, no Crawl-delay; a named group of 44 agents, ours not among them, disallows `/` | `yahoo/robots_finance_yahoo_com.txt` |

    - Built 2026-11-16: `the_committed_hosts_file_pins_the_committed_robots_body` reads every row (mutations 2026-11-16 and 2026-11-17, one hex digit of the PRN pin, then of the CNW and the Yahoo pin, changed: each red; the Fed pin set to the 404 page's digest: red).
    - Built 2026-11-16, unknown for every row: whether the live bytes are still identical, and whether `ureq`'s decoding gives the bytes curl or the probe decoded; a first live refresh that differs exits 1 and the pin is edited by hand. The CNW pin was not fetched again for this PR (2026-11-17), nor the Fed's robots path for its PR (2026-11-17), nor Yahoo's for its PR (pin: the 2026-11-16 08:04:29Z body, 2,539 B).
    - An admitted row with no pin, and a pin that is neither 64 lowercase hex nor `absent`, are `HostsUnparseable`: `an_admitted_hosts_row_without_a_robots_pin_is_refused`, `a_robots_pin_that_is_neither_64_lowercase_hex_nor_absent_is_refused` (mutations 2026-11-15, the admitted check removed / any text read as a digest: each red).
    - `collect` reads the pin since 2026-11-15: an admitted row is refused unless the store's newest answered robots fetch of its host is the pin (§8 row R4, robots clause). No live refresh has run, so a live store refuses GNW, PRN, CNW, the Fed and Yahoo until the owner runs one.
  - Rules file `rules/open-web-feed.rules.json`, one rule per hosts row; its digest is `extract_rules_version`. Every rule takes the title from `meta[property="og:title"]` `content` and the canonical link from `link[rel="canonical"]` `href`, and reads paragraph `p, li, figcaption` (decision s, 2026-11-16; `p, li` before s, `p` alone before decision 8):

    | rule | `index`; `valid_field` | `valid_format` | `path_prefix` | `rewrite_hosts` | body `container` |
    |---|---|---|---|---|---|
    | `gnw` | RSS (field absent); `pubDate` | `%a, %d %b %Y %H:%M GMT` | `/news-release/` | `www.globenewswire.com` → `rss.globenewswire.com` | `div.main-body-container` |
    | `prn`, Built 2026-11-16 | RSS (field absent); `pubDate` | `%a, %d %b %Y %H:%M:00 +0000` | `/news-releases/` | none | `section.release-body` |
    | `cnw`, Built 2026-11-17 | `news_sitemap`; `news:publication_date` | `%Y-%m-%dT%H:%M:00%:z` | `/news-releases/` | none | `section.release-body` |
    | `fed`, Built 2026-11-17 | RSS (field absent); `pubDate` | `%a, %-d %b %Y %H:%M:00 GMT` | `/newsevents/pressreleases/` | none | `div#article > div.col-md-8:not(.heading)` |
    | `yahoo`, Built 2026-11-17 | `news_sitemap`; `news:publication_date` | `%Y-%m-%dT%H:%M:00Z` | `/` | none | `div[data-testid="article-body"]` |

    - Built 2026-11-17: the loader refuses an `index` other than `rss` or `news_sitemap`, and a `news_sitemap` rule whose `valid_field` is not `news:<name>`, as `RulesUnparseable`: `an_unknown_index_or_a_sitemap_field_outside_news_is_refused` (mutations 2026-11-17, an unknown value read as `rss`: red; the `news:` check removed: red).
    - Built 2026-11-17, `yahoo` only: `provider_keys`, the key filter of §11.2. `image_key_after` `https://media.zenfs.com/en/`; `follow` `business-wire.com`, `accesswire.ca`; `cross_check` `prnewswire.com`, `globenewswire.com`, `cnwgroup.com`. The sitemap reader also reads each row's first `image:image/image:loc` and `news:news/news:title` (namespace-matched, as the other fields).
      - The loader refuses as `RulesUnparseable`: `provider_keys` on an `rss` index, a key both followed and cross-checked, an empty `follow`, a mark not ending in `/`, a key holding `/`: `a_provider_key_filter_nothing_can_read_whole_is_refused` (5 fixtures; mutations 2026-11-17, each check removed: each red).

    - P5, owner decision 2026-11-16: the extraction version covers code too, as a source hash of the provider crate, stored per attempt in its own `attempts` column, with a golden test over body digests.
      - Built 2026-11-16: `extract_version` (§2) over the rules digest and every file of `src/`, in `attempts.extract_version` (§3.2, user_version 5) and the collect envelope; `extract_rules_version` stays the rules digest alone. Golden file and tests: §8 row R17.

    - Built 2026-11-16: GNW's `/news-release/` does not prefix PRN's links: `/news-releases/x` does not start with `/news-release/`. `the_real_pr_newswire_index_yields_one_follow_up_per_release_with_its_raw_pub_date` (mutation 2026-11-16, PRN's prefix set to `/news-release/`: red).
    - Built 2026-11-16: the 40 real PRN links (2026-11-14, 2026-11-16) are all under `/news-releases/`, so no real link tests the prefix from the wide side. `a_pr_newswire_link_outside_news_releases_is_filtered_out_and_counted`: a synthetic `/apac/news-releases/` link on `www.prnewswire.com` is filtered out and counted (mutation 2026-11-16, PRN's prefix set to `/`: red; before the test, that mutation left every test of `open-web-feed` and `newsvault` green).
    - Built 2026-11-17, CNW: the 2,600 real sitemap rows of 2026-11-16 are all `https://www.newswire.ca/news-releases/…`, so the same two shapes are tested: `the_real_canada_newswire_sitemap_yields_one_follow_up_per_release_with_its_raw_date` (mutation 2026-11-17, the prefix GNW's `/news-release/`: red) and `a_canada_newswire_link_outside_news_releases_is_filtered_out_and_counted`, a synthetic `/news/an-organisation/` link, the shape of a page's own organisation link (mutation 2026-11-17, the prefix `/`: red).
    - Built 2026-11-17, Fed: the 20 real links of 2026-11-16 (one feed body in all 26 polls) are all `https://www.federalreserve.gov/newsevents/pressreleases/<name>.htm`, so the same two shapes are tested: `the_real_federal_reserve_feed_yields_one_follow_up_per_release_with_its_raw_pub_date` (mutation 2026-11-17, PRN's `/news-releases/`: red) and `a_federal_reserve_link_outside_press_releases_is_filtered_out_and_counted`, a synthetic `/newsevents/speech/` link (mutation 2026-11-17, the prefix `/`: red).
  - Classify: index 200 with rows `Usable`, with none `Empty`, not RSS `PermanentFailure`; document 200 `Usable` (extraction decides `NoArticleBody`, `PageNotUtf8`, `PageTooDeep`, `ArticleContainerAmbiguous` from the page, `UnusableRow::decided_by` `Page`; `NoParseableValidTime` from `listed_date` and `BlankSourceItemId` from the url, unreachable through `plan` (§11.1), are decided by the index row, `IndexRow`, since 2026-11-15: `an_unparseable_listed_time_is_decided_by_the_index_row_and_a_page_reason_by_the_page`, which also shows a bad listing beside a bad page giving `NoParseableValidTime`/`IndexRow`; mutation that day, `NoParseableValidTime` marked `Page`: red, and both listing driver tests (§8 row R16)); 408, 429, 5xx and transport errors transient, except a body over `BODY_LIMIT` (2 MiB), permanent (§8 row R21); every other status permanent.
  - Precision ceiling `Minute`; a `pubDate` with seconds is `NoParseableValidTime`, never an instant. So is a `pubDate` the rule's format does not print back byte for byte: chrono's `%Y` read `Sat, 14 Nov 26 19:00 GMT` as the year 26, and `%H` read `9:05` (review 2026-11-15); `a_globenewswire_pubdate_is_a_minute_and_never_an_instant` (mutation 2026-11-15, the round trip removed: red). Unknown: how GNW spells a day before the 10th; the 20 items of 2026-11-14 are all day 14. Today `Sun, 01 Nov 2026 19:00 GMT` is a minute and `Sun, 1 Nov 2026 19:00 GMT` is `NoParseableValidTime` (chrono reads both; only `01` prints back): `a_single_digit_day_is_a_minute_only_when_zero_padded` (mutations 2026-11-15, the round trip removed / the reprint under `%-d`: each red). The rule changes only after a real GNW `pubDate` on days 1-9 is recorded.
    - Built 2026-11-16, PRN: `:00` and `+0000` are literals of the format, so a `pubDate` with a non-zero second or another offset is `NoParseableValidTime`/`IndexRow`, never truncated to its minute and never an instant. Cost: that story is not held and is fetched by every poll while listed (the listing-unusable row of the held table below); observed 0 of 40.
    - Measured 2026-11-16, construction: every item `<pubDate>` of both PRN feeds parsed with Python's ElementTree: 20 of 20 in the 2026-11-14 feed (`date` 19:21:04 GMT) and 20 of 20 in the 2026-11-16 feed (request 08:04:39Z) have the shape `Ddd, DD Mon YYYY HH:MM:00 +0000`.
    - Measured 2026-11-16: the item `:00` is a minute printed with a zero second: the channel `<pubDate>` of the same feeds carries real seconds (19:20:24, 08:02:47), and the 2026-11-16 `x-feed-generated-at` header carries milliseconds (08:02:47.250Z). The pages agree: 302939553's `meta name="date"` is `2026-11-14T17:15:00Z` and 302938569's `17:01:00Z`, the `pubDate` of RSS items 3 and 4.
    - Built 2026-11-16: the loader's `reads_a_minute` accepts the format, because seconds 0 and 31 print the same text.
    - Built 2026-11-16: `a_pr_newswire_pubdate_is_a_minute_and_never_an_instant` (17:15:30, offset `-0500` and year `26` each refused; mutations 2026-11-16: `%S` for `:00` with `reads_a_minute` accepting any format and `minute_of` truncating to the minute: red; the round trip removed: red).
    - Built 2026-11-16, unknown: how PRN spells a day before the 10th; only days 14 and 16 were observed.
    - Built 2026-11-17, CNW: `news:publication_date` is an instant with an offset, read by `minute_of` at that offset and stored as its UTC minute; `valid_raw` keeps the text with its offset. `:00` is a literal of the format, so a value with a non-zero second, a `Z` or no seconds field is `NoParseableValidTime`/`IndexRow`. The loader's `reads_a_minute` prints its probe minute at offset zero, so a format holding `%:z` passes it.
    - Measured 2026-11-16, construction in §10's CNW table: 2,600 of 2,600 sitemap values have the shape `YYYY-MM-DDTHH:MM:00-05:00`; the pages agree: on all 11 CNW page files `meta name="date"` is the sitemap minute in UTC (805423040: `2026-11-16T07:22:00Z` for `02:22:00-05:00`).
    - Built 2026-11-17: `a_canada_newswire_publication_date_is_a_minute_at_its_own_offset_and_never_an_instant` (the 3 fixture values, `-04:00` and `+00:00` read as their UTC minute; second 30, `Z`, no seconds field and year `26` each refused; mutations 2026-11-17: the offset dropped from `minute_of`: red; the round trip removed: red).
    - Unknown: whether CNW prints `-04:00` during daylight time; only November values (`-05:00`) were observed. The format reads either.
    - Built 2026-11-17, Fed: `%-d` reads a day with no leading zero and prints it back without one; `:00` and `GMT` are literals. So a zero-padded day, a non-zero second, no seconds field, `+0000` or a two-digit year is `NoParseableValidTime`/`IndexRow`, never an instant.
    - Measured 2026-11-16, construction in §10's Fed table: 20 of 20 values have the shape `Ddd, D Mon YYYY HH:MM:00 GMT`, `D` one or two digits; 9 of 20 fall on a day before the 10th, each unpadded (`Mon, 9 Nov 2026 21:00:00 GMT`), so GNW's and PRN's `%d` would refuse 9 of 20. They run from 2026-09-25 to 2026-11-13, both sides of the US clock change of 2026-11-01, all `GMT`.
    - Measured 2026-11-16: the pages agree on 5 of 5 URLs: `For release at 4:15 p.m. EST` for `21:15:00 GMT` (bcreg20261113a), 1:30 p.m. for 18:30, 12:45 p.m. for 17:45, 11:00 a.m. for 16:00, 4:00 p.m. for 21:00.
    - Built 2026-11-17: `a_federal_reserve_pubdate_is_a_minute_with_an_unpadded_day_and_never_an_instant` (the 3 fixture values, the 5 refused shapes above; mutations 2026-11-17: `%d` for `%-d`: red; the round trip removed: red).
  - Index rows: only an element with no namespace is an RSS field; `has_tag_name(&str)` compared the local name alone, so a `dc:pubDate` or `atom:link` placed first was read instead (`a_namespaced_element_named_like_an_rss_field_is_never_read_as_that_field`; mutation 2026-11-15, the namespace check removed: red). A link whose path holds a `.` or `..` segment is filtered out (`/news-release/../rssfeed/` passed the prefix; `a_link_off_the_path_filter_off_the_host_or_absent_is_filtered_out_and_counted`, mutation 2026-11-15: red; the `.` arm alone removed: red since 2026-11-15, green before `/news-release/./x.html` was added); percent-encoded dots are not decoded. A second row keying a story an earlier row of the same index keyed is counted in `filtered_out`, not followed up (§8 row R3).
    - Built 2026-11-17, news sitemap (CNW): the root must be `urlset` in the sitemaps.org 0.9 namespace, else the answer is `PermanentFailure`; each `url` is one row, its `loc` the link and `news:news/<valid_field>` in Google's news namespace the publication value; an element of another namespace with the same local name is never read: `a_sitemap_element_named_like_a_field_in_another_namespace_is_never_read` (an `image:loc` and an `other:publication_date` placed first; mutation 2026-11-17, the namespace test dropped: red). `the_canada_newswire_index_is_its_news_sitemap_and_an_rss_answer_is_permanent`: the sitemap fixture `Usable`, an empty `urlset` `Empty`, PRN's RSS at the CNW url `PermanentFailure`, the sitemap at PRN's url `PermanentFailure` (mutation 2026-11-17, the `cnw` rule's `index` `rss`: red).
    - Built 2026-11-17, Fed: the feed opens with a UTF-8 byte order mark, ends its lines CRLF and wraps `link`, `guid` and `pubDate` in CDATA (the p0800 body, 2026-11-16); `roxmltree` 0.21.2 reads all three, so no reader changed: `the_real_federal_reserve_feed_yields_one_follow_up_per_release_with_its_raw_pub_date` reads the byte-for-byte cut feed. The whole p0800 body gives 20 follow-ups, 20 minutes and `filtered_out` 0 (`OpenWebFeed::extract`, temporary test, 2026-11-17).
  - Body: the container's lines in document order, one per outermost `paragraph` match and one per table row (table below), skipping `script`, `style`, `noscript` and `template` text; a line counts only if it holds a letter or a digit (U+200B alone passed before, review 2026-11-15). `paragraph_text_skips_script_style_and_noscript_and_needs_a_letter_or_a_digit` (mutations 2026-11-15, the skip removed: red; the letter-or-digit test replaced by non-empty: red).
    - Built 2026-11-16: a `br` element inside a paragraph is read as a space, then collapsed. Before, 302939553's `<b>About SIGNAL<br /></b>SIGNAL is` was stored as `About SIGNALSIGNAL is`, and the body blob digest is `body_ref`, an `item_id` input. `a_line_break_inside_a_paragraph_is_read_as_a_space` (synthetic `prn/synthetic_heading_line_break.html`; mutation 2026-11-16, the `br` arm removed: red).
    - Owner decision 2026-11-16: the extraction rules below, built 2026-11-16 in one path for every host, CNW's included since 2026-11-17 (`parse::blocks`, `rows`, `skipped`, `prose`, `collapse`). Each test reads a synthetic fixture whose head says so, except where a row names a trimmed real page; mutations 2026-11-16, each run once with `cargo test --locked --no-fail-fast -p open-web-feed`, each red on its test and the golden test:

      | rule | decision | test | mutation, red |
      |---|---|---|---|
      | 8, accepted | paragraph `p, li` on PRN and GNW; only the outermost match is read, so an `li` holding a `p` is one paragraph; block children inside one paragraph are separated by a space | `a_list_item_holding_a_paragraph_or_a_nested_list_is_one_line_read_once` (`prn/synthetic_lists.html`) | a match's own matches read too; block edges without a space; PRN's rule `p` alone |
      | 9, rejected as written | excluding a `<p>` inside a table cell (contact tables) from the body; tables are read instead (P2) | — | — |
      | P1 | an element of class `__cf_email__`, and an `a` whose `href` holds `/cdn-cgi/l/email-protection`, is skipped with its text | `a_cloudflare_placeholder_and_an_address_never_reach_the_body_and_a_mailto_name_does` (`prn/synthetic_addresses.html`, both shapes and a classless link; named `a_cloudflare_email_placeholder_and_a_mailto_link_never_reach_the_body` before r) | the class check removed; the path check removed |
      | P2 | a table is read row-wise: one line per `tr`, cells joined by ` \| `; a nested table is flattened into its outer cell; a cell's `p` is read once, in its row; no marker says a line came from a table | `a_table_is_read_one_line_per_row_and_a_nested_table_inside_its_outer_cell` (`prn/synthetic_tables.html`) | tables not read by row; empty cells kept; the caption not read; a table in a matched `li` read into that line; the skip on a `__cf_email__` cell removed (review 2026-11-16; before, the golden test alone went red) |
      | P3 | extraction is maximal; cutting text belongs to `derive`; one exception, r | — | — |
      | r, owner decision 2026-11-16, an exception to P3 | every email address is removed from body text on every host, in `parse::collapse` (P6), by the research regex `[A-Za-z0-9._%+'-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}` (`parse::without_addresses`, no regex crate); names, phone numbers and labels such as `E-mail:` stay; owner's words: "jesli Cloudflare pozostawia telefony a usuwa tylko maile, to tez tak zrobmy" (if Cloudflare keeps phones and removes only emails, do the same) | `an_address_in_link_text_or_plain_text_leaves_a_globenewswire_body_and_the_contact_stays` (`gnw/article_3403177.html`, `gnw/article_3403225.html`, trimmed real, names, phone digits and addresses replaced by `example.com` ones of the same shape); the P1 test; `parse::tests::every_address_the_regex_matches_is_removed_and_nothing_around_it` (expected values from Python `re.sub` with the regex, 2026-11-16); `parse::tests::a_handle_a_profile_url_and_a_bare_at_sign_are_kept`; the joined-letters limit below: `letters_joined_to_an_address_with_no_space_are_removed_with_it` (`prn/synthetic_joined_address.html`), `parse::tests::letters_joined_to_an_address_are_matched_as_part_of_it` | the scrub removed: red on the r test, the P1 test, the P6 test and the golden test; top label of 1 letter: red on the regex-cases test; the local part read across an earlier match: red on the regex-cases test; `%` or `_` dropped from the local part: red on the regex-cases test (review 2026-11-16; before, the golden test alone) |
      | `mailto:` skip, dropped 2026-11-16 by r | an `a` whose `href` starts `mailto:` is no longer skipped: its visible text is read and the scrub removes any address in it | the P1 test: `Contact <a href="mailto:…">Jane Example</a>` reads `Contact Jane Example` | the skip restored: red on the P1 test and the golden test |
      | P6 | one shared code path for every host; only the container differs | `a_globenewswire_page_reads_lists_tables_and_addresses_through_the_same_path` (`gnw/synthetic_lists_tables_and_addresses.html`) | a match's own matches read too; tables not read by row; the class check removed |
      | s | `figcaption` is a paragraph match on PRN and GNW; the outermost match only, as in 8 | `a_figure_caption_is_one_line_in_document_order_on_a_pr_newswire_page` (`prn/article_302939742.html`, trimmed real: lead, 2 of the release's 4 figures, 1 later paragraph); GNW: the P6 test, whose fixture holds one `figcaption` | PRN's paragraph `p, li`; GNW's paragraph `p, li` (red on the P6 test) |

      - Rejected for r: keeping the `mailto:` skip beside the scrub. It removes a link's visible text, so a name shown as a `mailto:` link is lost, against "only emails"; where the text is an address, the scrub already removes it, so the skip changes no body the scrub leaves. It fired on none of the PRN, GNW or CNW pages: `grep -il mailto:` over the 34 research and 27 probe7 page files (2026-11-16) hits 5 files, all Yahoo or businessinsider, which have no rule.
      - Rejected for r: skipping an `a` whose text or `title` is an address. It left 4 plain-text addresses (3 `first.last`) on 4 of the 15 GNW pages, and with the scrub beside it no test can go red on it (review 2026-11-16). A `[email]` token: PRN and CNW, whose Cloudflare skip emits nothing, would differ.
      - r measured 2026-11-16 on 57 page files (the 34 research pages, probe7 r1/r2 and the fidelity `o-*` originals; CNW read under PRN's rule), each through `OpenWebFeed::extract` by a temporary test not kept, at PR head 718101e (before) and with r (after). Address count: the regex above over the body; other change: the before body with every match removed, re-collapsed and lines with no letter or digit dropped, compared with the after body. This compares the scrub with the regex over the same body; it cannot see text the regex itself takes with an address (the joined-letters limit below).

        | quantity | GNW | PRN | CNW (PRN rule) |
        |---|---|---|---|
        | unique URLs / page files | 15 / 23 | 20 / 28 | 4 / 6 |
        | URLs with an address, before | 10 | 0 | 0 |
        | addresses, unique URLs, before → after | 15 → 0 | 0 → 0 | 0 → 0 |
        | addresses, page files, before → after | 20 → 0 | 0 → 0 | 0 → 0 |
        | page files whose body changes otherwise | 0 | 0 | 0 |
        | body characters, unique URLs, before → after | 155,655 → 155,282 | 126,370 → 126,370 | 68,004 → 68,004 |
        | lines dropped (the line held only an address; 3403228) | 1 | 0 | 0 |

        - The one `@` left in any body is `Follow @TimHortonsChina` (GNW 3403223), not an address.
        - Not measured: whether an address sits in a link's `title` or `href` alone with a name as text on any host; neither is read, so neither reaches the body.
        - Limit, not guarded against: `prose` joins inline elements with no space and the regex's local part and `[A-Za-z]{2,}` top label run on into letters, so letters joined to an address go with it. `Media Contact<a href="mailto:…">jane@example.com</a>` read `Media Contact` at 718101e (the `mailto:` skip) and `Media` with r; `…</a>Tel: +1 …` reads `: +1 …`. Pinned by the two joined-letters tests in the r row.
        - That limit on real pages, 2026-11-16: the character before and after each of the 20 matches in the before bodies of 58 page files (the 57 above plus `legacy-cnw-canaccord`, which has no body) is whitespace, end of text, or a `.` then a space; 0 joined.
        - An address inside a sentence leaves its punctuation: GNW 3403231 reads `…to the Takeover Panel at . The Panel's…` (1 of the 20 matches), the shape P1 already leaves on PRN (`Press: or ,`).

    - Measured 2026-11-16 on the 34 PRN and GNW article pages fetched 2026-11-16 09:42–09:45Z (§3.9's compression table; not kept in the repository), each read through `OpenWebFeed::extract` under the committed rules, by a temporary test not kept. Coverage: characters of the container's trimmed text nodes outside `script`/`style`/`noscript`/`template` that sit under a `p` (before) or under a `p`, `li` or `table` and under no skipped element (after), over all such characters. Placeholder: the text `[email protected]` with a space or a no-break space.

      | quantity | PRN, 19 pages | GNW, 15 pages |
      |---|---|---|
      | coverage, median, before → after | 0.969 → 0.979 | 0.757 → 1.000 |
      | coverage, lowest page, before → after | 0.166 → 0.954 | 0.029 → 1.000 |
      | body characters, summed, before → after | 102,717 → 122,354 | 67,114 → 155,655 |
      | placeholders in bodies, before → after | 21 → 0 | 0 → 0 |
      | lines from tables after (body lines less outermost `p`/`li` lines) | 272 | 1,593 |

      - Baidu's PRN release 302940183: 1,225 lines before (every `p`, nested ones included), 268 after, 206 of them table rows; 26,476 → 33,272 characters.
      - By construction, skipped address text and text outside every `p`, `li` and `table` count as not covered. Where the uncovered PRN text sits, 2026-11-16: non-whitespace characters of the container's text nodes outside `script`/`style`/`noscript`/`template`, from a review's per-node dump through `scraper` of the 19 PRN pages:

        | where | text nodes | characters |
        |---|---|---|
        | under a `p`, `li` or `table`, read | 2,394 | 101,999 |
        | `figcaption` (image captions in `div.inline-gallery`), read since decision s | 15 | 701 |
        | Cloudflare placeholder, skipped (P1) | 21 | 336 |
        | an `a` outside every `p`, `li` and `table` (`Continue Reading`) | 11 | 165 |
        | anything else | 0 | 0 |

      - GNW, the same construction over its 15 pages: every character sits under a `p`, `li` or `table`.
      - Measured 2026-11-16 after decision s, the same 34 pages through `OpenWebFeed::extract` by a temporary test not kept, under the committed rules and under a copy with paragraph `p, li`; characters are Python `len` of the body text:

        | quantity | PRN, 19 pages | GNW, 15 pages |
        |---|---|---|
        | pages whose body changes | 11 | 0 |
        | lines added | 15 | 0 |
        | body characters, summed, `p, li` → `p, li, figcaption` | 122,354 → 123,190 (+836; 701 not whitespace) | 155,655 → 155,655 |
        | pages whose first line changes | 0 | 0 |

      - Where the 15 GNW addresses of r sat before it: 11 the text of a `www.globenewswire.com/Tracker?data=…` link, 10 of them with the address also as `title`; 4 plain text in a `p` (2026-11-16, html5ever ancestor chains of the address nodes).
      - A negative number whose `)` sits in a cell of its own is read `(121,809 | )` (GNW 3403224's `Operating loss` row): 192 times in the 34 bodies (2026-11-16, count of ` | )`), for `derive` to read as −121,809.
      - The 6 PRN and GNW URLs fetched twice 30 s apart (§3.9, r1 and r2) give one body each under the new rules, 6 of 6, with 0 placeholders.
  - A page whose bytes are not UTF-8 is `PageNotUtf8`; it was decoded lossily and its `meta charset` is never read (`a_page_that_is_not_utf8_is_unusable_and_never_decoded_lossily`). A page matching the container selector more than once is `ArticleContainerAmbiguous`; only the first was read (`a_page_holding_the_article_container_twice_is_unusable_as_ambiguous`). Each red under its mutation, 2026-11-15. The untrimmed release 3403075 holds one `div.main-body-container`, as the trimmed fixture does (measured 2026-11-15 with `scraper` on the 2026-11-14 probe body, 71,657 B, HTTP 200; that body is not kept in the repository).
    - Built 2026-11-16, PRN: `section.release-body` (class `release-body container`) occurs once on each of the 2026-11-14 probe pages 302939553 and 302938569 (198,912 B and 212,991 B, both valid UTF-8, not kept in the repository). `<article>`, the §11.6 probe's rule, adds 2 `<p>` per page: the dateline (`Nov 14, 2026, 12:15 ET`; `12:01 ET`) and `Share this article` (§8 row R17).
    - Built 2026-11-16, PRN: 302939553's section opens with a `<ul>` of 3 summary `<li>` before its first `<p>`; `<article>` and paragraph `p` did not read them. Since 2026-11-16 decision 8 reads them as the body's first 3 lines: `the_extracted_body_is_article_text_and_never_page_html_on_a_pr_newswire_page`.
    - Built 2026-11-17, CNW: the same Cision template as PRN (`<article class="news-release inline-gallery-template">`, `section.release-body` holding `div.release-content`), so the same container. `section.release-body` and `div.release-content` each occur once on all 11 CNW page files of 2026-11-16 (`grep` of the class attributes, 2026-11-17); the inline gallery's `figcaption` lines sit inside `div.release-content`.
      - `the_extracted_body_is_article_text_and_never_page_html_on_a_canada_newswire_page` (trimmed real 805423040: the lead, then 2 gallery captions in document order, `Continue Reading`, the dateline, the share line, the ld+json and `nav-main`'s `<p>` absent; mutations 2026-11-17, the container `body`, `article` and `section`: each red).
      - `a_canada_newswire_page_with_no_release_body_is_unusable_as_no_article_body` (synthetic: a bare `div.release-content` outside any section; mutations 2026-11-17, the container `article` and `div.release-content`: each red). No real page tells the two inner selectors apart (§10's CNW table), so only this synthetic page pins `section.release-body`.
    - Measured 2026-11-17 on the 11 CNW page files of 2026-11-16, 7 URLs: probe6's p0800 bodies 1-5, probe7's r1 and r2 of the first two, and the fidelity originals of 818939118 and 832568593 (not kept in the repository); each read through `parse::page` under the committed `cnw` rule by a temporary test not kept. Coverage: non-whitespace characters of `section.release-body`'s text nodes outside `script`/`style`/`noscript`/`template` that sit under a `p`, `li`, `table` or `figcaption` and under no skipped element, over all such characters. Body characters: `chars().count()` of the extracted body, `\n` included.

      | release | files | page B | body lines | body chars | coverage | `nesting_depth`; html5ever depth |
      |---|---|---|---|---|---|---|
      | 805423040 | 3 | 184,065 | 17 | 2,952 | 2,557 / 2,588 = 0.988 | 21; 21 |
      | 823884942 | 3 | 179,123 | 11 | 3,039 | 2,592 / 2,623 = 0.988 | 21; 21 |
      | 807505308 | 1 | 176,347 | 19 | 5,325 | 4,545 / 4,577 = 0.993 | 21; 21 |
      | 832285478 | 1 | 180,630 | 17 | 3,932 | 3,341 / 3,372 = 0.991 | 21; 21 |
      | 854074945 | 1 | 179,159 | 10 | 3,352 | 2,855 / 2,871 = 0.994 | 21; 21 |
      | 818939118 | 1 | 199,440 | 53 | 23,107 | 19,701 / 19,717 = 0.999 | 21; 21 |
      | 832568593 | 1 | 277,497 | 152 | 38,906 | 32,831 / 32,847 = 0.9995 | 21; 21 |

      - Left out: a 12th file naming the host, fidelity's `legacy-cnw-canaccord`, HTTP 301 with 0 B of body, url `/en/releases/archive/November2026/15/c2235.html`, outside `/news-releases/`.
      - Not covered, the 7 URLs: 3 `Continue Reading` link texts and 8 Cloudflare placeholders (P1); nothing else.
      - Placeholders and `@` in the 11 bodies: 0 and 0. The 3 files of each of the first two URLs give one body each (6 of 6, equal digests).
      - `div.release-content` gives the same body as `section.release-body` on 11 of 11 files; `article` adds 2 lines on 11 of 11.
      - Before r, the 2026-11-16 measurement read 4 CNW URLs under PRN's rule (the r table above); this one supersedes it for CNW's own rule.
    - Built 2026-11-17, Fed: the container is the release column, `div#article > div.col-md-8:not(.heading)`. `div#article` holds a heading block (`div.heading.col-md-8`: date, `h3` title, issuing agencies, release time, share menu), an empty `div.hidden-sm`, the release column, and a right-hand column (the Related Information panel on bcreg20261113a). The column holds the release paragraphs, the attachments panel and the media contacts block.
      - Counted by `grep` of the class attributes on the 9 files (2026-11-17): `div#article` and the heading block 9 of 9; the attachments panel (`panel-attachments … col-md-8`, inside the column) 8 of 9, not monetary20261112a; the issuing-agency list and the Related Information panel 3 of 9, bcreg20261113a's files.
      - `the_extracted_body_is_article_text_and_never_page_html_on_a_federal_reserve_page`: trimmed real bcreg20261113a gives the 4 release paragraphs, then the 2 attachment links, and none of the date, agencies, release time, `Share`, the related release's title, the `nav` paragraph, the breadcrumb, the page title or the media contacts; trimmed real orders20261109a is read whole, its address a Cloudflare placeholder. Mutations 2026-11-17, the container `body`, `div#article`, `div#article > div.col-md-8` (also the heading block: ambiguous) and `div.col-md-8:not(.heading)` (also the attachments panel: ambiguous): each red.
      - `a_federal_reserve_page_with_no_article_block_is_unusable_as_no_article_body` (synthetic: the heading block and a release column under another `div`; mutations 2026-11-17, the container `body` and `div.col-md-8:not(.heading)`: each red).
      - P6 holds with no Fed selector: the paragraph is `p, li, figcaption` and the container alone keeps the heading, the right-hand column and the nav out. The media contacts are `div` cells, which no paragraph match reads.
      - §11.6's body probe found no plausible container (0 of 5) because it measured shares of all page paragraphs: the 40 `nav` paragraphs sit outside `div#article`, so they lower that share and never reach the body.
    - Measured 2026-11-17 on the 9 Fed page files of 2026-11-16, 5 URLs: probe6's p0800 bodies 1-5 and probe7's r1 and r2 of the first two (not kept in the repository); each read through `OpenWebFeed::extract` under the committed `fed` rule, and under a copy with container `div#article`, by a temporary test not kept. Coverage and body characters as the CNW table above, over the container's text nodes; html5ever's depth counts `html`.

      | release | files | page B | body lines | body chars | coverage | `div#article`: lines; coverage | `nesting_depth`; html5ever depth |
      |---|---|---|---|---|---|---|---|
      | bcreg20261113a | 3 | 85,671 | 6 | 1,650 | 1,428 / 1,568 = 0.911 | 14; 0.884 | 13; 12 |
      | other20261112a | 3 | 85,668 | 7 | 1,981 | 1,688 / 1,704 = 0.991 | 10; 0.899 | 13; 12 |
      | monetary20261112a | 1 | 82,881 | 12 | 770 | 668 / 684 = 0.977 | 15; 0.872 | 13; 12 |
      | enforcement20261112a | 1 | 83,059 | 5 | 383 | 332 / 348 = 0.954 | 8; 0.795 | 13; 12 |
      | orders20261109a | 1 | 83,168 | 4 | 446 | 378 / 394 = 0.959 | 7; 0.787 | 13; 12 |

      - Not covered, the 5 URLs: 4 Cloudflare placeholders (P1, 16 characters each) and bcreg20261113a's media contacts block (140 characters: its `h5` and 12 `div` cells naming 4 agencies, 4 spokespeople and their numbers); nothing else.
      - Lines `div#article` adds: the date, the release time and `Share` on 5 of 5 URLs; on bcreg20261113a also the 4 issuing-agency `li` and the Related Information `p`, the title of another release (`Agencies seek comment on proposed third-party risk management guidance …`).
      - Placeholders and `@` in the 9 bodies: 0 and 0. The 3 files of each of the first two URLs give one body each (6 of 6, equal digests).
      - The scan counts 1 deeper than html5ever on all 9 files: the heading's `p.releaseTime` is never closed and a `ul` follows, which html5ever's tree closes and the scan does not (`<div><p>x<ul><li>y</li></ul></div>`: 4 against 3, temporary test 2026-11-17).
      - A release names a person where its subject is one: enforcement20261112a's second line names the individual the action is against. P3 keeps release text and r removes addresses only. It is not a fixture.
    - Built 2026-11-17, Yahoo: the container is `div[data-testid="article-body"]`, the probe's (§11.6), with no Yahoo selector or skip rule (P6 holds). Inside it: `div.bodyItems-wrapper` (the first paragraphs), `div.readmore` (the `Story Continues` button), `div.read-more-wrapper` (`display: none`, the rest of the body, in the server HTML) and `div[data-testid="inarticle-ad"]` slots.
      - The button is a `button` holding a `span`, and an ad slot holds only `div` elements (15 of 16 pages; the 16th has no slot), so no paragraph match reads either; the paid-release disclaimer is outside the container on 16 of 16. Read 2026-11-17 by a Python `html.parser` walk, not kept.
      - Yahoo adds inside the body, kept as body lines: a logo caption (`figcaption`, decision s) and, on PRN and CNW reprints, a `Cision` caption and a `View original content…` line. Neither is on a followed key's page here: the one `business-wire.com` page ends with BW's own `View source version on businesswire.com: …` line.
      - Measured 2026-11-17 on the 16 Yahoo page files of 2026-11-16 (probe7's 5 r1 pages, the fidelity run's 11 `y-` pages; not kept in the repository), each read through `OpenWebFeed::extract` under the committed `yahoo` rule by a temporary test not kept; 16 of 16 usable, none `PageTooDeep`:

      | page | key | page B | `Story Continues` buttons | ad slots | body lines | body chars, line breaks not counted |
      |---|---|---|---|---|---|---|
      | Laverock | `business-wire.com` | 964,940 | 1 | 3 | 15 | 3,860 |
      | Horizon | `prnewswire.com` | 962,896 | 1 | 4 | 21 | 6,245 |
      | Baidu Q3 | `prnewswire.com` | 2,086,568 | 1 | 10 | 269 | 33,113 |
      | Hapoalim Q3 | `prnewswire.com` | 997,003 | 1 | 4 | 50 | 13,202 |
      | Ericsson, two urls | `prnewswire.com` | 1,006,651; 931,398 | 1; 1 | 2; 2 | 28; 28 | 3,302; 3,299 |
      | Niu Q3 | `globenewswire.com` | 1,409,354 | 1 | 9 | 177 | 23,072 |
      | H World Q3 | `globenewswire.com` | 1,939,115 | 1 | 17 | 384 | 42,837 |
      | Danske, GNW 3403184, Sydbank buybacks | `globenewswire.com` | 1,019,827; 1,023,107; 984,094 | 0; 0; 0 | 1; 1; 0 | 37; 36; 17 | 2,738; 3,669; 1,836 |
      | Advantage, Canaccord | `cnwgroup.com` | 1,016,631; 1,252,441 | 1; 1 | 11; 17 | 53; 152 | 22,747; 38,606 |
      | Fool reprint | `motleyfool.com` | 1,002,016 | 1 | 5 | 25 | 5,252 |
      | Nvidia, Shenzhen animation | none | 991,493; 948,262 | 0; 1 | 2; 2 | 14; 11 | 2,854; 3,842 |

      - `Story Continues` in 0 of 16 bodies, on 12 pages; `Advertisement` in 0 of 16 bodies.
      - Keys from the 26 polls' sitemaps (§11.6). Only Laverock is under a followed key; no `accesswire.ca` page was fetched.
      - Baidu Q3 is 2,086,568 B, 10,584 B under `BODY_LIMIT` (2,097,152 B); a page over it is a `PermanentFailure` stored cut (§3.9). Its key is not followed; no followed-key page measured near the limit.
  - Depth bound: `PageTooDeep` past 112, read by `depth::nesting_depth` from the source tags before html5ever parses. html5ever is superlinear in nesting: 2,000 / 4,000 / 8,000 nested `<div>` parsed in 0.011 / 0.027 / 0.070 s (release, 2026-11-15), about ×2.5 per doubling; a second run that day: 0.009 / 0.024 / 0.068 s.

    | quantity, 2026-11-15 unless the row names another date, release build, one run each | value |
    |---|---|
    | `nesting_depth` of `article_3403075.html` (trimmed); html5ever's element depth of it | 7; 7 |
    | untrimmed release 3403075, the 2026-11-14 probe body (71,657 B, not kept in the repository): `nesting_depth`; html5ever's element depth (debug test build) | 14; 14 |
    | 2026-11-16, debug test build: untrimmed PRN 302939553 / 302938569, the 2026-11-14 probe bodies (not kept): `nesting_depth`; html5ever's element depth | 21 / 21; 21 / 21 |
    | 2026-11-16, debug test build: trimmed `prn/article_302939553.html` / `prn/article_302938569.html`: `nesting_depth`; html5ever's element depth | 10 / 9; 10 / 9 |
    | 2026-11-17, debug test build: the 11 untrimmed CNW page files of 2026-11-16 (table above): `nesting_depth`; html5ever's element depth | 21; 21, each |
    | 2026-11-17, debug test build: trimmed `cnw/article_805423040.html` / `cnw/article_823884942.html`: `nesting_depth`; html5ever's element depth | 15 / 15; 15 / 15 |
    | 2026-11-17, debug test build: the 9 untrimmed Fed page files of 2026-11-16 (table above): `nesting_depth`; html5ever's element depth | 13; 12, each |
    | 2026-11-17, debug test build: trimmed `fed/article_bcreg20261113a.html` / `fed/article_orders20261109a.html`: `nesting_depth`; html5ever's element depth | 12 / 12; 12 / 12 |
    | bound: 14 × 8, so 8 times the untrimmed GNW page's depth; not recomputed for PRN, CNW or the Fed, whose 21, 21 and 13 are below it | 112 |
    | a 2 MiB page of closed 112-deep `<div>` blocks (2,096,864 B): scan / parse | 0.007 s / 0.061 s |
    | a 2 MiB page of 419,430 open `<div>`: scan, before the parse is refused | 0.008 s |

    - The scan's first version was itself quadratic: the same 2 MiB page took 346.6 s, because each tag searched the open stack for `svg`/`math`; each entry now carries that count.
    - The count is meant never to fall below html5ever's depth less the implied `html` and `body`: an end tag closes its element only when it is on top or below implied-end elements (`p`, `li`, ...) alone, and is otherwise ignored, as html5ever ignores an end tag naming no open element; raw text is skipped except inside `svg`/`math`; a comment ends where html5ever ends it (`<!-->`, `--!>`); a `>` inside a quoted attribute does not end a tag. `depth::tests::each_rule_of_the_scan_counts_its_own_row`, 17 rows; 14 mutations 2026-11-15, one per rule: 14 red; the stray-end-tag rule's mutation (the implied-end elements popped although the name is not open): red on its 2 rows.
    - Before that rule (review 2026-11-15, release build, one run each): `<rb><rt></x>` repeated 16,000 times (192,000 B) counted 2 while html5ever held 32,002 and parsed in 1.78 s; `<ul><li></x>` × 300 counted 301, html5ever 602. Both now count 600 for 300 rounds.
    - `a_page_nested_past_the_bound_is_unusable_and_one_at_the_bound_is_read` (mutations 2026-11-15, the check removed: red; `>=` for `>`: red).
    - Unguarded: html5ever's reconstruction of active formatting elements adds nodes no source tag opens. Probe 2026-11-15: 800 rounds of `<p><b x=i>t</p>` (13,490 B) built 322,004 nodes, depth 804, in 0.154 s; the scan counted 1,600 and refuses it, but no general rule bounds that reconstruction.
    - Sloppy markup (`<span><b>x</span>`) counts deeper than html5ever's tree, so a real page could be refused. The `PageTooDeep` share per host cannot be read: no `UnusableReason` is stored, only the count `unusable_rows` in the run envelope and, per attempt, `attempts.listing_unusable`, which names no reason (grep of `newsvault`'s store and envelope, 2026-11-15).
  - `universe_version` is the digest of zero bytes. A follow-up whose story is held is dropped (`OpenWebFeed::follow_ups_to_issue`); one with no `story_of` is issued: `open_web_feed::a_follow_up_with_no_story_is_issued_and_a_held_one_dropped`, a port-bearing url built by hand beside a held one (mutation 2026-11-15, `is_none_or` made `is_some_and`: red). `index_rows_of` cannot emit such a follow-up: every url it builds is `https://{host}{path}` from a pair `rewritten` already accepted. User-Agent by credential name `open_web_contact`; `cmd_collect::Registered::credential` names the same credential for each provider: `every_registered_credential_is_the_one_its_providers_user_agent_names` (mutation 2026-11-15, the open-web entry naming edgar's credential: red, green before the test).
  - Conformance: `the_open_web_feed_breaks_no_claim_of_the_seam` (harness over the fixtures, plus every produced item at exactly the ceiling: mutation 2026-11-15, ceiling `Instant`: red).
- Live only: `plan` refuses any target that is not the run's UTC day, before any request; no `vendor_archive` request can exist.
- Every UTC day is collected, weekends included. Investor-relations pages are never fetched. Observation time is `Wire`'s `received_at`: `an_open_web_run_collects_a_globenewswire_day_end_to_end`, whose stub receives one second after it sends (mutation 2026-11-15, `collect` observing at `requested_at`: red; green before the stub told the two apart). The store refuses an observation time set by the provider: `Committer::rows_of` in `cmd_collect.rs`, `StoreError::LiveItemPlacedItsOwnObservation`, `a_live_row_that_placed_its_own_observation_is_refused`.

| host | valid time |
|---|---|
| PRN, GNW, Fed | RSS `pubDate`, `ValidSource::VendorPublished` |
| CNW | sitemap `news:publication_date`, a full instant with offset, `ValidSource::VendorPublished`; built 2026-11-17 (rules table above) |
| CNBC | the page's `article:published_time`, `ValidSource::OurExtraction`; never its sitemap (§11.6) |
| Yahoo | its syndication time field, `ValidSource::VendorPublished`; the field is named in its host PR: built 2026-11-17 as the sitemap's `news:publication_date` (rules table above), `Z` with second `00` on 2,017 of 2,017 followed-key rows of the 26 polls of 2026-11-16. Same minute as the wire's own feed, matched by normalised title, distinct Yahoo urls of the 26 polls (Python, 2026-11-17): `business-wire.com` 28 of 28 against BW's RSS; `prnewswire.com` 339 of 391, 15 later by more than 5 min (at most 90), 32 earlier |
| Fool, BI | the sitemap's publication field, `ValidSource::VendorPublished`; the field is named in their host PR, as Yahoo's is |
| never | `lastmod`, `dateModified`, fetch time, a date folded into midnight |

- `ValidSource::PageMetadata` stays unbuilt.
- **`ValidSource` per host, decided 2026-11-15 by the owner.** Both arms exist (`provider.rs:41-46`); `CollectedItem.valid_source` is required (`provider.rs:147`); since 2026-11-17 CNW, Fed and Yahoo items are built `VendorPublished`; no item from Fool or BI is built yet.
  - `VendorPublished`: a value read from a structured feed field the publisher states (RSS `pubDate`, sitemap `news:publication_date`, a sitemap's publication field, Yahoo's syndication time field; that field is the sitemap's `news:publication_date`, named 2026-11-17).
  - `lastmod` stays excluded (the `never` row), so only a sitemap's publication field is `VendorPublished`, and a sitemap with no publication field gives no valid time.
  - `OurExtraction`: a value read out of an article page under a hashed rule (CNBC's `article:published_time`).
  - Rejected: `OurExtraction` for sitemaps, because the arm would then mean "we parsed XML" and no longer separate "the publisher stated it" from "we inferred it from the page".
- Bot protection (HTTP 403, a challenge page, a web application firewall, DataDome) is a `PermanentFailure`, never retried in another form.
- robots.txt is respected; a robots.txt 404 is recorded as "no robots" and is not digest-pinned. Sites' terms of use are not a criterion (owner, 2026-11-14).
  - Built 2026-11-15: `newsvault::robots::Robots`, our own RFC 9309 subset, no new dependency: only the `user-agent: *` groups, merged; `Allow`/`Disallow` by longest pattern, `Allow` winning a tie, `*` any run of characters, a final `$` the end; the largest `Crawl-delay` of those groups, in decimal seconds, a non-zero digit past the ninth rounding up one nanosecond so a delay above 10 s never reads as 10 s; a line ends at CR, LF or CRLF (RFC 9309 §2.2), a leading byte order mark is skipped as §10 row "key and line spellings" (e) states, a `#` cuts the rest of a line, an empty `Allow`/`Disallow` value is skipped; since 2026-11-17 a line keeps its first 16,663 bytes and ends at its first NUL, a pattern is escaped and an `index.htm` `Allow` also allows its directory, as Google's robots.cc does (§10 row "key and line spellings"). Refused: a `*` `Crawl-delay` that is not digits with an optional fraction; since 2026-11-17 a body that is not UTF-8 is read as bytes (§10 (e)). Unknown lines are ignored. Key and line spellings, grouping, empty, no-group, HTML and orphan bodies: the §10 robots rows (owner decisions 2 and 3, the orphan-line decision and recommendations #1 and #2, 2026-11-16).
  - Tests in `services/newsvault/src/robots.rs`, over the 2026-11-14 18:40:10 GMT probe bodies of `www.nasdaq.com` (`*` Crawl-delay 30, bingbot 10), `www.accessnewswire.com` (5) and `www.prnewswire.com` (none), copied byte for byte to `services/newsvault/tests/fixtures/robots/` (PRN's moved 2026-11-16 to `providers/open-web-feed/tests/fixtures/prn/robots_www_prnewswire_com.txt`, the one copy, which the PRN pin names), the GNW fixture and ten synthetic files: `the_star_group_is_read_and_a_named_bot_group_is_not`, `a_named_group_after_the_star_group_adds_no_star_rule`, `a_cr_only_or_bom_led_body_keeps_its_star_rules_and_crawl_delay` (before 2026-11-15's fix, review that day: each body parsed with no `*` rule and no delay), `a_trailing_comment_is_cut_from_a_rule_and_a_crawl_delay`, `an_empty_disallow_disallows_nothing`, `allow_wins_a_tie_and_the_longest_match_decides`, `wildcard_and_end_anchor_match_as_rfc_9309_states` (GNW's `*/news-release/*.html?pdf=1` blocks a release's PDF form, not `/news-release/`), `a_crawl_delay_that_is_not_a_number_is_refused`, `the_gnw_robots_allows_the_rssfeed_index_and_the_news_release_prefix`. Mutations 2026-11-15 (review round), each run once with `cargo test --locked --no-fail-fast -p newsvault --lib robots`: `lines()` restored; the BOM strip, the group reset, the comment cut and the empty-value skip each removed: 5 of 5 red, each its named test only.
  - Built 2026-11-17, CNW: `the_cnw_robots_allows_the_news_sitemap_and_the_news_releases_prefix` (`/sitemap-news.xml?page=1` and `/news-releases/` allowed, `/templates/get-bitly-url.txt` disallowed, no Crawl-delay; mutations 2026-11-17, every `Disallow` read as `Allow`: red; `Disallow` lines skipped: red). `the_committed_canada_newswire_row_asks_its_sitemap_with_its_query_and_its_prefix` (`open-web-feed`): the row's admitted paths are those two. The body (251 B, LF, no byte order mark) has one `User-Agent: *` line, on line 1; its `Sitemap` lines are no rules; no Crawl-delay, so CNW's `per_host` stays 10 s.
  - Built 2026-11-17, Fed: the robots path answered 404 twice (§11.1's hosts table), so the row pins `absent` and `collect` reads it as no robots (`PinnedRobots::NoRobots`): no `Disallow`, no Crawl-delay, `per_host` 10 s. No `robots.rs` test, because no body is parsed. `the_committed_hosts_file_refreshed_with_its_committed_bodies_matches_every_pin` answers the Fed url 404 with the trimmed real page and expects `absent`, `no_rules_absent` and a matching pin; `an_admitted_row_pinned_absent_is_admitted_only_by_a_ledgered_404` (step 3) is the rule's own test. `the_committed_federal_reserve_row_asks_its_feed_and_its_prefix` (`open-web-feed`): the row's admitted paths are `/feeds/press_all.xml` and `/newsevents/pressreleases/`.
  - Built 2026-11-16, PRN: `the_prn_robots_allows_the_rss_index_and_the_news_releases_prefix` (`/rss/news-releases-list.rss` and `/news-releases/` allowed, `/multivu/` disallowed, no Crawl-delay; mutations 2026-11-16, every `Disallow` read as `Allow`: red; `Disallow` lines skipped: red). The body (3,457 B, LF line ends, no byte order mark) has one `User-Agent` line, `User-Agent: *` on line 1, so no rule precedes a group; its 8 `Disallow` lines sit in that group; it has no Crawl-delay line, so PRN's `per_host` stays 10 s and §8 row R19 hole 3 does not apply to it.
  - Built 2026-11-16, owner decisions 2 and 3, tests in `robots.rs`: `a_crawl_delay_does_not_end_a_user_agent_run` (the B2 body, and bingbot's run before `*`: 30 s and `/prywatne/` disallowed in both; `synthetic_star_shares_a_group.txt` now gives `*` googlebot's `Disallow: /`), `an_empty_or_comment_only_body_reads_as_no_rules`, `an_html_body_is_refused`, `a_rule_before_the_first_user_agent_is_ignored`, `an_orphan_allow_widens_nothing` (the orphan-line decision), `every_verdict_has_its_own_name`; recommendations #1 and #2: the tests their §10 rows name; through `collect`: `a_pinned_robots_body_that_does_not_parse_is_refused` (HTML refused; no-group text and a comment-only body admitted); through `robots refresh`: `robots_refresh.rs::a_refresh_reports_the_parse_verdict_of_each_answered_row` (all 9 verdicts and `null`, JSON and human, equal to the schema's list). Mutations: §11.5 step 3.
  - Re-check 2026-11-16, recommendations #1 and #2: `Robots::parse` at main 1332a46 and at 3f46223, over the 7 bodies probe6 saved that morning (`run-2026-11-16/robots/*.body`) and the 14 committed robots bodies (12 under `services/newsvault/tests/fixtures/robots/`, GNW's and PRN's under the provider), printing each `Debug` result (every `*` rule, the Crawl-delay, the reading); a temporary test file, not kept (CONTRIBUTING rule 3). All 21 results identical; no admitted or planned host changes.
    - Why nothing moved: none of the 21 bodies has a typo key, a colon-less line or a `User-agent: *` followed by more (a `grep` of each body, comments cut, 2026-11-16); every non-HTML body has a `user-agent` line.

    | body | both readings |
    |---|---|
    | GNW, PRN, CNW, Business Wire, Yahoo, Business Insider (probe6, 200) | `rules`, no Crawl-delay |
    | Business Wire: 143 `user-agent` lines, 133 `Crawl-delay: 10` lines in named-bot runs, `User-agent: *` on line 6 closed by its `Disallow` lines | the merged named-bot runs hold no `*` line |
    | `www.federalreserve.gov` (probe6, a 404 HTML page, BOM first; a 404 is `absent` and never parsed) | `HtmlPage` (3b); no line contains `agent` or `allow`, any case (`grep`, 2026-11-16), so Google's reading would be no rules |
    | `synthetic_html_page.txt` | `HtmlPage` |
    | the other 12 committed bodies | as parsed; nasdaq's 30 s is refused by `collect` (decision 1) |

  - Re-check 2026-11-17, Google parity: `Robots::parse` at main 6319f1e and at c355655, and robots.cc 50a0dac compiled with clang++, over 23 bodies (the 7 probe6 bodies, the 12 under `services/newsvault/tests/fixtures/robots/`, the provider's GNW, PRN, CNW and Fed 404 fixtures) × 24 paths (the 8 configured paths and 16 others named by the robots tests):

    | count | rows |
    |---|---|
    | before equal to after | 552 of 552 |
    | after equal to Google | 456 of 552 |
    | refused by ours (3 HTML bodies, 1 bad Crawl-delay) | 96 |

  - Re-check 2026-11-17, (e) of §10 row "key and line spellings": `Robots::parse` at main 21989d7 and at 3e6d723, and robots.cc 50a0dac compiled with clang++, over every robots body on disk (234 files: the scratchpad probe runs' `robots/*.body`, the 12 under `services/newsvault/tests/fixtures/robots/`, the provider's robots fixtures; 29 distinct bodies, none not UTF-8) × the same 24 paths:

    | count | rows |
    |---|---|
    | before equal to after | 5,616 of 5,616 |
    | after equal to Google | 5,376 of 5,616 |
    | refused by ours (3 distinct HTML bodies, 1 bad Crawl-delay; 10 files) | 240 |

  - Re-check 2026-11-17, the byte-based HTML check (§10 row "HTML"): the check at 3e6d723 and at 16062d4, each run on the body after `skip_byte_order_mark`, over 229 files (213 `robots/*.body` under the scratchpad probe runs, the 12 under `services/newsvault/tests/fixtures/robots/`, the 4 provider robots fixtures; 24 distinct bodies): the same answer for 229 of 229, 9 files `HtmlPage` under both. Only that check changed, so no other verdict can move.

  - A path is matched as given, as Google's robots.cc matches it: `%c3` and `%C3` in a path are two spellings (owner decision 4, 2026-11-17). The loaders refuse a checked path holding `%`, `#`, a non-ASCII byte or a control byte; a later per-document check uses the document path the same way, with no separate RFC 9309 reading.
  - Unguarded: a group naming our own product token is never read, because the token is `BELLWETHER_FEED_CONTACT`'s, not code's; document urls are not checked per request, so a Disallow narrower than the path prefix (`/news-release/2026/`) passes admission and its documents are fetched; the per-document check is a hard gate before the first live run (owner decision 2026-11-17, §11.5 step 7), not built.
- `item_key(url)` is the one function behind `source_item_id`; a page's canonical link feeds `doc_id` only.
  - Built 2026-11-15, default taken without the owner: one rule for an href with a scheme and a scheme-relative one (`//` gets `https:`): kept as written when it names any authority, port, userinfo and a host `HostKey` refuses included; with none (`https://`, `https:///x`) the page's url. A root-relative href is resolved against the page's url; a path-relative or non-web href, or none, leaves the page's url. Another host is kept, because `doc_id` is cross-source.
  - Rejected: the href as written, which digested the bare path `/news-release/...` (review probe 2026-11-15). Rejected: `host_and_path`'s check on either form, which replaced a `:443` canonical by the page's url, so one link spelled `https://h:443/x` and `//h:443/x` gave two `doc_id` values (reviews 2026-11-15).
  - `a_relative_canonical_link_is_resolved_against_the_page_url_before_it_reaches_doc_id`, 9 hrefs; mutations 2026-11-15: href passed as written: red; `//` sent through `host_and_path`: red; an empty authority kept: red (both green before their cases).
  - GNW's `doc_id` is not cross-source: the real page's canonical names the requested host `rss.globenewswire.com`, its `og:url` names `www.globenewswire.com` (`tests/fixtures/gnw/article_3403075.html` lines 20 and 21). No code reads `doc_id` (2026-11-15); reversing the rewrite or preferring `og:url` is picked when a second source citing GNW is built.
  - Built 2026-11-16: PRN's `item_key` is `www.prnewswire.com` plus the link's path, with no rewrite. In both PRN feeds (2026-11-14, 2026-11-16) 40 of 40 links are `https://www.prnewswire.com/news-releases/<slug>-<9 digits>.html`, with no query or fragment, `guid` equal to `link` in 40 of 40, and no link twice in one feed; 2 of the 40 slugs hold an uppercase letter (`schlieWen`, `schlieWt`). On both probe pages the canonical link and `og:url` equal the RSS link, so PRN's `doc_id` is that link. `the_real_pr_newswire_article_carries_its_title_its_canonical_link_and_its_www_item_key` (§8 row R3).
    - Measured 2026-11-16: 4 releases are each listed twice under two 9-digit ids with the same title, RSS `description`, `dc:contributor` and slug; the ids differ in all 4 pairs, and the `pubDate` too in Ericsson's (07:54 / 07:51). 8 of the 40 items. Each id is its own `item_key` and `story_id`, and its own `doc_id` while the canonical link equals the RSS link (as on both probe pages), so the 8 items are 8 stories. Unknown: whether a pair is one release; the `description` is a summary of about 296-298 chars, and no page of these ids was fetched. A near-duplicate rule is an open owner question; `item_key` is unchanged.

      | feed | ids | `pubDate` (UTC) |
      |---|---|---|
      | 2026-11-14 | SmallRig 302938896 / 302938911 | 15:00 / 15:00 |
      | 2026-11-16 | Stark 302939742 / 302939741 | 08:00 / 08:00 |
      | 2026-11-16 | SAS 302939752 / 302939731 | 08:00 / 08:00 |
      | 2026-11-16 | Ericsson 302940173 / 302940169 | 07:54 / 07:51 |

    - Measured 2026-11-16: 13 of the 40 items are not `en-US` by `dc:language`; each is its own id, so its own story with a non-English body. One announcement fans out to up to 6 ids: Stark has 2 English ids plus `fr`, `de`, `es` and `sv`. A language filter is an open owner question; none is built.

      | feed | `en-US` | `fr` | `es` | `de` | `sv` |
      |---|---|---|---|---|---|
      | 2026-11-14 | 17 | 1 | 1 | 1 | 0 |
      | 2026-11-16 | 10 | 3 | 3 | 3 | 1 |

    - Construction of both tables: per item of both feeds, Python's ElementTree read `dc:language`, title, `description`, `dc:contributor` and the slug without its id; `OpenWebFeed::extract` over both feeds gave each of the 40 links its own follow-up, `filtered_out` 0.
  - Built 2026-11-17: CNW's `item_key` is `www.newswire.ca` plus the `loc`'s path, with no rewrite. All 2,600 sitemap rows of 2026-11-16 are `https://www.newswire.ca/news-releases/<slug>-<9 digits>.html`; on all 11 page files the canonical link and `og:url` equal the `loc`, so CNW's `doc_id` is that link. `the_real_canada_newswire_article_carries_its_title_its_canonical_link_and_its_item_key` (mutation 2026-11-17, a rewrite `www.newswire.ca` → `newswire.ca`: red).
    - Measured 2026-11-16, 26 sitemap polls: `news:language` is `en` on 2,600 of 2,600 rows, and the RSS feed's `dc:language` on 520 of 520. CNW's robots.txt names `/fr/sitemap-news.xml`; neither index read lists a `/fr/` release, and how many French releases a day CNW publishes is unknown.
  - Built 2026-11-17: the Fed's `item_key` is `www.federalreserve.gov` plus the link's path, with no rewrite. All 20 feed links are `https://www.federalreserve.gov/newsevents/pressreleases/<name>.htm`, `guid` equal to `link` on 20 of 20. No Fed page names a canonical link (0 of 9 files), so `doc_id` is the requested url, which `og:url` equals on 9 of 9 (`grep`, 2026-11-17). `the_real_federal_reserve_release_carries_its_title_its_own_url_and_its_item_key` (mutation 2026-11-17, a rewrite `www.federalreserve.gov` → `federalreserve.gov`: red). The canonical pick on a synthetic canonical: §8 row R3. The `og:title` ends with a space, which `picked` trims.
- Pace: `interval = max(Crawl-delay, 10 s)`; a host may be slowed, never sped up. Busy time of one host = its summed paced waits plus its request times; worst case ≤ 50% of a day per host (owner decision 1, 2026-11-17, §10). The stop deadline caps the run's wall time (§11.2); `collect` serves hosts round-robin since 2026-11-17, so the refetch worst-case poll takes 1,000 s of waits, not 1,380 s, while no host holds a `Retry-After` (§10's round-robin table).
  - Built: one `per_host` of 10 s (`INTERVAL`) for every host and no per-IP pace, so each host is its own queue (§8 row R19 hole 7; §10). Since 2026-11-16 (owner decision 1) a host whose `*` Crawl-delay is above it is refused by `collect`; per-host intervals (§11.3) are the path for such a host.
  - Per host the paced waits stay under 50% with the volumes seen 2026-11-16 (largest PRN, 17.71%, below) and in the cost-bound refetch case without retries (largest CNW, 35.88%); request times are not counted. CNW's refetch case with every fetch failing twice is 107.64%, which only step 7's follow-up cap and stop deadline bound.

    | host | distinct items dated 2026-11-16 (§11.6) | index requests a day | requests × (1 + `MAX_TRANSIENT_RETRIES` = 2) | paced s at 10 s | share of 86,400 s |
    |---|---|---|---|---|---|
    | GNW | 466 | 31 | 1,491 | 14,910 | 17.26% |
    | PRN | 479 | 31 | 1,530 | 15,300 | 17.71% |
    | CNW | 157 | 31 | 564 | 5,640 | 6.53% |
    | Fed | 0 | 31 | 93 | 930 | 1.08% |

    - Construction (2026-11-17, arithmetic on §11.6's weekday probe): every item seen fetched once, every request failing transiently twice; waits only, request times not counted; 31 polls a day as in the cost-bound table below. GNW and PRN counts are lower bounds (§11.6).
    - Headroom: at × 3, a host reaches 50% at 1,409 items a day (43,200 s / 30 s per item − 31 index requests); the most seen on 2026-11-16 was PRN's 479.
  - A host may also slow itself (§8 row R19, server slow-down, 2026-11-16): a 429 or any 5xx holds it until its `Retry-After`, or without one doubles its interval for the run. The 50% worst case holds only while no host answers so; nothing caps a slow-down until the stop deadline exists (§11.2; §8 row R19 hole 12), which owner condition A puts before the first live run (§11.5 step 7).
  - Probe material of 2026-11-14 (`probe/robots/`, `probe2/`) and 2026-11-16 (probe6): Crawl-delay above 10 s only at `www.nasdaq.com` (30 s), which is not planned; `www.accessnewswire.com` and `www.newswire.com` state 5 s; GNW, PRN, CNW, Business Wire (`*` group), Yahoo and Business Insider state none; the Fed's robots path answered 404.
- Hosts file: one hashed file per SourceId under `providers/open-web-feed/hosts/`; its digest is `hosts_version` (§2), stored in its own `attempts` column and in the run envelope. Only admitted rows are planned.
  - Built 2026-11-15: `collect --hosts`; the envelope field is the stdout envelope's (`newsvault.collect/2`), not the `runs/` file's (`newsvault.run/1`), which names no input digest.
- `universe_version` = digest of this provider's ticker roster file (empty for the POC). Path filters and the Yahoo key list live in the rules file, so `extract_rules_version` covers them.
- Held is keyed on (SourceId, `story_id`) over every target window, never the run's window alone.

| story state | held? |
|---|---|
| has a committed item | yes |
| `Document` attempt closed `Usable`, `Empty`, `PermanentFailure` with no 3xx status, or `Usable` with only unusable rows the page decided | yes (an `Empty` or unusable page: its raw blob is linked by the attempt's `payloads` row for an attempt closed at user_version 6 or later, and is a file no row names before it (§3.9), and no re-extraction reads it, P11 not being built; a re-fetch of a PRN, CNW or GNW page that yielded text gave the same extracted text, on PRN and CNW in other bytes (§3.9), and no `Empty` or unusable page was re-fetched); `a_document_that_yielded_only_unusable_rows_is_not_fetched_again` (`NoArticleBody`, the page-decided control) |
| `Document` attempt closed `Usable` with an unusable row the index row decided (`attempts.listing_unusable` > 0) | no (owner decision 2026-11-15, §10): no rule reads the page's own `datePublished`, and the listed pubDate is kept only in the index attempt's raw blob and, since 2026-11-16, the document attempt's `payloads.listed_date` (§3.9), which no rule re-reads (P11 not built), so only a refetch lets a later listing or a corrected pubDate rule (days 1-9, above) reach the story; `a_document_whose_listed_time_did_not_parse_is_fetched_again_and_committed_once_the_listing_parses`, `a_usable_document_with_a_listing_unusable_row_holds_nothing_and_a_v3_or_page_one_holds`. Cost: fetched by every poll while the index lists it with the bad value, `a_story_whose_listing_stays_unparseable_is_fetched_by_every_poll` (3 polls, 3 fetches); counted per source and day by `ledger`'s `listing_unusable` (§8 row R4); bound below |
| `Document` attempt closed `PermanentFailure` with a 3xx `transport_status` | no (owner decision 2026-11-15, §10): a redirected release is fetched again by the next poll, so step 5's refusal of redirects (§11.2) does not hold it forever; `a_document_answered_with_a_redirect_is_fetched_again_by_the_next_poll` (a loopback server's 301 through the real open-web `Wire`, its target never sent, §8 row R21), `a_redirect_whose_body_is_over_the_limit_is_fetched_again_by_the_next_poll` (the same 301 with a body over `BODY_LIMIT`, below), `a_permanent_document_answered_with_a_redirect_holds_nothing_and_one_with_no_status_holds` |
| `TransientFailure`, interrupted by the startup sweep, in flight | no |

- `Usable`, `Empty`, `PermanentFailure`, `TransientFailure` are `news_provider::ledger::Verdict` arms; §3.2's `OutcomeKind` maps both failures to `failed` (`Outcome::decide`, `news-provider/src/ledger.rs`). `Finalised` carries the `Verdict` inside `Outcome` (`Outcome::verdict`, None from `Outcome::abandoned`), stored as `attempts.verdict` (§3.2), since 2026-11-15.
  - `Outcome` derives `PartialEq`, so receipts compared in tests compare verdicts too; a replay with the same items and another verdict is still accepted, because the replay rule compares items only (`StoreError::ReplayWithDifferentItems`).
- The held read: `Committer::held_story_ids(source)`, behind `Archive::held_story_ids`, since 2026-11-15: committed `items` joined to their attempt's `source`, UNION closed `attempts` of that `source` with a non-NULL `story_id` and `verdict` in (`usable`, `empty`, `permanent_failure`); no `target_window`, no cutoff (the driver holds the lease, as for `covered_story_ids`).
  - a `permanent_failure` whose `COALESCE(transport_status, 0)` is in 300..=399 (`REDIRECTS`, `store.rs`) is excluded, since 2026-11-15 (owner decision, §10); a permanent failure with no status stays held: without the `COALESCE` the `NOT (...)` is NULL and drops the row.
  - a body over `BODY_LIMIT` keeps its head's status since 2026-11-15: `Wire::exchange` puts the status it read on the `TransportError` (`TransportError::status`) and `AttemptSlot::close` writes it as `transport_status`, so a 3xx whose body is over the limit is not held either; before, it stored NULL and was held for good.
  - `a_redirect_whose_body_is_over_the_limit_is_fetched_again_by_the_next_poll`: a 301 with open-web's `BODY_LIMIT` + 1 bytes through the real open-web `Wire`; 301 stored and reported, `permanent_failure`, fetched again by the next poll. Mutations 2026-11-15, each red: `exchange` dropping the status (also `news_provider::wire::tests::a_redirect_whose_body_is_over_the_limit_fails_with_the_status_its_head_carried` and `…::a_body_over_the_limit_fails_as_its_own_kind_and_one_at_the_limit_is_read`); `AttemptSlot::close` writing no status; the envelope's attempt row reporting no status.
  - a NULL-status permanent failure is now reached only through rows written before 2026-11-15 and the store test: a `TransportError` with no head is `Conversation`, which all three classifiers (open-web, edgar, `cmd_robots::verdict_of`) close `TransientFailure` (read from the code 2026-11-15).
  - `a_permanent_document_answered_with_a_redirect_holds_nothing_and_one_with_no_status_holds`: permanent at 300, 301, 399 not held; at 400, 404 and with no status held; usable at 200 held; mutations 2026-11-15: the exclusion dropped: red, and `a_document_answered_with_a_redirect_is_fetched_again_by_the_next_poll` red; the `COALESCE` dropped: red; upper bound 398: red; lower bound 301: red.
  - unguarded: the exclusion's `verdict = permanent_failure` condition; dropping it stayed green (2026-11-15, re-run after the loopback conversion: `cargo test --locked -p newsvault --no-fail-fast`, all green), because only a permanent row carries a 3xx today: open-web's `classify` returns `Usable` for 200 alone.
  - a `usable` row whose `COALESCE(listing_unusable, 0)` is above 0 is excluded, since 2026-11-15 (owner decision, §10); a NULL count (closed before user_version 4) stays held through the `COALESCE`. `a_usable_document_with_a_listing_unusable_row_holds_nothing_and_a_v3_or_page_one_holds`: count 1 not held, count NULL and count 0 held; mutations 2026-11-15: the exclusion dropped: red, and both listing driver tests; the `COALESCE` dropped: red; `> 0` made `>= 0`: red, with 13 other driver and store tests (`cargo test --locked --no-fail-fast` at d2f8a78, 2026-11-15), `the_ledger_counts_every_fetch_of_a_story_whose_listing_stayed_unparseable` among them.
  - unguarded: that exclusion's `verdict = usable` condition, for the 3xx one's reason: only a usable answer is extracted, so no other verdict carries a count; dropping it stayed green (2026-11-15).
  - open-web's `BlankSourceItemId` is `IndexRow`, but its empty `item_key` also makes `story_of` `None`, so `story_id` is NULL and the row is never held either way.
  - unguarded: that `IndexRow` marking; marked `Page`, the suite stayed green (2026-11-15). Unreachable through `plan` and through `extract`: `document_of` returns nothing unless `rule_for` reads a host, and then `item_key` is that host plus a path starting with `/` (`host_and_target`), never empty.
  - cost bound, unguarded: only the index sizes bound it, and with the Fed admitted their 20 + 20 + 100 + 20 = 160 items are above step 7's follow-up cap of 150 (§11.2), which is not built; GNW, PRN and CNW alone were 140. A story stops costing when it leaves the index; a CNW story stays listed 7.1 to 60.95 h (§11.2's window). Worst case, every listed story of every host listing-unusable:

    | quantity | GNW | PRN | CNW | Fed | construction |
    |---|---|---|---|---|---|
    | items per poll | 20 | 20 | 100 | 20 | GNW's RSS 20 (2026-11-14 probe); PRN's RSS 20 (each of the 2026-11-14 and 2026-11-16 fetches); CNW's news sitemap 100 and the Fed's RSS 20 (each of the 26 polls of 2026-11-16) |
    | polls a day | 31 | 31 | 31 | 31 | 23 slots at 30 min in 11:00–22:00 + 8 outside (23:30, 00:30, 02:00, 03:30, 05:00, 06:30, 08:00, 09:30), the weekday probe's schedule (§11.6, 2026-11-16) |
    | extra fetches a day | 620 | 620 | 3,100 | 620 | items × polls |
    | paced busy time a day | at least 6,200 s | at least 6,200 s | at least 31,000 s | at least 6,200 s | fetches × 10 s, each host its own queue (§10, owner decision 1, 2026-11-17) |
    | share of 86,400 s | 7.18% | 7.18% | 35.88% | 7.18% | before request times, which the Pace bullet also counts, and before each host's 31 index requests a day (with them: GNW and the Fed 7.53% and 22.60% × 3, CNW 36.24% and 108.72% × 3); observed: 0 of the 2,600 CNW values and 0 of the 520 Fed values listing-unusable (§10's CNW and Fed tables) |
    | share with each fetch × 3 (`MAX_TRANSIENT_RETRIES` = 2) | 21.53% | 21.53% | 107.64% | 21.53% | fetches × 3 × 10 s |

  - unguarded, CNW, a store with no held CNW story (the first live run): its first poll follows up every sitemap row, 100 on each of the 26 polls of 2026-11-16, 1,000 s at 10 s each, above a 30-min slot's 870 s stop deadline (§11.2, S7-D3), which is not built. Its Fed lane adds 20 follow-ups (200 s), served between CNW's under round-robin, so the poll's waits stay 1,000 s.

  - edgar's `classify` also closes every 3xx `PermanentFailure`, and since 2026-11-16 each reaches it as the answer (owner decision v, §8 row R19); the 300..=399 exclusion leaves it unheld, and edgar live ignores the held set while its archive arm reads `covered_story_ids` (bullet below), so the accession is fetched again by the next run.
  - `held_story_ids_names_each_state_the_held_table_holds_and_no_other`: one row per table state (an item whose attempt names no story, usable with no item, empty, permanent, transient, swept, in flight); mutations 2026-11-15: the items arm dropped: red; the verdict arm dropped: red; `transient_failure` admitted: red.
  - `held_story_ids_spans_every_target_window_and_one_source_only`: each arm holds one story under yesterday's window only, beside a story under today's; mutations 2026-11-15: either arm filtered to the latest window: red; either arm's `source` predicate dropped: red.
  - unguarded: `closed_at IS NOT NULL` is redundant with the `verdict` test, since an in-flight row has NULL `verdict`; dropping it stayed green (2026-11-15).
  - only-unusable rows: such a `Document` closes `Usable` and commits no item, so the verdict arm holds it (the "usable with no item" row) when the page decided every row; one the index row decided is excluded (above).
  - `empty`: neither provider's `classify` returns `Empty` for a `Document` (2026-11-15); that arm is reached by the store test alone.
  - the real-path caller, since 2026-11-15: `news_provider::collect::attempt_once` hands a live index's follow-ups the held set; the `vendor_archive` arm keeps `covered_story_ids` (one window, items only), so edgar's archive convergence (R20) is unchanged. `EdgarPress::follow_ups_to_issue` ignores the set for `live_ingest`, so edgar live still issues every follow-up; it costs one held read per live index attempt.
  - unguarded: within one run, a story listed by two index attempts is fetched twice; each index reads the set before the other's documents run. Unreachable with GNW, PRN, CNW, the Fed and Yahoo admitted (2026-11-17): `index_rows_of` keeps a link only when its host after the rule's rewrite is the index url's own host (`at == host`), and `item_key` is that host plus the path, so a GNW index keys only `rss.globenewswire.com/…`, a PRN index only `www.prnewswire.com/…`, a CNW index only `www.newswire.ca/…`, a Fed index only `www.federalreserve.gov/…` and a Yahoo index only `finance.yahoo.com/…`; the hosts file refuses a host named twice, so no two admitted rows index one host. That covers equal keys only: one release that PRN and CNW both list carries two keys, so it is two stories and is fetched by both (step 6's CNW holes, §11.5).
  - pre-v3 rows carry no `story_id` or `verdict`: a document that failed permanently or yielded only unusable rows before the upgrade is fetched once more; a committed story is still held through `items`. Rows closed at v3 carry a NULL `listing_unusable` and stay held, whichever input decided their reason: not backfilled, because no reason was stored.
- New columns `attempts.story_id` and `attempts.held` (§3.2, both built 2026-11-15); `ledger --check` subtracts `held` (§8 row R4). First sighting is unchanged: a skip writes no item. A held story's re-listed minute is a `relisted` row on the index attempt (owner decision 10, 2026-11-16, §8 row R15). A listing-unusable story is not held, so its corrected value reaches `record` (§8 row R15).
- Schema versioned with `PRAGMA user_version`, read at `Committer::open`: a lower version is migrated by explicit `ALTER TABLE` (a table rebuild for a `CHECK` change); an unknown higher version is refused. Built 2026-11-15 at user_version 1 (§3.1); step 2 grew that one migration rather than adding a version per commit; step 3 added user_version 2, the `robots` table; step 4 added user_version 3: `attempts.verdict`, `attempts.story_id` and `attempts.held`, one migration grown across the step; owner decision 6 (2026-11-15) added user_version 4: `attempts.listing_unusable`; P5 (2026-11-16) added user_version 5: `attempts.extract_version`; P10 (2026-11-16) added user_version 6: the `payloads` table (§3.9); owner decision 10 (2026-11-16) added user_version 7: the `relisted` table (§8 row R15).

### 11.2 POC

- One SourceId, `open_web_wires`; `open_web_outlets` arrives with its first outlet (Target). A second SourceId in one invocation needs a run envelope with a list of `hosts_version` values.
  - Built 2026-11-15: a second distinct `--source` is `invalid_argument` (`cmd_collect::Registered::chosen`); `two_sources_in_one_invocation_are_refused` (mutation 2026-11-15, first source accepted: red).
- Hosts, in order: GNW, then PRN, CNW, Fed, Yahoo (wire keys only). One PR per host, as each robots fetch lands. CNW and Fed are admitted only after a body probe; one ran 2026-11-16 (§11.6's body table) and decides no container.
  - Admitted: GNW 2026-11-15, PRN 2026-11-16, CNW, the Fed and Yahoo 2026-11-17 (§11.5 step 6; the Fed and Yahoo approved by the owner 2026-11-17). CNW's container was chosen from that probe's 5 pages and 6 more files of the same day, the Fed's from its 5 pages and 4 more files (§11.1's CNW and Fed coverage tables).
- GNW article URLs are rewritten from `www.` to `rss.` (both GNW's own): `www.globenewswire.com` returned Akamai 403 on robots.txt twice; its article pages were never tested.
- Yahoo: only provider keys of wires not fetched directly (`business-wire.com`, `accesswire.ca`), matched exactly, never as substrings. Not PRN, GNW, CNW keys: each release would be stored twice. Deferred: Zero Hedge, Newswire.com, Business Wire direct.
  - Built 2026-11-17: the `yahoo` rule's `provider_keys` (§11.1) and the fifth hosts row. A row whose key is not a `follow` key, or that has none, is counted in the index attempt's `filtered_out` and in the envelope's `cross_check.result.rows` by class, and is not followed. Tests: §8 row R3.
    - Measured 2026-11-16 over the 26 polls (26,000 rows; Python ElementTree, 2026-11-17): rows with no key 4,655 (4,304 with no `image:image`, 351 with an image not under the mark); `business-wire.com` 1,530 and `accesswire.ca` 487 rows, 488 distinct urls; followed rows per poll 32–141, new per poll at most 57 (p1500).
  - Owner decision 2026-11-16 (option C): the key list stays, and the Yahoo index is also read as a coverage cross-check of the direct wires: a Yahoo row under a PRN, GNW or CNW key that no direct index listed names a possible silent drop on our side; it stores no item row.
    - Built 2026-11-17: `CrossChecker` (`providers/open-web-feed/src/crosscheck.rs`), run by `newsvault collect` after each run that answered the Yahoo index (`cmd_collect::cross_check_of`). Output: the collect envelope's `cross_check`, nothing stored; the inputs are stored blobs, so it can be recomputed.
    - Read: the direct index answers the store holds received at or after `read_from`, `DIRECT_LEAD` (2 h) before the earliest `cross_check` minute of this Yahoo answer. Yahoo dated a row after a direct answer had listed it on 29 of 4,042 matched row-polls of 2026-11-16, at most 72 min (review 2026-11-17, `CrossChecker` over the 26 polls).
      - Owner decision 2026-11-17 (~16:10Z, "zgoda"), not built: `DIRECT_LEAD` is 90 min, the longest poll period (the table below), a construction; it replaces the 2 h, which was set after seeing the 72 min. It covers that 72 min.
    - A `cross_check` row is `not_compared` when its minute is before `watched_from`, the first direct index answer the store holds; `not_looked` when some admitted direct index has no answer that parses, received at or after the row's minute closed. Otherwise it is compared: listed means its normalised title (§11.6's construction: lowercase, each run of non-alphanumerics one space) equals a non-empty title (`title`, `news:title`) in a read answer that parses.
      - Owner decision 2026-11-17 (~16:10Z, "zgoda"), not built: a key→index map, `prnewswire.com` → PRN, `globenewswire.com` → GNW, `cnwgroup.com` → CNW. A row is then `not_looked` only when the index its key maps to has no such answer, so one silent index no longer makes every key's rows `not_looked`.
    - Reported per run: `direct_payloads_read`, `direct_payloads_parsed`, rows by key class, `not_compared`, `not_looked`, `listed_directly`, and each unlisted row's key, url, title and raw minute; `error` when the store could not be read. Tests: `a_cross_check_row_no_direct_index_listed_is_named_and_a_listed_one_counted`, `a_cross_check_row_dated_before_the_first_direct_answer_is_not_compared`, `titles_match_after_case_and_punctuation_fold_and_never_by_a_prefix`, `the_earliest_cross_check_minute_is_the_oldest_cross_check_row`, `a_direct_answer_received_before_the_keyed_minute_still_lists_the_row`, `a_row_no_direct_index_answered_after_is_not_looked_and_never_unlisted`, `an_empty_title_or_an_empty_key_matches_nothing`; through collect `a_yahoo_row_under_a_wire_key_no_direct_index_listed_is_named_in_the_run_envelope`, `a_cross_check_row_no_direct_index_answered_after_is_not_looked`, `a_damaged_direct_blob_is_a_cross_check_error_and_the_run_finishes`.
    - Replayed 2026-11-17 over the 26 polls of 2026-11-16 through `CrossChecker::read_from` and `check` (a temporary test, not kept): direct = the stored GNW RSS, PRN RSS, CNW sitemap and Fed RSS answers up to each poll, received times from each answer's `fetch_instant`, `watched_from` 08:04Z; 4 to 52 direct answers read a poll, every one parsed; `not_looked` 0 at every poll:

      | key | distinct Yahoo urls compared | unlisted at some poll |
      |---|---|---|
      | `prnewswire.com` | 688 | 323 |
      | `globenewswire.com` | 772 | 488 |
      | `cnwgroup.com` | 141 | 11 |

      - Of the 822 urls unlisted at some poll, 12 were listed by a later poll.
      - Unlisted per poll: 0 at p0800, then 21–333 (most at p1430). Most are PRN and GNW releases their 20-item RSS dropped between polls (§11.2's period table), the silent drop option C exists to name.
      - Limits of the match: a retitled release, or a Yahoo title spelled otherwise than the wire's, is a false "unlisted"; two releases with one title (GNW's buybacks, §11.6) hide each other; PRN's regional releases (`prnewswire.co.uk`, `/apac/`) under `prnewswire.com` are never in the US RSS. CNW's 11 of 141 bound the false share where the direct index covers the day: 7.8%, false and true together.
  - Rejected: Yahoo alone with every key, because one release is listed up to 3 times (`cnwgroup` and `prnewswire` twice), the Fed is absent, non-English wire releases are absent (GNW 0 of 11 by `dc:language`; PRN by reading the titles) and a GNW reprint carries no release id (§11.6, 2026-11-16).
  - Owner decision 2026-11-16, not built: the `newsfile` key joins the list after a body probe of its pages passes; no such probe has run.
  - Owner decision 2026-11-16: Reuters and AFP through Yahoo come later, as `open_web_outlets` (§11.3).
- Robots: a refresh command fetches robots.txt through `Wire` under an attempt row; the hosts file pins the robots digest. How a robots attempt is labelled and kept out of the expected-run count: §10's "calendar, listing and roster are FILES" paragraph.
  - Built 2026-11-15: `newsvault robots refresh` (`services/newsvault/src/cmd_robots.rs`), one request `https://<host>/robots.txt` per hosts-file row under an `AttemptSlot`, through the collector's `Wire`, credential and pace files; the body of a 200 is the `robots` row's pinned body (§3.2), and since 2026-11-16 every answered body, a 404's and a 403's included, is a blob linked by `payloads` (§3.9); its exits are §5.1's; `newsvault schema` path `["robots", "refresh"]`, checked against the parser by `every_option_the_robots_refresh_parser_takes_is_in_the_machine_readable_schema`.
  - Tests over a stub `Exchange` and a fixed clock: `a_robots_refresh_writes_one_attempt_per_hosts_row_under_source_robots`; `a_robots_404_is_recorded_as_absent_and_pins_no_digest` (mutation 2026-11-15, 404 a failure: red); `a_robots_403_5xx_or_transport_error_is_a_failed_attempt_with_no_robots_row`, 429 included (mutations 2026-11-15, 403 absent / 429 permanent: each red); `a_fetched_robots_digest_other_than_the_pin_exits_one_and_names_both` (mutation 2026-11-15, `matches_pin` always true: red); `a_candidate_whose_fetch_differs_from_its_pin_exits_zero` (mutation 2026-11-15, `row.admitted` dropped from `exit_of`: red); `a_robots_refresh_dry_run_plans_one_url_per_row_and_writes_nothing`, through the library and the binary.
  - The refresh's `Wire` takes open-web's profile (`cmd_robots::wire_for`, called by `main.rs` and the loopback test; §8 row R19), so since 2026-11-15 a robots.txt 3xx is not followed: it is the answer, `verdict_of`'s `_` arm closes it `PermanentFailure`, the attempt stores `transport_status` 301, no `robots` row is written and the refresh exits 8 (§10).
  - `a_robots_redirect_is_a_permanent_failure_with_no_robots_row_and_its_target_never_fetched`: GNW's robots url relayed through a real open-web `Wire` to a loopback server answering `301 Location: /elsewhere`, where `/elsewhere` serves the pinned GNW body; the server sees `/robots.txt` only. Mutations, each red: `MAX_REDIRECTS` 1, 2026-11-16; a `300..=399` arm in `verdict_of` mapped to `Empty`; the same arm mapped to `Usable` (2026-11-15).
  - Collect's robots check reads the newest fetch that has a `robots` row (`RobotsLedger::newest_robots` joins `robots`), so a 3xx is skipped as a 403 is: after a matching fetch the host stays admitted, and a host with only a 3xx is `robots_unverified`.
  - Tests, each with a 301 case since 2026-11-15: `a_failed_robots_fetch_after_a_matching_one_leaves_the_host_admitted` (503, 301), `an_admitted_row_without_a_ledgered_robots_fetch_is_refused` (403, 301); mutation 2026-11-15, a `300..=399 => Empty` arm in `verdict_of`: both red.
  - Unguarded: a refresh that never runs, or dies, is invisible to the ledger, which counts only `collect` envelopes; no timer runs it (step 7).
  - No run against a live host yet.
- The robots and floor checks run in `newsvault` (only the driver reads the store); the provider receives no Crawl-delay (owner decision 1, 2026-11-16).
  - Built 2026-11-15: `cmd_collect::open_provider` reads each admitted row's pinned blob by digest (`store::blob_bytes`, no commit log) and parses it before any `Wire` exists. After the pin check (§8 row R4), `refuse_what_the_pinned_robots_forbids` refuses the row's paths (`OpenWebFeed::admitted_paths`: the index url's path with its query, then the rule's `path_prefix`) and the floor (§8 row R19); dry run included; codes §5.1.
  - Tests in `services/newsvault/tests/open_web_collect.rs`: `an_admitted_row_whose_index_or_path_prefix_robots_disallows_is_refused`, `a_hosts_row_whose_robots_crawl_delay_is_above_the_shared_interval_is_refused`, `a_pinned_robots_body_that_does_not_parse_is_refused`, `a_robots_blob_written_after_the_provider_opened_is_unverified`, `a_path_or_unreadable_robots_refusal_names_its_fact_and_suggests_no_refresh`; in `open-web-feed`: `the_admitted_paths_are_each_admitted_index_with_its_query_and_its_rule_prefix`.
- Redirects refused: `Wire` sets `max_redirects(0)` (`MAX_REDIRECTS`); a 3xx is a `PermanentFailure` and is not held (§11.1), which closes R19 hole 9 by refusal.
  - A 3xx whose body is over `body_limit` included, since 2026-11-15: its `TransportError` keeps the 3xx (§11.1); for robots, `a_robots_redirect_whose_body_is_over_the_limit_keeps_its_status` (301 on the attempt row and in the report, no robots row, exit 8; mutation 2026-11-15, the report row reading no status from a failure: red).
  - Built 2026-11-15 for open-web, 2026-11-16 for edgar (owner decision v, §10); tests and mutations: §8 row R19.
  - Unguarded: the `Location` target is recorded nowhere. `RawResponse` carries P9's headers since 2026-11-16, and `Location` is not in the allowlist (§3.9): the loopback 301's link row holds its `Content-Type` alone (`a_robots_redirect_is_a_permanent_failure_with_no_robots_row_and_its_target_never_fetched`).
  - A target written in the 3xx body is reachable from the commit log since 2026-11-16, in collect and robots refresh alike: the body is stored and linked by the attempt's `payloads` row, cut at `body_limit` when over it (§3.9); no reader parses it.
- One interval for all hosts: every admitted host is at 10 s, each in its own queue since 2026-11-17 (§10). R19 hole 7 stays open; a host whose Crawl-delay is above 10 s is refused (§11.1 Pace).
- Decided: `ValidPrecision::Minute` for GNW's minute-resolution `pubDate`, since 2026-11-16 for PRN's and since 2026-11-17 for CNW's and Yahoo's `news:publication_date` and the Fed's `pubDate` (§11.1); the five hosts end to end: `an_open_web_run_collects_a_gnw_a_prn_a_cnw_a_fed_and_a_yahoo_day_end_to_end` (§11.5 step 6). Built 2026-11-15; producer: `open-web-feed`'s document extraction: `pit::datedness::Minute` and `Published::AtMinute`, `canon::Stamp::Minute` with `canon/vectors/item_id_minute_precision.json`, `items.valid_minute` in the rebuilt table (§3.1, §8 row M1). Driven by `newsvault collect --source open_web_wires` end to end: `an_open_web_run_collects_a_globenewswire_day_end_to_end` (§11.5 step 2). `items.valid_raw`, `valid_field`, `valid_source` stay nullable after the rebuild: rows committed at user_version 0 have no evidence (§3.1).

| window (UTC) | poll every | run stops issuing requests at (S7-D2, S7-D3: wall time; §11.5 step 7, not built) |
|---|---|---|
| 11:00–22:00 (pre-market through after-close releases), 23 slots | 30 min | slot start + 15 min − 30 s |
| outside, 8 slots: 23:30, 00:30, 02:00, 03:30, 05:00, 06:30, 08:00, 09:30 | 90 min, except 23:30 → 00:30, which is 60 min | min(slot start + 45 min, next slot) − 30 s; for 23:30 that is 00:14:30, before the 00:30 slot |

- Both poll periods are the owner's choice. S7-D1 (owner, 2026-11-17 ~16:10Z, "zgoda") keeps them for every index, the PRN sitemap included; probe10's 5-min period (§11.6) was a probe's, not a plan's.
  - Rejected: a 5-min tick (step-7 ops report S2; its numbers from step-7 critique C7, 2026-11-17). With one store lease, every run gets a window of at most 150 s, and a host answering 5xx with no `Retry-After` is asked 864 to 1,152 times a day, against 202 under the 31 slots (arithmetic 2026-11-17: doublings reset each run; 288 ticks × 3 or 4 requests, against 23 × 6 + 8 × 8).
- Measured 2026-11-16 (Monday) against each index, 26 polls on the table's schedule (§11.6, weekday probe):
  - no overlap = the later poll's oldest item is newer than the earlier poll's newest; the publication time between the two is an uncovered stretch, and an item listed and dropped inside it is never seen.
  - PRN and GNW RSS: no overlap at both periods (table); items lost in the uncovered stretches are not counted, because the index cannot show them.
  - CNW sitemap, CNW RSS and Yahoo: every pair overlapped and kept the previous poll's newest item.
  - Fed: no new item all day, so the period was not tested.

| index | items per fetch | window h, min / median / max | pairs with no overlap: 30-min / 90-min | uncovered stretch per no-overlap pair, min–max | fewest of the previous poll's items still present |
|---|---|---|---|---|---|
| GNW RSS | 20 | 0.15 / 0.4585 / 1.033 | 15 of 22 / 3 of 3 | 1–75 min | 0 of 20, in 18 pairs |
| PRN RSS | 20 | 0.2 / 0.4335 / 0.983 | 15 of 22 / 3 of 3 | 1–50 min | 0 of 20, in 17 pairs |
| CNW sitemap | 100 | 7.1 / 11.375 / 60.95 | 0 of 22 / 0 of 3 | — | 86 of 100 (14:00→14:30, 22:00→23:30) |
| CNW RSS | 20 | 0.867 / 2.225 / 16.183 | 0 of 22 / 0 of 3 | — | 5 of 20 (14:00→14:30) |
| Fed RSS | 20 | 1,176.75, every poll | 0 of 22 / 0 of 3 | — | 20 of 20, every pair |
| Yahoo sitemap | 1,000 | 1.783 / 3.438 / 20.367 | 0 of 22 / 0 of 3 | — | 576 of 1,000 (14:00→14:30) |

  - Uncovered stretch = the later poll's oldest publication value − the earlier poll's newest, minute precision; computed here from the probe's per-poll rows.
  - The true PRN and GNW daily volume is unknown and may be higher than what was seen: first sightings reach 40 in an hour, the most two 20-item polls can show (§11.6's hourly table).
  - The remedy for PRN and GNW is the completeness table below: owner decisions `PG-D2` to `PG-D6` of 2026-11-17 ("tak, zgadzam sie z rekomendacjami", after the PRN/GNW research of that day) and S7-D1 (above). Decided, not built: no Rust, rules or hosts file names a PRN sitemap page, a PRN archive or a GNW id url (`git grep` 2026-11-17).

    | host | what is read | rule | ObservationTime | rejected, and the symptom |
    |---|---|---|---|---|
    | PRN | the RSS, as today, plus the news sitemap `sitemap-news.xml?page=<n>` | each poll reads page 1, then the next page, until a page shares a key with the previous complete poll; the poll is complete only when every page boundary it read also shares a key; otherwise that stretch is recorded "not looked" | the document fetch, as today | the RSS alone: 0 of the 55 releases of 14:00Z on 2026-11-16 seen by the 20-item polls; the sitemap alone: 7 RSS keys of 2026-11-16 are in no sitemap page, 2 of them across a boundary whose pages shared rows; one generation stamp for every page read: 0 of 20 neighbouring page pairs shared one, because Cloudflare caches each page url for 300 s (PRN/GNW research, 2026-11-17, over the 2026-11-16 bodies) |
    | PRN | the monthly archive `Sitemap_Index_<Mon>_<YYYY>.xml.gz` | names what the RSS and the sitemap both missed; an item found only there is stored; unchecked: its size against `BODY_LIMIT` and its `<lastmod>` as publication time (§10's still-open list) | the archive fetch (PG-D5), weeks after publication | — |
    | PRN | — | a release the RSS lists and the sitemap leaves out stays an ordinary RSS item; the absence is recorded as a fact, never as a withdrawal (PG-D6) | — | reading it as withdrawn: why PRN leaves a release out is unknown |
    | GNW | the RSS as the frontier source, plus an id walk on `rss.globenewswire.com` (`/news-release/<y>/<m>/<d>/<id>/0/en/x.html`, probe10's url) | each poll walks ids up to the highest id the PREVIOUS RSS poll showed (PG-D2: a one-period lag by construction, so an id is asked at least one poll period after a higher id was listed; whether an id stays non-public longer, an embargo, is unknown, bounded by probe10 in §11.6; an id asked early answers 410, which GNW's CDN keeps for that url for 30 days); a 301 to another language is recorded "id exists, language X, not fetched" and not followed (PG-D4); stories keyed `(dc:identifier, language)` | the instant the id fetch returned | GNW's `latest.xml`: left out 143 of 406 English releases and has no time field (research 2026-11-17, not re-checked); following the 301: one more request per non-English id, against `MAX_REDIRECTS` 0 (§10) |

    - `sitemaps.globenewswire.com`'s robots.txt answered 404: read as allow-all (PG-D3), as §10's robots row reads every 404.
    - Cost, a projection and not a measurement (step-7 numbers report, 2026-11-17, read at main 21989d7: four hosts, Yahoo counted only where a bullet says so, against a 900 s deadline; from probe6's indexes and the research's sitemap bodies; waits only, 10 s per request):
      - GNW: 1,036 ids over 08:00–23:30Z of 2026-11-16 plus 31 RSS requests, 37.05% of a day per host at × 3 (1 + `MAX_TRANSIENT_RETRIES`); a lower bound, the night not counted.
      - PRN: sitemap items per 30 min, each window shifted by the 10.8 min median generation lag, were 66, 75 and 109 at 13:00, 13:30 and 14:00Z on 2026-11-16, and 120 at 13:00Z on 2026-11-17 (from that day's sitemap pages); a page holds 100, so a peak poll needs page 2. PRN's daily share under S7-D1 is not computed.
      - Unmeasured at 30 min: how often a multi-page PRN poll is not complete, because Cloudflare serves each page url from a cache of 300 s, so a later page can come from an older generation sharing no key with page 1; probe10's one multi-page read was not complete (§11.6), and 18 of 40 page boundaries in the research had a later page from an older generation (PRN/GNW research, 2026-11-17).
      - Inside one 30-min run, at the 2026-11-16 peak, four hosts: the run reaches 1,170 s at p1430, 1,110 s at p1500 and 880 s at p1600, so S7-D3's 870 s deadline binds at all three; the cap of 150 binds at p1430 (183 follow-ups, 33 cut, PRN's tail).
      - With Yahoo's followed keys added: the cap binds at p1430 (228, 78 cut) and p1500 (203, 53 cut) (numbers report §3), and at p1330 (175, 25 cut) and p1400 (180, 30 cut), derived 2026-11-17 by adding the report's §2 Yahoo column to its §3 follow-ups; Yahoo's lane is at most 58 requests, 570 s (p1500).
    - Measured at a 5-min period, not at S7-D1's 30 min: §11.6's probe10.
  - CNW's admitted index is the sitemap row (§10's CNW table, 2026-11-17); the CNW RSS row is the rejected alternative.
- The stop deadline (the table's third column, S7-D3) is what holds the run's wall time ≤ 50% of a day (arithmetic 2026-11-17):
  - requests stop by 23 × 870 s + 8 × 2,670 s = 41,370 s, 47.9% of 86,400 s;
  - with a last request running its 30 s patience in every run, wall time ≤ 23 × 900 s + 8 × 2,700 s = 42,300 s, 48.96%;
  - this assumes ureq's 30 s `timeout_global` covers the body read on every path, which nobody has tested (step-7 critique, open items).
  - Since round-robin by host (§10, 2026-11-17) that wall time is about the busiest host's paced waits plus every request time, not the waits summed over hosts. A `Retry-After` hold breaks this: it is waited out on the held host's turn and every other host waits with it, so the requests a deadline would leave `deferred` are the other hosts' (§10 table: 150 s against block order's 130 s; §8 row R19, hole 12).
  - Unissued requests are `deferred` rows (S7-D4), and the next run carries each unresolved one the store does not hold to the front of its host's lane, closing it `expired` after 24 h (S7-D5); §11.5 step 7.
  - Not built: no Rust file under `services/`, `providers/` or `news-provider/` names a stop deadline or `deferred` (`git grep` 2026-11-17).
  - Until S7-D5 is built, a deferred open-web story is fetched again only if a later index lists it; PRN's and GNW's next poll shared no item with the previous one in 18 of 25 pairs each (table above).
- Follow-up cap: 150 per invocation, the owner's choice, not measured; global, counted at issue in round-robin order, and the GNW walk has its own cap (S7-D6, §11.5 step 7; not built). Worst case per run with no host slowing itself (§11.1 Pace): (150 + index requests) × (1 + `MAX_TRANSIENT_RETRIES` = 2) × 10 s.
  - Busy time against the stop deadline and this cap is projected, not measured, and a lower bound: the completeness cost bullets above (round-robin, 2026-11-17); §11.6's weekday probe busy-time table (2026-11-16) is the serial sum from before round-robin.
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
  - Built 2026-11-16 in `providers/open-web-feed/tests/fixtures/prn/`: `rss_3_items.xml`, items 2-4 of the 20 in the 2026-11-14 19:21:04 GMT feed; releases 302939553 and 302938569 cut to their head and the first 3 `<p>` of `section.release-body`, keeping the dateline and share `<p>` and one `<script>` outside it; 302939553 also keeps the real page's `section.nav-main` holding one `<p>` (`Searching for your content...`), so a container of `section` matches twice on it (§11.5 step 6's `review` rows); `synthetic_no_release_body.html`, a page with no `section.release-body`; `robots_www_prnewswire_com.txt` (§11.1). Fixture item 1 (the feed's item 2) is a correction notice under release link 302913655 (§8 row R3); in the two-host test it is answered with the synthetic page.
  - Built 2026-11-16: `tests/fixtures/hosts/gnw_admitted.json`: the GNW row alone, a byte copy of the hosts file before PRN was admitted; the GNW-only `newsvault` tests read it, so they seed and stub GNW alone.
  - Built 2026-11-17 in `providers/open-web-feed/tests/fixtures/cnw/`, all from the weekday probe's p0800 slot (§11.6): `sitemap_news_3_urls.xml`, the 08:04:39Z sitemap cut to its first 3 of 100 `url` rows; releases 805423040 and 823884942 (08:05:10Z and 08:05:20Z fetches) cut to their head, the ld+json `<script>`, `section.nav-main`'s one `<p>`, the dateline and share `<p>`, and a part of `section.release-body` (805423040: the lead, 2 of 3 gallery figures, 4 later `<p>`, the contact line; 823884942: the paragraphs naming no driver), personal data replaced (§10's CNW table); `synthetic_no_release_body.html`; `robots_www_newswire_ca.txt` (§11.1). Fixture url 3, 807505308, has no kept page and is answered with the synthetic page in the four-host test.
  - Built 2026-11-17 in `providers/open-web-feed/tests/fixtures/fed/`, all from the weekday probe's p0800 slot (§11.6): `press_all_3_items.xml`, items 1, 3 and 5 of the 20 in the 08:04:39Z feed, byte for byte (byte order mark, CRLF, CDATA), item 5 for a real unpadded day; releases bcreg20261113a and orders20261109a (08:05:00Z and 08:06:01Z fetches) cut to their head, one `nav` paragraph, the breadcrumb and `div#article`, blank lines removed and CRLF kept, the media contacts' names and numbers replaced (§10's Fed table); `synthetic_no_article_block.html`; `robots_404_www_federalreserve_gov.html` (§11.1). Fixture item 2, monetary20261112a, is answered with the synthetic page in the five-host test.
  - Built 2026-11-17 in `providers/open-web-feed/tests/fixtures/yahoo/`: `news_sitemap_8_urls.xml`, 8 of the 1,000 `url` rows of the p0930 (09:30:00Z) sitemap byte for byte, one per key class (`motleyfool.com`, none, `business-wire.com`, `newsfile_64`, `globenewswire.com`, `prnewswire.com`, `accesswire.ca`, and `prnewswire.com` under the CNW fixture's ELEGRP title); the Laverock (`business-wire.com`) and Horizon (`prnewswire.com`) reprints (probe7 r1 fetches, 10:52–10:53Z) cut to their head, the paid-release disclaimer, the `h1` and `div[data-testid="article-body"]` whole, each contact's name and address replaced; `synthetic_no_article_body.html`; `robots_finance_yahoo_com.txt` (§11.1). The `accesswire.ca` row has no kept page and is answered with the synthetic page in the five-host test.
  - Built 2026-11-17: `tests/fixtures/hosts/cnw_and_yahoo_admitted.json`, the committed CNW and Yahoo rows alone, for the two-run cross-check test.
- Scheduler: a systemd user timer on the VPS, outside the crate (S7-D8 to S7-D10, §11.5 step 7; not built).

### 11.3 Target shape

| item | target |
|---|---|
| sources | every candidate that passes robots and a weekday probe, incl. Business Wire if stable (its Earnings feed on 2026-11-16: §11.6; no stability threshold is fixed), Zero Hedge, Newswire.com (measured zone, date-only valid time) |
| `open_web_outlets` | Fool, CNBC, Yahoo outlet syndication, Reuters and AFP through Yahoo (owner decision 2026-11-16); Fool's earnings-call transcripts are a separate item kind, field named when built; Business Insider is not added for now (owner decision j, 2026-11-16) |
| per-host intervals | R19 hole 7 closed (`HostPaces::Listed`); the path for a host whose Crawl-delay is above 10 s, refused until then (owner decision 1, 2026-11-16) |
| redirects | followed as their own paced, ledgered requests; today `Wire` follows none for any provider (open-web since 2026-11-15, edgar since 2026-11-16), which closed R19 hole 9 by refusing; needs `Location` kept (§11.2) and `Document`-phase follow-ups |
| robots | re-fetched on a schedule; a changed robots digest blocks the host until the hosts file is updated. Built 2026-11-15 without the schedule and wider than a host: `robots_refused` refuses the whole run (§5.1) |
| collectors | per SourceId in parallel; needs a startup sweep that tells live runs from dead ones; then the lease covers only `Committer::reserve` through COMMIT (§6) |
| follow-up cap, run length | run length: S7-D3's construction; cap: 150, the owner's choice (S7-D6), to be revisited against measured weekday busy time; busy seconds in every collect envelope and run file (S7-D11, §11.5 step 7, not built); only a projection exists (§11.2's cap bullet) |
| PRN and GNW completeness | every release named per poll (PRN sitemap pages) or per id (GNW walk), and per month (PRN archive): §11.2's completeness table, not built |
| `NoArticleBody` share per host | a ledger reading; its threshold is fixed before its first measurement |
| outbox poller | wakes `derive` and feeds the private stream (memo D-7) |

### 11.4 Gates planned (each leaves this table for the §8 row named when it bites; none of the rows below exists for the hosts it names, 2026-11-16)

| gate | turns red when | §8 row |
|---|---|---|
| a rolled-back `record` leaves no outbox row | the insert moves outside the transaction | new row O1 |
| a zero-item attempt writes one outbox row | the insert is skipped when rows are empty | R14 |
| CNBC valid time is the page meta (Target) | the sitemap value is used | R13 |

- Unguarded in the POC: no reader of the stored raw valid-time fields (§3.1); items lost between polls (an index whose oldest item is newer than the previous poll's newest): §8 row R16.

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
     - ureq 3.5.2 followed redirects, up to 10, unpaced: closed for open-web by step 5 and for edgar on 2026-11-16 (owner decision v, §8 row R19 hole 9).
     - no run against the live host yet.
3. Robots refresh command; `newsvault` checks the pinned digest and the floor.
   - Built 2026-11-15: the refresh command, the `robots` table at user_version 2, the hosts-file pin (§11.1, §11.2), the envelope `kind` (§10).
   - Built 2026-11-15: the collect-side digest check, `refuse_an_unverified_robots` in `cmd_collect.rs`, in `run` after the window pre-flight and in the dry-run `planned` after the plan, so usage errors still come first (§8 row R4, robots clause; codes §5.1). Every open-web collect test seeds its store through one helper, `services/newsvault/tests/seed/mod.rs`, which runs `cmd_robots::run` over a stub answering the committed GNW body.
   - Mutations 2026-11-15, each run once against its named test in `services/newsvault/tests/open_web_collect.rs`, 7 of 7 red: the check returning before the lookup; the reader ordered oldest first; an `absent` pin skipped without the lookup; a missing blob skipped; the dry-run call removed; the reader taking a failed fetch as an answer; the binary writing the refusal as plain text.
   - Built 2026-11-15: the robots parser, the path and floor checks (§11.1 robots and Pace, §11.2, §8 row R19).
   - Built 2026-11-16, owner decisions 1 to 3 (§10 robots rows): a Crawl-delay above the shared 10 s refused, the hand-off to the provider removed; B2 grouping; empty, HTML and orphan bodies, orphan lines ignored by the owner decision of that day; the refresh's `parse_verdict`.
   - Built 2026-11-16, recommendations #1 and #2 (§10 robots rows): Google's key and line spellings; text with no group read as no rules, `newsvault.robots/2`.
   - Built 2026-11-17, owner decision of that day (§10 row "key and line spellings"): Google's index page rule, pattern escaping, line cut and NUL; decision 4's loader refusals (§11.1). Its mutations ran with `cargo test --locked -p newsvault --lib robots::` or `cargo test --locked -p open-web-feed --test open_web_feed`, every red test listed; an open-web-feed mutation also turns `every_golden_page_extracts_to_its_committed_body_under_the_committed_version` red, because any source change moves `extract_version`.
   - Built 2026-11-17, owner decision "w punkcie 3 tak" (§10 row "key and line spellings", (e)): a robots body that is not UTF-8 read as bytes, `refused_not_utf8` removed, `newsvault.robots/3`. Its mutations ran with `cargo test --locked --no-fail-fast -p newsvault --lib robots::` or `--test robots_refresh`, every red test listed.
   - Mutations, one at a time on the final tree, each file restored: 2026-11-15 rows with `cargo test --locked -p <crate> --no-fail-fast -- <test>`, each its named test only; 2026-11-16 rows with `cargo test --locked --no-fail-fast -p newsvault`, every red test listed.

| mutation | named test red |
|---|---|
| the first group read instead of `*` | `the_star_group_is_read_and_a_named_bot_group_is_not` |
| `Disallow` wins a tie; the tie-break dropped (the later rule wins) | `allow_wins_a_tie_and_the_longest_match_decides` (2) |
| a wildcard's last piece matched only where the rest starts; a final `$` ignored | `wildcard_and_end_anchor_match_as_rfc_9309_states` (2) |
| a Crawl-delay fraction not checked | `a_crawl_delay_that_is_not_a_number_is_refused` |
| the path check removed | `an_admitted_row_whose_index_or_path_prefix_robots_disallows_is_refused` |
| 2026-11-16: the Crawl-delay arm of `refuse_a_pace_below_the_floor` removed | `a_hosts_row_whose_robots_crawl_delay_is_above_the_shared_interval_is_refused`, `a_path_or_unreadable_robots_refusal_names_its_fact_and_suggests_no_refresh` |
| 2026-11-16: `>` as `>=` in `crawl_delay_over_interval`; the 10 s `per_host` floor removed | `a_hosts_row_whose_robots_crawl_delay_is_above_the_shared_interval_is_refused` (2) |
| 2026-11-16: a Crawl-delay ends the `user-agent` run again; a Crawl-delay kept only when the group already names `*` | `a_crawl_delay_does_not_end_a_user_agent_run` (2) |
| 2026-11-16, re-run after recommendation #2: the `<` check removed | `an_html_body_is_refused`, `a_pinned_robots_body_that_does_not_parse_is_refused`, `a_path_or_unreadable_robots_refusal_names_its_fact_and_suggests_no_refresh`, `a_refresh_reports_the_parse_verdict_of_each_answered_row` |
| 2026-11-16, re-run after recommendation #2: an empty body read as `NoRulesNoGroup` | `an_empty_or_comment_only_body_reads_as_no_rules`, `a_refresh_reports_the_parse_verdict_of_each_answered_row` |
| 2026-11-16, orphan-line decision: an orphan `Disallow` applied to `*` again | `a_rule_before_the_first_user_agent_is_ignored` |
| 2026-11-16, orphan-line decision: an orphan `Allow` and `Disallow` applied to `*` | `a_rule_before_the_first_user_agent_is_ignored`, `an_orphan_allow_widens_nothing` |
| 2026-11-16, orphan-line decision: the orphan lines' Crawl-delay applied to `*` again | `a_rule_before_the_first_user_agent_is_ignored`, `an_orphan_allow_widens_nothing`, `a_refresh_reports_the_parse_verdict_of_each_answered_row` |
| 2026-11-16, orphan-line decision: the verdict not set by an orphan `Allow`/`Disallow` | `a_rule_before_the_first_user_agent_is_ignored`, `an_orphan_allow_widens_nothing` |
| 2026-11-16, orphan-line decision: the verdict not set by an orphan Crawl-delay | `a_rule_before_the_first_user_agent_is_ignored`, `a_refresh_reports_the_parse_verdict_of_each_answered_row` |
| 2026-11-16, re-run after recommendation #2: orphan lines with no `user-agent` line read as `orphan_rules_ignored` | `text_with_no_user_agent_line_reads_as_no_rules`, `a_key_spelling_google_does_not_accept_is_not_read` |
| 2026-11-16, recommendation #2: text with no group refused again (as `HtmlPage`) | `text_with_no_user_agent_line_reads_as_no_rules`, `a_key_spelling_google_does_not_accept_is_not_read`, `a_pinned_robots_body_that_does_not_parse_is_refused`, `a_refresh_reports_the_parse_verdict_of_each_answered_row` |
| 2026-11-16, recommendation #2: text with no group read as `NoRulesEmpty` | `text_with_no_user_agent_line_reads_as_no_rules`, `a_key_spelling_google_does_not_accept_is_not_read`, `a_refresh_reports_the_parse_verdict_of_each_answered_row` |
| 2026-11-16, recommendation #1: a key matched whole instead of by prefix; the `useragent`/`user agent` typos removed; the five `disallow` typos removed | `every_key_spelling_google_accepts_is_read` (3; the second also `a_key_and_value_split_by_whitespace_alone_is_read`) |
| 2026-11-16, recommendation #1: the colon-less split removed; its two-word check removed; `* more` not naming every agent | `a_key_and_value_split_by_whitespace_alone_is_read` (3; the first also `only_the_crawl_delay_key_itself_is_read`) |
| 2026-11-16, recommendation #1: the Crawl-delay key matched by prefix | `only_the_crawl_delay_key_itself_is_read` |
| 2026-11-16 review: `*` named every agent only when a space follows; only when a space or tab follows | `a_key_and_value_split_by_whitespace_alone_is_read` (2) |
| 2026-11-16 review: the line stripped by `str::trim`; vertical tab dropped from `ASCII_SPACE` | `only_ascii_whitespace_is_stripped` (2; the second also `a_key_and_value_split_by_whitespace_alone_is_read`) |
| 2026-11-17: `cmd_robots::SCHEMA` set back to `newsvault.robots/2` | `a_refresh_report_is_robots_schema_major_three` |
| 2026-11-16 review: an unknown line ending the `user-agent` run | `a_crawl_delay_does_not_end_a_user_agent_run` |
| 2026-11-16 review: `parse_verdict_of` comparing with `>=`; the dry run's `parse_verdict` set to `rules` | `a_refresh_reports_the_parse_verdict_of_each_answered_row`, `a_robots_refresh_dry_run_plans_one_url_per_row_and_writes_nothing` (one each) |
| 2026-11-16 review: a fractional digit past the ninth dropped again | `a_crawl_delay_that_is_not_a_number_is_refused` |
| 2026-11-16 review, re-run after recommendation #2: a BOM after leading whitespace not skipped before the `<` check | `an_html_body_is_refused` |
| 2026-11-16: the Crawl-delay check dropped from `parse_verdict_of`; `parse_verdict` dropped from `render_human` | `a_refresh_reports_the_parse_verdict_of_each_answered_row` (2) |
| 2026-11-16 review: `orphan_rules_ignored` returned by `parse_verdict_of` before the Crawl-delay check | `a_refresh_reports_the_parse_verdict_of_each_answered_row` |
| 2026-11-16 review: an empty orphan `Allow`/`Disallow` sets `orphan_rules_ignored`; an empty orphan Crawl-delay sets it | `a_rule_before_the_first_user_agent_is_ignored` (2) |
| 2026-11-16: the `CrawlDelayOverInterval` arm of `main.rs`'s `robots_refusal` never matching | `a_path_or_unreadable_robots_refusal_names_its_fact_and_suggests_no_refresh` |
| an unreadable body read as no robots | `a_pinned_robots_body_that_does_not_parse_is_refused` |
| a blob missing at open read as no robots | `a_robots_blob_written_after_the_provider_opened_is_unverified` |
| the path arm dropped from `main.rs`'s `robots_refusal` | `a_path_or_unreadable_robots_refusal_names_its_fact_and_suggests_no_refresh` |
| the index url's query dropped | `the_admitted_paths_are_each_admitted_index_with_its_query_and_its_rule_prefix` |
| 2026-11-17: the index page rule removed; applied to `Disallow` too; its prefix test made exact (`/index.htm` or `/index.html` only) | `an_index_page_allow_also_allows_its_directory_as_google_reads_it` (3) |
| 2026-11-17: a non-ASCII pattern byte kept raw; escaped with lowercase hex | `a_pattern_is_escaped_as_googles_parser_escapes_it`, `an_index_page_allow_also_allows_its_directory_as_google_reads_it`, `a_long_line_is_cut_where_googles_parser_cuts_it` (2) |
| 2026-11-17: a pattern's `%xx` not uppercased | `a_pattern_is_escaped_as_googles_parser_escapes_it` |
| 2026-11-17: `Disallow` wins a tie | `a_pattern_is_escaped_as_googles_parser_escapes_it`, `allow_wins_a_tie_and_the_longest_match_decides`, `an_index_page_allow_also_allows_its_directory_as_google_reads_it` |
| 2026-11-17: no line cut; the cut one byte later; one byte earlier | `a_long_line_is_cut_where_googles_parser_cuts_it` (3) |
| 2026-11-17: the NUL cut removed; removed with NUL stripped as whitespace | `a_line_ends_at_its_first_nul_as_googles_parser_reads_it` (2) |
| 2026-11-17: the hosts loader's check removed | `an_index_target_holding_a_percent_a_non_ascii_or_a_control_byte_is_refused` |
| 2026-11-17: the rules loader's check removed | `a_path_filter_holding_a_percent_a_non_ascii_or_a_control_byte_is_refused` |
| 2026-11-17: `%`, a non-ASCII byte, a control byte each let through `plain_path` | `an_index_target_holding_a_percent_a_non_ascii_or_a_control_byte_is_refused`, `a_path_filter_holding_a_percent_a_non_ascii_or_a_control_byte_is_refused` (3) |
| 2026-11-17 review: `#` let through `plain_path` | `a_path_filter_holding_a_percent_a_non_ascii_or_a_control_byte_is_refused` |
| 2026-11-17 review: an index directory's priority made its `Allow` pattern's length | `an_index_page_allow_also_allows_its_directory_as_google_reads_it` |
| 2026-11-17 review: a `%xx`'s second hex digit kept as written | `a_pattern_is_escaped_as_googles_parser_escapes_it` |
| 2026-11-17 review: the line cut after the whitespace strip | `a_long_line_is_cut_where_googles_parser_cuts_it` |
| 2026-11-17 review: a Crawl-delay read from the cut line; cut at its NUL only | `a_crawl_delay_line_is_read_whole_and_not_cut_as_googles_parser_cuts_a_rule_line` (2) |
| 2026-11-17, (e): the UTF-8 refusal restored as `NotUtf8` | `a_body_that_is_not_utf8_is_read_as_bytes_as_googles_parser_reads_it`, `an_html_body_is_refused`, `the_bodies_google_reads_and_ours_refuses_are_the_named_exceptions` |
| 2026-11-17, (e): a non-UTF-8 body refused again (as `HtmlPage`) | `a_refresh_reports_the_parse_verdict_of_each_answered_row`, `a_body_that_is_not_utf8_is_read_as_bytes_as_googles_parser_reads_it`, `an_html_body_that_is_not_utf8_or_starts_with_unicode_space_is_refused`, `the_bodies_google_reads_and_ours_refuses_are_the_named_exceptions` (4, rerun at 16062d4; 4 also at 7954f8d, with `an_html_body_is_refused` in place of the third) |
| 2026-11-17, (e): only a whole BOM skipped | `a_body_that_is_not_utf8_is_read_as_bytes_as_googles_parser_reads_it`, `an_html_body_is_refused` |
| 2026-11-17, (e): the `<` check made to need a whole UTF-8 body | `an_html_body_is_refused` |
| 2026-11-17 review, 3b: the `<` check read over the leading valid UTF-8 again; non-ASCII bytes not skipped | `an_html_body_is_refused`, `an_html_body_that_is_not_utf8_or_starts_with_unicode_space_is_refused` (2 each) |
| 2026-11-17 review, 3b: NUL not skipped; only BOM bytes skipped, no Unicode or Latin-1 whitespace | `an_html_body_that_is_not_utf8_or_starts_with_unicode_space_is_refused` (1 each) |
| 2026-11-17 review: a Crawl-delay value decoded lossily again | `the_bodies_google_reads_and_ours_refuses_are_the_named_exceptions` |

4. Held filter: `Finalised` carries the `Verdict`, `attempts.story_id`, `attempts.held`, live covered set, ledger check. Amends R16 and R20.
   - Built 2026-11-15: the `Verdict` on `Finalised` and `attempts.verdict`; `attempts.story_id` and the held read; the live arm's held set and the open-web filter; `attempts.held` and the `ledger --check` subtraction (§11.1, §8 rows R4, R16, R20, R21).
   - Every mutation named for step 4 in this document was re-run at `8c071ff` (code as of `f5dedec`, step 4's last code commit; before PR #24's review fixes and before decisions 6 and 7), 2026-11-15, one at a time, each file copied aside and restored, `cargo test --locked -p <crate> --no-fail-fast -- <test>`: 26 mutations, 30 red results of 30, each its named test (some mutations checked against 2 or 3 tests).
   - Review round, 2026-11-15, same procedure, not in the table below: 8 mutations, each red on the test named beside it in §3.2 `held`, §8 rows R4 and M1, and §11.1.
   - Owner decisions 6 and 7 (§10), 2026-11-15, not in the table below: the mutations named beside their tests in §3.2 `listing_unusable`, §8 rows R4, R16, R21 and M1, and §11.1 were each run once when the test was written, and not re-run on the final tree.
   - Re-run on d2f8a78, 2026-11-15, `cargo test --locked --no-fail-fast`, each file copied aside and restored: `MIGRATIONS[3]` without its `ALTER` and `>= 0` in the listing exclusion (the collateral counts in §8 row M1 and §11.1); the listing exclusion's `verdict = usable` dropped and open-web's `BlankSourceItemId` marked `Page` (both green, §11.1). The robots 408 mutation (§10) ran on the new test.

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
   - must keep the held read's 3xx exclusion (§11.1, owner decision 2026-11-15); its loopback test must show a refused redirect fetched again by the next poll.
   - Built 2026-11-15: `Wire` refusing redirects for open-web over a loopback server; since 2026-11-16 for every provider, with no profile knob (owner decision v, §8 row R19).
   - Built 2026-11-15: the held read's 3xx exclusion reached through the real `Wire`, `a_document_answered_with_a_redirect_is_fetched_again_by_the_next_poll` over `services/newsvault/tests/loopback/mod.rs` (§8 row R21).
   - Built 2026-11-15: a robots.txt 3xx refused through the real `Wire`, `a_robots_redirect_is_a_permanent_failure_with_no_robots_row_and_its_target_never_fetched` (§11.2).
   - Every mutation named for step 5 in this document was re-run at `906f87a` (step 5's last code commit), 2026-11-15, one at a time, each file copied aside and restored, `cargo test --locked -p <crate> --no-fail-fast -- <test>`: 11 mutations, 16 test runs, 15 red and 1 green, the green one expected (the table's last row). Five of them mutated the `Redirects` knob removed 2026-11-16 and are no longer listed; that day's redirect mutations are §8 row R19's.

| mutation | named test red (the tests run, one per `-- <test>` filter; a mutation may turn others red too) |
|---|---|
| `.max_redirects(..)` removed from `Wire::with_sleeper` (ureq's default, 10, applies); re-run 2026-11-16 after decision v, `cargo test -p news-provider redirect` and `-p newsvault redirect` | at `906f87a`: `a_refused_redirect_is_answered_as_its_3xx_and_its_target_never_requested`, `a_document_answered_with_a_redirect_is_fetched_again_by_the_next_poll`, `a_robots_redirect_is_a_permanent_failure_with_no_robots_row_and_its_target_never_fetched` (3); 2026-11-16: `a_redirect_is_answered_as_its_3xx_and_its_target_never_requested` (the first, renamed), `every_redirect_is_answered_as_itself`, `a_redirect_whose_body_is_over_the_limit_fails_with_the_status_its_head_carried`, `an_edgar_document_answered_with_a_redirect_is_a_permanent_failure_and_its_hop_never_sent`, the other two above and their over-limit twins (8) |
| a `300..=399 => TransientFailure` arm in open-web `classify` | `a_redirect_is_permanent_never_transient` |
| a `300..=399` arm in `cmd_robots::verdict_of` mapped to `Empty`; mapped to `Usable` | `a_robots_redirect_is_a_permanent_failure_with_no_robots_row_and_its_target_never_fetched` (2) |
| the held read's 3xx exclusion dropped (`AND NOT (0 AND ?5 = ?6)`, so both bounds stay bound) | `a_permanent_document_answered_with_a_redirect_holds_nothing_and_one_with_no_status_holds`, `a_document_answered_with_a_redirect_is_fetched_again_by_the_next_poll` (2) |
| that exclusion's `COALESCE` dropped | `a_permanent_document_answered_with_a_redirect_holds_nothing_and_one_with_no_status_holds`; `a_document_answered_with_a_redirect_is_fetched_again_by_the_next_poll` green, expected: its row carries 301, never NULL |

   - Not re-run: the `verdict = permanent_failure` condition of the 3xx exclusion, which no test guards (§11.1).
   - Holes step 5 leaves open: the `Location` target is recorded nowhere (§11.2); the 3xx refetch cost is counted by no ledger or envelope field (§8 row R4); no run against a live host yet.
6. One PR per further host.
   - Built 2026-11-16: PRN. The `prn` rule and the second hosts row (§11.1), its fixtures (§11.2), and the two-host tests. No production code changed outside the two JSON files and the crate doc comment. Its end-to-end run is the four-host one below, since 2026-11-17.
   - Every PRN mutation was run at `ae0bfa2` (PRN's last code commit), 2026-11-16, one at a time, each file copied aside and restored, `cargo test --locked -p <crate> --no-fail-fast -- <test>`: 13 mutations, 18 test runs, each running exactly its 1 named test, 18 red. The 5 rows not marked `review` that name the end-to-end test were run again 2026-11-17 against the three-host test (the CNW mutation method below), and again that day against the four-host test, the plan-order test and the dry run (the Fed method below): each red there. The rest were not re-run after the renames.
   - Rows marked `review` are not in that count: run 2026-11-16 after review, one at a time, the rules file copied aside and restored, `cargo test --locked -p open-web-feed -p newsvault --no-fail-fast`, every test of both crates; the cell names every test that went red.

| mutation | named test red |
|---|---|
| the PRN hosts row `candidate` | `the_committed_hosts_file_plans_gnw_then_prn_then_cnw_then_the_fed_then_yahoo`, `an_open_web_dry_run_plans_one_index_request_per_admitted_host_and_writes_nothing`, `an_open_web_run_collects_a_gnw_a_prn_a_cnw_a_fed_and_a_yahoo_day_end_to_end` (3) |
| the two hosts rows swapped | `the_committed_hosts_file_plans_gnw_then_prn_then_cnw_then_the_fed_then_yahoo` |
| the PRN hosts row removed | `an_open_web_run_collects_a_gnw_a_prn_a_cnw_a_fed_and_a_yahoo_day_end_to_end` |
| one hex digit of the PRN pin changed | `the_committed_hosts_file_pins_the_committed_robots_body`, `an_open_web_run_collects_a_gnw_a_prn_a_cnw_a_fed_and_a_yahoo_day_end_to_end`, `the_committed_hosts_file_refreshed_with_its_committed_bodies_matches_every_pin` (3) |
| PRN's `path_prefix` set to GNW's `/news-release/` | `the_real_pr_newswire_index_yields_one_follow_up_per_release_with_its_raw_pub_date` |
| PRN's `path_prefix` set to `/multivu/` | `an_open_web_run_collects_a_gnw_a_prn_a_cnw_a_fed_and_a_yahoo_day_end_to_end` |
| `%S` for `:00` in PRN's format, with `reads_a_minute` accepting any format and `minute_of` truncating to the minute | `a_pr_newswire_pubdate_is_a_minute_and_never_an_instant` |
| `minute_of`'s round trip removed | `a_pr_newswire_pubdate_is_a_minute_and_never_an_instant` |
| PRN's container `body` | `the_extracted_body_is_article_text_and_never_page_html_on_a_pr_newswire_page` |
| PRN's container `article` | `the_extracted_body_is_article_text_and_never_page_html_on_a_pr_newswire_page`, `a_pr_newswire_page_with_no_release_body_is_unusable_as_no_article_body` (2) |
| a PRN rewrite `www.prnewswire.com` → `prnewswire.com` | `the_real_pr_newswire_article_carries_its_title_its_canonical_link_and_its_www_item_key` (the key printed without `www.`) |
| `document_of` labelling the minute `Published::At` | `an_open_web_run_collects_a_gnw_a_prn_a_cnw_a_fed_and_a_yahoo_day_end_to_end` |
| every robots `Disallow` read as `Allow`; after review, 2026-11-16, run again (`allow: true`) and with `Disallow` lines skipped instead, `cargo test --locked -p newsvault --lib --no-fail-fast -- the_prn_robots_allows`: each red | `the_prn_robots_allows_the_rss_index_and_the_news_releases_prefix` |
| `review`: PRN's container `section`, which matches 5 elements on each untrimmed probe page (`nav-junior`, `nav-main`, the release body, `read-more-explore`, `lead-wrapper`); the trimmed `article_302939553.html` keeps `nav-main` with one `<p>` | `the_extracted_body_is_article_text_and_never_page_html_on_a_pr_newswire_page`, `a_pr_newswire_pubdate_is_a_minute_and_never_an_instant`, `the_real_pr_newswire_article_carries_its_title_its_canonical_link_and_its_www_item_key`, `the_open_web_feed_breaks_no_claim_of_the_seam`, `an_open_web_run_collects_a_gnw_a_prn_a_cnw_a_fed_and_a_yahoo_day_end_to_end` (5; each page is `ArticleContainerAmbiguous`) |
| `review`: PRN's `path_prefix` set to `/` | `a_pr_newswire_link_outside_news_releases_is_filtered_out_and_counted` |

   - Holes step 6 leaves open for PRN:
     - one page template seen (2 pages, both `<article class="news-release inline-gallery-template">`): another template is `NoArticleBody`, decided by the page, so held and never fetched again; only the `NoArticleBody` share per host (§11.3, not built) would show it.
     - a retitled release changes its slug, so its `item_key`, while the 9-digit id stays (§8 row R3).
     - a correction notice listed under a release link (302913655, 2026-11-14) keys that release's story: when the release is not held, the correction's minute is stored for it; when it is held, its listed minute is a `relisted` row (owner decision 10, 2026-11-16; §8 rows R3, R15).
     - 4 same-title, same-slug pairs under distinct ids, and 13 of 40 items not `en-US`: each id is its own story (§11.1).
     - GNW now depends on PRN's robots row: `refuse_an_unverified_robots` (`cmd_collect.rs`) checks every admitted row and refuses the whole run at the first failure, so a missing PRN refresh, an unverified PRN blob or a changed PRN robots body also stops GNW collection (§8 row R4; `a_second_admitted_host_without_a_ledgered_fetch_is_refused`). Refusing per host is an owner option.
     - no run against the live host yet.
   - Built 2026-11-17: CNW. The `cnw` rule and the third hosts row (§11.1), the news sitemap reader and the offset-reading `minute_of` (`parse.rs`, `rules.rs`, `lib.rs`; `extract_version` moved), its fixtures (§11.2), and the three-host tests. Index choice, container and every other default: §10's CNW table.
   - Its end-to-end run is the five-host one of the Yahoo bullet below, since 2026-11-17; its CNW rows: `valid_minute` 07:22 and 06:47Z, `valid_raw` the `-05:00` text (`02:22:00-05:00`, `01:47:00-05:00`), `valid_field` `news:publication_date`.
   - Removed with the step 2 clock: the two-host run's check that GNW's 19:00 minute is still open at 19:00:45Z; `an_open_web_run_collects_a_globenewswire_day_end_to_end` keeps it (step 2's table).
   - Every CNW mutation below was run 2026-11-17 on the working tree before the first CNW commit, one at a time, the file restored after each by a script (not kept), `cargo test --locked --no-fail-fast -p open-web-feed -p newsvault`, every test of both crates: 18 mutations, 18 red. Rows marked review were run 2026-11-17 after the PR's review, the same way, on the commit adding their tests; the og:url row is the reviewer's run, not repeated. A cell names the CNW test the mutation was aimed at and the count of all red tests; every mutation of the rules file also turns `every_golden_page_extracts_to_its_committed_body_under_the_committed_version` red, because the rules digest is in `extract_version`. The 4 rows naming the end-to-end test, the plan-order test or the dry run were run again 2026-11-17 against the five-host tests (the Yahoo bullet below): each red.

| mutation | named test red (all red) |
|---|---|
| CNW's container `div.release-content` | `a_canada_newswire_page_with_no_release_body_is_unusable_as_no_article_body`, `an_open_web_run_collects_a_gnw_a_prn_a_cnw_a_fed_and_a_yahoo_day_end_to_end` (4) |
| CNW's container `body` | `the_extracted_body_is_article_text_and_never_page_html_on_a_canada_newswire_page` (5) |
| CNW's container `section` (`nav-main` kept in both real fixtures) | `the_extracted_body_is_article_text_and_never_page_html_on_a_canada_newswire_page` (6; each real page `ArticleContainerAmbiguous`) |
| CNW's container `article` | `the_extracted_body_is_article_text_and_never_page_html_on_a_canada_newswire_page`, `a_canada_newswire_page_with_no_release_body_is_unusable_as_no_article_body` (5) |
| CNW's `path_prefix` `/` | `a_canada_newswire_link_outside_news_releases_is_filtered_out_and_counted`, `the_committed_canada_newswire_row_asks_its_sitemap_with_its_query_and_its_prefix` (3) |
| CNW's `path_prefix` GNW's `/news-release/` | `the_real_canada_newswire_sitemap_yields_one_follow_up_per_release_with_its_raw_date` (11) |
| a CNW rewrite `www.newswire.ca` → `newswire.ca` | `the_real_canada_newswire_article_carries_its_title_its_canonical_link_and_its_item_key` (10; every CNW link leaves its index's host) |
| the `cnw` rule's `index` `rss` | `the_canada_newswire_index_is_its_news_sitemap_and_an_rss_answer_is_permanent` (11) |
| one hex digit of the CNW pin changed | `the_committed_hosts_file_pins_the_committed_robots_body`, `the_committed_hosts_file_refreshed_with_its_committed_bodies_matches_every_pin`, `an_open_web_run_collects_a_gnw_a_prn_a_cnw_a_fed_and_a_yahoo_day_end_to_end`, `an_open_web_dry_run_plans_one_index_request_per_admitted_host_and_writes_nothing` (4) |
| the CNW hosts row `candidate` | `the_committed_hosts_file_plans_gnw_then_prn_then_cnw_then_the_fed_then_yahoo`, `an_open_web_dry_run_plans_one_index_request_per_admitted_host_and_writes_nothing` (16) |
| `minute_of` reading the offset's local time as UTC | `a_canada_newswire_publication_date_is_a_minute_at_its_own_offset_and_never_an_instant`, `an_open_web_run_collects_a_gnw_a_prn_a_cnw_a_fed_and_a_yahoo_day_end_to_end` (3) |
| `minute_of`'s round trip removed | `a_canada_newswire_publication_date_is_a_minute_at_its_own_offset_and_never_an_instant` (5, the GNW and PRN minute tests among them) |
| the sitemap reader matching local names in any namespace | `a_sitemap_element_named_like_a_field_in_another_namespace_is_never_read` (2) |
| the loader's `printed` without the offset (`%:z` cannot print) | every test of the two crates that opens the committed rules file (144; `FormatReadsNoMinute` for `cnw`) |
| review, 2026-11-17: the sitemap text read untrimmed | `a_sitemap_row_is_a_top_level_url_with_its_trimmed_loc_and_its_news_child_only` (2) |
| review: the `news:news` step skipped, the field read anywhere under `url` | `a_sitemap_row_is_a_top_level_url_with_its_trimmed_loc_and_its_news_child_only` (2) |
| review: `urlset`'s `children()` read as `descendants()` | `a_sitemap_row_is_a_top_level_url_with_its_trimmed_loc_and_its_news_child_only` (2) |
| review: the root check refusing only an `rss` root | `the_canada_newswire_index_is_its_news_sitemap_and_an_rss_answer_is_permanent` (2; a `sitemapindex` and a `urlset` with no namespace) |
| review: `minute_of` keeping the local date with the UTC time of day | `a_canada_newswire_publication_date_is_a_minute_at_its_own_offset_and_never_an_instant` (2; `20:30:00-05:00` is 01:30Z the next day, 13 of 246 keys on 2026-11-16) |
| review: the `cnw` canonical pick `meta[property="og:url"]` | none but the golden test: equal to the canonical link on all 11 page files, so equivalent on the data |
| the loader's `news:` check removed | `an_unknown_index_or_a_sitemap_field_outside_news_is_refused` (2) |
| an unknown `index` read as `rss` | `an_unknown_index_or_a_sitemap_field_outside_news_is_refused` (2) |
| every robots `Disallow` read as `Allow` | `robots::tests::the_cnw_robots_allows_the_news_sitemap_and_the_news_releases_prefix` (19, PRN's twin among them) |
| robots `Disallow` lines skipped | `robots::tests::the_cnw_robots_allows_the_news_sitemap_and_the_news_releases_prefix` (19) |

   - Holes step 6 leaves open for CNW:
     - one page template seen (11 files, all `<article class="news-release inline-gallery-template">`), as for PRN; another template is `NoArticleBody`, decided by the page, so held.
     - no real page tells `section.release-body` from `div.release-content` (§11.1); only the synthetic page pins the choice.
     - French releases: robots.txt names `/fr/sitemap-news.xml`, and neither English index lists a `/fr/` release; none is collected, and their daily count is unknown (§11.1).
     - the worst-case refetch cost of CNW's own queue is at least 35.88% of a day (waits only, request times not counted), and 107.64% when every fetch also fails twice, above the Pace bullet's 50% bound; a first run on an empty store asks 100 CNW pages in one poll, above a 30-min slot's stop deadline (§11.1's first-run hole); both wait on step 7's cap and deadline (§11.1's cost bound).
     - CNW now depends on GNW's and PRN's robots rows and they on its: one unverified row refuses the whole run (§8 row R4), as for PRN.
     - a value at `-04:00` (daylight time) is read by the format but never observed (§11.1).
     - CNW lists a later date under the same `loc`: 3 of 246 sitemap keys over the 26 polls of 2026-11-16 (§10's CNW table), 2 of them retitled `/C O R R E C T I O N - …/`; the RSS `pubDate` moved the same way on the same 3. When the release is not held, the first sighting's minute and body are stored, whichever listing that was; when it is held, the later minute is a `relisted` row and the corrected body is never fetched (owner decision 10, 2026-11-16; §8 rows R3, R15).
     - one release listed by both PRN and CNW is two stories, two `doc_id`s and two fetches: `item_key` is the index's own host plus the path. Measured 2026-11-17 over the 26 polls of 2026-11-16 (Python ElementTree; titles lowercased, runs of non-word characters made one space; every title either index listed for a key): 34 (PRN key, CNW key) pairs share a title, over 30 of the 246 CNW keys; 29 share a UTC minute; 2 share the 9-digit id (834609094, 808938924). A lower bound: PRN's RSS lists 20 items a poll. Owner decision `PG-D9` of 2026-11-17: such a release stays two stories in the archive; merging them belongs to `derive`.
     - no run against the live host yet; the pin is the 2026-11-16 08:04:29Z body, not re-fetched.
   - Built 2026-11-17: the Fed. The `fed` rule and the fourth hosts row (§11.1), its fixtures (§11.2), and the multi-host tests; no reader changed, `lib.rs`'s doc comment did, so `extract_version` moved. Container, format, pin and every other default: §10's Fed table.
   - Its end-to-end run is the five-host one of the Yahoo bullet below, since 2026-11-17; its Fed rows: `valid_minute` 2026-11-13T21:15Z and 2026-11-09T21:00Z, `valid_raw` `Fri, 13 Nov 2026 21:15:00 GMT` and `Mon, 9 Nov 2026 21:00:00 GMT`, `valid_field` `pubDate`, `doc_id` from the requested url.
   - Every Fed mutation below was run 2026-11-17 on the working tree before the first Fed commit, one at a time, the file restored after each by a script (not kept), `cargo test --locked --no-fail-fast -p open-web-feed -p newsvault`, every test of both crates. Rows marked review were run 2026-11-17 after the PR's review, the same way, on the commit adding their tests; the stub swaps ran `-p newsvault --test open_web_collect` only. 19 mutations: 18 red on a named behaviour test, 1 (the paragraph without `figcaption`) red only through the golden test. A cell names the Fed test the mutation was aimed at and the count of all red tests; every mutation of the rules file also turns the golden test red (`extract_version`).

| mutation | named test red (all red) |
|---|---|
| the Fed's container `body` | `the_extracted_body_is_article_text_and_never_page_html_on_a_federal_reserve_page`, `a_federal_reserve_page_with_no_article_block_is_unusable_as_no_article_body` (5) |
| the Fed's container `div#article` | `the_extracted_body_is_article_text_and_never_page_html_on_a_federal_reserve_page` (2) |
| the Fed's container `div#article > div.col-md-8` | `the_extracted_body_is_article_text_and_never_page_html_on_a_federal_reserve_page` (6; each real page `ArticleContainerAmbiguous`) |
| the Fed's container `div.col-md-8:not(.heading)` | `the_extracted_body_is_article_text_and_never_page_html_on_a_federal_reserve_page`, `a_federal_reserve_page_with_no_article_block_is_unusable_as_no_article_body` (7) |
| the Fed's `path_prefix` PRN's `/news-releases/` | `the_real_federal_reserve_feed_yields_one_follow_up_per_release_with_its_raw_pub_date` (14) |
| the Fed's `path_prefix` `/` | `a_federal_reserve_link_outside_press_releases_is_filtered_out_and_counted`, `the_committed_federal_reserve_row_asks_its_feed_and_its_prefix` (3) |
| `%d` for `%-d` in the Fed's format | `a_federal_reserve_pubdate_is_a_minute_with_an_unpadded_day_and_never_an_instant`, `an_open_web_run_collects_a_gnw_a_prn_a_cnw_a_fed_and_a_yahoo_day_end_to_end` (6) |
| `minute_of`'s round trip removed | `a_federal_reserve_pubdate_is_a_minute_with_an_unpadded_day_and_never_an_instant` (6, the GNW, PRN and CNW minute tests among them) |
| a Fed rewrite `www.federalreserve.gov` → `federalreserve.gov` | `the_real_federal_reserve_release_carries_its_title_its_own_url_and_its_item_key` (13; every Fed link leaves its index's host) |
| review: the Fed's canonical pick `meta[property="og:url"]` `content` | `a_federal_reserve_page_naming_a_canonical_link_is_stored_under_it` (2); on the 9 real files `og:url` equals the requested url, so only the synthetic canonical tells the two apart |
| review: the Fed's canonical pick `link[rel="x"]` | `a_federal_reserve_page_naming_a_canonical_link_is_stored_under_it` (2) |
| review: the Fed's `path_prefix` without its trailing `/` | `a_federal_reserve_link_outside_press_releases_is_filtered_out_and_counted`, `the_committed_federal_reserve_row_asks_its_feed_and_its_prefix` (3) |
| review: the Fed's paragraph `p, li` | none but the golden test (1): no Fed fixture holds a `figcaption` (0 of 5 files, `grep -c`, 2026-11-17); §8 row R17 |
| review: the stub's pages for Fed releases 0 and 2 swapped; the same for PRN 1 and 2, CNW 0 and 1 | `an_open_web_run_collects_a_gnw_a_prn_a_cnw_a_fed_and_a_yahoo_day_end_to_end` (1 each); the Fed swap green before the row-content assertion |
| the Fed hosts row `candidate` | `the_committed_hosts_file_plans_gnw_then_prn_then_cnw_then_the_fed_then_yahoo`, `an_open_web_dry_run_plans_one_index_request_per_admitted_host_and_writes_nothing` (18) |
| the Fed hosts row removed | `an_open_web_run_collects_a_gnw_a_prn_a_cnw_a_fed_and_a_yahoo_day_end_to_end` (19) |
| the Fed pin set to the 404 page's digest | `the_committed_hosts_file_pins_the_committed_robots_body`, `the_committed_hosts_file_refreshed_with_its_committed_bodies_matches_every_pin`, `an_open_web_run_collects_a_gnw_a_prn_a_cnw_a_fed_and_a_yahoo_day_end_to_end`, `an_open_web_dry_run_plans_one_index_request_per_admitted_host_and_writes_nothing` (8) |

   - Holes step 6 leaves open for the Fed:
     - one page template seen (9 files, 5 URLs, all `div#article` with a heading block and one release column); another template is `NoArticleBody` or `ArticleContainerAmbiguous`, decided by the page, so held and never fetched again.
     - the lag from a `pubDate` minute to the feed listing it is one point, not a distribution: all 26 polls of 2026-11-16 answered `Last-Modified: Fri, 13 Nov 2026 21:15:18 GMT` (one ETag, one body sha256, Cloudflare `HIT`), 18 s after the newest item's `pubDate` 21:15:00 GMT (bcreg20261113a). `Last-Modified` is when the file changed, not when a client could first read it. No new item was listed that day; the 20 items span 2026-09-25 to 2026-11-13, so a release stays listed for weeks (window 1,176.75 h, §11.2).
     - a `pubDate` with a zero-padded day is `NoParseableValidTime` and fetched by every poll while listed; never observed (0 of 9 single-digit days, 2026-11-16).
     - release text that names a person is stored as it stands: enforcement20261112a names the individual the action is against (P3; r removes addresses only). Media contacts are `div` cells and are not read; a release that puts them in a `p` would store names and numbers, as on PRN and CNW (decision r).
     - the Fed depends on every other admitted robots row and they on its: one unverified row refuses the whole run (§8 row R4), as for PRN and CNW. Its row needs a ledgered 404; a robots.txt that appears later answers 200, which differs from `absent`, so the refresh exits 1 and `collect` refuses the Fed row until the hosts file is edited.
     - no run against the live host yet; the 404 is the 2026-11-16 08:04:29Z answer, not fetched again.
   - Built 2026-11-17: Yahoo, approved by the owner 2026-11-17 ("tak, ruszaj z 1 i 2"). The `yahoo` rule with `provider_keys` and the fifth hosts row (§11.1, §11.2), the sitemap reader's `image:loc` and `news:title`, the cross-check (§11.2, option C) with its store read (`Committer::index_payloads`) and envelope field, its fixtures (§11.2), and the five-host tests. Every default: §10's Yahoo table.
   - End to end, `services/newsvault/tests/open_web_collect.rs::an_open_web_run_collects_a_gnw_a_prn_a_cnw_a_fed_and_a_yahoo_day_end_to_end`: the committed hosts file, four robots rows refreshed with their committed bodies and the Fed's answered 404 with its trimmed page, a stub `Exchange` and a clock at 2026-11-16T09:30:30Z, the Yahoo fixture's own poll and after every fixture's newest minute (Yahoo's 09:17Z), so no item is observed before it was published.

| what the run asserts | value |
|---|---|
| attempts | 19: per host 1 index + 3 documents, Yahoo 1 index + 2; the 5 indexes first, GNW, PRN, CNW, Fed, Yahoo |
| per index `vendor_rows` / `unusable_rows` | 3 / 0; Yahoo 8 / 0 |
| items committed | 9: 2 per host, 1 for Yahoo; 1 page per host `NoArticleBody` |
| PRN rows | `valid_minute` 17:15 and 17:01 (2026-11-14), `valid_raw` the `+0000` `pubDate` text |
| CNW and Fed rows | as their bullets above |
| Yahoo row | `valid_minute` 2026-11-16T09:17Z, `valid_raw` `2026-11-16T09:17:00Z`, `valid_field` `news:publication_date`, `doc_id` from the page's canonical link, its own url |
| row content, all 9 | per story: `raw_blob` the sha256 of the fixture page that story's url was answered with, `body_blob` that page's digest in `golden_bodies.json`, `item_id` over that page's literal `og:title` and that body |
| `attempts.hosts_version`, `attempts.extract_version` | the committed file's digest and the envelope's version, on all 19 |
| `items --at` 09:30:00Z / 09:31:00Z | 0 / 9 rows: nothing before the run's observation (09:30:29Z) |
| `cross_check` | `watched_from` this run's first direct answer (09:30:29Z), 4 direct payloads read and 4 parsed; rows: followed 2, cross-checked 3, other key 2, no key 1; all 3 cross-checked rows predate `watched_from`, so `not_compared` 3 and none unlisted; `error` null; the human line `0 unlisted directly, 0 listed, 3 not compared, 0 not looked`; a dry run of the same ask carries `cross_check` null |

   - Also: `the_committed_hosts_file_refreshed_with_its_committed_bodies_matches_every_pin` (exit 0, 5 rows, Yahoo's with verdict `rules`); the dry run plans 5 index requests (§11.2); the round-robin order and replay tests serve 5 lanes, Yahoo's 2 releases running out first (§10).
   - The cross-check through collect, `a_yahoo_row_under_a_wire_key_no_direct_index_listed_is_named_in_the_run_envelope`: the CNW and Yahoo rows alone; a run at 05:00:30Z whose Yahoo index answers 503 three times (`result` null), then one at 09:30:30Z whose Yahoo index answers 503 once and then the fixture. `watched_from` is the first run's CNW answer (05:00:29Z); that answer predates `read_from` (07:22Z less 2 h), so 1 direct payload is read; ELEGRP listed, Geely (`globenewswire.com`) and Horizon (`prnewswire.com`) unlisted.
   - `a_cross_check_row_no_direct_index_answered_after_is_not_looked`: the same two hosts, a run at 00:00:30Z with Yahoo 503 three times, then one at 09:30:30Z with CNW 503 three times: 3 payloads read (the 503 bodies), 0 parsed, 3 rows `not_looked`, none unlisted. `a_damaged_direct_blob_is_a_cross_check_error_and_the_run_finishes`: the first run's CNW sitemap blob altered, then removed, before a run whose CNW answers 503: `error.code` `blob_altered`, then `blob_absent`, `result` null, exit `PartialCollect` (the CNW 503), all 3 run envelopes finished.
   - Every Yahoo mutation below but the `review` rows was run 2026-11-17 on the working tree before the first Yahoo commit, one at a time, the file restored after each by a script (not kept), `cargo test --locked --no-fail-fast` on `-p open-web-feed`, `-p newsvault` or both, the crates whose tests the file reaches. 38 mutations: 37 red on a named behaviour test; 1 green but for the golden test, kept as a row. 13 more were run 2026-11-17 after the review fixes, the same way (the `review` rows, the earliest-minute row's second mutation and the `since` row's second test): 13 red on a named test. Every mutation of the crate's `src/` or rules file also turns the golden test red (`extract_version`), not listed below.

| mutation | named test red |
|---|---|
| the Yahoo hosts row `candidate` | `the_committed_hosts_file_plans_gnw_then_prn_then_cnw_then_the_fed_then_yahoo`, `an_open_web_dry_run_plans_one_index_request_per_admitted_host_and_writes_nothing`, the five-host end-to-end and round-robin tests |
| one hex digit of the Yahoo pin changed | `the_committed_hosts_file_pins_the_committed_robots_body`, `the_committed_hosts_file_refreshed_with_its_committed_bodies_matches_every_pin`, the five-host end-to-end test |
| the Yahoo hosts row removed | `an_open_web_run_collects_a_gnw_a_prn_a_cnw_a_fed_and_a_yahoo_day_end_to_end`, `the_committed_hosts_file_refreshed_with_its_committed_bodies_matches_every_pin` |
| the key matched by `contains`; by being contained; ignoring ASCII case | `a_provider_key_is_matched_whole_and_never_as_a_substring` (3) |
| the key filter removed; a row with no key followed | `the_real_yahoo_sitemap_follows_only_its_business_wire_and_accesswire_rows`, `a_provider_key_is_matched_whole_and_never_as_a_substring` (2) |
| `key_of` keeping the rest of the url after the mark | `the_real_yahoo_sitemap_follows_only_its_business_wire_and_accesswire_rows` and 10 more |
| `prnewswire.com` moved from `cross_check` to `follow`; `image_key_after` without `en/` | `the_real_yahoo_sitemap_follows_only_its_business_wire_and_accesswire_rows` (2) |
| the container `body`; `div.bodyItems-wrapper` | `the_extracted_body_is_article_text_and_never_page_html_on_a_yahoo_page` (2) |
| the container `article`; `div.body` | `a_yahoo_page_with_no_article_body_is_unusable_as_no_article_body` (2); the body test green: both read the trimmed fixture the same |
| the Yahoo `path_prefix` `/markets/` | `the_committed_yahoo_row_asks_its_sitemap_and_every_path`, `the_real_yahoo_sitemap_follows_only_its_business_wire_and_accesswire_rows` |
| a Yahoo rewrite `finance.yahoo.com` → `yahoo.com` | `the_real_yahoo_reprint_carries_its_title_its_canonical_link_and_its_item_key` |
| `minute_of`'s round trip removed | `a_yahoo_publication_date_is_a_minute_and_never_an_instant`, and the GNW, PRN, CNW and Fed minute tests |
| each of the 5 `provider_keys` loader checks removed | `a_provider_key_filter_nothing_can_read_whole_is_refused` (5) |
| every title read as listed; a `cross_check` row counted as followed | `a_cross_check_row_no_direct_index_listed_is_named_and_a_listed_one_counted` (2) |
| a title matched by a prefix; the case kept; a payload under an unknown url read under any direct rule that parses it | `titles_match_after_case_and_punctuation_fold_and_never_by_a_prefix` (3) |
| a payload under an unknown url read under the first direct rule | none: that rule is GNW's RSS, which reads the CNW sitemap the test hands it as nothing; the row above is the one that bites |
| `>=` as `>` against `watched_from` | `a_cross_check_row_dated_before_the_first_direct_answer_is_not_compared` |
| `min` as `max` for the earliest minute; the minimum over every row, not only `cross_check` rows (review 2026-11-17) | `the_earliest_cross_check_minute_is_the_oldest_cross_check_row` (2) |
| the keyed index's first attempt read; the `since` bound removed; `watched_from` the newest direct answer | `a_yahoo_row_under_a_wire_key_no_direct_index_listed_is_named_in_the_run_envelope` (3); the `since` bound also `a_cross_check_row_no_direct_index_answered_after_is_not_looked` |
| the cross-check never run | that test and the five-host end-to-end test |
| the stub's two Yahoo pages swapped | the five-host end-to-end test |
| every robots `Disallow` read as `Allow`; `Disallow` lines skipped | `robots::tests::the_yahoo_robots_allows_the_news_sitemap_and_every_path_but_its_disallowed_ones` (2) |
| review 2026-11-17: `DIRECT_LEAD` zero | `a_direct_answer_received_before_the_keyed_minute_still_lists_the_row`, `a_damaged_direct_blob_is_a_cross_check_error_and_the_run_finishes` |
| review: the `not_looked` check removed; an unparsed answer counted as looked | `a_row_no_direct_index_answered_after_is_not_looked_and_never_unlisted`, `a_cross_check_row_no_direct_index_answered_after_is_not_looked` (2) |
| review: any one direct index answering enough; the minute's start for its close; the oldest answer per index for the newest | `a_row_no_direct_index_answered_after_is_not_looked_and_never_unlisted` (2); the last `a_direct_answer_received_before_the_keyed_minute_still_lists_the_row` |
| review: the empty-title filter removed; `key_of` accepting an empty key | `an_empty_title_or_an_empty_key_matches_nothing` (2) |
| review: the human `cross-check` line dropped; a dry run's `cross_check` `{}` | the five-host end-to-end test (2) |
| review: a missing direct blob read as empty | `a_damaged_direct_blob_is_a_cross_check_error_and_the_run_finishes` |

   - Renamed 2026-11-17 for the fifth host: the end-to-end, plan-order, round-robin and replay tests. Every earlier mutation this document names against one of them was run again on the new name, the same way, `-- <name filter>`: 22, the PRN rows (candidate, rows swapped, row removed, pin digit, `/multivu/`, `Published::At`, container `section`), the CNW rows (container `div.release-content`, pin digit, candidate, offset local time as UTC), the Fed rows (`%d`, the 3 stub swaps, candidate, row removed, pin a digest), `commit_opened` binding NULL for `extract_version` (§3.2), and §10's 3 round-robin mutations (one lane, a turn that does not move, lanes sorted by host): each red.
   - Holes step 6 leaves open for Yahoo:
     - one followed page seen, Laverock (`business-wire.com`); no `accesswire.ca` page was fetched, so its template and container are untested. The container was read on 15 pages of other keys (§11.1).
     - Yahoo's own reprint is stored, not the wire's release: the body adds Yahoo's captions and drops nothing measured on GNW, while PRN and CNW reprints lose the `SOURCE` trailer and CNW's contact block (§11.6). No Business Wire or Accesswire original was compared.
     - the path filter is `/`, so collect checks robots on `/` and the sitemap only; Yahoo's `*` group has 32 `Disallow` lines below `/` (`/m/`, `/p/`, `/quote/*/earnings/*.html`, …). No followed row of the 26 polls of 2026-11-16 was under one (2,017 rows in 9 sections); a later one would be fetched. The per-document check that closes this is step 7's second hard gate.
     - a Yahoo url is not a stable identity: a retitled or moved row is a new key, so a new story and a second fetch (§11.6). Measured 2026-11-17 over the 26 polls of 2026-11-16 (Python): 488 followed urls carry 486 (key, title) pairs; 2 `business-wire.com` releases (Xsolla Clubhouse; the Pierre & Vacances correction) are each under 2 urls, so each would be stored twice.
     - a key is matched exactly: `business-wire.com` spelled otherwise, or a BW release Yahoo files under another key or none, is not followed and is counted only in `filtered_out` and the `cross_check` rows.
     - the index sizes now sum to 301 items a poll at most (20 + 20 + 100 + 20 + 141 followed Yahoo rows at p1500), above step 7's follow-up cap of 150, which is not built; new followed Yahoo rows were at most 57 a poll (§11.2).
     - a followed page near `BODY_LIMIT` is a `PermanentFailure`, stored cut: Yahoo pages measured 0.93–2.09 MB, Baidu Q3 (`prnewswire.com`) 10,584 B under the limit (§11.1).
     - the cross-check is in the collect envelope only: the ledger does not read it, so a run whose stdout is lost loses it; it is recomputable from the stored blobs. Its title match has the false readings listed in §11.2.
     - one direct index that does not answer, or answers with something that does not parse, makes every cross-check row `not_looked`, of every key: the rules map no key to its index (a map decided 2026-11-17, not built, §11.2).
     - `DIRECT_LEAD` (2 h) was set after seeing the 72 min it covers; a Yahoo minute later than 2 h after the direct listing names a listed row unlisted (90 min decided 2026-11-17, not built, §11.2).
     - Yahoo depends on every other admitted robots row and they on its (§8 row R4), as for the other hosts.
     - no run against the live host yet; the pin is the 2026-11-16 08:04:29Z body, not re-fetched.
7. VPS timer, follow-up cap with the run deadline, busy seconds in the envelope.
   - Hard gates before the first live run, each not built (`git grep` 2026-11-17):
     - Owner condition A (2026-11-16, "zgadzam sie na warunek A"): step 7 MUST build the run-window cap before the first live run. A host whose `Retry-After`, or doubled wait, ends past the run window is stopped, and its remaining planned requests are recorded as not looked.
     - A per-document robots check, owner decision 2026-11-17 (~16:10Z, "zgoda"): each document url's path is checked against its host's pinned robots before it is asked, read as robots decision 4 (2026-11-17) reads a path (§11.1). Trigger: Yahoo's `path_prefix` is `/`, so collect checks `/` while Yahoo's `*` group has 32 `Disallow` lines below it (step 6's Yahoo holes).
   - Today a long `Retry-After` is waited out in full and persists in `pace/<host>.per-host` (§8 row R19 holes 11 and 12); the hold has no ceiling (`LAST_STAMPED_YEAR` 9999); the 5xx doubling lives in `Wire::backoff` memory and restarts at 10 s each run (step-7 critique C3, C4, read at main 21989d7, 2026-11-17).
   - Owner decisions S7-D2 to S7-D11, 2026-11-17 (~16:10Z, "zgoda" to the step-7 critique's picks; S7-D5 and S7-D7 at 24 h, the example offered). S7-D1 stands in §11.2. Not built.

     | decision | rule | rejected, and the symptom |
     |---|---|---|
     | S7-D2, what the deadline measures | the run's wall time | busy time summed over hosts: cuts runs about 4× earlier; a deadline per host: a run can still hold the lease when the next slot starts, giving exit 7 and an unfinished run file |
     | S7-D3, the deadline | `min(slot_start + period/2, next_slot) − VENDOR_PATIENCE` (open-web's, 30 s, `providers/open-web-feed/src/lib.rs`), the slot read from the schedule as data (§11.2's table) | process start + period/2: 50.03% of a day in the worst case (31 runs with one last request running its 30 s), and a catch-up start more than period/2 late (more than 15 min in a 30-min slot) runs into the next slot |
     | S7-D4, "not looked" | a `deferred` table (run, host, url, phase, story_id, listed_date, reason in `run_deadline`, `host_held`, `follow_up_cap`) plus a ledger count | an attempt row with no call: breaks §8 row R16, one attempt row per HTTP call; the envelope only: a JSON file cannot feed S7-D5, and "nothing happened" and "we did not look" must differ in the store |
     | S7-D5, deferred items | carried: the next run puts each unresolved deferred item the store does not hold at the front of its host's lane; one older than 24 h is closed `expired` | re-listing only: PRN's and GNW's next poll shared no item in 18 of 25 pairs (§11.2) |
     | S7-D6, the follow-up cap | 150, global, counted at issue in round-robin order; the GNW walk has its own cap, set by the owner from a construction, not set yet | ⌊150/5⌋ = 30 per host: on a first run's 301 follow-ups (20 + 20 + 100 + 20 + 141 Yahoo rows at p1500) CNW loses 70 and Yahoo 111, against 55 and 96 under the global cap (arithmetic 2026-11-17; the critique's four-host case, before Yahoo: 37 per host, CNW loses 63 against 10) |
     | S7-D7, a server's hold | persisted in the pace file, capped at 24 h, with an alert above it; the pace file then holds a grant no call used, the opposite of §10's rejected "grant earlier than the call" | memory only: asks a held host again every run; uncapped: `Retry-After: 315360000` silences the host for 10 years |
     | S7-D8, the exit | every reason exits 8 `partial_collect` (§5.1); the collect unit sets `SuccessExitStatus=8`, so a failed unit means 2, 4 or 7; an hourly `ledger --check` watchdog alerts on `host_held` and `expired` counts | exit 0 for a cut: hides a lost tail; exit 8 as a unit failure: the unit would read failed on most peak runs |
     | S7-D9, overlap | a run refused `lease_held` rewrites its run file with `finished_at` and `refused: lease_held`, and the ledger leaves it out of observed runs; no `Restart=`; `TimeoutStartSec` = the longest deadline + 2 min | skipping the run file: breaks `cmd_collect.rs`'s rule that it is written before the lease, so a run that never gets one is a record |
     | S7-D10, deployment | build on the VPS from a tagged clean clone and run the tests there; `/etc/bellwether/env` mode 640; an NTP check before each run; a manual first `robots refresh` before go-live; no `RandomizedDelaySec`; the VPS store is the first copy (N1, §10) | — |
     | S7-D11, busy seconds | an additive `busy` object in `newsvault.collect/2`, per host: waited, requesting, requests, stopped, not_looked, held_until, doublings, plus `wall_s`; `wall_s` and per-host `busy_s` in `runs/<id>.json`, summed by the ledger against 50% | stdout only: nothing on a real path reads it |

     - Deferred by the owner until the first live data: letting a lane whose slot is not due be skipped (§10's earliest-due-first row).
     - Unknown, VPS: rustup, `libsqlite3-dev`, free disk, NTP sync, linger, CPU headroom beside AlphaLens; no read-only check has run.
   - PR order (step-7 critique §D, 2026-11-17), each passing the gate alone:
     1. docs: these decisions.
     2. feat(collect): the `deferred` table, its migration and ledger count, with the cap of 150 as its first producer (`follow_up_cap`).
     3. feat(news-provider): condition A's core: a bounded reservation `PastDeadline { go_at }` that writes nothing, taken before `AttemptSlot::open` (which today opens before the pace wait); the held host's lane drained into `deferred` (`run_deadline`, `host_held`); S7-D7's ceiling; gate tests that go red when the rule is deleted.
     4. feat(newsvault): the schedule as data, S7-D3's deadline, S7-D11's busy fields, S7-D9's `lease_held` finish.
     5. feat(collect): S7-D5's carried queue, the reader of `deferred`, with expiry.
     6. chore(deploy): systemd user units under `deploy/`: oneshot, `OnCalendar … UTC`, `AccuracySec=1s`, `SuccessExitStatus=8`, the watchdog, a separate scheduler lock for `robots refresh`; installed by hand after the read-only VPS checks.
     - PRs 2 and 3 and the per-document robots check are the hard gates; the robots check's place in the order is not set.
     - PR 5 may follow the first live run only while neither the PRN sitemap nor the GNW walk is built: with today's five indexes, the cap and the deadline bind only on the first run on an empty store (CNW's 100 follow-ups, 1,000 s; Yahoo's 141 at p1500, 1,410 s; the cap binds first); with both built, of the 5 peak polls of 2026-11-16 the deadline binds at 2 and the cap at 4 with Yahoo's keys counted (§11.2's completeness cost).
8. Outbox, WAL, `bundled`, and the §6 poller as `newsvault events --after <cursor>`.

### 11.6 Evidence, 2026-11-14 (Saturday), 2026-11-16 (Monday) and 2026-11-17 (Tuesday)

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
- PRN RSS, `date` 19:21:04 GMT: 20 items over 5.02 h (14:00-19:01).
- PRN's committed rule is `<p>` in `section.release-body`, not the row's `<article>` (§11.1). The same two probe bodies, each method run 2026-11-16:

| method | 302939553 paragraphs | 302939553 chars | 302938569 paragraphs | 302938569 chars |
|---|---|---|---|---|
| the probe method above, `<p>` in `<article>` (the row's rule, reproduced) | not recorded | 9,463 | not recorded | 19,337 |
| the probe method above, `<p>` in `section.release-body` | 40 | 9,421 | 62 | 19,295 |
| `OpenWebFeed::extract` under the committed rules file, the `\n` between paragraphs counted, with the `br` space (§11.1) | 39 | 9,409 | 62 | 19,307 |

  - 302939553's 40th probe paragraph is `_______________________`, which holds no letter or digit, so the extractor drops it (§11.1). Before the `br` space, the extractor gave 9,397 and 19,295 chars.

**Weekday probe (probe6), 2026-11-16 (Monday), 08:04–23:30Z.**

- Construction: a one-off Python stdlib script, not kept (CONTRIBUTING rule 3); pace `max(Crawl-delay, 10 s)` per host; robots.txt read through `newsvault::robots` before any other request to a host; one GET per index per §11.2 slot, the period table's schedule; two extra Yahoo sitemap GETs at 19:20Z and 22:36Z, the clock times of the 2026-11-14 pair; all times UTC.
- Run record: 26 of 26 poll slots 08:00–23:30Z ran (08:00 started 268.851 s late); the 5 slots 00:30–06:30Z were missed because the process was not running; no sleep gap above 5 s.
- Index window (§11.2's period table): item key = https + lowercased host + path; publication value = RSS `pubDate` or `news:publication_date`; window = newest − oldest; the first poll per index is the baseline and yields no first sighting.
- Distinct items dated 2026-11-16 seen over the day: GNW 466, PRN 479, CNW 157 (sitemap and RSS alike), Fed 0, Yahoo 5,591. For PRN and GNW this is a lower bound: first sightings per hour, by the hour of the poll that first listed the item (2 polls an hour, 20 items each, so 40 is the most a hour can show):

  | hour | 11 | 12 | 13 | 14 | 15 | 16 | 17 | 18 |
  |---|---|---|---|---|---|---|---|---|
  | GNW | 40 | 35 | 40 | 40 | 40 | 40 | 40 | 36 |
  | PRN | 33 | 36 | 40 | 40 | 40 | 39 | 40 | 26 |

- Business Wire `https://feeds.businesswire.com/BW/Earnings_News-rss`, one GET per poll slot after its robots.txt answered 200: 26 of 26 answered 200; 25 of 26 well-formed RSS (complete body, parses, root `rss`, a channel, at least 1 item); the one other, the 08:04Z poll, was a 1,048 B RSS with no item; longest run of failures 1; body 1,048 / 17,849 / 46,169 B (min / median / max); items 1 at 09:30Z, 38 at 23:30Z. No stability threshold was fixed beforehand, so only counts are reported.
- Article bodies: the 5 newest items of each host's first index fetch, one GET each. Plausible container, fixed before the data: one element holding ≥ 70% of the page's paragraph text (`<p>` text with a letter or a digit, outside `script`/`style`/`noscript`/`template`), ≥ 3 paragraphs and ≥ 500 chars. No selector is decided here.

  | host | plausible | deepest element holding ≥ 70% of the page's paragraph text | its share of the page's paragraph text |
  |---|---|---|---|
  | CNW (sitemap items) | 5 of 5 | `div.release-content` | 0.8474–0.9452 |
  | Fed | 0 of 5 | none passes; `div#article` holds 0.2128–0.5633; a navigation list `ul.nav.navbar-nav` holds 40 paragraphs, 978 chars, on every page | — |
  | Business Insider | 5 of 5 | `section.post-body-content.post-story-body-content`; JSON-LD `articleBody` present on 5 of 5 | 0.9975–0.9986 |

- Busy time, a projection and not a measurement: (index requests + new items of GNW, PRN, CNW sitemap and Fed + new Yahoo rows under the `business-wire.com` or `accesswire.ca` key) × 10 s, serially, against the 2026-11-16 rule (stop deadline = half the period, serial order) and the cap of 150 (§11.2); under round-robin and S7-D3 it is replaced by §11.2's completeness cost bullets. Over the 900 s deadline in 3 of 26 polls, below; the most follow-ups in any poll is 105, below the cap:

  | poll | follow-ups | projected busy s |
  |---|---|---|
  | 13:30 | 105 | 1,100 |
  | 14:30 | 99 | 1,040 |
  | 15:00 | 103 | 1,080 |

  - A lower bound: GNW and PRN add at most 20 follow-ups each per poll, their index size, and both reached 20 in all 3 rows above; no retry is counted, where §11.2's worst case multiplies by 1 + `MAX_TRANSIENT_RETRIES`.

**Yahoo, 2026-11-16 (Monday).**

- The 3-poll rows below were taken by 11:24Z, before 08:00 ET (13:00Z) and before the NYSE session (14:30–21:00Z): news-sitemap polls at 08:04:40, 09:30:00 and 11:00:02Z, article fetches at 10:52–11:24Z. Rows marked weekday probe span 08:04–23:30Z (above).
- The 2026-11-14 rows are a Saturday.
- Lag during weekday US hours (13:00–21:00Z, the research's window) is unmeasured.
- Sitemap window: each poll holds 1,000 rows; the span is newest minus oldest `news:publication_date`. The weekday probe's 26 polls: §11.2's period table.
- The 2026-11-14 22:36Z row is a Saturday probe body, recomputed 2026-11-16; the 19:20Z one (18.27 h) is above.

  | poll (UTC) | oldest – newest `publication_date` | span |
  |---|---|---|
  | 2026-11-14 22:36Z | not recorded | 20.23 h |
  | 2026-11-16 08:04:40 | 11-15 11:38 – 11-16 08:00 | 20.37 h |
  | 2026-11-16 09:30:00 | 11-15 15:12 – 11-16 09:24 | 18.21 h |
  | 2026-11-16 11:00:02 | 11-15 20:46 – 11-16 10:58 | 14.20 h |

- Turnover between consecutive polls, rows keyed by URL:

  | pair | elapsed | rows in common | rows replaced | replaced per hour |
  |---|---|---|---|---|
  | 2026-11-14 19:20Z → 22:36Z | 3.26 h | 882 | 118 | about 36 |
  | 2026-11-16 08:04Z → 09:30Z | 1.42 h | 824 | 176 | 123.7 |
  | 2026-11-16 09:30Z → 11:00Z | 1.50 h | 720 | 280 | 186.6 |
  | 2026-11-16 14:00Z → 14:30Z, weekday probe, the day's highest rate | 0.50 h | 576 | 424 | 848.44 |
  | 2026-11-16 19:20Z → 22:36Z, weekday probe, the Saturday row's clock times | 3.27 h | 153 | 847 | 259.29 |

  - Of the rows replaced, the oldest left first: 840 of 847 on Monday 19:20→22:36Z, 117 of 118 on Saturday (removed rows among the earlier fetch's oldest by `publication_date`). 58 of Monday's 847 added rows carry a `publication_date` before the earlier fetch's newest row (Saturday: 19 of 118). The same code reproduced the Saturday counts from the saved 2026-11-14 bodies.

  - 3 rows of the second 2026-11-16 pair left although newer than the next poll's oldest row: one URL retitled, one moved to another path under the same numeric suffix, one removed. So a Yahoo URL is not a stable identity.
  - 32 and 38 URLs first appeared in the later poll of each 2026-11-16 pair although their `publication_date` predates the earlier poll; the delay after that date reached 48.7 and 689.8 min (lower bounds).
- Coverage of the direct wires, union of the 3 polls (1,456 URLs); a wire item counts when its `pubDate` lies between 2026-11-15T11:38Z (the first poll's oldest row) and 09:00Z (the last poll minus 2 h); matched by equal normalised title (HTML unescaped, lowercase, non-alphanumeric runs collapsed); English by `dc:language` for GNW and, for PRN and CNW, which carry no language field, by a title whose every code point is below U+2000 and that holds an English stopword (`cov2.py`):

  | wire | eligible | found on Yahoo | English | English found on Yahoo | non-English found on Yahoo |
  |---|---|---|---|---|---|
  | PRN | 36 | 17 | 18 | 12 | 5 of 18, each an English title the heuristic rejects (`€`, or no stopword) |
  | GNW | 35 | 23 | 24 | 23 | 0 of 11 |
  | CNW | 30 | 30 | 26 | 26 | 4 of 4, each an English title the heuristic rejects |

  - Found on Yahoo: `coverage` in `out.json`; non-English found = found minus English found; titles read 2026-11-16 by re-running `cov.py`'s parse over the same probe6 bodies.
  - So no non-English PRN or CNW title was found on Yahoo: every rejected title found is English, and PRN's Spanish and German titles the heuristic calls English are misses.
  - 2 of PRN's 6 English misses are the heuristic's errors (a Spanish and a German title), so PRN is 12 of 16.
  - PRN and GNW RSS hold 20 items each, so their eligible items run from 07:30Z on 2026-11-16 (oldest 07:31Z and 07:30Z), about 1.5 h; CNW's sitemap reaches back to 2026-11-13.
  - Fed: 0 of 20 Fed items in each of the 3 polls, matched by the first 50 normalised title characters against all 3 sitemaps.
  - 12 GNW law-firm notices of 10:46–10:47Z were absent from the 11:00Z poll, and no Yahoo title of the union starts with `investor notice` or holds `securities fraud` or `class action`; exclusion and lag cannot be told apart from one batch.
- Latency: Yahoo's `publication_date` copies the wire's minute (median difference 0 min over 17 PRN, 23 GNW and 30 CNW matches), so it is not an observation time. By first listing poll, 6 items (3 PRN, 3 GNW, all wire time 08:00Z) reached Yahoo one poll after the wire's own index: more than about 4 and at most 86 min later.
- Duplicates, same 1,456-URL union, grouped by equal title:

  | group | count |
  |---|---|
  | same-title groups, all keys | 41 (68 extra rows) |
  | of them holding a wire key | 29 (44 extra rows) |
  | one release under `cnwgroup` + `prnewswire` + `prnewswire` | 12 |

  - Title grouping also merges distinct releases: GNW's `Transactions in connection with share buyback programme` is two issuers at 08:46 and 10:46.
  - Ericsson's buyback is 3 Yahoo URLs; the 2 fetched point to 2 PRN release ids (302940182, 302940173).
- Reprint fidelity, 9 pairs (4 PRN, 3 GNW, 2 CNW) of releases published 2026-11-15/16, Yahoo `div[data-testid=article-body]` without scripts and ad slots against the original's container (`section.release-body` for PRN and CNW, `#main-body-container` for GNW); word share = original words inside `difflib.SequenceMatcher` matching blocks over NFKC lowercase words, divided by all original words. 2 of 8 planned pairs, both GNW, had no findable original and were replaced.

  | measure | result |
  |---|---|
  | share of the original's words on Yahoo | 97.14–100% |
  | table rows and cells | equal in 7 of 7 pairs with a table |
  | `ul` and `li` counts | equal in 9 of 9 |
  | title (h1, JSON-LD `headline`) | equal in 9 of 9 |
  | publication minute (sitemap, JSON-LD `datePublished`, URL suffix) | equal to the original's in 9 of 9; GNW's second is cut |
  | dropped | the `SOURCE` trailer, PRN and CNW 6 of 6; CNW's contact block 2 of 2 |
  | added | `Story Continues` button text, 9 of 12 Yahoo pages, with the rest of the body in the same HTML; a `Cision` / `View original content` trailer, PRN 4 of 4, CNW 2 of 2 |
  | link back to the release | PRN 4 of 4 (release id); CNW a legacy archive URL only, whose 301 costs one more request; GNW none, 3 of 3 (only `globenewswire.com/Tracker?data=…` links), so a GNW reprint carries no release id |

- Article pages, probe7, 5 Yahoo URLs fetched 10:52:47–10:53:30Z: status 200 and no redirect 5 of 5; 948,262–1,002,016 B decoded; 44.5–47.1% of the bytes inside `<script>` elements (regex over `<script…</script>`, measured 2026-11-16); no JSON-LD `articleBody`; the article text is in the server HTML, after `Story Continues` too (4 of the 5 carry that text, with 993–3,745 chars of `<p>` text after its first occurrence: Python `html.parser`, each paragraph whitespace-collapsed, summed, 2026-11-16; a regex over `<p>…</p>` gives 1,008–3,745). Compressed size: §3.9.

**PRN and GNW indexes at a 5-min period (probe10), 2026-11-17 (Tuesday), 12:00–16:00Z.**

- Construction: a one-off Python stdlib script, not kept (CONTRIBUTING rule 3); approved by the owner 2026-11-17 (`PG-D7`) for at most 288 requests; 48 cycles, one every 5 min from 12:00Z; pace `max(Crawl-delay, 10 s)` per host; robots read from probe6's saved bodies before each request; no redirect followed; `www.globenewswire.com` never asked; counts by the probe's own analysis script over its fetch log, 2026-11-17, unless a row says otherwise.
  - Each cycle: PRN news sitemap page 1, and page 2 only when page 1 shared no key with the previous cycle's union; PRN RSS; GNW RSS; GNW ids: the 2 lowest ids above an earlier frontier (the highest id an RSS poll showed) that no RSS poll showed, each asked at least 10 min after the frontier passed it, at `https://rss.globenewswire.com/news-release/<y>/<m>/<d>/<id>/0/en/x.html`.
- Run record: 158 requests, all 200, 0 lost, 0 robots skips; 48 of 48 cycles ran, starting at most 0.017 s late. PRN 97 (49 sitemap, 48 RSS), GNW 61 (48 RSS, 13 ids).

| PRN news sitemap | value |
|---|---|
| page 1 shared a key with the previous cycle's union | 46 of 47 cycles (the first has no basis) |
| page 2 fetched | 1 cycle, 14:10Z; it shared keys with the previous union and none with that cycle's page 1 |
| generation lag, page 1: fetch time − `x-sitemap-generated-at`, n 48 | min 2.4 / median 10.0 / max 17.1 min |
| page-1 generation stamp moving backwards between consecutive fetches | 0 times |

- The 14:10Z cycle, read from the raw bodies (Python ElementTree, 2026-11-17):
  - page 1 (generation 14:01:42Z) held 100 rows, all dated 14:00–14:01Z;
  - page 2 came from an older generation (13:55:47Z, Cloudflare's per-url cache of 300 s) and held 13:00–13:19Z;
  - the previous page 1 ended at 13:50Z, so no sitemap page read covered 13:51–13:59Z;
  - under §11.2's rule that poll is not complete: its page boundary shares no key.

| PRN keys, RSS against every sitemap page | keys |
|---|---|
| RSS, union of 48 polls | 438 |
| sitemap, union of 49 pages | 744 |
| in both | 417 |
| sitemap only, published inside the RSS span (11:30–15:51Z) | 243 |
| RSS only | 21 |
| RSS only, published inside the sitemap span (10:00–15:48Z) | 17, all 13:51–13:59Z, the stretch no page covered |
| RSS only, after the sitemap span | 4, 15:49–15:51Z, the run's last minutes |

- No RSS-only key fell in a stretch a sitemap page covered.
- The 7 RSS keys of 2026-11-16 absent from every sitemap page (§11.2's completeness table) had no counterpart in these 4 h.

| GNW | value |
|---|---|
| frontier | 3404422 at 12:00Z, 3404988 at 15:55Z: +566 ids in 3 h 55 min (47 cycles) |
| frontier movement per cycle, n 47 | min 0 / median 8 / max 70 (14:05Z) |
| ids an RSS poll showed below an earlier frontier | 0, every cycle |
| ids between consecutive frontiers, bands closed in cycles 1–45 | 506; 140 of them shown by no RSS poll of the run (the sum of the analysis's band column, 2026-11-17) |
| ids probed | 13: 200 on 13; 301, 410 and 404 on 0; 0 later shown by an RSS poll |

- Limits:
  - one weekday, 4 h of the US morning, a 5-min period; at S7-D1's 30 min neither the page count per PRN poll nor the walk's size per poll is measured;
  - 13 of 13 ids at 200 bounds the share of ids not public 10 min after the frontier passed them at 20.6%, the exact one-sided 95% bound 1 − 0.05^(1/13) (3/13, 23%, by the rule of three); the sample is not random: the 2 lowest unshown ids of the newest band at least 10 min old, older bands skipped (the probe's id planner);
  - no 429 or 5xx was seen, so no slow-down was tested.

