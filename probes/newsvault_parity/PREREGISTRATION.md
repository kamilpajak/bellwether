# Pre-registration: the archive-reading parity comparator

**Status:** REGISTERED 2026-11-13, before the comparator was run against the Rust reader for the
first time. Nothing above the dated section at the end may be edited after a run; a later
disagreement with it goes in a new section headed with its date.

**Why this document exists.** The project pre-registers before it measures, and a parity run is a
measurement: it can return anything. Writing down afterwards what a disagreement "really meant" is
how a reader ends up adjusted until it agrees, which would make the gate a tautology.

## What is being compared, and what the old number does not say

`probes/parity_baseline.py` and `services/newsvault/src/lake.rs` are two implementations of one
stated set of archive-reading rules. Architecture §10 records that they agreed on **156 of 156**
days, re-derived 2026-11-12. Three things are true of that number and none of them is "two readers
encode one rule":

1. It is a digest over the sorted distinct **vendor ids** — §3.1's `source_item_id`. It says the
   id SET came back. It would match with every title, every body and every url different.
2. The corpus holds **none of the shapes the two readers can disagree on**: across 779 run files
   and 243,226 rows there are zero null ids, zero empty-string ids, zero null publication or
   ingest times, zero inverted rows and no file missing a column. So `unusable_rows` and §5's
   rows 4 and 5 have never fired on real data.
3. There was **no comparator**: the probe wrote a baseline nothing read, `newsvault` emitted no
   digest field, and the two sides were compared by eye once.

This comparator answers all three. It reads the committed corpus under `lake/`, which holds one
partition per shape the archive lacks; it compares every reported field, not only the id digest;
and it is a command with an exit status rather than an afternoon's eyeballing.

## The rules both readers implement

Stated here once, and in the probe's own docstring beside the code. Where the two documents and
the code disagree, the code wins and this section is edited in the same commit.

1. Read every `run=*.parquet` under `session_date=<d>/`, in filename order.
2. Drop and COUNT a row whose id is null, empty or blank, or whose publication or ingest time is
   null.
3. Drop and COUNT a row whose ingest time precedes its publication value, at any precision.
4. A publication value stamped exactly at UTC midnight is a DAY and not an instant. The lake has
   no precision column, and 7,767 of its 243,226 rows (3.19%) carry that stamp — for source
   `perplexity` it is 7,141 of 7,141 (measured 2026-11-13).
5. Collapse repeats across runs keeping the FIRST sighting, the earliest ingest time.
6. Count a publication-value disagreement whenever a row re-reports an id, on both paths through
   the dedup, and count it in both units: ROW events and the ITEMS they fall on.
7. Refuse a day whole when a run file lacks one of the seven columns. Never read it with a hole.

## The two digests, and why one of them is weak on purpose

| digest | over | what a match proves |
|---|---|---|
| `id_digest` | newline-joined sorted distinct ids | the id set agrees. Kept because it is the number §10 quotes, so the old and the new runs are comparable. |
| `content_digest` | every field of every item in id order, each value length-prefixed as `<bytes>:<value>` | the whole item set agrees: source, publication value WITH its precision, observation instant, title, body, url and tag list. |

Length-prefixing rather than a separator byte, because a value holding the separator would forge a
field boundary — the defect `canon` was reviewed for on 2026-11-13, which it closed by refusing
three bytes. A digest cannot refuse an archived value, so it prefixes instead.

Neither digest covers the ORDER of the items: both sort by id. The lake reader's own order is
`(publication lower bound, id)` and nothing here checks it.

## What each possible answer means, decided now

| outcome | what it means, and what happens |
|---|---|
| every field of every day agrees | The two readers encode one rule **over the shapes the corpus holds**. The claim stays bounded by that list; it is not "the readers agree". |
| a **count** differs (`raw_rows`, `unusable_rows`, either conflict counter) | One reader admits or refuses a row the other does not. The RULES above are ambiguous at that shape, not one side wrong: the ambiguity is resolved in this document first, then in both readers, in one commit. |
| only `id_digest` differs | A row became an item in one reader and not the other, without changing any count. That is a dedup-key defect; the kept-sighting rule is where to look. |
| only `content_digest` differs | The id set agrees and a FIELD does not — a rendering difference (precision, zone, truncation) or a kept-sighting difference that preserved the id set. This is the case the 156-of-156 number could not see at all. |
| a day one reader refuses and the other reads | Fail-closed against fail-open, which §5 row 1 settles in favour of refusing. The reader that read it is the one that changes. |
| the probe cannot run (`pandas`, `pyarrow`, no binary) | Not a result. It exits 4 with a named input, and the number is not quoted until it runs. |

**The comparator is not the gate CI runs.** CI runs no Python (`.github/workflows/ci.yml` has no
interpreter step). `expected.json` is this probe's committed output and
`services/newsvault/tests/parity.rs` is the only reader of it, so `cargo test` compares the Rust
reader against a second implementation's recorded answer. Re-run the probe, and rewrite
`expected.json` with `--write-expected`, whenever either reader's rules change — that rewrite is
the change under review, and a commit that moves a digest without moving a rule is the thing to
stop at.

## What this still does not check

- The corpus is **synthetic**. It holds the shapes the lake lacks and it is not a sample of
  anything; a shape in neither the lake nor `SHAPES` is unchecked.
- The two readers share one INPUT: the same parquet files, read through arrow on one side and
  pyarrow on the other. A defect in how a value is encoded into those files is invisible to both.
- Nothing compares `newsvault`'s new archive (`items`, §3.1) against a second reader. This gate is
  over the lake reader only, which is the reader the migration depends on.
- The probe compares the day READING. It does not run `--at`, so no visibility predicate is
  compared; `pit`'s own tests carry that.

## Run log

Entries are appended, never edited.

**2026-11-13, first run.** Corpus of 9 partitions, 11 run files. One disagreement, on the shape
the lake holds zero of: a row whose ingest time precedes a publication value stamped at midnight.
The probe dropped it by rule 3; the Rust reader kept it as a day-dated item, because its ordering
test sat inside `Visibility::from_live` and the day branch never reached that constructor. Rule 3
is the rule both readers were written from, so the reader changed: the test is now in
`lake.rs::datedness` ahead of both branches, pinned by
`a_row_fetched_before_its_own_day_stamp_is_unusable_and_not_day_dated`. After that fix, 9 of 9
partitions agree on all eight fields, `content_digest` included.
