//! A question document: named questions, each a `noul` (yes/no) or a `choice` over described
//! options. No type carries a number or a date, which is the rule of
//! `docs/design/architecture.md` section 3.5 as a closed enum.

use canon::digest::{question_version, QuestionFields};
use canon::FileDigest;
use serde::Deserialize;

pub const QUESTIONS_SCHEMA: &str = "modelgate.questions/1";
/// Bytes the encoded `questions` object may reach. Fixed apart from the state budget
/// (`request.rs`), so a longer document never shortens the text the model reads. Why 22,600:
/// section 3.5, `request_format`.
pub const MAX_QUESTIONS_BYTES: usize = 22_600;
const NOUL_OPTIONS: [&str; 2] = ["true", "false"];
const MIN_CHOICE_OPTIONS: usize = 2;

/// The answer primitives. Closed: a third name in a file fails the parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionKind {
    Noul,
    Choice,
}

impl QuestionKind {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Noul => "noul",
            Self::Choice => "choice",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionOption {
    pub id: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Question {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: QuestionKind,
    pub instructions: String,
    pub options: Vec<QuestionOption>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    schema: String,
    name: String,
    questions: Vec<Question>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QuestionError {
    #[error("the question file is not a {QUESTIONS_SCHEMA} document: {0}")]
    Shape(String),
    #[error("schema is {0:?}; this build reads {QUESTIONS_SCHEMA:?} only")]
    Schema(String),
    #[error("the document asks nothing")]
    NoQuestions,
    #[error("`{0}` is not an id: lower-case letters, digits and `_` only")]
    Id(String),
    #[error("the id `{0}` is used twice in one list")]
    Duplicate(String),
    #[error("noul `{0}` must list exactly the options `true` and `false`, in that order")]
    NoulOptions(String),
    #[error("choice `{0}` lists fewer than {MIN_CHOICE_OPTIONS} options")]
    ChoiceOptions(String),
    #[error("`{0}` has an empty instruction or option description")]
    EmptyText(String),
    #[error("a text holds a canonical-form separator, so the document has no version")]
    Separator,
    #[error("the encoded questions are {0} bytes; the limit is {MAX_QUESTIONS_BYTES}")]
    TooLong(usize),
}

/// A parsed, versioned document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionDoc {
    name: String,
    questions: Vec<Question>,
    version: String,
    file_sha256: String,
    wire: String,
}

fn is_id(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

/// A JSON string literal. `serde_json` cannot fail on a `&str`; the empty fallback is never sent,
/// because `encode` output is re-parsed by `the_wire_form_is_json_with_the_documents_order`.
fn quoted(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_default()
}

fn check(question: &Question) -> Result<(), QuestionError> {
    if !is_id(&question.id) {
        return Err(QuestionError::Id(question.id.clone()));
    }
    let ids: Vec<&str> = question.options.iter().map(|opt| opt.id.as_str()).collect();
    match question.kind {
        QuestionKind::Noul if ids != NOUL_OPTIONS => {
            return Err(QuestionError::NoulOptions(question.id.clone()));
        }
        QuestionKind::Choice if ids.len() < MIN_CHOICE_OPTIONS => {
            return Err(QuestionError::ChoiceOptions(question.id.clone()));
        }
        _ => {}
    }
    for (position, id) in ids.iter().enumerate() {
        if !is_id(id) {
            return Err(QuestionError::Id((*id).to_owned()));
        }
        if ids[..position].contains(id) {
            return Err(QuestionError::Duplicate((*id).to_owned()));
        }
    }
    let texts = std::iter::once(&question.instructions)
        .chain(question.options.iter().map(|option| &option.description));
    if texts.into_iter().any(|text| text.trim().is_empty()) {
        return Err(QuestionError::EmptyText(question.id.clone()));
    }
    Ok(())
}

/// The vendor's `questions` object: per id, `type`, `instructions` and `criteria` (option id to
/// description). Written by hand because `serde_json` without `preserve_order` sorts an object's
/// keys (no manifest enables it, 2026-11-23), and the order is part of the question. The shape is
/// the predecessor client's, unverified against a live call (section 3.5, "Not verified").
fn encode(questions: &[Question]) -> String {
    let mut out = String::from("{");
    for (position, question) in questions.iter().enumerate() {
        if position > 0 {
            out.push(',');
        }
        out.push_str(&quoted(&question.id));
        out.push_str(":{\"type\":");
        out.push_str(&quoted(question.kind.name()));
        out.push_str(",\"instructions\":");
        out.push_str(&quoted(&question.instructions));
        out.push_str(",\"criteria\":{");
        for (index, option) in question.options.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push_str(&quoted(&option.id));
            out.push(':');
            out.push_str(&quoted(&option.description));
        }
        out.push_str("}}");
    }
    out.push('}');
    out
}

impl QuestionDoc {
    pub fn parse(bytes: &[u8]) -> Result<Self, QuestionError> {
        let file: File =
            serde_json::from_slice(bytes).map_err(|why| QuestionError::Shape(why.to_string()))?;
        if file.schema != QUESTIONS_SCHEMA {
            return Err(QuestionError::Schema(file.schema));
        }
        if file.questions.is_empty() {
            return Err(QuestionError::NoQuestions);
        }
        for (position, question) in file.questions.iter().enumerate() {
            check(question)?;
            if file.questions[..position]
                .iter()
                .any(|earlier| earlier.id == question.id)
            {
                return Err(QuestionError::Duplicate(question.id.clone()));
            }
        }
        let wire = encode(&file.questions);
        if wire.len() > MAX_QUESTIONS_BYTES {
            return Err(QuestionError::TooLong(wire.len()));
        }
        let options: Vec<(Vec<&str>, Vec<&str>)> = file
            .questions
            .iter()
            .map(|question| {
                (
                    question.options.iter().map(|o| o.id.as_str()).collect(),
                    question
                        .options
                        .iter()
                        .map(|o| o.description.as_str())
                        .collect(),
                )
            })
            .collect();
        let fields: Vec<QuestionFields> = file
            .questions
            .iter()
            .zip(&options)
            .map(|(question, (ids, descriptions))| QuestionFields {
                id: &question.id,
                kind: question.kind.name(),
                instructions: &question.instructions,
                option_ids: ids,
                option_descriptions: descriptions,
            })
            .collect();
        let version = question_version(&file.name, &fields)
            .map_err(|_| QuestionError::Separator)?
            .hex()
            .to_owned();
        Ok(Self {
            name: file.name,
            questions: file.questions,
            version,
            file_sha256: FileDigest::of(bytes).hex().to_owned(),
            wire,
        })
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn questions(&self) -> &[Question] {
        &self.questions
    }

    /// `question_version` (section 3.5).
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    #[must_use]
    pub fn file_sha256(&self) -> &str {
        &self.file_sha256
    }

    /// The encoded `questions` object, exactly as a request carries it.
    #[must_use]
    pub fn wire(&self) -> &str {
        &self.wire
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{QuestionDoc, QuestionError, MAX_QUESTIONS_BYTES};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    pub(crate) const TWO_QUESTIONS: &str = r#"{
      "schema": "modelgate.questions/1",
      "name": "test/0",
      "questions": [
        {"id": "scope", "type": "choice", "instructions": "Whom does the text concern?",
         "options": [{"id": "issuer_only", "description": "Only its publisher."},
                     {"id": "whole_category", "description": "A category of companies."}]},
        {"id": "states_fact", "type": "noul", "instructions": "Does the text state a fact?",
         "options": [{"id": "true", "description": "It does."},
                     {"id": "false", "description": "It does not."}]}
      ]
    }"#;

    fn error_of(text: &str) -> Option<QuestionError> {
        QuestionDoc::parse(text.as_bytes()).err()
    }

    /// Section 8 row MG8. Mutation 2026-11-23, a `Number` variant added to `QuestionKind` with the
    /// two match arms the compiler then demands (E0004 without them): red.
    #[test]
    fn a_question_of_an_unknown_type_does_not_parse() {
        for kind in ["number", "score", "date", "text"] {
            let text = TWO_QUESTIONS.replace("\"noul\"", &format!("\"{kind}\""));
            assert!(
                matches!(error_of(&text), Some(QuestionError::Shape(_))),
                "{kind}"
            );
        }
    }

    /// Section 8 row MG8. Mutation 2026-11-23, option ids sorted before the digest: red.
    #[test]
    fn a_reordered_option_list_or_question_list_is_another_question_version() -> TestResult {
        let base = QuestionDoc::parse(TWO_QUESTIONS.as_bytes())?;
        let options_swapped = TWO_QUESTIONS
            .replace("issuer_only", "#")
            .replace("whole_category", "issuer_only")
            .replace('#', "whole_category");
        let reworded = TWO_QUESTIONS.replace("Only its publisher.", "Only the publisher.");
        let other_options = QuestionDoc::parse(options_swapped.as_bytes())?;
        assert_ne!(base.version(), other_options.version());
        assert_ne!(
            base.version(),
            QuestionDoc::parse(reworded.as_bytes())?.version()
        );
        let respaced = TWO_QUESTIONS.replace('\n', " ");
        let same = QuestionDoc::parse(respaced.as_bytes())?;
        assert_eq!(base.version(), same.version());
        assert_ne!(base.file_sha256(), same.file_sha256());
        Ok(())
    }

    /// Exact bytes: questions and options in the document's order, not sorted by key. Mutation
    /// 2026-11-23, `encode` building a `serde_json::Map`: red (`states_fact` after `scope` holds,
    /// `issuer_only` before `whole_category` holds, `criteria` moves before `instructions`).
    #[test]
    fn the_wire_form_is_json_with_the_documents_order() -> TestResult {
        let reversed = TWO_QUESTIONS
            .replace("issuer_only", "zz_first")
            .replace("\"scope\"", "\"zz_scope\"");
        let doc = QuestionDoc::parse(reversed.as_bytes())?;
        assert_eq!(
            doc.wire(),
            concat!(
                r#"{"zz_scope":{"type":"choice","instructions":"Whom does the text concern?","#,
                r#""criteria":{"zz_first":"Only its publisher.","#,
                r#""whole_category":"A category of companies."}},"#,
                r#""states_fact":{"type":"noul","instructions":"Does the text state a fact?","#,
                r#""criteria":{"true":"It does.","false":"It does not."}}}"#
            )
        );
        let parsed: serde_json::Value = serde_json::from_str(doc.wire())?;
        assert_eq!(parsed["states_fact"]["criteria"]["false"], "It does not.");
        Ok(())
    }

    #[test]
    fn each_malformed_document_is_refused_with_its_own_reason() {
        let noul_options = TWO_QUESTIONS.replace("\"true\"", "\"yes\"");
        let duplicate = TWO_QUESTIONS.replace("states_fact", "scope");
        let bad_id = TWO_QUESTIONS.replace("states_fact", "States-Fact");
        let empty = TWO_QUESTIONS.replace("It does.", " ");
        let schema = TWO_QUESTIONS.replace("questions/1", "questions/2");
        let long = TWO_QUESTIONS.replace("It does.", &"x".repeat(MAX_QUESTIONS_BYTES));
        let separator = TWO_QUESTIONS.replace("It does.", "It\\u001fdoes.");
        let extra = TWO_QUESTIONS.replace("\"name\"", "\"threshold\": 0.5, \"name\"");
        // Mutations 2026-11-23, each red: `deny_unknown_fields` removed from `Question`; from
        // `QuestionOption`.
        let in_question = TWO_QUESTIONS.replace("\"type\"", "\"threshold\": 0.5, \"type\"");
        let in_option =
            TWO_QUESTIONS.replace("\"description\"", "\"threshold\": 0.5, \"description\"");
        assert_eq!(
            error_of(&noul_options),
            Some(QuestionError::NoulOptions("states_fact".to_owned()))
        );
        assert_eq!(
            error_of(&duplicate),
            Some(QuestionError::Duplicate("scope".to_owned()))
        );
        assert_eq!(
            error_of(&bad_id),
            Some(QuestionError::Id("States-Fact".to_owned()))
        );
        assert_eq!(
            error_of(&empty),
            Some(QuestionError::EmptyText("states_fact".to_owned()))
        );
        assert!(matches!(error_of(&schema), Some(QuestionError::Schema(_))));
        assert!(matches!(error_of(&long), Some(QuestionError::TooLong(_))));
        assert_eq!(error_of(&separator), Some(QuestionError::Separator));
        for unknown_field in [&extra, &in_question, &in_option] {
            assert!(
                matches!(error_of(unknown_field), Some(QuestionError::Shape(_))),
                "{unknown_field}"
            );
        }
    }

    const COMMITTED: &[u8] = include_bytes!("../questions/ripple-shadow-1.json");
    const DESIGN: &str = include_str!("../../../docs/design/architecture.md");
    const LEVELS: [&str; 4] = ["none", "minor", "major", "critical"];

    pub(crate) fn committed() -> Result<QuestionDoc, QuestionError> {
        QuestionDoc::parse(COMMITTED)
    }

    /// The synthetic stories of `tests/fixtures`, each with the reading its author expects: per
    /// question id, the options that would not be a surprise. Hand-written, measured by nothing.
    pub(crate) fn examples() -> Result<Vec<serde_json::Value>, serde_json::Error> {
        let file: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/ripple-shadow-1.examples.json"
        ))?;
        Ok(file["examples"].as_array().cloned().unwrap_or_default())
    }

    fn option_ids<'a>(doc: &'a QuestionDoc, id: &str) -> Vec<&'a str> {
        let asked = doc.questions().iter().find(|question| question.id == id);
        asked.map_or_else(Vec::new, |question| {
            question.options.iter().map(|o| o.id.as_str()).collect()
        })
    }

    /// The committed instrument parses, and section 3.5 names its version and its encoded size;
    /// an edit to the file without an edit there is red.
    #[test]
    fn the_committed_ripple_shadow_document_parses_and_has_the_version_the_design_names(
    ) -> TestResult {
        let doc = committed()?;
        let bytes = doc.wire().len();
        let size = format!("{},{:03} encoded bytes", bytes / 1_000, bytes % 1_000);
        assert_eq!(doc.name(), "ripple-shadow/1");
        assert!(DESIGN.contains(doc.version()), "{}", doc.version());
        assert!(DESIGN.contains(&size), "{size}");
        Ok(())
    }

    /// Every instruction and option description of the questions `id` names, as one text.
    fn texts(doc: &QuestionDoc, id: &str) -> String {
        let asked = doc.questions().iter().filter(|question| question.id == id);
        asked
            .flat_map(|question| {
                let options = question.options.iter().map(|o| o.description.as_str());
                std::iter::once(question.instructions.as_str()).chain(options)
            })
            .collect::<Vec<&str>>()
            .join(" ")
    }

    /// Section 8 row MG13, where the mutations of 2026-11-23 are listed: significance is asked
    /// twice on one scale of sizes, no level is a kind of notice, and one phrase names the company.
    #[test]
    fn significance_is_two_questions_on_one_ordered_scale_and_none_is_called_materiality(
    ) -> TestResult {
        let doc = committed()?;
        let with_off_scale = [&LEVELS[..], &["cannot_tell"]].concat();
        assert_eq!(option_ids(&doc, "issuer_significance"), with_off_scale);
        assert_eq!(option_ids(&doc, "category_significance"), LEVELS);
        assert_eq!(
            option_ids(&doc, "cx_magnitude_size"),
            ["not_stated", "no_base", "small", "large"]
        );
        assert!(doc
            .questions()
            .iter()
            .all(|q| !q.id.contains("material") || q.id == "cx_issuer_calls_it_material"));
        assert!(texts(&doc, "content_kind").contains("release is a new event, however ordinary"));
        let (issuer, category) = (
            texts(&doc, "issuer_significance"),
            texts(&doc, "category_significance"),
        );
        assert!(issuer.contains("that company alone"));
        assert!(category.contains("whatever the event means for that company"));
        for scale in [&issuer, &category] {
            assert!(scale.contains("for better or worse"), "{scale}");
            for kind_of_notice in ["routine", "promotional", "ordinary for", "leadership"] {
                assert!(!scale.contains(kind_of_notice), "{kind_of_notice}: {scale}");
            }
        }
        let size = texts(&doc, "cx_magnitude_size");
        assert!(
            size.contains("a tenth") && !size.contains("calls it"),
            "{size}"
        );
        for question in doc.questions() {
            let text = texts(&doc, &question.id);
            for other_name in [
                "main company",
                "its subject",
                "The subject",
                "subject company",
                "mainly about",
            ] {
                assert!(!text.contains(other_name), "{}: {other_name}", question.id);
            }
        }
        Ok(())
    }

    fn expected<'a>(
        examples: &'a [serde_json::Value],
        example: &str,
        question: &str,
    ) -> &'a serde_json::Value {
        let found = examples.iter().find(|e| e["id"] == example);
        found.map_or(&serde_json::Value::Null, |e| &e["expect"][question])
    }

    /// Every expected reading is inside the committed answer spaces, and the contrasts the owner
    /// named on 2026-11-23 are held as literals. Mutations: section 8 row MG13.
    #[test]
    fn every_expected_reading_of_a_synthetic_example_is_an_option_of_the_committed_document(
    ) -> TestResult {
        let (doc, examples) = (committed()?, examples()?);
        assert_eq!(examples.len(), 17);
        for example in &examples {
            let expected = example["expect"].as_object().ok_or("no expect object")?;
            assert!(!expected.is_empty(), "{example}");
            for (id, wanted) in expected {
                let offered = option_ids(&doc, id);
                let wanted = wanted.as_array().ok_or("an expectation is not a list")?;
                assert!(!wanted.is_empty(), "{id}");
                for option in wanted {
                    let named = option.as_str().unwrap_or_default();
                    assert!(offered.contains(&named), "{id}: {named}");
                }
            }
        }
        let (low, high) = (&LEVELS[..2], &LEVELS[2..]);
        let (yes, no) = (&["true"][..], &["false"][..]);
        let models = ["validates", "lowers_cost_or_barrier"];
        let (forced, planned) = (
            "chief_executive_forced_out_under_charges",
            "chief_executive_planned_retirement",
        );
        let held: [(&str, &str, &[&str]); 22] = [
            ("open_models_for_a_category", "issuer_significance", low),
            ("open_models_for_a_category", "category_significance", high),
            ("open_models_for_a_category", "scope", &["whole_category"]),
            ("open_models_for_a_category", "stance_to_category", &models),
            (
                "open_models_in_the_words_of_a_release",
                "issuer_significance",
                low,
            ),
            (
                "open_models_in_the_words_of_a_release",
                "category_significance",
                high,
            ),
            (
                "open_models_in_the_words_of_a_release",
                "stance_to_category",
                &models,
            ),
            (
                "platform_enters_the_category",
                "category_significance",
                high,
            ),
            (
                "platform_enters_the_category",
                "stance_to_category",
                &["threatens_to_absorb"],
            ),
            (
                "investment_in_one_named_company",
                "category_significance",
                &["none"],
            ),
            (
                "investment_in_one_named_company",
                "as_matters_more_to_others",
                yes,
            ),
            (forced, "issuer_significance", high),
            (planned, "issuer_significance", low),
            (forced, "cx_forced_by_another_party", yes),
            (planned, "cx_forced_by_another_party", no),
            (forced, "cx_previously_announced", no),
            (planned, "cx_previously_announced", yes),
            (
                "outlook_lowered_by_a_fiftieth",
                "cx_magnitude_size",
                &["small"],
            ),
            (
                "outlook_lowered_by_two_fifths",
                "cx_magnitude_size",
                &["large"],
            ),
            (
                "offering_of_a_stated_amount",
                "cx_magnitude_size",
                &["no_base"],
            ),
            ("law_firm_alert", "cx_investigation_open", no),
            (
                "rule_for_a_whole_industry",
                "issuer_significance",
                &["cannot_tell"],
            ),
        ];
        for (example, question, reading) in held {
            assert_eq!(
                expected(&examples, example, question),
                &serde_json::json!(reading),
                "{example}: {question}"
            );
        }
        Ok(())
    }

    /// Section 3.5, "The channel to other companies": the sign code reads from each channel.
    const CHANNEL_SIGNS: [(&str, Option<&str>); 7] = [
        ("state_action", Some("adverse")),
        ("demand_rises", Some("favourable")),
        ("demand_falls", Some("adverse")),
        ("contagion", Some("adverse")),
        ("rival_weakened", Some("favourable")),
        ("none_of_these", None),
        ("cannot_tell", None),
    ];

    /// Section 8 row MG14, where the mutations of 2026-11-23 are listed: the channel is asked
    /// beside the stance, and an example's direction for the others is its channel's sign.
    #[test]
    fn each_channel_to_other_companies_fixes_their_sign_or_has_none() -> TestResult {
        let (doc, examples) = (committed()?, examples()?);
        let channels: Vec<&str> = CHANNEL_SIGNS.iter().map(|(channel, _)| *channel).collect();
        assert_eq!(option_ids(&doc, "spillover_channel"), channels);
        assert_eq!(
            option_ids(&doc, "spillover_effect"),
            ["favourable", "adverse", "neutral", "cannot_tell"]
        );
        assert_eq!(
            option_ids(&doc, "stance_to_category"),
            [
                "validates",
                "lowers_cost_or_barrier",
                "opens_market",
                "threatens_to_absorb",
                "substitutes",
                "undermines_category",
                "neutral",
                "cannot_tell"
            ]
        );
        let channel = texts(&doc, "spillover_channel");
        assert!(channel.contains("state_action first, then a demand option"));
        assert!(texts(&doc, "spillover_effect").contains("other than the company reported on"));
        let held = [
            ("export_ban_on_one_company", "state_action"),
            ("customer_signs_a_long_supply_contract", "demand_rises"),
            (
                "customer_lowers_its_forecast_and_its_orders",
                "demand_falls",
            ),
            ("bank_loses_a_quarter_of_its_deposits", "contagion"),
            ("chip_maker_delays_its_next_product", "rival_weakened"),
            ("share_repurchase_with_no_channel", "none_of_these"),
            ("platform_enters_the_category", "none_of_these"),
        ];
        for (example, reading) in held {
            assert_eq!(
                expected(&examples, example, "spillover_channel"),
                &serde_json::json!([reading]),
                "{example}"
            );
        }
        for (channel, sign) in CHANNEL_SIGNS {
            let reading = serde_json::json!([channel]);
            let mut shown = 0;
            for example in &examples {
                if example["expect"]["spillover_channel"] != reading {
                    continue;
                }
                shown += 1;
                let effect = &example["expect"]["spillover_effect"];
                assert!(effect.is_array(), "{example}");
                if let Some(sign) = sign {
                    assert_eq!(effect, &serde_json::json!([sign]), "{example}");
                }
            }
            assert!(shown > 0 || channel == "cannot_tell", "{channel}");
        }
        Ok(())
    }
}
