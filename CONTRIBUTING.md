# Contributing

Four rules about size. They exist because the predecessor system was measured on 2026-11-12 and the
shape of its growth is specific enough to name.

## What was measured

| date | source lines (Python) | test lines | ratio |
|---|---|---|---|
| 2026-05-01 | 30,140 | 26,121 | 0.87× |
| 2026-07-01 | 91,045 | 99,396 | 1.09× |
| 2026-09-01 | 148,763 | 196,779 | 1.32× |
| 2026-11-12 | 197,364 | 285,589 | **1.45×** |

Seven months, 5,839 Python lines to 482,953 — eighty-three fold, about 65,000 lines a month,
linear, never decelerating. The whole repository is 3,458 files and 898,230 lines.

**It was not dead code.** Packages marked closed or archived account for 3,058 lines, 0.6% of the
Python. That part was handled well and is worth copying.

**It was the auxiliary material outgrowing the thing it serves.** Tests passed source in June and
never looked back. In the two busiest areas the ratio is worse than the average: 20,289 source lines
of news handling are touched by 77,540 lines of test, and 34,742 lines of broker code by 107,838.
Add 136 one-off scripts at 50,711 lines and 1,144 markdown files at 128,968, and the auxiliary
material is 336,000 lines against 147,000 of actual source — **2.3 times the thing it serves**.

The cost is not disk. At 736 test files, "which test covers this?" stops having an answer, and the
predecessor's own notes record the symptoms: a repair round closes the named mutation and grows a
defect on the branch beside it; a count of failures hides which failure. That is a problem of
test *number*, not of test quality. It also decided a design question in this project: 77,540 lines
of test were the main argument against rewriting the news handling, not because they are good but
because nobody can say what is in them.

## The four rules

### 1. Tests do not grow faster than code

CI prints the ratio of test lines to source lines on every run. It is not a gate and it fails
nothing. It exists so that the number is in front of whoever is adding a file.

When it passes 1.0×, that is a question in the pull request — does this test cover a case nothing
covers, or the same case again? The predecessor crossed 1.0× in June and nobody noticed, because
nobody was counting.

### 2. The tree is for what lives; history is for what was

Git already keeps a deleted file. Keeping it in the working tree as well stores the same thing
twice, and only the second copy costs anything: it appears in every grep, every search result and
every future reader's idea of what this project is.

So a closed experiment's code is deleted and its **finding** goes into a document. The predecessor
did this and it is the reason its dead-code share is 0.6%.

### 3. A measurement script is kept only if it will be run again

Four live under `probes/`. They are there because each will run again — on a rule change, on a panel
rebuild, at every stage of the migration. Roughly as many were deliberately not kept, and their
numbers live in `docs/design/` with the construction that produced them, which is what a reader
needs.

A script that answered its question once is a receipt, not an asset. The number plus its
construction is the artefact; the code is only worth the space when it has a second run ahead of it.

### 4. One design document, amended

`docs/design/` holds two documents and they are edited in place. The predecessor holds 354 research
memos, of which 125 carry a status line and the rest are one-off reports.

The natural path is a new file per decision, which is why this needs to be a rule rather than a
preference. A decision goes into the document it changes, with its date, and the superseded text
stays visible as superseded rather than being deleted — a design document is also a record of what
was believed when.

## Everything else

- The gate is `.github/workflows/ci.yml`: `cargo fmt --check`, `cargo clippy --locked --all-targets
  -- -D warnings`, `cargo test --locked --no-fail-fast`. The commands are the ones `README.md`
  names; change one and change the other, or the document starts lying.
- `--no-fail-fast` is load-bearing: measured on this tree, one failing unit test made plain
  `cargo test` exit without ever running the doc test, which is the only enforcement point for the
  claim that the two time types cannot be compared.
- `unwrap`, `expect` and `panic!` are denied, `unsafe` is forbidden, and a discarded `Result` is an
  error under `unused_must_use` and `clippy::let_underscore_must_use`. The table is
  `[workspace.lints]` in the root `Cargo.toml`, so a new crate is covered by one stanza in its own
  manifest: `[lints] workspace = true`. Two measured limits, both named in §8: that stanza is
  opt-in and nothing checks for it (row G1), and the five `deny` lints are switched off from inside
  a crate by one `#![allow]`, by wrapping the call in a local `macro_rules!`, or by writing it in a
  doc test (row G2). Only `unsafe_code` is `forbid`, and only `forbid` refuses a local `allow`.
- Every number in a document carries the construction and the date that produced it. A number
  without its construction is an anecdote, and the predecessor has a measured case of four studies
  sharing one upstream count and being wrong together.
- An agent session working here should read this file and `docs/design/architecture.md` §8 before
  changing anything, because §8 is the list of what is actually enforced and what is only claimed.
