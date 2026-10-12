use std::{ffi::OsStr, path::Path};

const REDACTED: &str = "[REDACTED]";

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
    let command = redact_private_keys(command);
    let command = redact_assignments(&command);
    let command = redact_secret_option_values(&command);
    let command = redact_credentialed_urls(&command);
    redact_secret_tokens(&command)
}

pub fn redact_arguments<S: AsRef<OsStr>>(arguments: impl IntoIterator<Item = S>) -> Vec<String> {
    let mut follows_secret_option = false;
    arguments
        .into_iter()
        .map(|argument| {
            let argument = argument.as_ref().to_string_lossy();
            let redacted = if follows_secret_option {
                REDACTED.to_string()
            } else {
                redact_command(&argument)
            };
            follows_secret_option = awaits_secret_value(&argument);
            redacted
        })
        .collect()
}

pub fn describe_command<'a>(
    program: &OsStr,
    arguments: impl IntoIterator<Item = &'a OsStr>,
    directory: Option<&Path>,
) -> String {
    let redact = |part: &OsStr| redact_command(&part.to_string_lossy());
    let mut description = format!("{:?}", redact(program));
    for argument in redact_arguments(arguments) {
        description.push_str(&format!(" {argument:?}"));
    }
    if let Some(directory) = directory {
        description.push_str(&format!(" in {:?}", redact(directory.as_os_str())));
    }
    description
}

fn is_text_delimiter(byte: u8) -> bool {
    byte.is_ascii_whitespace() || byte.is_ascii_control() || b",;'\"()[]{}<>`|&".contains(&byte)
}

fn is_value_delimiter(byte: u8) -> bool {
    byte.is_ascii_whitespace() || byte.is_ascii_control()
}

fn is_key_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn is_inline_space(byte: u8) -> bool {
    byte == b' ' || byte == b'\t'
}

fn has_environment_prefix(before_key: &[u8]) -> bool {
    let ends_with_ignore_case = |suffix: &[u8]| {
        before_key.len() >= suffix.len()
            && before_key[before_key.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
    };
    let opens_elvish_variable = before_key.ends_with(b"E:")
        && (before_key.len() == 2 || is_text_delimiter(before_key[before_key.len() - 3]));
    ends_with_ignore_case(b"$env:")
        || ends_with_ignore_case(b"$env.")
        || before_key.ends_with(b"$")
        || opens_elvish_variable
}

fn follows_authorization_scheme(before_key: &[u8]) -> bool {
    let Some(scheme_end) = before_key
        .iter()
        .rposition(|byte| !is_inline_space(*byte))
        .map(|index| index + 1)
    else {
        return false;
    };
    let scheme_start = before_key[..scheme_end]
        .iter()
        .rposition(|byte| !byte.is_ascii_alphabetic())
        .map_or(0, |index| index + 1);
    let scheme = &before_key[scheme_start..scheme_end];
    scheme_end < before_key.len()
        && (scheme.eq_ignore_ascii_case(b"Basic") || scheme.eq_ignore_ascii_case(b"Bearer"))
}

fn redact_assignments(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut output = String::with_capacity(text.len());
    let mut copied = 0;
    let mut position = 0;
    while let Some(offset) = bytes[position..].iter().position(|byte| *byte == b'=') {
        let equals = position + offset;
        position = equals + 1;
        let key_end = bytes[..equals]
            .iter()
            .rposition(|byte| !is_inline_space(*byte))
            .map_or(0, |index| index + 1);
        let mut key_start = bytes[..key_end]
            .iter()
            .rposition(|byte| !is_key_byte(*byte) && *byte != b'-')
            .map_or(0, |index| index + 1);
        let prefixed = has_environment_prefix(&bytes[..key_start]);
        if key_end < equals && !prefixed {
            continue;
        }
        let mut long_option = false;
        if !prefixed && bytes.get(key_start) == Some(&b'-') {
            let opens_token = key_start == 0 || is_text_delimiter(bytes[key_start - 1]);
            if !opens_token {
                continue;
            }
            long_option = bytes.get(key_start + 1) == Some(&b'-');
            if long_option {
                if !is_secret_option_name(&text[key_start..key_end]) {
                    continue;
                }
            } else {
                key_start += 2;
            }
        }
        if key_start >= key_end || key_start < copied {
            continue;
        }
        let key = &text[key_start..key_end];
        let mut value_start = equals + 1;
        if prefixed {
            while bytes.get(value_start).copied().is_some_and(is_inline_space) {
                value_start += 1;
            }
        }
        let value_end = assignment_value_end(bytes, value_start);
        if value_end == value_start {
            continue;
        }
        let base64_padding = bytes[value_start..value_end]
            .iter()
            .all(|byte| *byte == b'=');
        if base64_padding && follows_authorization_scheme(&bytes[..key_start]) {
            continue;
        }
        if long_option || should_redact(key) {
            output.push_str(&text[copied..value_start]);
            output.push('"');
            output.push_str(REDACTED);
            output.push('"');
            copied = value_end;
            position = value_end;
        }
    }
    output.push_str(&text[copied..]);
    output
}

const SECRET_OPTION_WORDS: &[&str] = &[
    "accesskey",
    "apikey",
    "authorization",
    "cookie",
    "credential",
    "passphrase",
    "passwd",
    "password",
    "privatekey",
    "secret",
    "token",
];

const SECRET_OPTION_SUFFIXES: &[&str] = &["key", "license", "pass"];

fn is_secret_option_name(option: &str) -> bool {
    let name = option
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|character| character.to_ascii_lowercase())
        .collect::<String>();
    SECRET_OPTION_WORDS.iter().any(|word| name.contains(word))
        || SECRET_OPTION_SUFFIXES
            .iter()
            .any(|suffix| name.ends_with(suffix))
}

fn awaits_secret_value(token: &str) -> bool {
    let option = token
        .trim_matches(|character: char| !character.is_ascii_alphanumeric() && character != '-');
    option.starts_with('-') && !token.contains('=') && is_secret_option_name(option)
}

fn redact_secret_option_values(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut output = String::with_capacity(text.len());
    let mut copied = 0;
    let mut position = 0;
    while position < bytes.len() {
        if is_value_delimiter(bytes[position]) {
            position += 1;
            continue;
        }
        let token_start = position;
        while bytes
            .get(position)
            .is_some_and(|byte| !is_value_delimiter(*byte))
        {
            position += 1;
        }
        let mut option_start = token_start;
        while awaits_secret_value(&text[option_start..position]) {
            let mut value_start = position;
            while bytes.get(value_start).copied().is_some_and(is_inline_space) {
                value_start += 1;
            }
            if bytes
                .get(value_start)
                .is_none_or(|byte| is_value_delimiter(*byte))
            {
                break;
            }
            let value_end = assignment_value_end(bytes, value_start);
            output.push_str(&text[copied..value_start]);
            output.push_str(REDACTED);
            copied = value_end;
            option_start = value_start;
            position = value_end;
        }
    }
    output.push_str(&text[copied..]);
    output
}

fn assignment_value_end(bytes: &[u8], start: usize) -> usize {
    let mut index = start;
    let mut quoted_end = start;
    while let Some(&byte) = bytes.get(index) {
        let fragment_end = match byte {
            b'"' | b'\'' | b'`' => closing_quote_end(bytes, index),
            b'$' if bytes.get(index + 1) == Some(&b'(') => {
                closing_parenthesis_end(bytes, index + 1)
            }
            b'\\' => {
                index = (index + 2).min(bytes.len());
                continue;
            }
            _ if is_value_delimiter(byte) => break,
            _ => {
                index += 1;
                continue;
            }
        };
        match fragment_end {
            Some(end) => {
                index = end;
                quoted_end = end;
            }
            None if byte == b'$' || bytes[start..index].iter().all(|byte| *byte == b'$') => {
                return bytes.len();
            }
            None => index += 1,
        }
    }
    while index > quoted_end && matches!(bytes[index - 1], b'.' | b':') {
        index -= 1;
    }
    index
}

fn closing_quote_end(bytes: &[u8], quote_index: usize) -> Option<usize> {
    let quote = *bytes.get(quote_index)?;
    let mut index = quote_index + 1;
    while let Some(&byte) = bytes.get(index) {
        index += 1;
        if byte == b'\\' {
            index += 1;
        } else if byte == quote {
            return Some(index);
        }
    }
    None
}

fn closing_parenthesis_end(bytes: &[u8], open_index: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut index = open_index;
    while let Some(&byte) = bytes.get(index) {
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index + 1);
                }
            }
            b'"' | b'\'' | b'`' => {
                if let Some(end) = closing_quote_end(bytes, index) {
                    index = end;
                    continue;
                }
            }
            b'\\' => index += 1,
            _ => {}
        }
        index += 1;
    }
    None
}

fn redact_secret_tokens(text: &str) -> String {
    let bytes = text.as_bytes();
    let is_delimiter = |byte: u8| is_text_delimiter(byte) || byte == b'=';
    let mut output = String::with_capacity(text.len());
    let mut copied = 0;
    let mut position = 0;
    let mut credential_minimum = None;
    let mut follows_authorization = false;
    while position < bytes.len() {
        if is_delimiter(bytes[position]) {
            position += 1;
            continue;
        }
        let start = position;
        while position < bytes.len()
            && ((bytes[position] == b'=' && credential_minimum.is_some())
                || !is_delimiter(bytes[position]))
        {
            position += 1;
        }
        let mut end = position;
        while end > start && matches!(bytes[end - 1], b'.' | b':') {
            end -= 1;
        }
        let token = &text[start..end];
        let secret = (credential_minimum.is_some_and(|minimum| is_credential_body(token, minimum))
            || is_secret_token(token))
        .then_some(start..end);
        credential_minimum = if secret.is_some() {
            None
        } else if token.eq_ignore_ascii_case("Basic") && follows_authorization {
            Some(4)
        } else if token.eq_ignore_ascii_case("Bearer") || token.eq_ignore_ascii_case("Basic") {
            Some(8)
        } else {
            None
        };
        follows_authorization = token.eq_ignore_ascii_case("Authorization");
        if let Some(secret) = secret {
            output.push_str(&text[copied..secret.start]);
            output.push_str(REDACTED);
            copied = secret.end;
        }
    }
    output.push_str(&text[copied..]);
    output
}

fn is_credential_body(token: &str, minimum: usize) -> bool {
    token.len() >= minimum
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"+/=._-~".contains(&byte))
}

fn is_secret_token(token: &str) -> bool {
    known_token_prefix(token) || is_jwt(token)
}

// Vendor token markers and the shortest body each vendor issues after it, from envctl's `known_token_prefix`.
const TOKEN_MARKERS: &[(&str, usize, &[u8])] = &[
    ("github_pat_", 20, b"_"),
    ("ghp_", 20, b""),
    ("gho_", 20, b""),
    ("ghu_", 20, b""),
    ("ghs_", 20, b""),
    ("ghr_", 20, b""),
    ("glpat-", 16, b"-_"),
    ("sk-ant-", 16, b"-_"),
    ("sk-proj-", 16, b"-_"),
    ("sk_live_", 16, b""),
    ("sk_test_", 16, b""),
    ("rk_live_", 16, b""),
    ("rk_test_", 16, b""),
    ("xoxb-", 16, b"-"),
    ("xoxp-", 16, b"-"),
    ("xoxa-", 16, b"-"),
    ("xoxr-", 16, b"-"),
    ("xoxs-", 16, b"-"),
    ("npm_", 24, b""),
    ("pypi-", 16, b"-_"),
    ("dop_v1_", 32, b""),
    ("whsec_", 16, b""),
    ("AIza", 24, b"-_"),
    ("SG.", 24, b"-_."),
    ("sk-", 17, b"-_"),
];

fn known_token_prefix(token: &str) -> bool {
    let aws_key = ["AKIA", "ASIA", "ABIA", "ACCA"].iter().any(|prefix| {
        token.strip_prefix(prefix).is_some_and(|body| {
            body.len() >= 16
                && body
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        })
    });
    aws_key
        || TOKEN_MARKERS.iter().any(|(prefix, shortest, extra)| {
            token.strip_prefix(prefix).is_some_and(|body| {
                body.bytes()
                    .take_while(|byte| byte.is_ascii_alphanumeric() || extra.contains(byte))
                    .count()
                    >= *shortest
            })
        })
}

fn is_jwt(token: &str) -> bool {
    let segments = token.split('.').collect::<Vec<_>>();
    token.len() >= 20
        && segments.len() == 3
        && segments.iter().all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        })
        && segments[..2]
            .iter()
            .all(|segment| segment.starts_with("ey"))
}

fn redact_credentialed_urls(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut copied = 0;
    let mut position = 0;
    while let Some(offset) = text[position..].find("://") {
        let start = position + offset + 3;
        position = start;
        let authority_end = text[start..]
            .find(|character: char| {
                character.is_whitespace()
                    || character.is_control()
                    || "/?#\"`<>[]{}|\\^".contains(character)
            })
            .map_or(text.len(), |offset| start + offset);
        let Some(userinfo_end) = text[start..authority_end]
            .rfind('@')
            .map(|offset| start + offset)
        else {
            continue;
        };
        let userinfo = &text[start..userinfo_end];
        if userinfo.contains(':') || userinfo.len() >= 8 {
            output.push_str(&text[copied..start]);
            output.push_str(REDACTED);
            copied = userinfo_end;
        }
        position = userinfo_end;
    }
    output.push_str(&text[copied..]);
    output
}

fn redact_private_keys(text: &str) -> String {
    const BEGIN: &str = "-----BEGIN ";
    const DASHES: &str = "-----";
    let mut output = String::with_capacity(text.len());
    let mut copied = 0;
    let mut position = 0;
    while let Some(offset) = text[position..].find(BEGIN) {
        let begin = position + offset;
        let label_start = begin + BEGIN.len();
        let Some(label_length) = text[label_start..].find(DASHES) else {
            break;
        };
        let label = &text[label_start..label_start + label_length];
        let header_end = label_start + label_length + DASHES.len();
        position = header_end;
        if label.contains('\n') || !label.ends_with("PRIVATE KEY") {
            continue;
        }
        let footer = format!("-----END {label}-----");
        let block_end = text[header_end..]
            .find(&footer)
            .map_or(text.len(), |offset| header_end + offset + footer.len());
        output.push_str(&text[copied..begin]);
        output.push_str(REDACTED);
        copied = block_end;
        position = block_end;
    }
    output.push_str(&text[copied..]);
    output
}

const JSON_ENVIRONMENT_KEYS: &[&str] = &["env", "envs", "environment"];

pub fn redact_json_environment(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            for (key, child) in object.iter_mut() {
                if JSON_ENVIRONMENT_KEYS.contains(&key.as_str()) {
                    redact_json_variables(child, should_redact);
                } else if key == "headers" {
                    redact_json_variables(child, |_| true);
                } else {
                    redact_json_environment(child);
                }
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(redact_json_environment),
        serde_json::Value::String(text) => *text = redact_command(text),
        _ => {}
    }
}

fn redact_json_variables(variables: &mut serde_json::Value, should_redact: fn(&str) -> bool) {
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
                "args": ["--verbose", "--token=ghp_16C7e42F292c6912E7710c838347Ae178B4a", "API_KEY=argument-secret"],
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
                    "args": ["--verbose", "--token=\"[REDACTED]\"", "API_KEY=\"[REDACTED]\""],
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
    fn test_redact_command_covers_spaced_shell_environment_assignments() {
        assert_eq!(
            redact_command(
                "$env:API_KEY = 'argument-secret'; $Env:TOKEN =\tsecret; $env.SECRET = \"nu secret\"; set E:KEY = elvish-secret; $XONSH_TOKEN = xonsh-secret; $env:PATH = 'C:\\bin'"
            ),
            "$env:API_KEY = \"[REDACTED]\" $Env:TOKEN =\t\"[REDACTED]\" $env.SECRET = \"[REDACTED]\" set E:KEY = \"[REDACTED]\" $XONSH_TOKEN = \"[REDACTED]\" $env:PATH = 'C:\\bin'"
        );
        let ordinary = "count = 5 and a == b";
        assert_eq!(redact_command(ordinary), ordinary);
    }

    #[test]
    fn test_redact_command_covers_quoted_and_chained_assignments() {
        assert_eq!(
            redact_command("+ export 'API_KEY=secret'"),
            "+ export 'API_KEY=\"[REDACTED]\""
        );
        assert_eq!(
            redact_command("cmd;API_KEY=secret next&&TOKEN=x|(SECRET=y) `KEY=z` {PATH=/bin}"),
            "cmd;API_KEY=\"[REDACTED]\" next&&TOKEN=\"[REDACTED]\" `KEY=\"[REDACTED]\" {PATH=/bin}"
        );
        assert_eq!(
            redact_command(
                "1API_KEY=digit-secret API_KEY=$(cat<<<hunter2) TOKEN=$(printf '%s' \"sub (secret)\") next"
            ),
            "1API_KEY=\"[REDACTED]\" API_KEY=\"[REDACTED]\" TOKEN=\"[REDACTED]\" next"
        );
        assert_eq!(
            redact_command("PATH=/bin;API_KEY=chained-secret HOME=/tmp&&TOKEN=and-secret next"),
            "PATH=/bin;API_KEY=\"[REDACTED]\" HOME=/tmp&&TOKEN=\"[REDACTED]\" next"
        );
        assert_eq!(
            redact_command("\"API_KEY=secret\" --log-level=debug -Dprop=value"),
            "\"API_KEY=\"[REDACTED]\" --log-level=debug -Dprop=\"[REDACTED]\""
        );
        assert_eq!(
            redact_command("docker exec -eAPI_KEY=argument-secret -ePATH=/usr/bin -e=x"),
            "docker exec -eAPI_KEY=\"[REDACTED]\" -ePATH=/usr/bin -e=x"
        );
        assert_eq!(
            redact_command(
                "docker run -e API-KEY=hyphen-secret -eSERVICE-TOKEN=compact-secret --log-level=debug"
            ),
            "docker run -e API-KEY=\"[REDACTED]\" -eSERVICE-TOKEN=\"[REDACTED]\" --log-level=debug"
        );
        assert_eq!(
            redact_command("--env=API_KEY=secret --env=PATH=/usr/bin --level=debug"),
            "--env=API_KEY=\"[REDACTED]\" --env=PATH=/usr/bin --level=debug"
        );
        assert_eq!(
            redact_command("API_KEY='quoted value' TOKEN=\"a \\\" b\" KEY=plain. SECRET= done"),
            "API_KEY=\"[REDACTED]\" TOKEN=\"[REDACTED]\" KEY=\"[REDACTED]\". SECRET= done"
        );
        assert_eq!(
            redact_command("API_KEY=$'argument secret' TOKEN=$\"locale secret\" PRICE=$5 next"),
            "API_KEY=\"[REDACTED]\" TOKEN=\"[REDACTED]\" PRICE=\"[REDACTED]\" next"
        );
        assert_eq!(
            redact_command("API_KEY=prefix,secret-suffix HOSTS=[a,b]{c} next"),
            "API_KEY=\"[REDACTED]\" HOSTS=\"[REDACTED]\" next"
        );
        assert_eq!(
            redact_command(
                "API_KEY=prefix\"secret-suffix\" TOKEN=a'b c'd SECRET=\"unterminated next"
            ),
            "API_KEY=\"[REDACTED]\" TOKEN=\"[REDACTED]\" SECRET=\"[REDACTED]\""
        );
        assert_eq!(
            redact_command("KEY=prefix\"stray-secret next"),
            "KEY=\"[REDACTED]\" next"
        );
        assert_eq!(
            redact_command("API_KEY=prefix$(printf hunter2"),
            "API_KEY=\"[REDACTED]\""
        );
        assert_eq!(
            redact_command("https://example.com/?a=1&token=secret&b=2"),
            "https://example.com/?a=\"[REDACTED]\""
        );
    }

    #[test]
    fn test_redact_command_masks_secret_shapes_without_a_name() {
        let input = "clone with ghp_16C7e42F292c6912E7710c838347Ae178B4a, \
             header Authorization: Bearer abcdef123456 \
             short Authorization: Basic YTpi padded Authorization: Basic YQ== \
             jwt eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIiwiaWF0IjoxNTE2MjM5MDIyfQ.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c. \
             aws AKIAIOSFODNN7EXAMPLE slack xoxb-1234567890-1234567890123-AbCdEfGhIjKlMnOpQrStUvWx \
             anthropic sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789 \
             url postgres://user:hunter2@db/app and https://ghp_16C7e42F292c6912E7710c838347Ae178B4a@github.com/x \
             comma postgres://user:hunter2,tail@db/app at mysql://admin:p@ss;word@db:3306/app \
             list postgres://u:first-secret@db,postgres://u:second-secret@backup/x \
             key -----BEGIN OPENSSH PRIVATE KEY-----\n\
             b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW\n\
             -----END OPENSSH PRIVATE KEY----- done";
        assert_eq!(
            redact_command(input),
            "clone with [REDACTED], \
             header Authorization: Bearer [REDACTED] \
             short Authorization: Basic [REDACTED] padded Authorization: Basic [REDACTED] \
             jwt [REDACTED]. \
             aws [REDACTED] slack [REDACTED] \
             anthropic [REDACTED] \
             url postgres://[REDACTED]@db/app and https://[REDACTED]@github.com/x \
             comma postgres://[REDACTED]@db/app at mysql://[REDACTED]@db:3306/app \
             list postgres://[REDACTED]@db,postgres://[REDACTED]@backup/x \
             key [REDACTED] done"
        );

        let ordinary = "ghp_short Bearer token Basic auth sk-learn steps.publish.outputs https://example.com/@user AKIA1234 \
             -----BEGIN PUBLIC KEY-----";
        assert_eq!(redact_command(ordinary), ordinary);
    }

    #[test]
    fn test_redact_json_environment_redacts_every_header() {
        let mut value = serde_json::json!({
            "headers": { "PATH": "Bearer hunter2", "USER": "hunter2" },
            "env": { "PATH": "/usr/bin" },
            "server": { "headers": [{ "name": "HOME", "value": "hunter2" }, "TERM=hunter2"] }
        });
        redact_json_environment(&mut value);
        assert_eq!(
            value,
            serde_json::json!({
                "headers": { "PATH": "[REDACTED]", "USER": "[REDACTED]" },
                "env": { "PATH": "/usr/bin" },
                "server": { "headers": [{ "name": "HOME", "value": "[REDACTED]" }, "TERM=[REDACTED]"] }
            })
        );
    }

    #[test]
    fn test_redact_command_covers_lowercase_names_and_leaves_flags_alone() {
        let input = "DATABASE_URL=postgres://user:hunter2@db/app\nhttp_proxy=http://user:pass@proxy:3128 --log-level=debug PATH=/usr/bin";
        assert_eq!(
            redact_command(input),
            "DATABASE_URL=\"[REDACTED]\"\nhttp_proxy=\"[REDACTED]\" --log-level=debug PATH=/usr/bin"
        );
    }

    #[test]
    fn test_redact_arguments_redacts_each_argument() {
        assert_eq!(
            redact_arguments([
                "--stdio",
                "-eTOKEN=option-secret",
                "xoxb-1234567890-1234567890123-AbCdEfGhIjKlMnOpQrStUvWx"
            ]),
            ["--stdio", "-eTOKEN=\"[REDACTED]\"", "[REDACTED]"]
        );
        assert_eq!(
            redact_arguments([
                "--token",
                "separate-secret",
                "--verbose",
                "--password",
                "-hunter2",
                "--next",
                "--api-key",
                "--token",
                "chained-secret",
                "plain"
            ]),
            [
                "--token",
                "[REDACTED]",
                "--verbose",
                "--password",
                "[REDACTED]",
                "--next",
                "--api-key",
                "[REDACTED]",
                "[REDACTED]",
                "plain"
            ]
        );
    }

    #[test]
    fn test_redact_command_covers_secret_named_options() {
        assert_eq!(
            redact_command(
                "--API_KEY=hunter2 --TOKEN=custom-secret --password hunter2 -token go-secret \
                 --api-key \"quoted secret\" --token-file --verbose --log-level=debug"
            ),
            "--API_KEY=\"[REDACTED]\" --TOKEN=\"[REDACTED]\" --password [REDACTED] -token [REDACTED] \
             --api-key [REDACTED] --token-file [REDACTED] --log-level=debug"
        );
        assert_eq!(
            redact_command(
                "--DB_PASS=hunter2 --SIGNING_KEY=hunter2 --LICENSE=hunter2 --keymap=vim --passthrough=1"
            ),
            "--DB_PASS=\"[REDACTED]\" --SIGNING_KEY=\"[REDACTED]\" --LICENSE=\"[REDACTED]\" --keymap=vim --passthrough=1"
        );
        assert_eq!(
            redact_command("--password -hunter2 --api-key --token chained-secret plain"),
            "--password [REDACTED] --api-key [REDACTED] [REDACTED] plain"
        );
        assert_eq!(
            redact_command(r#"["--token", "debug-secret"]"#),
            r#"["--token", [REDACTED]"#
        );
    }

    #[test]
    fn test_command_debug_omits_the_environment() {
        let mut command = crate::command::new_command("ssh");
        command
            .args(["-T", "host", "API_KEY=argument-secret run"])
            .current_dir(std::env::temp_dir().join("API_KEY=directory-secret"))
            .env("DATABASE_URL", "postgres://user:hunter2@db/app");
        let description = format!("{command:?}");
        assert!(
            description.starts_with(r#""ssh" "-T" "host" "API_KEY=\"[REDACTED]\" run" in "#),
            "{description}"
        );
        for secret in [
            "argument-secret",
            "directory-secret",
            "DATABASE_URL",
            "hunter2",
        ] {
            assert!(!description.contains(secret), "{description}");
        }
    }
}
