# probes

A probe is a measurement script kept so its number can be re-derived. It is not a service and not
production code: it answers one question whose answer we do not know in advance, writes a result
file, and is expected to run rarely.

**The boundary that matters is not internal versus external, it is whether the answer is known in
advance.** A test checks our code against our expectations and has its answer up front. A probe
measures something we do not control — a vendor's volume, our own archive's composition, the
standard error of a statistic — and could return anything.

**A probe is kept only if it will be run again** (rule 3 of `../CONTRIBUTING.md`). The table below
is the list, and the count: six scripts and three packages, each with a second run ahead of it.
Roughly as many measurements from the same work were
deliberately not kept: their numbers are in `../docs/design/` with the construction that produced
them, which is what a reader needs. A script that answered its question once is a receipt.

| file | the question it answers | when it runs again |
|---|---|---|
| `parity_baseline.py` | does an independent reading of the archive rules agree with the Rust one, day by day? | at every migration stage. It writes a baseline over the real lake and nothing reads it; the comparison is `newsvault_parity.py` |
| `newsvault_parity.py` | do the two readings agree field by field, on the shapes the lake holds none of? | whenever either reader's rules change. **This is the archive-reading parity gate** (memo §5.1): its committed `expected.json` is read by `services/newsvault/tests/parity.rs`, so `cargo test` compares the Rust reader against a second implementation without running Python |
| `panel_recount.py` | how many company-sessions does the panel treat, under which rule version? | on every rule change, which is requirement R18 |
| `panel_power.py` | what is the measured power of the registered procedure on this panel? | on every panel rebuild |
| `matcher_audit.py` | what is the entity precision of the counterparty matcher, and what does the text cap cost? | on every change to the matching rules |
| `news_gap.py` | how stale may the newest item be before that is an incident? | against the first real vendor, because the thresholds it produced are provisional on D-1 |
| `vendor_probe/` | four questions about a news vendor (Rust `main.rs`, a standalone crate with its own HTTP client, so no CI step compiles it); **run 2026-11-14**. The one-time 12-month history decision, its three dated amendments and their numbers are in `vendor_probe/PREREGISTRATION.md`: NewsAPI.ai is closed (509.85 USD after its one narrowing, above 300), Webz.io registers no lag until a one-month order is measured. Those amended numbers came from one-off scripts, not from `main.rs`, whose Q4 still uses the first construction | once more at least 7 days later for the stability half, on the Webz.io one-month order, and whenever a vendor's plan changes |
| `observation_lag/` | how many seconds after a filing's acceptance does EDGAR's daily index carry it, which is the `MeasuredLag` a `vendor_archive` window is refused without (R11); **run 2026-11-13, 27 requests of a registered 30: 94,922 s, band 24 h - 7 d** | the same two days again at least 7 days later, which is what tells a final index file from one that is still being rewritten, and on any change to the daily-index construction |
| `newsvault_parity/` | the corpus `newsvault_parity.py` runs over, one partition per shape the archive lacks, plus that probe's pre-registration and its committed output | rebuilt with `--write-corpus` when a shape is added |

Three of them take their inputs from the environment and abort with a named error when one is
absent, which is requirement R6 applied to the measurement scripts themselves: run one with no
environment set to see what it needs. `matcher_audit.py` and `panel_recount.py` do **not** — both
die on `import ahocorasick` before any input check, and there is no dependency list in the tree to
install from, so neither can be re-run today. Rule 3 keeps a script so its number can be
re-derived, which makes that a defect and not a detail.

Two rules, both of which exist because of the predecessor's history:

**A probe's decision rule is written before it runs.** The memo's doctrine is to pre-register before
testing, and that applies to a vendor evaluation as much as to a study: a threshold chosen after
seeing the number is not a threshold. Each probe therefore ships a `PREREGISTRATION.md` stating
what each possible answer will mean, committed before any key exists.

**A probe is not a vendor client.** It talks to a vendor URL directly, which production code here
may not do — every vendor call in a service goes through that vendor's one canonical client. If a
vendor is adopted, its calls move behind such a client and the probe stays as the record of why.
