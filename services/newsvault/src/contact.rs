//! The User-Agent's contact address: a provider's own variable, or the file its `_FILE` twin names
//! (the official-image convention for a mounted secret). Every error names a variable or a path
//! and never the value: the address is the one thing in a request that is a person.

use std::ffi::OsString;
use std::path::PathBuf;

/// Why no contact was read. Each maps to one section 5.1 code through [`ContactError::code`].
#[derive(Debug, thiserror::Error)]
pub enum ContactError {
    /// Two sources for one value: neither is chosen silently.
    #[error("{variable} and {file_variable} are both set; set one")]
    BothSet {
        variable: &'static str,
        file_variable: &'static str,
    },
    #[error("{file_variable} names {path}, and no file is there")]
    FileAbsent {
        file_variable: &'static str,
        path: PathBuf,
    },
    /// `why` is the io error's own text or "not UTF-8"; neither carries the file's bytes.
    #[error("{file_variable} names {path}, which could not be read: {why}")]
    FileUnreadable {
        file_variable: &'static str,
        path: PathBuf,
        why: String,
    },
    #[error("{file_variable} names {path}, which holds no contact")]
    FileEmpty {
        file_variable: &'static str,
        path: PathBuf,
    },
    /// A second line or a carriage return would reach the User-Agent header.
    #[error("{file_variable} names {path}, whose contact holds a control character")]
    ControlCharacter {
        file_variable: &'static str,
        path: PathBuf,
    },
}

impl ContactError {
    /// The section 5.1 code. An empty file is `credential_contact_missing`, as an unset variable is.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::BothSet { .. } => "invalid_argument",
            Self::FileAbsent { .. } => "input_absent",
            Self::FileUnreadable { .. } => "io_failed",
            Self::FileEmpty { .. } => "credential_contact_missing",
            Self::ControlCharacter { .. } => "input_unparseable",
        }
    }

    #[must_use]
    pub const fn is_usage(&self) -> bool {
        matches!(self, Self::BothSet { .. })
    }
}

/// The contact from `variable`, else from the file `file_variable` names, else empty, which the
/// provider refuses as `credential_contact_missing`. One trailing `\n` is removed from a file.
pub fn read(
    variable: &'static str,
    file_variable: &'static str,
    env: impl Fn(&str) -> Option<OsString>,
) -> Result<String, ContactError> {
    match (env(variable), env(file_variable)) {
        (Some(_), Some(_)) => Err(ContactError::BothSet {
            variable,
            file_variable,
        }),
        (_, Some(path)) => from_file(file_variable, PathBuf::from(path)),
        (Some(value), None) => Ok(value.into_string().unwrap_or_default()),
        (None, None) => Ok(String::new()),
    }
}

fn from_file(file_variable: &'static str, path: PathBuf) -> Result<String, ContactError> {
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(why) if why.kind() == std::io::ErrorKind::NotFound => {
            return Err(ContactError::FileAbsent {
                file_variable,
                path,
            })
        }
        Err(why) => {
            return Err(ContactError::FileUnreadable {
                file_variable,
                path,
                why: why.to_string(),
            })
        }
    };
    let Ok(text) = String::from_utf8(bytes) else {
        return Err(ContactError::FileUnreadable {
            file_variable,
            path,
            why: "not UTF-8".to_owned(),
        });
    };
    let value = text.strip_suffix('\n').unwrap_or(&text);
    if value.is_empty() {
        return Err(ContactError::FileEmpty {
            file_variable,
            path,
        });
    }
    if value.chars().any(char::is_control) {
        return Err(ContactError::ControlCharacter {
            file_variable,
            path,
        });
    }
    Ok(value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{read, ContactError};
    use std::ffi::OsString;
    use std::path::Path;

    const VARIABLE: &str = "BELLWETHER_FEED_CONTACT";
    const FILE_VARIABLE: &str = "BELLWETHER_FEED_CONTACT_FILE";
    const CONTACT: &str = "bellwether collector contact@example.test";

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        }
    }

    fn from_a_file(bytes: &[u8]) -> Result<Result<String, ContactError>, std::io::Error> {
        let dir = tempfile::TempDir::new()?;
        let path = dir.path().join("contact");
        std::fs::write(&path, bytes)?;
        let shown = path.to_string_lossy().into_owned();
        Ok(read(
            VARIABLE,
            FILE_VARIABLE,
            env(&[(FILE_VARIABLE, &shown)]),
        ))
    }

    fn code(read: &Result<String, ContactError>) -> Option<&'static str> {
        read.as_ref().err().map(ContactError::code)
    }

    #[test]
    fn both_set_is_a_usage_error_naming_both_variables_and_not_the_value() {
        let read = read(
            VARIABLE,
            FILE_VARIABLE,
            env(&[(VARIABLE, CONTACT), (FILE_VARIABLE, "/run/secrets/contact")]),
        );
        let shown = read.as_ref().err().map(ToString::to_string);
        assert_eq!(
            (
                code(&read),
                read.as_ref().err().map(ContactError::is_usage),
                shown
            ),
            (
                Some("invalid_argument"),
                Some(true),
                Some(format!(
                    "{VARIABLE} and {FILE_VARIABLE} are both set; set one"
                ))
            )
        );
    }

    #[test]
    fn the_variable_alone_is_read_as_before_and_neither_is_empty() {
        assert_eq!(
            (
                read(VARIABLE, FILE_VARIABLE, env(&[(VARIABLE, CONTACT)])).ok(),
                read(VARIABLE, FILE_VARIABLE, env(&[])).ok()
            ),
            (Some(CONTACT.to_owned()), Some(String::new()))
        );
    }

    /// One trailing newline is what `echo` and an editor leave; it is not part of the address.
    #[test]
    fn a_file_is_read_with_one_trailing_newline_removed() -> Result<(), std::io::Error> {
        let with = from_a_file(format!("{CONTACT}\n").as_bytes())?;
        let without = from_a_file(CONTACT.as_bytes())?;
        assert_eq!(
            (with.ok(), without.ok()),
            (Some(CONTACT.to_owned()), Some(CONTACT.to_owned()))
        );
        Ok(())
    }

    #[test]
    fn an_empty_file_or_a_lone_newline_is_credential_contact_missing() -> Result<(), std::io::Error>
    {
        assert_eq!(
            (
                code(&from_a_file(b"")?),
                code(&from_a_file(b"\n")?),
                code(&from_a_file(b"\n\n")?)
            ),
            (
                Some("credential_contact_missing"),
                Some("credential_contact_missing"),
                Some("input_unparseable")
            )
        );
        Ok(())
    }

    #[test]
    fn a_second_newline_or_a_carriage_return_is_refused() -> Result<(), std::io::Error> {
        assert_eq!(
            (
                code(&from_a_file(format!("{CONTACT}\n\n").as_bytes())?),
                code(&from_a_file(format!("{CONTACT}\r\n").as_bytes())?),
                code(&from_a_file(format!("{CONTACT}\nsecond").as_bytes())?)
            ),
            (
                Some("input_unparseable"),
                Some("input_unparseable"),
                Some("input_unparseable")
            )
        );
        Ok(())
    }

    #[test]
    fn an_absent_file_is_input_absent_and_a_directory_or_non_utf8_is_io_failed(
    ) -> Result<(), std::io::Error> {
        let dir = tempfile::TempDir::new()?;
        let at = |path: &Path| {
            let shown = path.to_string_lossy().into_owned();
            code(&read(
                VARIABLE,
                FILE_VARIABLE,
                env(&[(FILE_VARIABLE, &shown)]),
            ))
        };
        assert_eq!(
            (
                at(&dir.path().join("absent")),
                at(dir.path()),
                code(&from_a_file(&[0xff, 0xfe])?)
            ),
            (Some("input_absent"), Some("io_failed"), Some("io_failed"))
        );
        Ok(())
    }

    /// Every refusal's text, the file's bytes being the contact: none carries it.
    #[test]
    fn no_refusal_carries_the_files_bytes() -> Result<(), std::io::Error> {
        for bytes in [
            format!("{CONTACT}\n\n"),
            format!("{CONTACT}\r\n"),
            format!("\u{7}{CONTACT}"),
        ] {
            let shown = from_a_file(bytes.as_bytes())?
                .err()
                .map(|refused| refused.to_string())
                .unwrap_or_default();
            assert!(
                !shown.is_empty() && !shown.contains("contact@example.test"),
                "{shown}"
            );
        }
        Ok(())
    }
}
