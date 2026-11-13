# CLAUDE.md

Rust research instrument, uv-free. The design names five services; `vault`'s archive reader and its
collector exist and the other four do not. Nothing here is validated and nothing earns capital.

Read before changing anything: @CONTRIBUTING.md — four size rules — and
@docs/design/architecture.md, whose §8 is the only count of what is actually enforced and what is
only claimed.

## Commands

- `cargo` is NOT on `PATH`. Every shell: `export PATH="$HOME/.cargo/bin:$PATH"`
- The gate, which is the definition of done: `cargo fmt --check`, then
  `cargo clippy --locked --all-targets -- -D warnings`, then `cargo test --locked --no-fail-fast`.
- Capture a gate command's exit status directly: `cargo clippy … -- -D warnings; echo "x=$?"`.
- `--no-fail-fast` is load-bearing: one failing unit test otherwise exits before the doc tests, and
  those are the only enforcement point for the two time types (§8 row R2).
- After adding a dependency, run one plain `cargo check` so the lock updates, then the gate.
- Read `cargo run -p newsvault -- schema` before quoting a subcommand. Never write one from memory.

## Workspace

- The lint floor is `[workspace.lints]` in the root `Cargo.toml` and inheritance is OPT-IN: every
  member needs its own top-level `[lints] workspace = true`. Written inside `[package]`, cargo
  ignores it without a warning.
- A crate must be reachable as a workspace member, or `cargo metadata` counts it as nothing and all
  three gate commands pass without ever compiling it (§8 row G1).
- The floor binds tests too: `unwrap`, `expect` or `panic!` in a test exits clippy 101. Use
  `assert!`, `assert_eq!`, a `match`, or `let Ok(x) = … else`.
- Repo-wide invariants live in `repo-invariants/`, never inside a crate they police. A gate over an
  artefact that crosses a boundary gets its own contract crate: `canon/`, `panel-contract/`.

## Rules this project exists for

- IMPORTANT: a gate must bite. Before claiming anything in §8, delete the rule and watch a NAMED
  test go red. If nothing goes red, the cell states what is unguarded instead.
- IMPORTANT: every enum variant, column, field and gate needs a producer or a reader on a real path
  in the same change. A branch nothing reaches is a gate that passes unconditionally.
- Publish a number with its construction and its date, or do not write it. Never pick a threshold
  after seeing the result.
- The design document states what IS: delete superseded text and move its measurement into the row
  that now carries the claim. The record of what was believed is the commit that changed it.
- One design document, amended in place. Never a new markdown file for a decision or a finding.
- A `compile_fail` doc test passes whenever its snippet stops compiling for ANY reason. Pair it with
  a positive control and assert each fence annotation EXACTLY: `compile_fail,ignore` must be red.

## Point in time, which is the whole point

- `ValidTime` (when the world published it) and `ObservationTime` (when we could first have known
  it) are separate types and cannot be compared. `Visibility::visible_at` requires both: the
  half-question admitted non-visible documents in 60.2% of 69 retrieval probes and the full question
  in none.
- A publication value that is a DATE stays a date — `pit::datedness::Datedness` — and is never
  folded into a midnight instant. `Published` carries it for ordering and display and cannot reach a
  cutoff.
- Identity is a content hash over one canonical form: `canon::sha256_canon(Domain::X, &fields)`,
  domain first, every digest returning `Result`. New digests ship a committed vector.
- Dedup keys on the vendor row identity (`story_id`) and never on the content hash, and first
  sighting wins: keeping the last postdates our own knowledge.
- A missing input aborts with a named error. An empty fallback is forbidden: a missing dictionary
  once produced an audit result of `0`, indistinguishable from a clean pass.
- No vendor call outside an open attempt row, and an attempt row is written whether or not anything
  came back. "Nothing happened" and "we did not look" must stay different.

## Decided by default, not settled

- Exit `1` means "the answer is no", never "an error happened"; errors use 2, 4, 7 and 8.
- Archive partitions are keyed by publication day.
Each is changeable in the one place §10 of the design names. Tell the owner before relying on either.

## Scope

- A `CLAUDE.md` belonging to a different repository does not apply here, even when a session started
  in that repository puts it in context.
- Code, comments and identifiers in English; math notation (α, ρ, ×, −) stays.
