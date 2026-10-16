use std::sync::Arc;

use collections::HashMap;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings_macros::{MergeFrom, with_fallible_options};

#[with_fallible_options]
#[derive(Debug, Default, PartialEq, Clone, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ExtensionSettingsContent {
    /// The extensions that should be automatically installed by Zed.
    ///
    /// This is used to make functionality provided by extensions (e.g., language support)
    /// available out-of-the-box.
    ///
    /// Default: { "html": true }
    #[serde(default)]
    pub auto_install_extensions: HashMap<Arc<str>, AutoInstallExtensionContent>,
    #[serde(default)]
    pub auto_update_extensions: HashMap<Arc<str>, bool>,
    /// Whether to suggest installing extensions based on the files you open.
    ///
    /// Default: true
    pub suggest_extensions: Option<bool>,
    /// The capabilities granted to extensions.
    pub granted_extension_capabilities: Option<Vec<ExtensionCapabilityContent>>,
}

/// Where an entry in `auto_install_extensions` installs its extension from.
#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum AutoInstallExtensionContent {
    /// Whether to install the extension from the extension marketplace.
    Marketplace(bool),
    /// Install the extension from a git repository and build it like a dev extension.
    Git {
        /// The repository URL, as accepted by `git fetch`.
        git: String,
        /// The branch, tag or commit to check out.
        ///
        /// Default: the remote's HEAD
        #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
        reference: Option<String>,
        /// The directory inside the repository that holds `extension.toml`.
        ///
        /// Default: the repository root
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
}

impl crate::merge_from::MergeFrom for AutoInstallExtensionContent {
    fn merge_from(&mut self, other: &Self) {
        *self = other.clone();
    }
}

/// A capability for an extension.
#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExtensionCapabilityContent {
    #[serde(rename = "process:exec")]
    ProcessExec {
        /// The command to execute.
        command: String,
        /// The arguments to pass to the command. Use `*` for a single wildcard argument.
        /// If the last element is `**`, then any trailing arguments are allowed.
        args: Vec<String>,
    },
    DownloadFile {
        host: String,
        path: Vec<String>,
    },
    #[serde(rename = "npm:install")]
    NpmInstallPackage {
        package: String,
    },
}
