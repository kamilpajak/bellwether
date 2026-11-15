//! A JSON reader that reports a key named twice in one object. `serde_json::Value` keeps the last
//! value: `{"status":"candidate","status":"admitted"}` read as admitted (review 2026-11-15).

use std::fmt;
use std::path::Path;

use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};

use crate::Refusal;

/// `bytes` as JSON, refusing a key any object names twice.
pub(crate) fn read(
    bytes: &[u8],
    path: &Path,
    unparseable: impl Fn(String) -> Refusal,
) -> Result<Value, Refusal> {
    let read: Strict = serde_json::from_slice(bytes).map_err(|why| unparseable(why.to_string()))?;
    match read.duplicate {
        Some(key) => Err(Refusal::DuplicateKey {
            path: path.to_path_buf(),
            key,
        }),
        None => Ok(read.value),
    }
}

/// The value as `serde_json` reads it, plus the first key any object of it names twice.
pub(crate) struct Strict {
    pub(crate) value: Value,
    pub(crate) duplicate: Option<String>,
}

impl Strict {
    const fn of(value: Value) -> Self {
        Self {
            value,
            duplicate: None,
        }
    }
}

impl<'de> Deserialize<'de> for Strict {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(StrictVisitor)
    }
}

struct StrictVisitor;

impl<'de> Visitor<'de> for StrictVisitor {
    type Value = Strict;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("any JSON value")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Strict, E> {
        Ok(Strict::of(Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Strict, E> {
        Ok(Strict::of(Value::Number(value.into())))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Strict, E> {
        Ok(Strict::of(Value::Number(value.into())))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Strict, E> {
        Ok(Strict::of(
            Number::from_f64(value).map_or(Value::Null, Value::Number),
        ))
    }

    fn visit_str<E>(self, value: &str) -> Result<Strict, E> {
        Ok(Strict::of(Value::String(value.to_owned())))
    }

    fn visit_string<E>(self, value: String) -> Result<Strict, E> {
        Ok(Strict::of(Value::String(value)))
    }

    fn visit_unit<E>(self) -> Result<Strict, E> {
        Ok(Strict::of(Value::Null))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Strict, A::Error> {
        let mut values = Vec::new();
        let mut duplicate = None;
        while let Some(element) = seq.next_element::<Strict>()? {
            duplicate = duplicate.or(element.duplicate);
            values.push(element.value);
        }
        Ok(Strict {
            value: Value::Array(values),
            duplicate,
        })
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Strict, A::Error> {
        let mut object = Map::new();
        let mut duplicate = None;
        while let Some((key, element)) = map.next_entry::<String, Strict>()? {
            duplicate = duplicate.or(element.duplicate);
            if object.contains_key(&key) {
                duplicate = duplicate.or_else(|| Some(key.clone()));
            }
            object.insert(key, element.value);
        }
        Ok(Strict {
            value: Value::Object(object),
            duplicate,
        })
    }
}
