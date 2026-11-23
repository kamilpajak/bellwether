//! What this build reads out of a vendor answer. The stored bytes are the record; everything here
//! is a reading of them under a shape taken from the predecessor's client and not yet checked
//! against a live call (`docs/design/architecture.md` section 3.5, "Not verified").

use serde_json::Value;

use crate::cap::micros_of;
use crate::question::{QuestionDoc, QuestionKind};

/// How an attempt closed. `Abandoned` is written by the next run's sweep, never by a response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// 2xx, a JSON object, a non-empty `answers` object.
    Ok,
    /// 2xx, a JSON object, no answer in it and no `error` member.
    Empty,
    /// 2xx and not a JSON object. The bytes are kept.
    Unparseable,
    /// No 2xx (a transport failure or another status), or a 2xx object that carries an `error`
    /// member and no answer.
    Failed,
    Abandoned,
}

impl Outcome {
    pub const ALL: [Self; 5] = [
        Self::Ok,
        Self::Empty,
        Self::Unparseable,
        Self::Failed,
        Self::Abandoned,
    ];

    /// The outcome a `finalised` row names.
    #[must_use]
    pub fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|outcome| outcome.name() == name)
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Empty => "empty",
            Self::Unparseable => "unparseable",
            Self::Failed => "failed",
            Self::Abandoned => "abandoned",
        }
    }

    /// Whether the request needs no further send. `Empty` and `Unparseable` are final: paid once,
    /// bytes kept, reported, never asked again (section 8 row MG4). A failed or abandoned attempt
    /// is retried by a later run. The store's queries read this function, not a second list.
    #[must_use]
    pub const fn answered(self) -> bool {
        matches!(self, Self::Ok | Self::Empty | Self::Unparseable)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    pub outcome: Outcome,
    /// The vendor's `usage.cost`, rounded up. `None` when it sent none or stated zero: a call
    /// stated free would otherwise leave every cap unarmed, so it counts at its worst case.
    pub cost_micro_usd: Option<i64>,
    /// The vendor's `model` string, raw. Compared with the id sent at read time only.
    pub answering_model: Option<String>,
}

const SUCCESS: std::ops::Range<u16> = 200..300;

#[must_use]
pub fn read(status: u16, body: &[u8]) -> Reading {
    let parsed = serde_json::from_slice::<Value>(body)
        .ok()
        .filter(Value::is_object);
    let cost_micro_usd = parsed
        .as_ref()
        .and_then(|value| value["usage"]["cost"].as_f64())
        .and_then(micros_of)
        .filter(|micros| *micros > 0);
    let answering_model = parsed
        .as_ref()
        .and_then(|value| value["model"].as_str())
        .map(str::to_owned);
    let outcome = match &parsed {
        _ if !SUCCESS.contains(&status) => Outcome::Failed,
        None => Outcome::Unparseable,
        Some(value) => match value["answers"].as_object() {
            Some(answers) if !answers.is_empty() => Outcome::Ok,
            _ if !value["error"].is_null() => Outcome::Failed,
            _ => Outcome::Empty,
        },
    };
    Reading {
        outcome,
        cost_micro_usd,
        answering_model,
    }
}

/// Per asked question, whether the answer reads as the type asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Buckets {
    /// A noul with a probability in `noul`; a choice whose `choice` is one of the options.
    pub answered: u64,
    /// Present and not readable as the type asked: a refusal, another type, an unknown option.
    pub not_answered: u64,
    pub missing: u64,
}

#[must_use]
pub fn buckets(doc: &QuestionDoc, body: &[u8]) -> Buckets {
    let parsed: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let mut found = Buckets::default();
    for question in doc.questions() {
        let answer = &parsed["answers"][question.id.as_str()];
        let typed = answer["type"].as_str() == Some(question.kind.name());
        let readable = match question.kind {
            QuestionKind::Noul => answer["noul"]
                .as_f64()
                .is_some_and(|p| (0.0..=1.0).contains(&p)),
            QuestionKind::Choice => answer["choice"]
                .as_str()
                .is_some_and(|picked| question.options.iter().any(|o| o.id == picked)),
        };
        if answer.is_null() {
            found.missing += 1;
        } else if typed && readable {
            found.answered += 1;
        } else {
            found.not_answered += 1;
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::{buckets, read, Buckets, Outcome};
    use crate::question::tests::TWO_QUESTIONS;
    use crate::question::QuestionDoc;

    const ANSWERED: &str = r#"{"model":"example/decide-1-20260101","usage":{"cost":0.00012,"input_tokens":900},
      "answers":{"scope":{"type":"choice","choice":"issuer_only","probabilities":{"issuer_only":0.9,"whole_category":0.1}},
                 "states_fact":{"type":"noul","noul":0.8}}}"#;

    /// A failed call, an empty one and an unreadable one never arrive as one answer. Mutation
    /// 2026-11-23, the `error` arm of `read` removed: red (a 200 carrying an error reads `empty`).
    #[test]
    fn each_kind_of_answer_reads_as_its_own_outcome() {
        let outcomes = [
            read(200, ANSWERED.as_bytes()),
            read(200, br#"{"model":"m","answers":{}}"#),
            read(200, br#"{"model":"m"}"#),
            read(200, b"<html>gateway</html>"),
            read(200, b"[1]"),
            read(
                200,
                br#"{"error":{"code":429,"message":"Rate limit exceeded"}}"#,
            ),
            read(200, br#"{"error":"overloaded","answers":{}}"#),
            read(429, br#"{"error":"slow down"}"#),
            read(500, ANSWERED.as_bytes()),
        ]
        .map(|reading| reading.outcome);
        assert_eq!(
            outcomes,
            [
                Outcome::Ok,
                Outcome::Empty,
                Outcome::Empty,
                Outcome::Unparseable,
                Outcome::Unparseable,
                Outcome::Failed,
                Outcome::Failed,
                Outcome::Failed,
                Outcome::Failed,
            ]
        );
    }

    #[test]
    fn the_cost_and_the_answering_id_are_read_raw_and_absent_ones_stay_absent() {
        let full = read(200, ANSWERED.as_bytes());
        assert_eq!(full.cost_micro_usd, Some(120));
        assert_eq!(
            full.answering_model.as_deref(),
            Some("example/decide-1-20260101")
        );
        let bare = read(200, br#"{"answers":{"a":{}}}"#);
        assert_eq!((bare.cost_micro_usd, bare.answering_model), (None, None));
    }

    /// Mutation 2026-11-23, the `> 0` filter removed from `read`: red.
    #[test]
    fn a_stated_cost_of_zero_reads_as_no_cost_stated() {
        let free = read(200, br#"{"usage":{"cost":0.0},"answers":{"a":{}}}"#);
        assert_eq!(free.cost_micro_usd, None);
    }

    #[test]
    fn every_outcome_is_found_again_by_its_name_and_three_of_the_five_are_final() {
        for outcome in Outcome::ALL {
            assert_eq!(Outcome::named(outcome.name()), Some(outcome));
        }
        assert_eq!(Outcome::named("collapsed"), None);
        let last = Outcome::ALL.map(Outcome::answered);
        assert_eq!(last, [true, true, true, false, false]);
    }

    #[test]
    fn an_answer_counts_as_answered_only_when_it_reads_as_the_type_asked(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let doc = QuestionDoc::parse(TWO_QUESTIONS.as_bytes())?;
        let refused = ANSWERED.replace(r#""type":"noul","noul":0.8"#, r#""type":"refusal""#);
        let unknown_option = ANSWERED.replace(r#""choice":"issuer_only""#, r#""choice":"other""#);
        let one_missing = r#"{"answers":{"scope":{"type":"choice","choice":"whole_category"}}}"#;
        let count = |body: &str| buckets(&doc, body.as_bytes());
        let of = |answered, not_answered, missing| Buckets {
            answered,
            not_answered,
            missing,
        };
        assert_eq!(count(ANSWERED), of(2, 0, 0));
        assert_eq!(count(&refused), of(1, 1, 0));
        assert_eq!(count(&unknown_option), of(1, 1, 0));
        assert_eq!(count(one_missing), of(1, 0, 1));
        assert_eq!(count("not json"), of(0, 0, 2));
        Ok(())
    }
}
