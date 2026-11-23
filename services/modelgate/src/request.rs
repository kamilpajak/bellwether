//! One `decide` request: the exact bytes sent, and the digests that name them. The layout and the
//! truncation rule together are [`REQUEST_FORMAT`], which is inside the idempotency key
//! (`docs/design/architecture.md` section 3.5).

use canon::digest::{model_request, ModelRequestFields};
use canon::FileDigest;

use crate::model::{ModelId, MAX_MODEL_ID_BYTES};
use crate::question::{QuestionDoc, MAX_QUESTIONS_BYTES};

pub const KIND_DECIDE: &str = "decide";
pub const REQUEST_FORMAT: &str = "modelgate.request/2";
/// Bytes the state may take once JSON-escaped, quotes excluded. Independent of the question
/// document, so two documents read the same cut of one story.
pub const STATE_BUDGET_BYTES: usize = 13_000;
/// The whole body stays under this. Why 34,200, and what it no longer bounds: section 3.5,
/// `request_format` and "Not verified".
pub const REQUEST_CEILING_BYTES: usize = 34_200;
const MODEL_KEY: &str = "{\"model\":";
const STATE_KEY: &str = ",\"state\":";
const QUESTIONS_KEY: &str = ",\"questions\":";
const CLOSE: &str = "}";
const QUOTES: usize = 2;
const ENVELOPE_BYTES: usize =
    MODEL_KEY.len() + STATE_KEY.len() + QUESTIONS_KEY.len() + CLOSE.len() + 2 * QUOTES;

const _: () = assert!(
    ENVELOPE_BYTES + MAX_MODEL_ID_BYTES + STATE_BUDGET_BYTES + MAX_QUESTIONS_BYTES
        <= REQUEST_CEILING_BYTES
);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecideRequest {
    /// The body as sent. No header is part of it, so a stored copy holds no credential.
    pub bytes: Vec<u8>,
    pub request_sha256: String,
    pub state_digest: String,
    /// Bytes of the text sent and of the whole body it was cut from.
    pub state_bytes: usize,
    pub body_bytes: usize,
    pub state_truncated: bool,
    pub repeat_index: u32,
    pub idempotency_key: String,
}

/// Bytes `serde_json` writes for one character inside a string literal.
const fn escaped_len(character: char) -> usize {
    match character {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
        other if (other as u32) < 0x20 => 6,
        other => other.len_utf8(),
    }
}

/// The longest prefix of `body`, cut at a character boundary, whose escaped form fits the budget.
fn cut(body: &str) -> &str {
    let mut spent = 0;
    for (start, character) in body.char_indices() {
        spent += escaped_len(character);
        if spent > STATE_BUDGET_BYTES {
            return &body[..start];
        }
    }
    body
}

/// The state's digest or the model id cannot enter a canonical form; no input here can cause it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the request has no idempotency key: {0}")]
pub struct RequestError(String);

pub fn build(
    doc: &QuestionDoc,
    model: &ModelId,
    body: &str,
    repeat_index: u32,
) -> Result<DecideRequest, RequestError> {
    let state = cut(body);
    let state_digest = FileDigest::of(state.as_bytes()).hex().to_owned();
    let mut text = String::with_capacity(REQUEST_CEILING_BYTES);
    text.push_str(MODEL_KEY);
    text.push_str(&serde_json::to_string(model.as_str()).unwrap_or_default());
    text.push_str(STATE_KEY);
    text.push_str(&serde_json::to_string(state).unwrap_or_default());
    text.push_str(QUESTIONS_KEY);
    text.push_str(doc.wire());
    text.push_str(CLOSE);
    let idempotency_key = model_request(&ModelRequestFields {
        kind: KIND_DECIDE,
        request_format: REQUEST_FORMAT,
        question_version: doc.version(),
        state_digest: &state_digest,
        model: model.as_str(),
        repeat_index,
    })
    .map_err(|why| RequestError(why.to_string()))?
    .hex()
    .to_owned();
    Ok(DecideRequest {
        request_sha256: FileDigest::of(text.as_bytes()).hex().to_owned(),
        bytes: text.into_bytes(),
        state_digest,
        state_bytes: state.len(),
        body_bytes: body.len(),
        state_truncated: state.len() < body.len(),
        repeat_index,
        idempotency_key,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        build, escaped_len, ENVELOPE_BYTES, REQUEST_CEILING_BYTES, REQUEST_FORMAT,
        STATE_BUDGET_BYTES,
    };
    use crate::model::{ModelId, MAX_MODEL_ID_BYTES};
    use crate::question::tests::{committed, TWO_QUESTIONS};
    use crate::question::{QuestionDoc, MAX_QUESTIONS_BYTES};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    const MODEL: &str = "example/decide-1-20260101";

    fn parts() -> Result<(QuestionDoc, ModelId), Box<dyn std::error::Error>> {
        Ok((
            QuestionDoc::parse(TWO_QUESTIONS.as_bytes())?,
            ModelId::parse(MODEL)?,
        ))
    }

    #[test]
    fn the_request_is_model_state_and_questions_in_that_order_and_parses_back() -> TestResult {
        let (doc, model) = parts()?;
        let request = build(&doc, &model, "A \"quoted\" line.\nSecond line.", 0)?;
        let text = String::from_utf8(request.bytes.clone())?;
        let expected = format!(
            "{{\"model\":\"{MODEL}\",\"state\":\"A \\\"quoted\\\" line.\\nSecond line.\",\"questions\":{}}}",
            doc.wire()
        );
        assert_eq!(text, expected);
        let parsed: serde_json::Value = serde_json::from_str(&text)?;
        assert_eq!(parsed["state"], "A \"quoted\" line.\nSecond line.");
        assert!(!request.state_truncated);
        Ok(())
    }

    /// The length rule must be `serde_json`'s own, or a body of escapes overruns the ceiling.
    #[test]
    fn the_escaped_length_of_every_low_character_is_what_serde_json_writes() -> TestResult {
        let samples = (0_u32..0x80)
            .filter_map(char::from_u32)
            .chain(['é', '€', '\u{1f600}']);
        for character in samples {
            let literal = serde_json::to_string(&character.to_string())?;
            assert_eq!(literal.len() - 2, escaped_len(character), "{character:?}");
        }
        Ok(())
    }

    /// Mutation 2026-11-23, the budget compared against raw instead of escaped length: red (a
    /// body of newlines is 26,000 escaped bytes).
    #[test]
    fn a_long_body_is_cut_at_a_character_boundary_inside_the_state_budget() -> TestResult {
        let (doc, model) = parts()?;
        for body in [
            "é".repeat(STATE_BUDGET_BYTES),
            "\n".repeat(STATE_BUDGET_BYTES),
            "\u{1}".repeat(STATE_BUDGET_BYTES),
        ] {
            let request = build(&doc, &model, &body, 0)?;
            let parsed: serde_json::Value = serde_json::from_slice(&request.bytes)?;
            let state = parsed["state"].as_str().unwrap_or_default();
            let escaped = serde_json::to_string(state)?.len() - 2;
            assert!(request.state_truncated);
            assert!(body.starts_with(state));
            assert!(escaped <= STATE_BUDGET_BYTES, "{escaped}");
            assert!(escaped + 6 > STATE_BUDGET_BYTES, "{escaped}");
            assert!(request.bytes.len() <= REQUEST_CEILING_BYTES);
            assert_eq!(request.state_bytes, state.len());
            assert_eq!(request.body_bytes, body.len());
        }
        Ok(())
    }

    /// The key names the layout and the three budgets as one token, so a budget moves only with
    /// the token and with section 3.5. Mutations 2026-11-23: see section 8 row MG8.
    #[test]
    fn the_format_token_and_its_three_budgets_are_the_ones_the_design_names() {
        assert_eq!(
            (
                REQUEST_FORMAT,
                STATE_BUDGET_BYTES,
                MAX_QUESTIONS_BYTES,
                REQUEST_CEILING_BYTES
            ),
            ("modelgate.request/2", 13_000, 21_000, 34_200)
        );
        let design = include_str!("../../../docs/design/architecture.md");
        for named in [
            "`modelgate.request/2` (2026-11-23)",
            "13,000 JSON-escaped bytes",
            "21,000-byte bound",
            "34,200-byte ceiling",
        ] {
            assert!(design.contains(named), "{named}");
        }
    }

    /// The budgets against the committed document: the longest id, a body over the state budget
    /// and all its questions stay under the ceiling, and the state is not shortened for them.
    /// Mutations 2026-11-23: see section 8 row MG8.
    #[test]
    fn the_committed_document_and_a_full_state_fit_the_ceiling_with_the_state_uncut() -> TestResult
    {
        let doc = committed()?;
        let id = format!("{}-20260101", "m".repeat(MAX_MODEL_ID_BYTES - 9));
        let body = "x".repeat(STATE_BUDGET_BYTES + 1);
        let request = build(&doc, &ModelId::parse(&id)?, &body, 0)?;
        assert_eq!(request.state_bytes, STATE_BUDGET_BYTES);
        assert!(doc.wire().len() <= MAX_QUESTIONS_BYTES);
        assert!(request.bytes.len() <= REQUEST_CEILING_BYTES);
        assert_eq!(
            request.bytes.len(),
            ENVELOPE_BYTES + MAX_MODEL_ID_BYTES + STATE_BUDGET_BYTES + doc.wire().len()
        );
        Ok(())
    }

    /// The cut does not depend on the document: a longer document reads the same state.
    #[test]
    fn two_documents_of_different_length_cut_one_body_at_the_same_point() -> TestResult {
        let (doc, model) = parts()?;
        let longer = TWO_QUESTIONS.replace("It does.", &"It does. ".repeat(200));
        let longer = QuestionDoc::parse(longer.as_bytes())?;
        let body = "word ".repeat(STATE_BUDGET_BYTES);
        let (first, second) = (
            build(&doc, &model, &body, 0)?,
            build(&longer, &model, &body, 0)?,
        );
        assert_eq!(first.state_digest, second.state_digest);
        assert_ne!(first.idempotency_key, second.idempotency_key);
        Ok(())
    }

    #[test]
    fn the_key_moves_with_the_state_the_build_and_the_repeat_index_and_not_with_a_story(
    ) -> TestResult {
        let (doc, model) = parts()?;
        let other_build = ModelId::parse("example/decide-1-20260102")?;
        let base = build(&doc, &model, "one body", 0)?;
        assert_eq!(base, build(&doc, &model, "one body", 0)?);
        let repeat = build(&doc, &model, "one body", 1)?;
        assert_eq!(base.bytes, repeat.bytes);
        assert_ne!(base.idempotency_key, repeat.idempotency_key);
        assert_ne!(
            base.idempotency_key,
            build(&doc, &model, "another body", 0)?.idempotency_key
        );
        assert_ne!(
            base.idempotency_key,
            build(&doc, &other_build, "one body", 0)?.idempotency_key
        );
        Ok(())
    }
}
