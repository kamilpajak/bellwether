//! A question document: named questions, each a `noul` (yes/no) or a `choice` over described
//! options. No type carries a number or a date, which is the rule of
//! `docs/design/architecture.md` section 3.5 as a closed enum.

use canon::digest::{question_version, QuestionFields};
use canon::FileDigest;
use serde::Deserialize;

pub const QUESTIONS_SCHEMA: &str = "modelgate.questions/1";
/// Bytes the encoded `questions` object may reach. Fixed apart from the state budget
/// (`request.rs`), so a longer document never shortens the text the model reads.
pub const MAX_QUESTIONS_BYTES: usize = 16_500;
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
        assert!(matches!(error_of(&extra), Some(QuestionError::Shape(_))));
    }

    /// The committed instrument parses, fits the budget, and its version is the one section 3.5
    /// names; an edit to the file without an edit there is red.
    #[test]
    fn the_committed_ripple_shadow_document_parses_and_has_the_version_the_design_names(
    ) -> TestResult {
        let bytes = include_bytes!("../questions/ripple-shadow-0.json");
        let doc = QuestionDoc::parse(bytes)?;
        let design = include_str!("../../../docs/design/architecture.md");
        assert_eq!(doc.name(), "ripple-shadow/0");
        assert!(design.contains(doc.version()), "{}", doc.version());
        Ok(())
    }
}
