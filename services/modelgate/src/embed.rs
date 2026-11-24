//! One `embed` request and what is read out of its answer: a vector that represents a text the
//! caller supplied. The layout, the cut and the encoding together are [`EMBED_FORMAT`], which is
//! inside the idempotency key. What a vector may be used for: `docs/design/architecture.md`
//! section 3.5, "The `embed` kind".

use canon::digest::{embed_request, EmbedRequestFields};
use canon::FileDigest;
use serde_json::Value;

use crate::cap::micros_of;
use crate::model::MAX_MODEL_ID_BYTES;
use crate::request::{cut, RequestError};
use crate::response::{Outcome, Reading};

pub const KIND_EMBED: &str = "embed";
pub const EMBED_FORMAT: &str = "modelgate.embed/1";
/// The model's full output width. No `dimensions` member is sent, so nothing shortens it.
pub const DIMS: usize = 4096;
/// The vendor's quantization names (its API reference, read 2026-11-23).
pub const QUANTIZATIONS: [&str; 12] = [
    "int4", "int8", "fp4", "mxfp4", "nvfp4", "fp6", "fp8", "mxfp8", "fp16", "bf16", "fp32",
    "unknown",
];
const MAX_PROVIDER_BYTES: usize = 64;
const ALIAS_PREFIX: char = '~';
pub const ENCODING_BASE64: &str = "base64";
pub const ENCODING_ARRAY: &str = "array";
const BYTES_PER_VALUE: usize = 4;
const LANES: usize = 8;
const PPM: f64 = 1e6;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SpaceError {
    #[error("the embed model id is empty, an alias (leading `~`), longer than {MAX_MODEL_ID_BYTES} bytes or holds a character outside `[A-Za-z0-9._/-]`")]
    Model,
    #[error("the provider slug is empty, longer than {MAX_PROVIDER_BYTES} bytes or holds a character outside `[A-Za-z0-9._/-]`")]
    Provider,
    #[error("the quantization is not one of {QUANTIZATIONS:?}")]
    Quantization,
}

/// What two vectors must share before code may compare them: the model, the one provider endpoint
/// that served it, its precision, and the operator's epoch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbedSpace {
    model: String,
    provider: String,
    quantization: String,
    epoch: u32,
}

impl EmbedSpace {
    pub fn parse(
        model: &str,
        provider: &str,
        quantization: &str,
        epoch: u32,
    ) -> Result<Self, SpaceError> {
        let plain = |text: &str, most: usize| {
            !text.is_empty()
                && text.len() <= most
                && text
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._/-".contains(&byte))
        };
        if model.starts_with(ALIAS_PREFIX) || !plain(model, MAX_MODEL_ID_BYTES) {
            return Err(SpaceError::Model);
        }
        if !plain(provider, MAX_PROVIDER_BYTES) {
            return Err(SpaceError::Provider);
        }
        if !QUANTIZATIONS.contains(&quantization) {
            return Err(SpaceError::Quantization);
        }
        Ok(Self {
            model: model.to_owned(),
            provider: provider.to_owned(),
            quantization: quantization.to_owned(),
            epoch,
        })
    }

    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    #[must_use]
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// The text every stored row carries; two rows are of one space when it is equal.
    #[must_use]
    pub fn name(&self) -> String {
        format!(
            "{EMBED_FORMAT};model={};provider={};quantization={};epoch={}",
            self.model, self.provider, self.quantization, self.epoch
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbedRequest {
    /// The body as sent. No header is part of it, so a stored copy holds no credential.
    pub bytes: Vec<u8>,
    pub request_sha256: String,
    pub input_digest: String,
    pub input_bytes: usize,
    pub body_bytes: usize,
    pub input_truncated: bool,
    pub repeat_index: u32,
    pub idempotency_key: String,
}

/// The input is the `decide` state: the same cut of `body`, so `input_digest` is `state_digest`.
pub fn build(
    space: &EmbedSpace,
    body: &str,
    repeat_index: u32,
) -> Result<EmbedRequest, RequestError> {
    let input = cut(body);
    let input_digest = FileDigest::of(input.as_bytes()).hex().to_owned();
    let quoted = |text: &str| serde_json::to_string(text).unwrap_or_default();
    let provider = quoted(&space.provider);
    let text = format!(
        "{{\"model\":{},\"input\":{},\"encoding_format\":\"{ENCODING_BASE64}\",\"provider\":\
         {{\"order\":[{provider}],\"only\":[{provider}],\"allow_fallbacks\":false,\
         \"quantizations\":[{}]}}}}",
        quoted(&space.model),
        quoted(input),
        quoted(&space.quantization),
    );
    let idempotency_key = embed_request(&EmbedRequestFields {
        kind: KIND_EMBED,
        embed_format: EMBED_FORMAT,
        model: &space.model,
        provider: &space.provider,
        quantization: &space.quantization,
        epoch: space.epoch,
        input_digest: &input_digest,
        repeat_index,
    })
    .map_err(|why| RequestError(why.to_string()))?
    .hex()
    .to_owned();
    Ok(EmbedRequest {
        request_sha256: FileDigest::of(text.as_bytes()).hex().to_owned(),
        bytes: text.into_bytes(),
        input_digest,
        input_bytes: input.len(),
        body_bytes: body.len(),
        input_truncated: input.len() < body.len(),
        repeat_index,
        idempotency_key,
    })
}

/// A vector of unit length and the length it arrived with.
#[derive(Debug, Clone, PartialEq)]
pub struct Embedding {
    pub unit: Vec<f32>,
    pub norm: f64,
    /// [`ENCODING_BASE64`] or [`ENCODING_ARRAY`]: which form the vendor answered in.
    pub encoding: &'static str,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EmbedReading {
    pub reading: Reading,
    /// The answer's `provider` string when it carries one; the vendor's reference names none.
    pub provider: Option<String>,
    /// `Some` exactly when the outcome is `ok`.
    pub embedding: Option<Embedding>,
}

const SUCCESS: std::ops::Range<u16> = 200..300;

/// Standard-alphabet base64, padded or not. `None` for any other character.
fn base64(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let (mut held, mut bits) = (0_u32, 0_u32);
    for byte in text.trim_end_matches('=').bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        held = (held << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((held >> bits) & 0xff).ok()?);
        }
    }
    Some(out)
}

/// The values of `data[0].embedding`: a base64 string of little-endian f32, or a number array.
fn values(embedding: &Value) -> Option<(Vec<f64>, &'static str)> {
    if let Some(text) = embedding.as_str() {
        let bytes = base64(text)?;
        let (chunks, rest) = bytes.as_chunks::<BYTES_PER_VALUE>();
        if !rest.is_empty() {
            return None;
        }
        let floats = chunks
            .iter()
            .map(|chunk| f64::from(f32::from_le_bytes(*chunk)))
            .collect();
        return Some((floats, ENCODING_BASE64));
    }
    let members = embedding.as_array()?;
    let floats: Option<Vec<f64>> = members.iter().map(Value::as_f64).collect();
    Some((floats?, ENCODING_ARRAY))
}

/// `ok` only for exactly [`DIMS`] finite values of non-zero length. One value that is not finite
/// makes the length not finite, so the one test below refuses both.
fn embedding(found: &Value) -> Option<Embedding> {
    let (floats, encoding) = values(found)?;
    if floats.len() != DIMS {
        return None;
    }
    let norm = floats.iter().map(|value| value * value).sum::<f64>().sqrt();
    if !norm.is_finite() || norm == 0.0 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    let unit = floats.iter().map(|value| (value / norm) as f32).collect();
    Some(Embedding {
        unit,
        norm,
        encoding,
    })
}

#[must_use]
pub fn read_embedding(status: u16, body: &[u8]) -> EmbedReading {
    let parsed = serde_json::from_slice::<Value>(body)
        .ok()
        .filter(Value::is_object);
    let text = |key: &str| {
        parsed
            .as_ref()
            .and_then(|value| value[key].as_str())
            .map(str::to_owned)
    };
    let cost_micro_usd = parsed
        .as_ref()
        .and_then(|value| value["usage"]["cost"].as_f64())
        .and_then(micros_of)
        .filter(|micros| *micros > 0);
    let mut found = None;
    let outcome = match &parsed {
        _ if !SUCCESS.contains(&status) => Outcome::Failed,
        None => Outcome::Unparseable,
        Some(value) => match &value["data"][0]["embedding"] {
            Value::Null if !value["error"].is_null() => Outcome::Failed,
            Value::Null => Outcome::Empty,
            carried => {
                found = embedding(carried);
                if found.is_some() {
                    Outcome::Ok
                } else {
                    Outcome::Unparseable
                }
            }
        },
    };
    EmbedReading {
        reading: Reading {
            outcome,
            cost_micro_usd,
            answering_model: text("model"),
        },
        provider: text("provider"),
        embedding: found,
    }
}

/// The dot product in [`LANES`] f32 accumulators, element `i` into lane `i mod 8`, the lanes then
/// summed in order. The order is part of `dedup.compare/1`: another order is another number.
#[must_use]
pub fn dot(left: &[f32], right: &[f32]) -> f32 {
    let mut lanes = [0.0_f32; LANES];
    let (left_chunks, left_rest) = left.as_chunks::<LANES>();
    let (right_chunks, right_rest) = right.as_chunks::<LANES>();
    let tail = left_rest
        .iter()
        .zip(right_rest)
        .map(|(a, b)| a * b)
        .sum::<f32>();
    for (a, b) in left_chunks.iter().zip(right_chunks) {
        for lane in 0..LANES {
            lanes[lane] += a[lane] * b[lane];
        }
    }
    lanes.iter().sum::<f32>() + tail
}

/// A similarity as integer parts per million, truncated toward zero.
#[must_use]
pub fn ppm(similarity: f32) -> i64 {
    #[allow(clippy::cast_possible_truncation)]
    let parts = (f64::from(similarity) * PPM).trunc() as i64;
    parts.clamp(-1_000_000, 1_000_000)
}

/// The stored form of a unit vector: its values as little-endian f32.
#[must_use]
pub fn to_blob(unit: &[f32]) -> Vec<u8> {
    unit.iter().flat_map(|value| value.to_le_bytes()).collect()
}

#[must_use]
pub fn from_blob(bytes: &[u8]) -> Option<Vec<f32>> {
    let (chunks, rest) = bytes.as_chunks::<BYTES_PER_VALUE>();
    (chunks.len() == DIMS && rest.is_empty()).then(|| {
        chunks
            .iter()
            .map(|chunk| f32::from_le_bytes(*chunk))
            .collect()
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use serde_json::json;

    use super::{
        base64, build, dot, from_blob, ppm, read_embedding, to_blob, EmbedSpace, SpaceError, DIMS,
        EMBED_FORMAT, ENCODING_ARRAY, ENCODING_BASE64,
    };
    use crate::request::{build as build_decide, STATE_BUDGET_BYTES};
    use crate::response::Outcome;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    pub(crate) const MODEL: &str = "example/embed-1";
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub(crate) fn space() -> Result<EmbedSpace, SpaceError> {
        EmbedSpace::parse(MODEL, "example-host", "bf16", 1)
    }

    /// A vector whose only non-zero values are at the two given positions.
    pub(crate) fn axis(first: usize, a: f32, second: usize, b: f32) -> Vec<f32> {
        let mut values = vec![0.0_f32; DIMS];
        values[first] = a;
        values[second] = b;
        values
    }

    pub(crate) fn encode(bytes: &[u8]) -> String {
        let mut text = String::new();
        for chunk in bytes.chunks(3) {
            let held = chunk
                .iter()
                .fold(0_u32, |held, byte| (held << 8) | u32::from(*byte))
                << (8 * (3 - chunk.len()));
            for position in 0..=chunk.len() {
                let index = (held >> (18 - 6 * position)) & 63;
                text.push(char::from(ALPHABET[index as usize]));
            }
            text.push_str(&"=".repeat(3 - chunk.len()));
        }
        text
    }

    /// An embeddings answer in the vendor's documented shape, base64 or a number array.
    pub(crate) fn answer(values: &[f32], as_base64: bool, cost: Option<f64>) -> String {
        let embedding = if as_base64 {
            json!(encode(&to_blob(values)))
        } else {
            json!(values)
        };
        let mut usage = json!({"prompt_tokens": 9, "total_tokens": 9});
        if let Some(cost) = cost {
            usage["cost"] = json!(cost);
        }
        json!({"object": "list", "model": MODEL, "usage": usage,
               "data": [{"object": "embedding", "index": 0, "embedding": embedding}]})
        .to_string()
    }

    /// Section 8 row MG19. Mutations 2026-11-23, each alone: `allow_fallbacks` sent `true`; the
    /// `only` member dropped; a `dimensions` member added: red each.
    #[test]
    fn the_request_pins_one_provider_with_fallbacks_off_and_cuts_the_text_as_decide_does(
    ) -> TestResult {
        let space = space()?;
        let request = build(&space, "A \"quoted\" line.\nSecond.", 0)?;
        assert_eq!(
            String::from_utf8(request.bytes.clone())?,
            "{\"model\":\"example/embed-1\",\"input\":\"A \\\"quoted\\\" line.\\nSecond.\",\
             \"encoding_format\":\"base64\",\"provider\":{\"order\":[\"example-host\"],\
             \"only\":[\"example-host\"],\"allow_fallbacks\":false,\"quantizations\":[\"bf16\"]}}"
        );
        let long = "é".repeat(STATE_BUDGET_BYTES);
        let (embed, decide) = (
            build(&space, &long, 0)?,
            build_decide(
                &crate::question::QuestionDoc::parse(
                    crate::question::tests::TWO_QUESTIONS.as_bytes(),
                )?,
                &crate::model::ModelId::parse("example/decide-1-20260101")?,
                &long,
                0,
            )?,
        );
        assert!(embed.input_truncated);
        assert_eq!(embed.input_digest, decide.state_digest);
        Ok(())
    }

    /// The token and what it names are literals here and in section 3.5, so one moves only with
    /// the other. Mutation 2026-11-24, the token renamed `modelgate.embed/2` in code alone: red.
    #[test]
    fn the_embed_format_token_and_its_width_are_the_ones_the_design_names() {
        assert_eq!((EMBED_FORMAT, DIMS), ("modelgate.embed/1", 4096));
        let design = include_str!("../../../docs/design/architecture.md");
        for named in ["`modelgate.embed/1` (2026-11-23)", "4,096 values"] {
            assert!(design.contains(named), "{named}");
        }
    }

    /// Section 8 row MG21: the key moves with every part of the space, and not with a story.
    /// Mutation 2026-11-23, `epoch` left out of the digest's fields: red.
    #[test]
    fn the_key_moves_with_the_text_and_each_part_of_the_space() -> TestResult {
        let base = build(&space()?, "one body", 0)?;
        assert_eq!(base, build(&space()?, "one body", 0)?);
        let others = [
            EmbedSpace::parse("example/embed-2", "example-host", "bf16", 1)?,
            EmbedSpace::parse(MODEL, "another-host", "bf16", 1)?,
            EmbedSpace::parse(MODEL, "example-host", "fp8", 1)?,
            EmbedSpace::parse(MODEL, "example-host", "bf16", 2)?,
        ];
        let mut keys = vec![base.idempotency_key.clone()];
        for other in &others {
            keys.push(build(other, "one body", 0)?.idempotency_key);
            assert_ne!(other.name(), space()?.name());
        }
        keys.push(build(&space()?, "another body", 0)?.idempotency_key);
        keys.push(build(&space()?, "one body", 1)?.idempotency_key);
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), 7);
        Ok(())
    }

    #[test]
    fn an_alias_an_unknown_quantization_and_a_malformed_slug_are_no_space() {
        let refused = [
            EmbedSpace::parse("~example/embed", "host", "bf16", 1),
            EmbedSpace::parse("", "host", "bf16", 1),
            EmbedSpace::parse(MODEL, "a host", "bf16", 1),
            EmbedSpace::parse(MODEL, "", "bf16", 1),
            EmbedSpace::parse(MODEL, "host", "bf17", 1),
        ]
        .map(Result::err);
        assert_eq!(
            refused,
            [
                Some(SpaceError::Model),
                Some(SpaceError::Model),
                Some(SpaceError::Provider),
                Some(SpaceError::Provider),
                Some(SpaceError::Quantization),
            ]
        );
    }

    /// Section 8 row MG20. Mutations, each alone: the width test removed; a zero vector accepted;
    /// the `error` arm removed (2026-11-23); the length's finite test removed; a ragged base64
    /// tail accepted; a member that is no number read as one (2026-11-24): red each.
    #[test]
    fn a_vector_is_ok_only_at_4096_finite_values_in_either_encoding() {
        let sound = axis(0, 3.0, 1, 4.0);
        let mut not_finite = sound.clone();
        not_finite[7] = f32::NAN;
        let short = &sound[..DIMS - 1];
        // One byte more than 4,096 values, and an array with one member that is no number.
        let ragged = [to_blob(&sound), vec![0]].concat();
        let ragged = json!({"data": [{"embedding": encode(&ragged)}]}).to_string();
        let mut mixed = json!({"data": [{"embedding": sound}]});
        mixed["data"][0]["embedding"][5] = json!("0.5");
        let mixed = mixed.to_string();
        let outcomes = [
            read_embedding(200, answer(&sound, true, Some(0.000_02)).as_bytes()),
            read_embedding(200, answer(&sound, false, None).as_bytes()),
            read_embedding(200, answer(short, true, None).as_bytes()),
            read_embedding(200, answer(short, false, None).as_bytes()),
            read_embedding(200, answer(&not_finite, true, None).as_bytes()),
            read_embedding(200, answer(&vec![0.0; DIMS], true, None).as_bytes()),
            read_embedding(200, br#"{"data":[{"embedding":"not*base64"}]}"#),
            read_embedding(200, br#"{"data":[]}"#),
            read_embedding(200, br#"{"error":{"code":503}}"#),
            read_embedding(200, b"<html>gateway</html>"),
            read_embedding(429, answer(&sound, true, None).as_bytes()),
            read_embedding(200, ragged.as_bytes()),
            read_embedding(200, mixed.as_bytes()),
        ];
        let read: Vec<(Outcome, bool)> = outcomes
            .iter()
            .map(|found| (found.reading.outcome, found.embedding.is_some()))
            .collect();
        assert_eq!(
            read,
            [
                (Outcome::Ok, true),
                (Outcome::Ok, true),
                (Outcome::Unparseable, false),
                (Outcome::Unparseable, false),
                (Outcome::Unparseable, false),
                (Outcome::Unparseable, false),
                (Outcome::Unparseable, false),
                (Outcome::Empty, false),
                (Outcome::Failed, false),
                (Outcome::Unparseable, false),
                (Outcome::Failed, false),
                (Outcome::Unparseable, false),
                (Outcome::Unparseable, false),
            ]
        );
        let (packed, listed) = (&outcomes[0], &outcomes[1]);
        let encodings = [packed, listed].map(|found| found.embedding.as_ref().map(|e| e.encoding));
        assert_eq!(encodings, [Some(ENCODING_BASE64), Some(ENCODING_ARRAY)]);
        assert_eq!(packed.embedding.as_ref().map(|e| e.norm), Some(5.0));
        assert_eq!(
            packed.embedding.as_ref().map(|e| (e.unit[0], e.unit[1])),
            Some((0.6, 0.8))
        );
        assert_eq!(packed.reading.cost_micro_usd, Some(20));
        assert_eq!(listed.reading.cost_micro_usd, None);
        assert_eq!(packed.reading.answering_model.as_deref(), Some(MODEL));
        assert_eq!(packed.provider, None);
    }

    #[test]
    fn base64_reads_every_padding_length_and_refuses_another_alphabet() {
        for bytes in [&b""[..], b"f", b"fo", b"foo", b"foob", b"\xff\x00\xfe"] {
            assert_eq!(base64(&encode(bytes)).as_deref(), Some(bytes));
        }
        assert_eq!(base64("Zm9v").as_deref(), Some(&b"foo"[..]));
        assert_eq!(base64("Zm9-"), None);
    }

    /// Vectors with known cosines. Mutation 2026-11-23, `ppm` rounding instead of truncating:
    /// red on 0.9999999.
    #[test]
    fn the_dot_of_two_unit_vectors_is_their_cosine_and_a_blob_reads_back() {
        let (a, b, c) = (
            axis(0, 0.6, 1, 0.8),
            axis(0, 0.8, 1, 0.6),
            axis(2, 1.0, 9, 0.0),
        );
        assert_eq!(ppm(dot(&a, &b)), 960_000);
        assert_eq!(ppm(dot(&a, &c)), 0);
        assert_eq!(ppm(dot(&c, &c)), 1_000_000);
        assert_eq!(ppm(0.999_999_9), 999_999);
        assert_eq!(dot(&[1.0; 11], &[2.0; 11]), 22.0);
        assert_eq!(from_blob(&to_blob(&a)), Some(a));
        assert_eq!(from_blob(&[0; 12]), None);
    }
}
