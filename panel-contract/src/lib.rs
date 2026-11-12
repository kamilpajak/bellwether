//! The panel manifest contract: the required shape of the JSON file beside a panel's Arrow data,
//! and the validator that decides whether one instance of it is admissible.
//!
//! Row R8 of `docs/design/architecture.md` section 8 holds the six rules and the measurement that
//! forced them, and its fifth correction holds the ceiling: this crate never opens the Arrow file
//! beside the manifest, and the two fields section 3.4 requires that are properties of a
//! *statistic* rather than of the row set are carried as [`Need::DeferredToR9`] instead of being
//! required here. The failure being encoded is memo sections 1.8 and 1.9. Why the contract is its
//! own crate, owned by neither side of the boundary: `CONTRIBUTING.md`, "Everything else".

use serde_json::{Map, Value};

/// The one schema version this validator knows. Any other value is rejected rather than read
/// optimistically, because field meanings are what change between versions.
pub const SCHEMA: &str = "panel.manifest/1";

const ROW: &str = "docs/design/architecture.md section 8 row R8";
const MEMO: &str = "docs/design/successor_system_design_2026_11_11.md section 1.8";

/// The attestation that zero attrition was counted rather than assumed.
pub const ZERO_ATTRITION: &str = "zero_attrition_counted";
pub const ROWS_EXCHANGEABLE: &str = "rows_are_exchangeable";

/// Shortest string accepted as an attestation sentence. A length floor, HEURISTIC, and what it
/// can and cannot tell apart is architecture.md section 8, fifth correction.
const ATTESTATION_CHARS: usize = 40;

/// The JSON shape a manifest field must carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Text,
    /// A non-negative integer. A count arriving as `78460.0` is rejected: a row count that went
    /// through a float has been through arithmetic that can lose it.
    Count,
    /// A non-negative number that need not be an integer: a measured ratio, carried by the two
    /// deferred fields only.
    Ratio,
    List,
    Table,
}

impl Shape {
    fn fits(self, value: &Value) -> bool {
        match self {
            Self::Text => value.as_str().is_some_and(|text| !text.trim().is_empty()),
            Self::Count => value.as_u64().is_some(),
            Self::Ratio => value.as_f64().is_some_and(|number| number >= 0.0),
            Self::List => value.is_array(),
            Self::Table => value.is_object(),
        }
    }

    /// How to name this shape to whoever has to fix the manifest.
    fn name(self) -> &'static str {
        match self {
            Self::Text => "a non-empty string",
            Self::Count => "a non-negative integer",
            Self::Ratio => "a non-negative number",
            Self::List => "an array",
            Self::Table => "an object",
        }
    }
}

/// When a field has to be there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    /// Every manifest, every time.
    Always,
    /// Only when one of the arithmetic rules asks for an attestation.
    WhenAttested,
    /// Listed by architecture.md section 3.4 and not enforced here - see the module docs.
    DeferredToR9,
}

/// One manifest field. One table, read by the validator and printed by `panel-contract schema`,
/// so the documented shape cannot drift from the enforced one.
#[derive(Debug)]
pub struct Field {
    pub name: &'static str,
    pub shape: Shape,
    pub need: Need,
    /// Quoted back in the failure message, so a manifest is never refused without its reason.
    pub why: &'static str,
}

/// The whole field table of `panel.manifest/1`.
pub const FIELDS: &[Field] = &[
    Field {
        name: "schema",
        shape: Shape::Text,
        need: Need::Always,
        why: "the contract version this manifest was written against",
    },
    Field {
        name: "panel_id",
        shape: Shape::Text,
        need: Need::Always,
        why: "the panel's own content hash; a number whose panel cannot be named is an anecdote",
    },
    Field {
        name: "snapshot_id",
        shape: Shape::Text,
        need: Need::Always,
        why: "the input set. A date range is not reproducible, because the archive grows under it",
    },
    Field {
        name: "rule_version",
        shape: Shape::Text,
        need: Need::Always,
        why: "the hashed rules file. An unversioned gazetteer once dropped a real company with \
              no diff, no test and no record",
    },
    Field {
        name: "calendar_version",
        shape: Shape::Text,
        need: Need::Always,
        why: "the session calendar, half-days and DST included. It changes the unit count",
    },
    Field {
        name: "cutoff",
        shape: Shape::Text,
        need: Need::Always,
        why: "the as-of instant every downstream claim quotes",
    },
    Field {
        name: "rows",
        shape: Shape::Count,
        need: Need::Always,
        why: "rows actually present, counted. Never a product of two lengths",
    },
    Field {
        name: "cardinalities",
        shape: Shape::List,
        need: Need::Always,
        why: "the sizes the panel was built from, named, so the attrition chain has two ends",
    },
    Field {
        name: "attrition",
        shape: Shape::List,
        need: Need::Always,
        why: "rows dropped per stage, so a missing row is accounted rather than absent",
    },
    Field {
        name: "exchangeable_units",
        shape: Shape::Table,
        need: Need::Always,
        why: "a row count with no unit count invites the independent-rows formula; the unit was \
              the session, 47 of them, not the 78,460 rows",
    },
    Field {
        name: "attestations",
        shape: Shape::Table,
        need: Need::WhenAttested,
        why: "named attestations, each a sentence. Required only when a rule below asks for one",
    },
    Field {
        name: "design_effect",
        shape: Shape::Ratio,
        need: Need::DeferredToR9,
        why: "measured ratio of the statistic's standard error to the independent-rows formula; \
              a property of the procedure, so row R9's consumer is where it can be required",
    },
    Field {
        name: "within_unit_correlation",
        shape: Shape::Ratio,
        need: Need::DeferredToR9,
        why: "measured, not assumed; same reason as design_effect",
    },
];

/// One reason a manifest is not admissible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// Machine-readable and never renamed, so a caller can branch on it.
    pub code: &'static str,
    /// Which lettered rule of row R8 this is, for a reader holding the document.
    pub rule: &'static str,
    /// True when the rule is a proxy for what matters rather than a proof of it. The message says
    /// so too, and a test asserts the two never disagree: they are read by different callers.
    pub heuristic: bool,
    /// What is wrong, with the numbers it was decided on.
    pub message: String,
}

impl Finding {
    fn new(code: &'static str, rule: &'static str, message: String) -> Self {
        Self {
            code,
            rule,
            heuristic: false,
            message,
        }
    }

    fn heuristic(code: &'static str, rule: &'static str, message: String) -> Self {
        Self {
            code,
            rule,
            heuristic: true,
            message,
        }
    }
}

/// A named cardinality the panel says it was built from.
#[derive(Debug)]
struct Cardinality {
    name: String,
    count: u64,
}

/// Validate one parsed manifest. An empty result is a pass. Every rule that can still be decided
/// is decided, so one run reports every problem; a rule that a malformed field makes undecidable
/// is skipped rather than guessed, because arithmetic over a half-read list is not a verdict.
pub fn validate(manifest: &Value) -> Vec<Finding> {
    let mut findings = Vec::new();

    let Some(table) = manifest.as_object() else {
        findings.push(Finding::new(
            "not_an_object",
            "R8.a",
            format!(
                "The manifest is not a JSON object but {}. The contract is a single object; see \
                 {ROW}.",
                kind_of(manifest)
            ),
        ));
        return findings;
    };

    check_fields(table, &mut findings);
    check_schema(table, &mut findings);
    check_units_shape(table, &mut findings);
    check_attestation_sentences(table, &mut findings);
    check_arithmetic(table, &mut findings);

    findings
}

/// Rule a. Every `Need::Always` field present and of the right shape.
fn check_fields(table: &Map<String, Value>, findings: &mut Vec<Finding>) {
    for field in FIELDS.iter().filter(|f| f.need == Need::Always) {
        match table.get(field.name) {
            None => findings.push(Finding::new(
                "missing_field",
                "R8.a",
                format!(
                    "`{}` is missing. It is required because: {}. ({ROW}.)",
                    field.name, field.why
                ),
            )),
            Some(value) if !field.shape.fits(value) => findings.push(Finding::new(
                "wrong_type",
                "R8.a",
                format!(
                    "`{}` must be {}, and is {}.",
                    field.name,
                    field.shape.name(),
                    kind_of(value)
                ),
            )),
            Some(_) => {}
        }
    }
}

/// Rule a. The version this validator is entitled to judge.
fn check_schema(table: &Map<String, Value>, findings: &mut Vec<Finding>) {
    if let Some(found) = table.get("schema").and_then(Value::as_str) {
        if found != SCHEMA {
            findings.push(Finding::new(
                "unknown_schema",
                "R8.a",
                format!(
                    "`schema` is {found:?}; this validator knows {SCHEMA:?} only. It is not \
                     reporting a pass over rules it may no longer be the right reader of."
                ),
            ));
        }
    }
}

/// Rule b. `exchangeable_units` carries a count, a named unit and a definition of the unit. The
/// definition is the third key because of memo section 1.9, where 47 nominal sessions were
/// effectively fewer and the name alone did not say what made two of them exchangeable.
fn check_units_shape(table: &Map<String, Value>, findings: &mut Vec<Finding>) {
    let Some(units) = table.get("exchangeable_units").and_then(Value::as_object) else {
        return;
    };
    for (key, shape) in [
        ("count", Shape::Count),
        ("unit", Shape::Text),
        ("definition", Shape::Text),
    ] {
        match units.get(key) {
            Some(value) if shape.fits(value) => {}
            found => findings.push(Finding::new(
                "exchangeable_units_incomplete",
                "R8.b",
                format!(
                    "`exchangeable_units.{key}` must be {}, and is {}. The count, the unit's \
                     name and what makes two units exchangeable are all required: the \
                     predecessor reported thousands of rows, no unit count, and a power of 88.2% \
                     against 27.4% measured ({MEMO}).",
                    shape.name(),
                    found.map_or("absent", kind_of)
                ),
            )),
        }
    }
}

/// Rules d and e, sentence half. HEURISTIC - see [`ATTESTATION_CHARS`].
fn check_attestation_sentences(table: &Map<String, Value>, findings: &mut Vec<Finding>) {
    let Some(attestations) = table.get("attestations").and_then(Value::as_object) else {
        return;
    };
    for (name, value) in attestations {
        let written = value.as_str().map_or(0, |text| text.trim().chars().count());
        if written < ATTESTATION_CHARS {
            findings.push(Finding::heuristic(
                "attestation_not_a_sentence",
                "R8.d/e",
                format!(
                    "Attestation `{name}` carries {} characters; at least {ATTESTATION_CHARS} \
                     are required. HEURISTIC: the rule is that someone wrote down how the number \
                     was counted, and length cannot check that. It rules out a checkbox, which is \
                     what the original failure already was.",
                    if value.is_string() {
                        written.to_string()
                    } else {
                        format!("no string but {}, so 0", kind_of(value))
                    }
                ),
            ));
        }
    }
}

/// Rules c, d, e and f: everything that is arithmetic over the stated numbers.
fn check_arithmetic(table: &Map<String, Value>, findings: &mut Vec<Finding>) {
    let Some(rows) = table.get("rows").and_then(Value::as_u64) else {
        return;
    };
    let units = table
        .get("exchangeable_units")
        .and_then(Value::as_object)
        .and_then(|u| u.get("count"))
        .and_then(Value::as_u64);
    let cardinalities = read_cardinalities(table, findings);
    let attrition = read_attrition(table, findings);

    if let Some(cardinalities) = &cardinalities {
        // Rule d needs only the cardinalities; rule c needs both ends of the chain.
        check_product(rows, cardinalities, table, findings);
        if let Some(attrition) = attrition {
            check_chain(rows, cardinalities, attrition, findings);
        }
    }
    if let Some(count) = units {
        check_units_against_rows(rows, count, table, findings);
    }
}

/// Rule c. The chain closes: product of the stated cardinalities, minus every row the manifest
/// says was dropped, equals the row count. The original failure reconciles only by claiming no
/// row was ever absent, and rule d then asks for that claim in writing.
fn check_chain(
    rows: u64,
    cardinalities: &[Cardinality],
    attrition: u128,
    findings: &mut Vec<Finding>,
) {
    let Some(product) = product_of(cardinalities) else {
        findings.push(Finding::new(
            "cardinalities_overflow",
            "R8.c",
            format!(
                "The product of the {} stated cardinalities overflows a 128-bit integer, so the \
                 attrition chain cannot be reconciled. The numbers themselves are the problem.",
                cardinalities.len()
            ),
        ));
        return;
    };
    let expected = product.checked_sub(attrition);
    if expected == Some(u128::from(rows)) {
        return;
    }
    let factors = factors_of(cardinalities);
    findings.push(Finding::new(
        "attrition_does_not_reconcile",
        "R8.c",
        match expected {
            Some(expected) => format!(
                "The attrition chain does not close: {product} ({factors}) minus {attrition} \
                 dropped is {expected}, and `rows` is {rows} - a difference of {}. Either a \
                 stage is missing from `attrition` or one of the three numbers is wrong; the \
                 manifest cannot say which, which is why this is an error rather than a note.",
                expected.abs_diff(u128::from(rows))
            ),
            None => format!(
                "The attrition chain does not close: the manifest drops {attrition} rows from a \
                 product of {product} ({factors}), which is more rows than the product holds, \
                 and `rows` is {rows}."
            ),
        },
    ));
}

/// Rule d. `rows` equal to a product of the stated cardinalities is allowed, because a full
/// rectangle is real, but it has to be attested: a written sentence is the difference between a
/// counted zero attrition and an assumed one.
fn check_product(
    rows: u64,
    cardinalities: &[Cardinality],
    table: &Map<String, Value>,
    findings: &mut Vec<Finding>,
) {
    let Some((product, factors)) = recognised_product(rows, cardinalities) else {
        return;
    };
    if attested(table, ZERO_ATTRITION) {
        return;
    }
    findings.push(Finding::heuristic(
        "rows_equal_a_product_unattested",
        "R8.d",
        format!(
            "`rows` is {rows}, which is exactly {factors} = {product}. That is allowed when every \
             row of the rectangle is really present, and it is also how the predecessor's panel \
             was computed: 78,584 rows where a recount found 78,460 ({MEMO}). Add an \
             `attestations.{ZERO_ATTRITION}` sentence saying how the zero attrition was counted, \
             not that it was assumed. HEURISTIC: a product can also be hit by coincidence, and \
             this rule cannot tell the two apart, which is why it asks for a sentence rather than \
             rejecting the manifest outright."
        ),
    ));
}

/// Rules e and f, against the row count.
fn check_units_against_rows(
    rows: u64,
    count: u64,
    table: &Map<String, Value>,
    findings: &mut Vec<Finding>,
) {
    if count > rows {
        findings.push(Finding::new(
            "units_exceed_rows",
            "R8.f",
            format!(
                "`exchangeable_units.count` is {count} and `rows` is {rows}. A unit is a group of \
                 rows, so there cannot be more units than rows: {} of them would hold nothing. \
                 This one has no attestation, because there is no state of the world it describes.",
                count - rows
            ),
        ));
        return;
    }
    if count == rows && !attested(table, ROWS_EXCHANGEABLE) {
        findings.push(Finding::new(
            "units_equal_rows_unattested",
            "R8.e",
            format!(
                "`exchangeable_units.count` equals `rows` ({rows}). That is the independence \
                 claim - one row, one unit - and the assumption behind the 88.2% power that \
                 measured 27.4% ({MEMO}). It can be true: a single-session cross-section has one \
                 unit per company. Add an `attestations.{ROWS_EXCHANGEABLE}` sentence saying what \
                 was checked."
            ),
        ));
    }
}

/// Whether the manifest carries a non-trivial attestation under `name`. The sentence is checked
/// separately, so a too-short one yields its own finding as well as failing the rule that asked.
fn attested(table: &Map<String, Value>, name: &str) -> bool {
    table
        .get("attestations")
        .and_then(Value::as_object)
        .and_then(|a| a.get(name))
        .and_then(Value::as_str)
        .is_some_and(|text| text.trim().chars().count() >= ATTESTATION_CHARS)
}

/// The product `rows` was recognised as, if any: any pair of the stated cardinalities, the whole
/// product, or a list of one. HEURISTIC and shallow - what it misses, and the measured manifest
/// that forced the list-of-one arm, are architecture.md section 8, fifth correction.
fn recognised_product(rows: u64, cardinalities: &[Cardinality]) -> Option<(u128, String)> {
    let rows = u128::from(rows);
    for (index, left) in cardinalities.iter().enumerate() {
        for right in cardinalities.iter().skip(index + 1) {
            // An overflowing pair cannot equal a `rows` that fits in 64 bits, and the whole
            // product then overflows too, which `check_chain` reports as its own finding.
            let Some(pair) = u128::from(left.count).checked_mul(u128::from(right.count)) else {
                continue;
            };
            if pair == rows {
                return Some((
                    pair,
                    format!(
                        "{} ({}) x {} ({})",
                        left.name, left.count, right.name, right.count
                    ),
                ));
            }
        }
    }
    if !cardinalities.is_empty() && product_of(cardinalities) == Some(rows) {
        return Some((rows, factors_of(cardinalities)));
    }
    None
}

/// The product of every stated cardinality. `None` on overflow, which is a finding of its own
/// rather than a wrapped number.
fn product_of(cardinalities: &[Cardinality]) -> Option<u128> {
    cardinalities
        .iter()
        .try_fold(1u128, |acc, c| acc.checked_mul(u128::from(c.count)))
}

/// `companies (1672) x sessions (47)`, for a message that names what it multiplied.
fn factors_of(cardinalities: &[Cardinality]) -> String {
    cardinalities
        .iter()
        .map(|c| format!("{} ({})", c.name, c.count))
        .collect::<Vec<_>>()
        .join(" x ")
}

/// Rule c, input half. `None` when the list cannot be read, so no arithmetic is reported over it.
fn read_cardinalities(
    table: &Map<String, Value>,
    findings: &mut Vec<Finding>,
) -> Option<Vec<Cardinality>> {
    let list = table.get("cardinalities")?.as_array()?;
    if list.is_empty() {
        findings.push(Finding::new(
            "cardinalities_empty",
            "R8.c",
            format!(
                "`cardinalities` is empty, so the attrition chain has no first end and `rows` is \
                 a number with no construction. State the sizes the panel was built from, each \
                 with a name ({ROW})."
            ),
        ));
        return None;
    }
    let mut read = Vec::with_capacity(list.len());
    for (index, entry) in list.iter().enumerate() {
        let name = entry.get("name").and_then(Value::as_str);
        let count = entry.get("count").and_then(Value::as_u64);
        match (name, count) {
            (Some(name), Some(count)) if !name.trim().is_empty() && count > 0 => {
                read.push(Cardinality {
                    name: name.to_owned(),
                    count,
                });
            }
            _ => {
                findings.push(Finding::new(
                    "cardinality_malformed",
                    "R8.c",
                    format!(
                        "`cardinalities[{index}]` must be an object with a non-empty `name` and a \
                         `count` above zero, and is {entry}. A cardinality with no name cannot \
                         appear in the sentence that explains the row count."
                    ),
                ));
                return None;
            }
        }
    }
    Some(read)
}

/// Rule c, the other input. `Some(0)` for an empty list: a panel may genuinely drop nothing, and
/// rule d is what asks whether that was counted.
fn read_attrition(table: &Map<String, Value>, findings: &mut Vec<Finding>) -> Option<u128> {
    let list = table.get("attrition")?.as_array()?;
    let mut total = 0u128;
    for (index, entry) in list.iter().enumerate() {
        let stage = entry.get("stage").and_then(Value::as_str);
        let dropped = entry.get("rows_dropped").and_then(Value::as_u64);
        match (stage, dropped) {
            (Some(stage), Some(dropped)) if !stage.trim().is_empty() => {
                total = total.checked_add(u128::from(dropped))?;
            }
            _ => {
                findings.push(Finding::new(
                    "attrition_malformed",
                    "R8.c",
                    format!(
                        "`attrition[{index}]` must be an object with a non-empty `stage` and a \
                         `rows_dropped` integer, and is {entry}. A stage with no name is an \
                         unaccounted row by another route."
                    ),
                ));
                return None;
            }
        }
    }
    Some(total)
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(n) if n.as_u64().is_some() => "a non-negative integer",
        Value::Number(_) => "a number that is not a non-negative integer",
        Value::String(s) if s.trim().is_empty() => "an empty string",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::path::{Path, PathBuf};

    /// `clippy::panic` is denied here too, so the failure path is `expect` on a `Result` whose
    /// error already carries the file name.
    fn fixture(name: &str) -> Value {
        let path = fixtures().join(name);
        let text = std::fs::read_to_string(&path)
            .map_err(|why| format!("read {}: {why}", path.display()))
            .expect("fixture is readable");
        serde_json::from_str(&text)
            .map_err(|why| format!("parse {}: {why}", path.display()))
            .expect("fixture is JSON")
    }

    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
    }

    fn codes(findings: &[Finding]) -> Vec<&str> {
        findings.iter().map(|f| f.code).collect()
    }

    fn joined(findings: &[Finding]) -> String {
        findings
            .iter()
            .map(|f| f.message.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Fixture 1, the historical failure of memo section 1.8: rejected twice over, and the message
    /// has to name the product it recognised.
    #[test]
    fn the_predecessors_panel_is_rejected_and_the_product_is_named() {
        let findings = validate(&fixture("historical_failure_78584.json"));
        assert!(
            codes(&findings).contains(&"rows_equal_a_product_unattested"),
            "{findings:#?}"
        );
        assert!(codes(&findings).contains(&"missing_field"), "{findings:#?}");
        let text = joined(&findings);
        assert!(
            text.contains("companies (1672) x sessions (47) = 78584"),
            "{text}"
        );
        assert!(text.contains("exchangeable_units"), "{text}");
    }

    /// Every finding whose message calls itself a heuristic carries the flag, and every flagged
    /// finding says so in its message: the two are read by a person and by a program.
    #[test]
    fn the_heuristic_flag_and_the_word_heuristic_never_disagree() {
        let all: Vec<Finding> = [
            "historical_failure_78584.json",
            "corrected_panel_78460.json",
            "units_equal_rows_unattested.json",
            "units_equal_rows_attested.json",
            "full_rectangle_attested.json",
            "attrition_does_not_reconcile.json",
            "units_exceed_rows.json",
        ]
        .iter()
        .flat_map(|name| validate(&fixture(name)))
        .collect();
        assert!(all.len() >= 5, "{all:#?}");
        for finding in &all {
            assert_eq!(
                finding.heuristic,
                finding.message.contains("HEURISTIC"),
                "{finding:#?}"
            );
        }
    }

    /// Fixture 2. The same panel recounted, as memo section 1.8 recounted it.
    #[test]
    fn the_recounted_panel_passes() {
        assert_eq!(validate(&fixture("corrected_panel_78460.json")), vec![]);
    }

    /// Fixture 3, both halves. One unit per row is the independence claim: permitted, and written.
    #[test]
    fn units_equal_to_rows_need_their_own_attestation() {
        let findings = validate(&fixture("units_equal_rows_unattested.json"));
        assert_eq!(codes(&findings), vec!["units_equal_rows_unattested"]);
        assert!(joined(&findings).contains("88.2%"), "{findings:#?}");
        assert_eq!(validate(&fixture("units_equal_rows_attested.json")), vec![]);
    }

    /// Fixture 4. A real full rectangle, attested. The rule is not a prohibition.
    #[test]
    fn a_counted_full_rectangle_passes_with_its_attestation() {
        assert_eq!(validate(&fixture("full_rectangle_attested.json")), vec![]);
    }

    /// Fixture 5. The arithmetic has to close, and the message carries both numbers.
    #[test]
    fn an_attrition_chain_that_does_not_close_is_rejected_with_both_numbers() {
        let findings = validate(&fixture("attrition_does_not_reconcile.json"));
        assert_eq!(codes(&findings), vec!["attrition_does_not_reconcile"]);
        let text = joined(&findings);
        assert!(text.contains("78484"), "{text}");
        assert!(text.contains("78460"), "{text}");
    }

    /// Fixture 6. More units than rows has no attestation, because it describes nothing.
    #[test]
    fn more_units_than_rows_is_always_an_error() {
        let findings = validate(&fixture("units_exceed_rows.json"));
        assert_eq!(codes(&findings), vec!["units_exceed_rows"]);
        assert!(
            joined(&findings).contains("124 of them would hold nothing"),
            "{findings:#?}"
        );
    }

    /// Every fixture on disk is reached by a test above; a fixture nobody validates is a file that
    /// looks like evidence. Only `.json` counts: measured 2026-11-12, an untracked `.DS_Store`,
    /// which Finder writes and `.gitignore` hides, turned this red on macOS while CI stayed green.
    #[test]
    fn the_fixture_directory_holds_exactly_the_seven_committed_cases() {
        let mut names: Vec<String> = std::fs::read_dir(fixtures())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|kind| kind == "json"))
            .map(|path| {
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "attrition_does_not_reconcile.json",
                "corrected_panel_78460.json",
                "full_rectangle_attested.json",
                "historical_failure_78584.json",
                "units_equal_rows_attested.json",
                "units_equal_rows_unattested.json",
                "units_exceed_rows.json",
            ]
        );
    }

    // ---- the rules against hand-built values, where a fixture would be an eighth file ----

    /// A pass with one field removed must fail, for every always-required field. A loop over
    /// `FIELDS` rather than one test per field, because a hand-written list could fall behind it.
    #[test]
    fn every_required_field_is_actually_required() {
        let base = fixture("corrected_panel_78460.json");
        for field in FIELDS.iter().filter(|f| f.need == Need::Always) {
            let mut broken = base.clone();
            let removed = broken.as_object_mut().unwrap().remove(field.name);
            assert!(
                removed.is_some(),
                "the fixture has no `{}` to remove, so this case proves nothing",
                field.name
            );
            let findings = validate(&broken);
            assert!(
                findings.iter().any(|f| f.code == "missing_field"
                    && f.message.contains(&format!("`{}` is missing", field.name))),
                "removing `{}` was accepted: {findings:#?}",
                field.name
            );
        }
    }

    /// The two fields section 3.4 lists and this crate does not enforce, so a reader of row R8
    /// cannot take it for more than it is.
    #[test]
    fn the_two_r9_fields_are_not_required_here() {
        let base = fixture("corrected_panel_78460.json");
        for field in FIELDS.iter().filter(|f| f.need == Need::DeferredToR9) {
            assert!(
                base.get(field.name).is_none(),
                "fixture carries `{}`, so this test proves nothing",
                field.name
            );
        }
        assert_eq!(validate(&base), vec![]);
    }

    #[test]
    fn a_manifest_that_is_not_an_object_is_one_finding_and_no_more() {
        let findings = validate(&serde_json::json!([1, 2, 3]));
        assert_eq!(codes(&findings), vec!["not_an_object"]);
    }

    #[test]
    fn a_schema_this_validator_does_not_know_is_refused_rather_than_read() {
        let mut manifest = fixture("corrected_panel_78460.json");
        manifest.as_object_mut().unwrap().insert(
            "schema".to_owned(),
            Value::String("panel.manifest/2".to_owned()),
        );
        let findings = validate(&manifest);
        assert_eq!(codes(&findings), vec!["unknown_schema"]);
    }

    #[test]
    fn a_row_count_that_is_not_an_integer_is_the_wrong_type() {
        let mut manifest = fixture("corrected_panel_78460.json");
        manifest
            .as_object_mut()
            .unwrap()
            .insert("rows".to_owned(), serde_json::json!(78460.0));
        let findings = validate(&manifest);
        assert!(codes(&findings).contains(&"wrong_type"), "{findings:#?}");
    }

    /// Rule b: the count alone is not an answer, and neither is the count plus a bare name.
    #[test]
    fn a_unit_count_without_a_name_or_a_definition_is_incomplete() {
        let mut manifest = fixture("corrected_panel_78460.json");
        manifest.as_object_mut().unwrap().insert(
            "exchangeable_units".to_owned(),
            serde_json::json!({"count": 47}),
        );
        let findings = validate(&manifest);
        assert_eq!(
            codes(&findings),
            vec![
                "exchangeable_units_incomplete",
                "exchangeable_units_incomplete"
            ]
        );
        assert!(
            joined(&findings).contains("`exchangeable_units.unit`"),
            "{findings:#?}"
        );
        assert!(
            joined(&findings).contains("`exchangeable_units.definition`"),
            "{findings:#?}"
        );
    }

    #[test]
    fn a_one_word_attestation_is_not_a_sentence_and_the_message_says_it_is_a_heuristic() {
        let mut manifest = fixture("full_rectangle_attested.json");
        manifest.as_object_mut().unwrap().insert(
            "attestations".to_owned(),
            serde_json::json!({ZERO_ATTRITION: "counted"}),
        );
        let findings = validate(&manifest);
        assert!(
            findings
                .iter()
                .any(|f| f.code == "attestation_not_a_sentence" && f.heuristic),
            "{findings:#?}"
        );
        assert!(joined(&findings).contains("HEURISTIC"), "{findings:#?}");
        // A too-short sentence does not satisfy the rule that asked for it either.
        assert!(
            codes(&findings).contains(&"rows_equal_a_product_unattested"),
            "{findings:#?}"
        );
    }

    /// Rule d recognises a pair inside a longer list and names that pair, not the whole product.
    #[test]
    fn a_product_of_two_cardinalities_among_three_is_recognised_and_named() {
        let findings = validate(&serde_json::json!({
            "schema": SCHEMA,
            "panel_id": "sha256:x", "snapshot_id": "sha256:y",
            "rule_version": "sha256:z", "calendar_version": "sha256:w",
            "cutoff": "2026-08-14T20:00:00Z",
            "rows": 200,
            "cardinalities": [
                {"name": "companies", "count": 10},
                {"name": "sessions", "count": 20},
                {"name": "arms", "count": 2}
            ],
            "attrition": [{"stage": "no_close", "rows_dropped": 200}],
            "exchangeable_units": {"count": 20, "unit": "session", "definition": "one session"}
        }));
        let text = joined(&findings);
        assert!(
            text.contains("companies (10) x sessions (20) = 200"),
            "{text}"
        );
    }

    /// The pre-multiplied manifest: the product done before the manifest was written, stated as a
    /// single cardinality. It was admissible until the whole-product arm covered a list of one -
    /// architecture.md section 8, fifth correction.
    #[test]
    fn a_product_stated_as_one_pre_multiplied_cardinality_is_still_a_product() {
        let findings = validate(&serde_json::json!({
            "schema": SCHEMA,
            "panel_id": "sha256:x", "snapshot_id": "sha256:y",
            "rule_version": "sha256:z", "calendar_version": "sha256:w",
            "cutoff": "2026-08-14T20:00:00Z",
            "rows": 78584,
            "cardinalities": [{"name": "company_sessions", "count": 78584}],
            "attrition": [],
            "exchangeable_units": {
                "count": 47, "unit": "trading_session", "definition": "One trading session."
            }
        }));
        assert_eq!(codes(&findings), vec!["rows_equal_a_product_unattested"]);
        assert!(
            joined(&findings).contains("company_sessions (78584) = 78584"),
            "{findings:#?}"
        );
    }

    /// Rule c when more rows are dropped than the rectangle holds: the subtraction does not wrap.
    #[test]
    fn attrition_larger_than_the_product_does_not_underflow() {
        let findings = validate(&serde_json::json!({
            "schema": SCHEMA,
            "panel_id": "sha256:x", "snapshot_id": "sha256:y",
            "rule_version": "sha256:z", "calendar_version": "sha256:w",
            "cutoff": "2026-08-14T20:00:00Z",
            "rows": 5,
            "cardinalities": [{"name": "companies", "count": 3}, {"name": "sessions", "count": 4}],
            "attrition": [{"stage": "everything", "rows_dropped": 999}],
            "exchangeable_units": {"count": 4, "unit": "session", "definition": "one session"}
        }));
        assert_eq!(codes(&findings), vec!["attrition_does_not_reconcile"]);
        assert!(
            joined(&findings).contains("more rows than the product holds"),
            "{findings:#?}"
        );
    }

    /// A cardinality the validator cannot read stops the arithmetic instead of guessing at it.
    #[test]
    fn an_unreadable_cardinality_suppresses_the_arithmetic_rules() {
        let mut manifest = fixture("historical_failure_78584.json");
        manifest.as_object_mut().unwrap().insert(
            "cardinalities".to_owned(),
            serde_json::json!([{"count": 1672}, {"name": "sessions", "count": 47}]),
        );
        let findings = validate(&manifest);
        // The whole code list, not a `contains`: the mutation this pins reports arithmetic over
        // the half of the list it could read, which shows up as an EXTRA finding. Measured
        // 2026-11-12, with `return None` changed to `continue` this manifest gains an
        // `attrition_does_not_reconcile` and a `contains`-only assertion passed it.
        assert_eq!(
            codes(&findings),
            vec!["missing_field", "cardinality_malformed"],
            "{findings:#?}"
        );
    }
}
