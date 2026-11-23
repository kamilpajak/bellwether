//! The twelve digests, each a field list over the form in `lib.rs`. `docs/design/architecture.md`
//! section 2's rows state the order of `item_id`, `story_id`, `snapshot_row` and `snapshot_id`,
//! section 3.2's that of `filter_params` and `result_digest`, section 3.5's that of
//! `question_version`, `model_request` and `embed_request`; only the code below states `doc_id` and `attempt_id`.

use crate::{sha256_canon, CanonError, Digest, Domain, Field, Stamp};
use chrono::{DateTime, Utc};

/// `item_id`'s inputs, in the order architecture.md section 2's row states them.
#[derive(Debug, Clone, Copy)]
pub struct ItemFields<'a> {
    pub source: &'a str,
    pub source_item_id: &'a str,
    pub url: &'a str,
    pub valid: Stamp,
    pub title: &'a str,
    /// Absent until a body is extracted, and then the blob's digest.
    pub body_ref: Option<&'a str>,
}

/// The content witness: a re-stamped time or an edited title is a new `item_id`.
pub fn item_id(fields: &ItemFields) -> Result<Digest, CanonError> {
    sha256_canon(
        Domain::ItemId,
        &[
            Field::Text(fields.source),
            Field::Text(fields.source_item_id),
            Field::Text(fields.url),
            Field::Stamp(fields.valid),
            Field::Text(fields.title),
            fields.body_ref.map_or(Field::Absent, Field::Text),
        ],
    )
}

/// The within-source dedup key, over the one part of a row a re-stamp does not touch.
pub fn story_id(source: &str, source_item_id: &str) -> Result<Digest, CanonError> {
    sha256_canon(
        Domain::StoryId,
        &[Field::Text(source), Field::Text(source_item_id)],
    )
}

/// Cross-source identity, over a url canonicalised by a hashed rule outside this crate.
pub fn doc_id(canonical_url: &str) -> Result<Digest, CanonError> {
    sha256_canon(Domain::DocId, &[Field::Text(canonical_url)])
}

/// `ordinal` is the attempt's position within its run, and it is in the field list because a
/// retry of one request inside one run is a second attempt with the same other four values.
pub fn attempt_id(
    run_id: &str,
    source: &str,
    target_window: &str,
    ordinal: u32,
) -> Result<Digest, CanonError> {
    sha256_canon(
        Domain::AttemptId,
        &[
            Field::Text(run_id),
            Field::Text(source),
            Field::Text(target_window),
            Field::Text(&ordinal.to_string()),
        ],
    )
}

/// One parameter as sent to a vendor.
#[derive(Debug, Clone, Copy)]
pub struct Param<'a> {
    pub name: &'a str,
    pub value: ParamValue<'a>,
}

#[derive(Debug, Clone, Copy)]
pub enum ParamValue<'a> {
    Literal(&'a str),
    /// A credential by name. There is no variant carrying a credential's value.
    Credential(&'a str),
}

/// What the vendor was actually asked. Ordered by parameter name over raw UTF-8 bytes, so two
/// callers assembling the same request in different orders agree.
pub fn filter_params(params: &[Param]) -> Result<Digest, CanonError> {
    let mut ordered: Vec<&Param> = params.iter().collect();
    ordered.sort_by(|left, right| left.name.as_bytes().cmp(right.name.as_bytes()));
    let mut fields = Vec::with_capacity(ordered.len() * 2);
    for param in ordered {
        fields.push(Field::Text(param.name));
        fields.push(match param.value {
            ParamValue::Literal(value) => Field::Text(value),
            ParamValue::Credential(name) => Field::Credential(name),
        });
    }
    sha256_canon(Domain::FilterParams, &fields)
}

/// The returned id SET: ordered over raw UTF-8 bytes and deduplicated, so a repeated query's
/// drift is detectable and a reordered answer is not drift.
pub fn result_digest(ids: &[&str]) -> Result<Digest, CanonError> {
    let mut set = ids.to_vec();
    set.sort_unstable_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    set.dedup();
    sha256_canon(Domain::ResultDigest, &[Field::List(&set)])
}

/// One snapshot row's columns, in the order architecture.md section 2's `snapshot_row` row states
/// them: every column of `snapshot.data/1` but `seq` and the body text, which `body_blob` names.
#[derive(Debug, Clone, Copy)]
pub struct SnapshotRowFields<'a> {
    pub item_id: &'a str,
    pub story_id: &'a str,
    pub doc_id: Option<&'a str>,
    pub source: &'a str,
    pub provenance: &'a str,
    pub valid: Stamp,
    /// The calendar a `day` value closes in; `Stamp::Day` carries no zone.
    pub valid_tz: Option<&'a str>,
    pub observed_at: DateTime<Utc>,
    pub body_blob: Option<&'a str>,
}

/// architecture.md section 2 row `snapshot_row`.
pub fn snapshot_row(fields: &SnapshotRowFields) -> Result<Digest, CanonError> {
    sha256_canon(
        Domain::SnapshotRow,
        &[
            Field::Text(fields.item_id),
            Field::Text(fields.story_id),
            fields.doc_id.map_or(Field::Absent, Field::Text),
            Field::Text(fields.source),
            Field::Text(fields.provenance),
            Field::Stamp(fields.valid),
            fields.valid_tz.map_or(Field::Absent, Field::Text),
            Field::Stamp(Stamp::Instant(fields.observed_at)),
            fields.body_blob.map_or(Field::Absent, Field::Text),
        ],
    )
}

/// architecture.md section 2 row `snapshot_id`: the cutoff, then each row's [`snapshot_row`] hex
/// in the caller's ascending `seq` order, neither re-sorted nor deduplicated.
pub fn snapshot_id(
    cutoff: DateTime<Utc>,
    rows_in_seq_order: &[&str],
) -> Result<Digest, CanonError> {
    sha256_canon(
        Domain::SnapshotId,
        &[
            Field::Stamp(Stamp::Instant(cutoff)),
            Field::List(rows_in_seq_order),
        ],
    )
}

/// architecture.md section 2 row `extract_version`: the rules file's digest, then each extraction
/// source file's name and digest, ordered by name over raw UTF-8 bytes.
pub fn extract_version(
    rules_version: &str,
    sources: &[(&str, &str)],
) -> Result<Digest, CanonError> {
    let mut ordered = sources.to_vec();
    ordered.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    let mut fields = Vec::with_capacity(1 + ordered.len() * 2);
    fields.push(Field::Text(rules_version));
    for (name, digest) in ordered {
        fields.push(Field::Text(name));
        fields.push(Field::Text(digest));
    }
    sha256_canon(Domain::ExtractVersion, &fields)
}

/// One question of a question document, as `question_version` reads it.
#[derive(Debug, Clone, Copy)]
pub struct QuestionFields<'a> {
    pub id: &'a str,
    /// The answer primitive's name, as the document spells it.
    pub kind: &'a str,
    pub instructions: &'a str,
    /// Option ids and their descriptions, in the document's order and paired by position.
    pub option_ids: &'a [&'a str],
    pub option_descriptions: &'a [&'a str],
}

/// architecture.md section 3.5 `question_version`: the document's name, then each question in the
/// caller's order as id, type, instructions, option ids, option descriptions. Nothing is sorted:
/// a reordered question or option list is another version.
pub fn question_version(name: &str, questions: &[QuestionFields]) -> Result<Digest, CanonError> {
    let mut fields = Vec::with_capacity(1 + questions.len() * 5);
    fields.push(Field::Text(name));
    for question in questions {
        fields.push(Field::Text(question.id));
        fields.push(Field::Text(question.kind));
        fields.push(Field::Text(question.instructions));
        fields.push(Field::List(question.option_ids));
        fields.push(Field::List(question.option_descriptions));
    }
    sha256_canon(Domain::QuestionVersion, &fields)
}

/// `model_request`'s inputs, in the order architecture.md section 3.5's `idempotency_key` row
/// states them.
#[derive(Debug, Clone, Copy)]
pub struct ModelRequestFields<'a> {
    pub kind: &'a str,
    /// Names the rule that turns these fields into request bytes: truncation and JSON layout.
    pub request_format: &'a str,
    pub question_version: &'a str,
    pub state_digest: &'a str,
    pub model: &'a str,
    pub repeat_index: u32,
}

/// architecture.md section 3.5 `idempotency_key`.
pub fn model_request(fields: &ModelRequestFields) -> Result<Digest, CanonError> {
    sha256_canon(
        Domain::ModelRequest,
        &[
            Field::Text(fields.kind),
            Field::Text(fields.request_format),
            Field::Text(fields.question_version),
            Field::Text(fields.state_digest),
            Field::Text(fields.model),
            Field::Text(&fields.repeat_index.to_string()),
        ],
    )
}

/// `embed_request`'s inputs, in the order architecture.md section 3.5's "The `embed` kind" states
/// them. No story field: two stories with one cut text share one vector.
#[derive(Debug, Clone, Copy)]
pub struct EmbedRequestFields<'a> {
    pub kind: &'a str,
    /// Names the rule that turns a body into request bytes: the cut, the layout, the encoding.
    pub embed_format: &'a str,
    pub model: &'a str,
    pub provider: &'a str,
    pub quantization: &'a str,
    /// Bumped by the operator when the provider's serving is seen to change.
    pub epoch: u32,
    pub input_digest: &'a str,
    pub repeat_index: u32,
}

/// architecture.md section 3.5, "The `embed` kind", `idempotency_key`.
pub fn embed_request(fields: &EmbedRequestFields) -> Result<Digest, CanonError> {
    sha256_canon(
        Domain::EmbedRequest,
        &[
            Field::Text(fields.kind),
            Field::Text(fields.embed_format),
            Field::Text(fields.model),
            Field::Text(fields.provider),
            Field::Text(fields.quantization),
            Field::Text(&fields.epoch.to_string()),
            Field::Text(fields.input_digest),
            Field::Text(&fields.repeat_index.to_string()),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::{
        attempt_id, doc_id, embed_request, extract_version, filter_params, item_id, model_request,
        question_version, result_digest, snapshot_id, snapshot_row, story_id, EmbedRequestFields,
        ItemFields, ModelRequestFields, Param, ParamValue, QuestionFields, SnapshotRowFields,
    };
    use crate::{CanonError, Digest, Stamp};
    use chrono::{DateTime, NaiveDate, TimeDelta, Utc};
    use serde_json::Value;
    use std::fs;
    use std::path::Path;

    const SOURCE: &str = "edgar_press";
    const SOURCE_ITEM_ID: &str = "0000320193-26-000071";
    const URL: &str = "https://www.sec.gov/Archives/edgar/data/320193/000032019326000071/ex991.htm";
    const BODY_REF: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const TITLE: &str = "Sch\u{e4}fer Holdings reports";
    const DECOMPOSED_TITLE: &str = "Scha\u{308}fer Holdings reports";
    const OTHER_TITLE: &str = "Schafer Holdings reports";
    /// Epoch microseconds of 2026-07-01T00:00:00Z, derived 2026-11-12 by subtracting one day from
    /// chrono's `timestamp_micros()` for 2026-07-02T00:00:00Z (1,782,950,400,000,000).
    const MIDNIGHT_MICROS: i64 = 1_782_864_000_000_000;
    /// Every file in `vectors/` is read by the loop below; the count is here so that deleting one
    /// is a failure rather than a smaller pass.
    const VECTOR_COUNT: usize = 26;

    fn at(micros: i64) -> DateTime<Utc> {
        DateTime::UNIX_EPOCH + TimeDelta::microseconds(micros)
    }

    fn day() -> NaiveDate {
        at(MIDNIGHT_MICROS).date_naive()
    }

    fn item(
        title: &'static str,
        valid: Stamp,
        body_ref: Option<&'static str>,
    ) -> ItemFields<'static> {
        ItemFields {
            source: SOURCE,
            source_item_id: SOURCE_ITEM_ID,
            url: URL,
            valid,
            title,
            body_ref,
        }
    }

    fn hex(result: Result<Digest, CanonError>) -> String {
        match result {
            Ok(digest) => digest.hex().to_string(),
            Err(error) => format!("refused: {error}"),
        }
    }

    #[test]
    fn an_absent_optional_field_contributes_an_empty_string_and_not_a_missing_one() {
        let absent = item(TITLE, Stamp::Instant(at(MIDNIGHT_MICROS)), None);
        let empty = item(TITLE, Stamp::Instant(at(MIDNIGHT_MICROS)), Some(""));
        assert_eq!(hex(item_id(&absent)), hex(item_id(&empty)));
    }

    #[test]
    fn an_nfc_decomposed_title_digests_equal_to_its_composed_form() {
        let decomposed = item(
            DECOMPOSED_TITLE,
            Stamp::Instant(at(MIDNIGHT_MICROS)),
            Some(BODY_REF),
        );
        let composed = item(TITLE, Stamp::Instant(at(MIDNIGHT_MICROS)), Some(BODY_REF));
        assert_eq!(hex(item_id(&decomposed)), hex(item_id(&composed)));
    }

    #[test]
    fn a_story_id_is_unchanged_while_an_item_id_moves_when_the_title_changes() {
        let before = item(TITLE, Stamp::Instant(at(MIDNIGHT_MICROS)), Some(BODY_REF));
        let after = item(
            OTHER_TITLE,
            Stamp::Instant(at(MIDNIGHT_MICROS)),
            Some(BODY_REF),
        );
        assert_ne!(hex(item_id(&before)), hex(item_id(&after)));
        assert_eq!(
            hex(story_id(before.source, before.source_item_id)),
            hex(story_id(after.source, after.source_item_id))
        );
    }

    #[test]
    fn a_day_precision_value_and_an_instant_at_the_same_microsecond_do_not_collide() {
        let dated = item(TITLE, Stamp::Day(day()), Some(BODY_REF));
        let stamped = item(TITLE, Stamp::Instant(at(MIDNIGHT_MICROS)), Some(BODY_REF));
        assert_ne!(hex(item_id(&dated)), hex(item_id(&stamped)));
    }

    #[test]
    fn a_minute_and_the_instant_at_its_first_microsecond_digest_differently() {
        let minute = item(TITLE, Stamp::Minute(at(MIDNIGHT_MICROS)), Some(BODY_REF));
        let stamped = item(TITLE, Stamp::Instant(at(MIDNIGHT_MICROS)), Some(BODY_REF));
        assert_ne!(hex(item_id(&minute)), hex(item_id(&stamped)));
    }

    /// Before the form named its digest, these two calls produced one sha256: both rendered as
    /// the same two text fields. Verified by mutation 2026-11-13 - dropping the domain field
    /// turns this assertion red.
    #[test]
    fn two_digests_over_the_same_two_values_differ_because_the_form_names_its_digest() {
        let as_story = story_id(SOURCE, SOURCE_ITEM_ID);
        let as_param = filter_params(&[Param {
            name: SOURCE,
            value: ParamValue::Literal(SOURCE_ITEM_ID),
        }]);
        assert_ne!(hex(as_story), hex(as_param));
    }

    #[test]
    fn a_value_holding_a_field_separator_is_refused_rather_than_forging_a_boundary() {
        let forged = doc_id("https://example.test/a\u{1f}b");
        assert_eq!(
            hex(forged),
            "refused: a value holding a canonical-form separator"
        );
    }

    #[test]
    fn a_list_member_holding_a_separator_is_refused_too() {
        let forged = result_digest(&["a\u{1e}b"]);
        assert_eq!(
            hex(forged),
            "refused: a value holding a canonical-form separator"
        );
    }

    /// The list arm carries its own copy of the NFC rule, and the title test above does not reach
    /// it: mutation 2026-11-13 removed NFC from the list arm with nothing red.
    #[test]
    fn an_nfc_decomposed_list_member_digests_equal_to_its_composed_form() {
        assert_eq!(
            hex(result_digest(&[DECOMPOSED_TITLE])),
            hex(result_digest(&[TITLE]))
        );
    }

    /// Same hole in the tagged arm, which is the one a credential NAME goes through.
    #[test]
    fn an_nfc_decomposed_credential_name_digests_equal_to_its_composed_form() {
        let decomposed = filter_params(&[Param {
            name: "key",
            value: ParamValue::Credential(DECOMPOSED_TITLE),
        }]);
        let composed = filter_params(&[Param {
            name: "key",
            value: ParamValue::Credential(TITLE),
        }]);
        assert_eq!(hex(decomposed), hex(composed));
    }

    fn text<'a>(value: &'a Value, key: &str) -> &'a str {
        value[key].as_str().unwrap_or_default()
    }

    fn strings<'a>(value: &'a Value, key: &str) -> Vec<&'a str> {
        value[key]
            .as_array()
            .map(|members| members.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default()
    }

    /// Exactly one of the three keys, or the vector is unreadable.
    fn stamp(value: &Value) -> Option<Stamp> {
        let keys = ["instant", "minute", "day"].map(|key| value[key].as_str());
        match keys {
            [Some(instant), None, None] => instant.parse().ok().map(Stamp::Instant),
            [None, Some(minute), None] => minute.parse().ok().map(Stamp::Minute),
            [None, None, Some(date)] => date.parse().ok().map(Stamp::Day),
            _ => None,
        }
    }

    fn param(value: &Value) -> Option<Param<'_>> {
        let name = value["name"].as_str()?;
        let value = match (value["literal"].as_str(), value["credential"].as_str()) {
            (Some(literal), None) => ParamValue::Literal(literal),
            (None, Some(credential)) => ParamValue::Credential(credential),
            _ => return None,
        };
        Some(Param { name, value })
    }

    /// The vector's own expectation, in the same string space as [`evaluate`].
    fn expectation(vector: &Value) -> String {
        match (vector["sha256"].as_str(), vector["refused"].as_str()) {
            (Some(digest), None) => digest.to_string(),
            (None, Some(reason)) => format!("refused: {reason}"),
            _ => "unreadable: a vector states neither sha256 nor refused".to_string(),
        }
    }

    fn evaluate(vector: &Value) -> String {
        let input = &vector["input"];
        let computed = match text(vector, "digest") {
            "item_id" => {
                let Some(valid) = stamp(&input["valid"]) else {
                    return "unreadable: valid".to_string();
                };
                item_id(&ItemFields {
                    source: text(input, "source"),
                    source_item_id: text(input, "source_item_id"),
                    url: text(input, "url"),
                    valid,
                    title: text(input, "title"),
                    body_ref: input["body_ref"].as_str(),
                })
            }
            "story_id" => story_id(text(input, "source"), text(input, "source_item_id")),
            "doc_id" => doc_id(text(input, "canonical_url")),
            "attempt_id" => attempt_id(
                text(input, "run_id"),
                text(input, "source"),
                text(input, "target_window"),
                u32::try_from(input["ordinal"].as_u64().unwrap_or_default()).unwrap_or_default(),
            ),
            "filter_params" => {
                let Some(params) = input["params"]
                    .as_array()
                    .and_then(|members| members.iter().map(param).collect::<Option<Vec<_>>>())
                else {
                    return "unreadable: params".to_string();
                };
                filter_params(&params)
            }
            "result_digest" => result_digest(&strings(input, "ids")),
            "snapshot_id" => {
                let Ok(cutoff) = text(input, "cutoff").parse::<DateTime<Utc>>() else {
                    return "unreadable: cutoff".to_string();
                };
                snapshot_id(cutoff, &strings(input, "rows"))
            }
            "snapshot_row" => {
                let (Some(valid), Ok(observed_at)) = (
                    stamp(&input["valid"]),
                    text(input, "observed_at").parse::<DateTime<Utc>>(),
                ) else {
                    return "unreadable: valid or observed_at".to_string();
                };
                snapshot_row(&SnapshotRowFields {
                    item_id: text(input, "item_id"),
                    story_id: text(input, "story_id"),
                    doc_id: input["doc_id"].as_str(),
                    source: text(input, "source"),
                    provenance: text(input, "provenance"),
                    valid,
                    valid_tz: input["valid_tz"].as_str(),
                    observed_at,
                    body_blob: input["body_blob"].as_str(),
                })
            }
            "extract_version" => {
                let Some(sources) = input["sources"].as_array().and_then(|members| {
                    members
                        .iter()
                        .map(|member| Some((member["name"].as_str()?, member["sha256"].as_str()?)))
                        .collect::<Option<Vec<_>>>()
                }) else {
                    return "unreadable: sources".to_string();
                };
                extract_version(text(input, "rules_version"), &sources)
            }
            "question_version" => {
                let Some(members) = input["questions"].as_array() else {
                    return "unreadable: questions".to_string();
                };
                let options: Vec<(Vec<&str>, Vec<&str>)> = members
                    .iter()
                    .map(|member| {
                        let listed = member["options"].as_array();
                        let pick = |key: &str| -> Vec<&str> {
                            listed
                                .into_iter()
                                .flatten()
                                .filter_map(|option| option[key].as_str())
                                .collect()
                        };
                        (pick("id"), pick("description"))
                    })
                    .collect();
                let questions: Vec<QuestionFields> = members
                    .iter()
                    .zip(&options)
                    .map(|(member, (ids, descriptions))| QuestionFields {
                        id: text(member, "id"),
                        kind: text(member, "type"),
                        instructions: text(member, "instructions"),
                        option_ids: ids,
                        option_descriptions: descriptions,
                    })
                    .collect();
                question_version(text(input, "name"), &questions)
            }
            "model_request" => model_request(&ModelRequestFields {
                kind: text(input, "kind"),
                request_format: text(input, "request_format"),
                question_version: text(input, "question_version"),
                state_digest: text(input, "state_digest"),
                model: text(input, "model"),
                repeat_index: u32::try_from(input["repeat_index"].as_u64().unwrap_or_default())
                    .unwrap_or_default(),
            }),
            "embed_request" => {
                let number = |key: &str| {
                    u32::try_from(input[key].as_u64().unwrap_or_default()).unwrap_or_default()
                };
                embed_request(&EmbedRequestFields {
                    kind: text(input, "kind"),
                    embed_format: text(input, "embed_format"),
                    model: text(input, "model"),
                    provider: text(input, "provider"),
                    quantization: text(input, "quantization"),
                    epoch: number("epoch"),
                    input_digest: text(input, "input_digest"),
                    repeat_index: number("repeat_index"),
                })
            }
            other => return format!("unreadable: digest {other}"),
        };
        hex(computed)
    }

    #[test]
    fn every_committed_vector_reproduces_the_digest_it_states() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("vectors");
        let Ok(entries) = fs::read_dir(&directory) else {
            assert_eq!(directory.display().to_string(), "a readable directory");
            return;
        };
        let mut read = 0;
        let mut mismatched: Vec<String> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(body) = fs::read_to_string(&path) else {
                assert_eq!(path.display().to_string(), "a readable vector");
                continue;
            };
            let Ok(vector) = serde_json::from_str::<Value>(&body) else {
                assert_eq!(path.display().to_string(), "parseable JSON");
                continue;
            };
            let computed = evaluate(&vector);
            if computed != expectation(&vector) {
                mismatched.push(format!(
                    "{}: states {}, computes {computed}",
                    path.display(),
                    expectation(&vector)
                ));
            }
            read += 1;
        }
        // Every mismatch at once rather than the first: a count of failures hides which failure.
        assert_eq!(mismatched, Vec::<String>::new());
        assert_eq!(read, VECTOR_COUNT);
    }
}
