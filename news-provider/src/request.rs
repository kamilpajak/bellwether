//! What a vendor is asked. A header value may be a credential's NAME and there is no variant
//! carrying its value, because `filter_params` is written into an archive nothing may delete:
//! `docs/design/architecture.md` section 3.2's `filter_params` row.

use canon::digest::{filter_params, Param as CanonParam, ParamValue};
use canon::{CanonError, Digest};

/// The four names a request and its ledger row are keyed by. One macro rather than four identical
/// impl blocks, and four types rather than `String`, because `canon::digest::attempt_id` takes
/// three of them positionally and three `String`s swap silently.
macro_rules! name_type {
    ($($(#[$doc:meta])* $type:ident),* $(,)?) => { $(
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub struct $type(String);

        impl $type {
            #[must_use]
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    )* };
}

name_type!(
    /// The adapter a request belongs to. `canon` takes a `&str` instead: it may not depend on the
    /// crate that owns this type, because cargo refuses the cycle.
    SourceId,
    /// What was asked for, as the ledger row names it.
    TargetWindow,
    /// One collection run.
    RunId,
    /// A credential by name. There is no type in this workspace that carries both a name and a
    /// value except `wire::Credentials`, which never leaves the process.
    CredentialRef,
);

name_type!(
    /// An HTTP header's name.
    HeaderName,
);

/// A header value, or a request parameter's: the same two shapes under the same rule, so one type
/// and not two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderValue {
    Literal(String),
    /// Substituted inside `Wire::send`, so the plan and the ledger hold the name.
    FromCredential(CredentialRef),
}

/// One parameter as sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    pub name: String,
    pub value: HeaderValue,
}

/// One vendor request. It holds no client and no clock: `Wire` has the client, and the two
/// instants are read either side of the socket by the code that can know them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub source: SourceId,
    pub target_window: TargetWindow,
    pub url: String,
    pub headers: Vec<(HeaderName, HeaderValue)>,
    pub params: Vec<Param>,
}

impl Request {
    /// The digest of what the vendor is asked, in which a credential appears as its name.
    pub fn filter_params(&self) -> Result<Digest, CanonError> {
        let params: Vec<CanonParam> = self
            .params
            .iter()
            .map(|param| CanonParam {
                name: &param.name,
                value: match &param.value {
                    HeaderValue::Literal(value) => ParamValue::Literal(value),
                    HeaderValue::FromCredential(credential) => {
                        ParamValue::Credential(credential.as_str())
                    }
                },
            })
            .collect();
        filter_params(&params)
    }
}
