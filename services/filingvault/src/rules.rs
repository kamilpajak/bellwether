//! The context's rules file: the forms an index day is read for (architecture.md section 12.3)
//! and how far back the first runs fill (section 12.2). Not hashed before step 11, whose
//! `rules_version` is its first reader.

use std::path::Path;

use serde_json::Value;

use crate::error::FilingsError;

pub const RULES_SCHEMA: &str = "filingvault.rules/1";
const KEYS: [&str; 3] = ["schema", "forms", "fill_days"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rules {
    /// The form list, as the index's `Form Type` column spells each form.
    pub forms: Vec<String>,
    /// Days before the first collect run's UTC date the fill reaches back to.
    pub fill_days: u32,
}

/// An absent file is `input_absent`; anything this reader cannot take whole is
/// `input_unparseable`, never a default.
pub fn read(path: &Path) -> Result<Rules, FilingsError> {
    let text = match std::fs::read(path) {
        Ok(text) => text,
        Err(why) if why.kind() == std::io::ErrorKind::NotFound => {
            return Err(FilingsError::Absent(path.to_path_buf()))
        }
        Err(why) => return Err(why.into()),
    };
    let refused = |why: String| FilingsError::Unparseable {
        what: path.display().to_string(),
        why,
    };
    let read: Value = serde_json::from_slice(&text).map_err(|why| refused(why.to_string()))?;
    let object = read
        .as_object()
        .ok_or_else(|| refused("not an object".to_owned()))?;
    if let Some(extra) = object.keys().find(|key| !KEYS.contains(&key.as_str())) {
        return Err(refused(format!("unknown key {extra}")));
    }
    if read["schema"] != RULES_SCHEMA {
        return Err(refused(format!("schema is not {RULES_SCHEMA}")));
    }
    let forms = read["forms"]
        .as_array()
        .and_then(|forms| {
            forms
                .iter()
                .map(|form| form.as_str().map(str::to_owned))
                .collect::<Option<Vec<String>>>()
        })
        .filter(|forms| !forms.is_empty())
        .ok_or_else(|| refused("forms is not a list of form names".to_owned()))?;
    // `sec_edgar::index_row` reads a row's form as its first word, so `DEF 14A` never matches.
    if let Some(spaced) = forms
        .iter()
        .find(|form| form.is_empty() || form.contains(char::is_whitespace))
    {
        return Err(refused(format!("the form {spaced:?} is not one word")));
    }
    let fill_days = read["fill_days"]
        .as_u64()
        .and_then(|days| u32::try_from(days).ok())
        .ok_or_else(|| refused("fill_days is not a count of days".to_owned()))?;
    Ok(Rules { forms, fill_days })
}
