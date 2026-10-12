mod context_server;
mod dap;
mod lsp;
mod slash_command;

use std::{ops::Range, path::PathBuf};

use util::redact::{redact_arguments, should_redact};

pub use context_server::*;
pub use dap::*;
pub use lsp::*;
pub use slash_command::*;

/// A list of environment variables.
pub type EnvVars = Vec<(String, String)>;

/// A command.
pub struct Command {
    /// The command to execute.
    pub command: PathBuf,
    /// The arguments to pass to the command.
    pub args: Vec<String>,
    /// The environment variables to set for the command.
    pub env: EnvVars,
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let filtered_env = self
            .env
            .iter()
            .map(|(k, v)| (k, if should_redact(k) { "[REDACTED]" } else { v }))
            .collect::<Vec<_>>();

        f.debug_struct("Command")
            .field("command", &self.command)
            .field("args", &redact_arguments(&self.args))
            .field("env", &filtered_env)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_command_debug_redacts_arguments_and_environment() {
        let command = Command {
            command: PathBuf::from("server"),
            args: vec!["Authorization: Bearer hunter2-bearer-token".to_string()],
            env: vec![(
                "DATABASE_URL".to_string(),
                "postgres://user:hunter2@db/app".to_string(),
            )],
        };
        let description = format!("{command:?}");
        assert!(
            description.contains(r#"args: ["Authorization: Bearer [REDACTED]"]"#),
            "{description}"
        );
        assert!(!description.contains("hunter2"), "{description}");
    }
}

/// A label containing some code.
#[derive(Debug, Clone)]
pub struct CodeLabel {
    /// The source code to parse with Tree-sitter.
    pub code: String,
    /// The spans to display in the label.
    pub spans: Vec<CodeLabelSpan>,
    /// The range of the displayed label to include when filtering.
    pub filter_range: Range<usize>,
}

/// A span within a code label.
#[derive(Debug, Clone)]
pub enum CodeLabelSpan {
    /// A range into the parsed code.
    CodeRange(Range<usize>),
    /// A span containing a code literal.
    Literal(CodeLabelSpanLiteral),
}

/// A span containing a code literal.
#[derive(Debug, Clone)]
pub struct CodeLabelSpanLiteral {
    /// The literal text.
    pub text: String,
    /// The name of the highlight to use for this literal.
    pub highlight_name: Option<String>,
}
