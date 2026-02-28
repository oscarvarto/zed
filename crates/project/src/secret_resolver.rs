use std::sync::Arc;

use anyhow::{Context as _, Result};
use collections::HashMap;
use parking_lot::Mutex;
use settings::{EnvValue, SecretReference};

/// Resolves secret references from external providers (1Password, `pass`, etc.).
///
/// Caches resolved values so that biometric/authentication prompts happen at most
/// once per session rather than once per server.
#[derive(Clone)]
pub struct SecretResolver {
    cache: Arc<Mutex<HashMap<SecretReference, String>>>,
    failures: Arc<Mutex<HashMap<SecretReference, Arc<str>>>>,
}

impl SecretResolver {
    pub fn new() -> Self {
        Self {
            cache: Arc::new(Mutex::new(HashMap::default())),
            failures: Arc::new(Mutex::new(HashMap::default())),
        }
    }

    /// Extracts all `SecretReference`s from an env map.
    pub fn collect_secrets(env: &HashMap<String, EnvValue>) -> Vec<SecretReference> {
        env.values()
            .filter_map(|v| v.as_secret().cloned())
            .collect()
    }

    /// Resolves all given secrets sequentially, populating the internal cache.
    ///
    /// Sequential resolution ensures that providers like 1Password only prompt
    /// for biometric authentication once — the first `op read` triggers the prompt,
    /// and subsequent calls reuse the session.
    pub async fn pre_resolve(&self, secrets: &[SecretReference]) -> Result<()> {
        let mut errors = Vec::new();
        for secret in secrets {
            if self.cache.lock().contains_key(secret) || self.failures.lock().contains_key(secret) {
                continue;
            }

            match resolve_secret(secret).await.with_context(|| {
                format!(
                    "failed to resolve secret (provider: {}, reference: {})",
                    secret.provider, secret.reference
                )
            }) {
                Ok(value) => {
                    self.cache.lock().insert(secret.clone(), value);
                }
                Err(err) => {
                    let message = err.to_string();
                    self.failures
                        .lock()
                        .insert(secret.clone(), Arc::<str>::from(message.as_str()));
                    errors.push(message);
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            anyhow::bail!(
                "failed to resolve {} secret(s): {}",
                errors.len(),
                errors.join("; ")
            );
        }
    }

    /// Resolves an env map by replacing `Secret` entries with `Plain` values from the cache.
    ///
    /// Returns an error if any secret reference has not been pre-resolved.
    pub fn resolve_env_map(
        &self,
        env: &HashMap<String, EnvValue>,
    ) -> Result<HashMap<String, EnvValue>> {
        let cache = self.cache.lock();
        let failures = self.failures.lock();
        env.iter()
            .map(|(key, value)| {
                let resolved = match value {
                    EnvValue::Plain(_) => value.clone(),
                    EnvValue::Secret { secret } => {
                        let resolved_value = match cache.get(secret) {
                            Some(resolved_value) => resolved_value,
                            None => {
                                if let Some(error) = failures.get(secret) {
                                    anyhow::bail!(
                                        "failed to resolve secret for '{}' (provider: {}, reference: {}): {}",
                                        key,
                                        secret.provider,
                                        secret.reference,
                                        error
                                    );
                                }

                                anyhow::bail!(
                                    "secret not pre-resolved for '{}' (provider: {}, reference: {})",
                                    key,
                                    secret.provider,
                                    secret.reference
                                );
                            }
                        };
                        EnvValue::Plain(resolved_value.clone())
                    }
                };
                Ok((key.clone(), resolved))
            })
            .collect()
    }
}

async fn resolve_secret(secret: &SecretReference) -> Result<String> {
    let (program, args) = provider_command(secret)?;

    let output = smol::process::Command::new(program)
        .args(&args)
        .output()
        .await
        .with_context(|| {
            format!(
                "failed to execute secret provider command: {} {}",
                program,
                args.join(" ")
            )
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "secret provider '{}' failed (exit code {:?}): {}",
            secret.provider,
            output.status.code(),
            stderr.trim()
        );
    }

    let value = String::from_utf8(output.stdout)
        .context("secret provider output is not valid UTF-8")?
        .trim()
        .to_string();

    Ok(value)
}

fn provider_command(secret: &SecretReference) -> Result<(&'static str, Vec<String>)> {
    match secret.provider.as_str() {
        "1password" => Ok(("op", vec!["read".to_string(), secret.reference.clone()])),
        "pass" => pass_command(secret),
        "command" => Ok(shell_command(&secret.reference)),
        other => anyhow::bail!("unsupported secret provider: '{}'", other),
    }
}

#[cfg(windows)]
fn pass_command(_secret: &SecretReference) -> Result<(&'static str, Vec<String>)> {
    anyhow::bail!("secret provider 'pass' is not supported on Windows")
}

#[cfg(not(windows))]
fn pass_command(secret: &SecretReference) -> Result<(&'static str, Vec<String>)> {
    Ok(("pass", vec!["show".to_string(), secret.reference.clone()]))
}

#[cfg(windows)]
fn shell_command(command: &str) -> (&'static str, Vec<String>) {
    (
        "pwsh",
        vec![
            "-NoLogo".to_string(),
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-Command".to_string(),
            command.to_string(),
        ],
    )
}

#[cfg(not(windows))]
fn shell_command(command: &str) -> (&'static str, Vec<String>) {
    ("sh", vec!["-c".to_string(), command.to_string()])
}

#[cfg(test)]
mod tests {
    use settings::SecretReference;

    use super::provider_command;

    #[cfg(windows)]
    #[test]
    fn command_provider_uses_powershell_core() {
        let secret = SecretReference {
            provider: "command".to_string(),
            reference: "$env:OPENAI_API_KEY".to_string(),
        };

        let (program, args) = provider_command(&secret).expect("command provider should resolve");

        assert_eq!(program, "pwsh");
        assert_eq!(
            args,
            vec![
                "-NoLogo".to_string(),
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-Command".to_string(),
                "$env:OPENAI_API_KEY".to_string(),
            ]
        );
    }

    #[cfg(windows)]
    #[test]
    fn pass_provider_is_rejected_on_windows() {
        let secret = SecretReference {
            provider: "pass".to_string(),
            reference: "ignored".to_string(),
        };

        let error = provider_command(&secret).expect_err("pass should be unsupported on Windows");

        assert!(
            error
                .to_string()
                .contains("secret provider 'pass' is not supported on Windows")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn command_provider_uses_posix_shell() {
        let secret = SecretReference {
            provider: "command".to_string(),
            reference: "printenv OPENAI_API_KEY".to_string(),
        };

        let (program, args) = provider_command(&secret).expect("command provider should resolve");

        assert_eq!(program, "sh");
        assert_eq!(
            args,
            vec!["-c".to_string(), "printenv OPENAI_API_KEY".to_string()]
        );
    }
}
