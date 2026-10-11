use std::sync::LazyLock;

static REDACT_REGEX: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"(^|\s)([A-Za-z_][A-Za-z0-9_]*)=("(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|\S+)"#)
        .unwrap()
});

const SHOWN_ENVIRONMENT_VARIABLES: &[&str] = &[
    "COLORTERM",
    "HOME",
    "LANG",
    "LANGUAGE",
    "LC_ALL",
    "LC_CTYPE",
    "LOGNAME",
    "PATH",
    "PATHEXT",
    "PWD",
    "RUST_BACKTRACE",
    "RUST_LOG",
    "SHELL",
    "SHLVL",
    "TEMP",
    "TERM",
    "TMP",
    "TMPDIR",
    "TZ",
    "USER",
    "USERNAME",
    "USERPROFILE",
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_RUNTIME_DIR",
    "XDG_STATE_HOME",
];

/// Whether a given environment variable name should have its value redacted
pub fn should_redact(env_var_name: &str) -> bool {
    !SHOWN_ENVIRONMENT_VARIABLES.contains(&env_var_name)
}

pub fn is_valid_environment_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|character| character != '=' && !character.is_control())
}

/// Redact a string which could include a command with environment variables
pub fn redact_command(command: &str) -> String {
    REDACT_REGEX
        .replace_all(command, |caps: &regex::Captures| {
            let separator = &caps[1];
            let var_name = &caps[2];
            let value = &caps[3];
            if should_redact(var_name) {
                format!(r#"{separator}{var_name}="[REDACTED]""#)
            } else {
                format!("{separator}{var_name}={value}")
            }
        })
        .to_string()
}

const JSON_VARIABLE_KEYS: &[&str] = &["env", "envs", "environment", "headers"];

pub fn redact_json_environment(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            for (key, child) in object.iter_mut() {
                if JSON_VARIABLE_KEYS.contains(&key.as_str()) {
                    redact_json_variables(child);
                } else {
                    redact_json_environment(child);
                }
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(redact_json_environment),
        _ => {}
    }
}

fn redact_json_variables(variables: &mut serde_json::Value) {
    let redacted = || serde_json::Value::String("[REDACTED]".to_string());
    match variables {
        serde_json::Value::Object(object) => {
            for (name, value) in object.iter_mut() {
                if should_redact(name) {
                    *value = redacted();
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                match item {
                    serde_json::Value::Object(variable) => {
                        let redact = variable
                            .get("name")
                            .and_then(serde_json::Value::as_str)
                            .is_none_or(should_redact);
                        if redact && let Some(value) = variable.get_mut("value") {
                            *value = redacted();
                        }
                    }
                    serde_json::Value::String(assignment) => {
                        if let Some((name, _)) = assignment.split_once('=')
                            && should_redact(name)
                        {
                            *assignment = format!("{name}=[REDACTED]");
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_redact_json_environment_covers_every_variable_shape() {
        let mut value = serde_json::json!({
            "request": "launch",
            "envs": { "PATH": "/usr/bin", "DATABASE_URL": "postgres://user:hunter2@db/app" },
            "configuration": {
                "program": "./app",
                "env": { "GH_TOKEN": "ghp_hunter2" },
                "environment": [
                    { "name": "HOME", "value": "/home/user" },
                    { "name": "API_SECRET", "value": "hunter2" }
                ]
            },
            "mcpServers": [{
                "env": ["SENTRY_DSN=https://hunter2@sentry.io/1", "TERM=xterm"],
                "headers": [{ "name": "Authorization", "value": "Bearer hunter2" }]
            }]
        });
        redact_json_environment(&mut value);
        assert_eq!(
            value,
            serde_json::json!({
                "request": "launch",
                "envs": { "PATH": "/usr/bin", "DATABASE_URL": "[REDACTED]" },
                "configuration": {
                    "program": "./app",
                    "env": { "GH_TOKEN": "[REDACTED]" },
                    "environment": [
                        { "name": "HOME", "value": "/home/user" },
                        { "name": "API_SECRET", "value": "[REDACTED]" }
                    ]
                },
                "mcpServers": [{
                    "env": ["SENTRY_DSN=[REDACTED]", "TERM=xterm"],
                    "headers": [{ "name": "Authorization", "value": "[REDACTED]" }]
                }]
            })
        );
    }

    #[test]
    fn test_redact_string_with_multiple_env_vars() {
        let input = r#"failed to spawn command cd "/code/something" && ANTHROPIC_API_KEY="sk-ant-api03-WOOOO" COMMAND_MODE="unix2003" GEMINI_API_KEY="AIGEMINIFACE" HOME="/Users/foo""#;
        let result = redact_command(input);
        let expected = r#"failed to spawn command cd "/code/something" && ANTHROPIC_API_KEY="[REDACTED]" COMMAND_MODE="[REDACTED]" GEMINI_API_KEY="[REDACTED]" HOME="/Users/foo""#;
        assert_eq!(result, expected);
    }

    #[test]
    fn test_redacts_every_value_outside_the_shown_names() {
        for name in [
            "DATABASE_URL",
            "SENTRY_DSN",
            "GH_PAT",
            "http_proxy",
            "api_token",
        ] {
            assert!(should_redact(name), "{name}");
        }
        for name in ["PATH", "HOME", "SHELL", "LANG", "TERM"] {
            assert!(!should_redact(name), "{name}");
        }
    }

    #[test]
    fn test_redact_command_covers_lowercase_names_and_leaves_flags_alone() {
        let input = "DATABASE_URL=postgres://user:hunter2@db/app\nhttp_proxy=http://user:pass@proxy:3128 --log-level=debug PATH=/usr/bin";
        assert_eq!(
            redact_command(input),
            "DATABASE_URL=\"[REDACTED]\"\nhttp_proxy=\"[REDACTED]\" --log-level=debug PATH=/usr/bin"
        );
    }
}
