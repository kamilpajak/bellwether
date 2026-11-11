# bellwether

A bellwether is the animal whose bell the flock follows. In a filing, a small company names the
customers and suppliers it depends on. When one of those named counterparties is in the news, the
companies behind it are worth a look before anyone has looked.

This repository builds the pipeline that finds them — and it is, first of all, a pipeline that
refuses to lie to itself about time.

## What it produces

A **shortlist of companies worth reviewing**, with the evidence that put each one there: which
counterparty was in the news, which filing disclosed the relationship, and the exact character span
in that filing where the disclosure sits.

It produces proposals for a person to read and argue with. It does not produce orders, position
sizes, or anything an execution system would accept. That boundary is structural rather than a
policy: no service here holds credentials for an order queue or knows a path to one.

**Nothing here is validated.** No strategy in this repository has an established edge, and none
earns capital. It is a research instrument and it is not investment advice.

## Why it exists, in one measurement

The predecessor system computed the statistical power of its own study as **88.2%** using the
textbook formula for independent observations. Measured through the randomization procedure the
study would actually run, the answer was **27.4%**, and after the input archive was corrected,
**10.9%**. The exchangeable unit was the trading session — 47 of them — not the row, of which there
were thousands.

The formula was not wrong. It was answering a different question than the one asked, and nothing in
the system could tell the difference. Every design rule here exists because something like that
happened and was measured.

## The discipline

Each item carries two instants that are **separate types and cannot be compared with each other**:
when the world published it, and when we could first have known it. A single query decides
visibility and it requires both, because the half-question — "was it published before T?" — is the
one whose answer leaked in 60.2% of a 69-probe retrieval test while the full question leaked in none.
The two instants cannot be compared with each other, so that leak cannot happen by confusing them;
the accessors are public, so a caller who sets out to ask the half-question still can.

From the same source: the archive is complete and append-only, and any cap is a derived view;
a missing input aborts rather than becoming an empty result; a failed network call must be handled
for the program to compile; counting is counting and never multiplication; and units are types, so
basis points and standard deviations cannot be added together.

Two implementations of the archive-reading rules, one here and one written independently in pandas,
were run over 156 days of the predecessor's news lake and agreed on every field on **156 of 156
days**, including a digest of the sorted item-id set. That agreement is the gate this project has to
keep passing.

## Layout

| path | what it holds |
|---|---|
| `docs/design/` | the founding design memo: requirements, architecture, and what is still open |
| `docs/inherited/` | two documents carried from the predecessor: the mandatory research gates, and the postmortem of eleven closed strategy paradigms |
| `services/newsvault/` | the point-in-time core and the archive reader, in Rust |

`docs/design/architecture.md` is the contract layer: what crosses each service boundary, in what
shape, and — in its §8 — the single place each invariant is enforced, including the three that are
not enforced anywhere yet.

`docs/design/successor_system_design_2026_11_11.md` is the document to read before changing anything
here. It carries eighteen numbered requirements, each paired with the measurement that forced it,
and it names the decisions that are still open rather than writing around them.

`docs/inherited/paradigm_failures_postmortem.md` is the more useful of the two inherited documents:
eleven strategy paradigms that were tried and closed, with what each cost and what of it was worth
keeping. It is here so this project is not free to repeat them.

## Running the archive reader

```
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --no-fail-fast          # 14 unit tests plus a compile_fail doc test
cargo run -p newsvault -- day 2026-07-01 --lake <path-to-lake> --format json
cargo run -p newsvault -- schema           # machine-readable description of the command tree
```

`unwrap`, `expect` and `panic!` are denied by lint in the `newsvault` crate, and `unsafe` is
forbidden.

## Status

Early. One of the five services named in the design exists, and the design itself is a draft with
five decisions still open — including which news vendor the archive is built on, and the date by
which the successor must reproduce the predecessor's outputs or be abandoned.
