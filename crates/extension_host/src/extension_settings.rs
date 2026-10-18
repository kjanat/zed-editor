use collections::HashMap;
use extension::{
    DownloadFileCapability, ExtensionCapability, NpmInstallPackageCapability, ProcessExecCapability,
};
use settings::{AutoInstallExtensionContent, RegisterSetting, Settings};
use std::sync::Arc;

#[derive(Debug, Default, Clone, RegisterSetting)]
pub struct ExtensionSettings {
    /// The extensions that should be automatically installed by Zed.
    ///
    /// This is used to make functionality provided by extensions (e.g., language support)
    /// available out-of-the-box.
    ///
    /// Default: { "html": true }
    pub auto_install_extensions: HashMap<Arc<str>, AutoInstallExtensionContent>,
    pub auto_update_extensions: HashMap<Arc<str>, bool>,
    pub suggest_extensions: bool,
    pub granted_capabilities: Vec<ExtensionCapability>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GitExtensionSource {
    pub url: String,
    pub reference: Option<String>,
    pub path: Option<String>,
}

impl ExtensionSettings {
    /// Returns whether the given extension should be auto-installed.
    pub fn should_auto_install(&self, extension_id: &str) -> bool {
        match self.auto_install_extensions.get(extension_id) {
            Some(AutoInstallExtensionContent::Marketplace(install)) => *install,
            Some(AutoInstallExtensionContent::Git { .. }) | None => true,
        }
    }

    pub fn marketplace_extensions(&self) -> impl Iterator<Item = &Arc<str>> {
        self.auto_install_extensions
            .iter()
            .filter(|(_, entry)| matches!(entry, AutoInstallExtensionContent::Marketplace(_)))
            .map(|(id, _)| id)
    }

    pub fn git_sources(&self) -> impl Iterator<Item = (&Arc<str>, GitExtensionSource)> {
        self.auto_install_extensions
            .iter()
            .filter_map(|(id, entry)| match entry {
                AutoInstallExtensionContent::Git {
                    git,
                    reference,
                    path,
                } => Some((
                    id,
                    GitExtensionSource {
                        url: git.clone(),
                        reference: reference.clone(),
                        path: path.clone(),
                    },
                )),
                AutoInstallExtensionContent::Marketplace(_) => None,
            })
    }

    pub fn should_auto_update(&self, extension_id: &str) -> bool {
        self.auto_update_extensions
            .get(extension_id)
            .copied()
            .unwrap_or(true)
    }
}

impl Settings for ExtensionSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self {
            auto_install_extensions: content.extension.auto_install_extensions.clone(),
            auto_update_extensions: content.extension.auto_update_extensions.clone(),
            suggest_extensions: content.extension.suggest_extensions.unwrap_or(true),
            granted_capabilities: content
                .extension
                .granted_extension_capabilities
                .clone()
                .unwrap_or_default()
                .into_iter()
                .map(|capability| match capability {
                    settings::ExtensionCapabilityContent::ProcessExec { command, args } => {
                        ExtensionCapability::ProcessExec(ProcessExecCapability { command, args })
                    }
                    settings::ExtensionCapabilityContent::DownloadFile { host, path } => {
                        ExtensionCapability::DownloadFile(DownloadFileCapability { host, path })
                    }
                    settings::ExtensionCapabilityContent::NpmInstallPackage { package } => {
                        ExtensionCapability::NpmInstallPackage(NpmInstallPackageCapability {
                            package,
                        })
                    }
                })
                .collect(),
        }
    }
}
