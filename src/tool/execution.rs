use serde::Deserialize;
use serde_json::Value;

use super::Escape;
use std::time::Duration;

pub(super) const DEFAULT_SECONDS: u64 = 90;
pub(super) const COMMAND_BYTES: usize = 32 * 1024;
pub(super) const MIN_SECONDS: f64 = 1.0;
pub(super) const MAX_SECONDS: f64 = 600.0;

const MAX_REASON_BYTES: usize = 4096;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    argv: Vec<String>,
    timeout: f64,
    #[serde(default)]
    env: std::collections::BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EscapingWire {
    argv: Vec<String>,
    timeout: f64,
    #[serde(default)]
    env: std::collections::BTreeMap<String, String>,
    sandbox: bool,
    reason: Option<String>,
}

pub(super) struct Request {
    pub(super) command: Argv,
    pub(super) timeout: Duration,
    pub(super) outside: Option<String>,
}

impl Request {
    pub(super) fn parse(arguments: Value, escape: Escape) -> Result<Self, String> {
        let invalid = |e: serde_json::Error| format!("invalid run request: {e}");
        let (command, timeout, outside, env) = match escape {
            Escape::Withheld => {
                let wire: Wire = serde_json::from_value(arguments).map_err(invalid)?;
                (wire.argv, wire.timeout, None, wire.env)
            }
            Escape::Offered => {
                let wire: EscapingWire = serde_json::from_value(arguments).map_err(invalid)?;
                let outside = outside(wire.sandbox, wire.reason)?;
                (wire.argv, wire.timeout, outside, wire.env)
            }
        };
        let command = Argv::parse(command)?.with_env(env)?;
        if !timeout.is_finite() || !(MIN_SECONDS..=MAX_SECONDS).contains(&timeout) {
            return Err(format!(
                "timeout must be a finite number from {MIN_SECONDS} to {MAX_SECONDS} seconds"
            ));
        }
        let timeout = Duration::try_from_secs_f64(timeout).map_err(|e| e.to_string())?;
        Ok(Self {
            command,
            timeout,
            outside,
        })
    }
}

pub(super) struct Argv {
    args: Vec<String>,
    env: std::collections::BTreeMap<String, String>,
}

impl Argv {
    pub(super) fn parse(argv: Vec<String>) -> Result<Self, String> {
        if argv.first().is_none_or(String::is_empty)
            || argv.iter().any(|arg| arg.contains('\0'))
            || argv
                .iter()
                .map(|arg| arg.len().saturating_add(1))
                .sum::<usize>()
                > COMMAND_BYTES
        {
            return Err("argv must be nonempty, with a nonempty executable, no NUL, and at most 32 KiB total".into());
        }
        Ok(Self {
            args: argv,
            env: Default::default(),
        })
    }

    pub(super) fn with_env(
        mut self,
        env: std::collections::BTreeMap<String, String>,
    ) -> Result<Self, String> {
        if env
            .iter()
            .any(|(key, value)| key.is_empty() || key.contains(['=', '\0']) || value.contains('\0'))
            || env
                .iter()
                .map(|(key, value)| key.len().saturating_add(value.len()).saturating_add(2))
                .sum::<usize>()
                > 64 * 1024
        {
            return Err("env must map nonempty names without '=' or NUL to strings without NUL, at most 64 KiB total".into());
        }
        self.env = env;
        Ok(self)
    }

    pub(super) fn env(&self) -> &std::collections::BTreeMap<String, String> {
        &self.env
    }

    pub(super) fn review_label(&self) -> String {
        if self.env.is_empty() {
            self.label()
        } else {
            serde_json::json!({"argv": self.args, "env": self.env}).to_string()
        }
    }

    pub(super) fn args(&self) -> &[String] {
        &self.args
    }
    pub(super) fn label(&self) -> String {
        serde_json::to_string(&self.args).expect("argv strings serialize")
    }
}

pub(super) fn outside(sandbox: bool, reason: Option<String>) -> Result<Option<String>, String> {
    match (sandbox, reason) {
        (true, None) => Ok(None),
        (true, Some(_)) => Err("reason goes with sandbox=False".into()),
        (false, Some(reason)) if !reason.trim().is_empty() && reason.len() <= MAX_REASON_BYTES => {
            Ok(Some(reason))
        }
        (false, _) => Err(format!(
            "sandbox=False needs a reason of 1 to {MAX_REASON_BYTES} bytes"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_is_validated_and_present_in_review_but_not_routine_labels() {
        let command = Argv::parse(vec!["python3".into()])
            .unwrap()
            .with_env([("TOKEN".into(), "sensitive-test-value".into())].into())
            .unwrap();
        assert!(!command.label().contains("sensitive-test-value"));
        assert!(command.review_label().contains("sensitive-test-value"));
        for key in ["", "a=b", "a\0b"] {
            assert!(
                Argv::parse(vec!["true".into()])
                    .unwrap()
                    .with_env([(key.into(), "value".into())].into())
                    .is_err()
            );
        }
        assert!(
            Argv::parse(vec!["true".into()])
                .unwrap()
                .with_env([("KEY".into(), "a\0b".into())].into())
                .is_err()
        );
    }
}
