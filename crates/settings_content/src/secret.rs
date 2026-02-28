use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings_macros::MergeFrom;

/// A reference to a secret stored in an external secret provider.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct SecretReference {
    /// The secret provider to use (e.g. "1password", "pass" on Unix-like systems, or "command").
    pub provider: String,
    /// The provider-specific reference to the secret (e.g. "op://vault/item/field" for 1Password).
    pub reference: String,
}

/// A string-like configuration value that is either plain text or a secret reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom)]
#[serde(untagged)]
pub enum EnvValue {
    /// A secret reference that will be resolved at runtime.
    Secret {
        #[serde(rename = "$secret")]
        secret: SecretReference,
    },
    /// A plain string value.
    Plain(String),
}

impl EnvValue {
    /// Returns the plain string value, if this is a `Plain` variant.
    pub fn as_plain(&self) -> Option<&str> {
        match self {
            EnvValue::Plain(value) => Some(value),
            EnvValue::Secret { .. } => None,
        }
    }

    /// Returns the secret reference, if this is a `Secret` variant.
    pub fn as_secret(&self) -> Option<&SecretReference> {
        match self {
            EnvValue::Secret { secret } => Some(secret),
            EnvValue::Plain(_) => None,
        }
    }

    /// Converts into a plain string, returning an error if this is a `Secret` variant
    /// that has not been resolved yet.
    pub fn into_plain_string(self) -> anyhow::Result<String> {
        match self {
            EnvValue::Plain(value) => Ok(value),
            EnvValue::Secret { secret } => {
                anyhow::bail!(
                    "unresolved secret reference (provider: {}, reference: {})",
                    secret.provider,
                    secret.reference
                )
            }
        }
    }
}
