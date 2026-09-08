use collections::{BTreeMap, BTreeSet};
use extension::{ExtensionGrammarProxy, ExtensionHostProxy, ExtensionLanguageProxy};
use language::LanguageName;
use std::sync::Arc;

use crate::ExtensionIndex;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Provider {
    BuiltIn,
    Extension(Arc<str>),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LanguageCollisionKey {
    BuiltIn(Arc<str>),
    Extensions(Arc<str>, Arc<str>),
}

impl LanguageCollisionKey {
    fn providers(&self) -> (Provider, Provider) {
        match self {
            Self::BuiltIn(extension) => (Provider::BuiltIn, Provider::Extension(extension.clone())),
            Self::Extensions(first, second) => (
                Provider::Extension(first.clone()),
                Provider::Extension(second.clone()),
            ),
        }
    }
}

#[derive(Default)]
struct CollisionDetails {
    resources: Vec<String>,
    conflicting_resources: Vec<String>,
}

enum Resource<'a> {
    Language(&'a str),
    Grammar(&'a str),
}

impl Resource<'_> {
    fn conflicts(&self, index: &ExtensionIndex, first: &Provider, second: &Provider) -> bool {
        let Self::Grammar(name) = self else {
            return true;
        };
        let (Provider::Extension(first), Provider::Extension(second)) = (first, second) else {
            return true;
        };
        let source = |id| {
            index
                .extensions
                .get(id)
                .and_then(|entry| entry.manifest.grammars.get(*name))
        };
        match (source(first), source(second)) {
            (Some(first), Some(second)) => first != second,
            _ => true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LanguageCollision {
    pub key: LanguageCollisionKey,
    pub providers: (String, String),
    pub resources: Vec<String>,
}

#[derive(Default)]
pub(crate) struct LanguageCollisions {
    diagnostics: BTreeMap<LanguageCollisionKey, LanguageCollision>,
    active: BTreeMap<LanguageCollisionKey, LanguageCollision>,
    warned: BTreeSet<LanguageCollisionKey>,
}

impl LanguageCollisions {
    pub fn update(
        &mut self,
        index: &ExtensionIndex,
        registered_languages: &BTreeMap<LanguageName, Arc<str>>,
        grammar_providers: &BTreeMap<Arc<str>, Arc<str>>,
        proxy: &ExtensionHostProxy,
    ) {
        let mut active = BTreeMap::new();
        for (name, providers) in &index.language_providers {
            let native = proxy.is_native_language(name);
            let winner = if native {
                Some(Provider::BuiltIn)
            } else {
                registered_languages
                    .get(name)
                    .cloned()
                    .map(Provider::Extension)
            };
            Self::record(
                &mut active,
                index,
                Resource::Language(name.as_ref()),
                providers,
                native,
                winner,
            );
        }

        let mut grammars: BTreeMap<Arc<str>, BTreeSet<Arc<str>>> = BTreeMap::new();
        for (id, extension) in &index.extensions {
            for name in extension.manifest.grammars.keys() {
                grammars.entry(name.clone()).or_default().insert(id.clone());
            }
        }
        for (name, providers) in grammars {
            let native = proxy.is_native_grammar(&name);
            let winner = if native {
                Some(Provider::BuiltIn)
            } else {
                grammar_providers
                    .get(&name)
                    .cloned()
                    .map(Provider::Extension)
            };
            Self::record(
                &mut active,
                index,
                Resource::Grammar(&name),
                &providers,
                native,
                winner,
            );
        }

        let mut diagnostics = BTreeMap::new();
        self.active.clear();
        for (pair, details) in active {
            let (first, second) = pair.providers();
            let collision = LanguageCollision {
                key: pair.clone(),
                providers: (
                    Self::describe(index, &first),
                    Self::describe(index, &second),
                ),
                resources: details.resources,
            };
            if self.diagnostics.get(&pair) != Some(&collision) {
                log::warn!(
                    "duplicate language/grammar providers {} and {}: {}",
                    collision.providers.0,
                    collision.providers.1,
                    collision.resources.join("; ")
                );
            }
            if !details.conflicting_resources.is_empty() {
                self.active.insert(
                    pair.clone(),
                    LanguageCollision {
                        resources: details.conflicting_resources,
                        ..collision.clone()
                    },
                );
            }
            diagnostics.insert(pair, collision);
        }
        self.diagnostics = diagnostics;
    }

    pub fn active_warnings(&self) -> &BTreeMap<LanguageCollisionKey, LanguageCollision> {
        &self.active
    }

    pub fn take_warnings(&mut self) -> Vec<LanguageCollision> {
        self.active
            .iter()
            .filter_map(|(pair, collision)| {
                self.warned.insert(pair.clone()).then(|| collision.clone())
            })
            .collect()
    }

    fn record(
        active: &mut BTreeMap<LanguageCollisionKey, CollisionDetails>,
        index: &ExtensionIndex,
        resource: Resource<'_>,
        extensions: &BTreeSet<Arc<str>>,
        native: bool,
        winner: Option<Provider>,
    ) {
        let mut providers = BTreeSet::new();
        if native {
            providers.insert(Provider::BuiltIn);
        }
        providers.extend(
            extensions
                .iter()
                .filter(|id| index.extensions.contains_key(*id))
                .cloned()
                .map(Provider::Extension),
        );
        let winner = winner
            .as_ref()
            .map(|provider| Self::describe(index, provider))
            .unwrap_or_else(|| "none registered".into());
        for (position, first) in providers.iter().enumerate() {
            for second in providers.iter().skip(position + 1) {
                let (kind, name) = match resource {
                    Resource::Language(name) => ("language", name),
                    Resource::Grammar(name) => ("grammar", name),
                };
                let conflicts = resource.conflicts(index, first, second);
                let mut description = format!("{kind} {name:?} (active provider: {winner})");
                if !conflicts {
                    description.push_str(" (matching source declarations)");
                }
                let pair = match (first, second) {
                    (Provider::BuiltIn, Provider::Extension(extension)) => {
                        LanguageCollisionKey::BuiltIn(extension.clone())
                    }
                    (Provider::Extension(first), Provider::Extension(second)) => {
                        LanguageCollisionKey::Extensions(first.clone(), second.clone())
                    }
                    _ => continue,
                };
                let details = active.entry(pair).or_default();
                if conflicts {
                    details.conflicting_resources.push(description.clone());
                }
                details.resources.push(description);
            }
        }
    }

    fn describe(index: &ExtensionIndex, provider: &Provider) -> String {
        match provider {
            Provider::BuiltIn => "Zed built-in support".into(),
            Provider::Extension(id) => match index.extensions.get(id) {
                Some(entry) => format!(
                    "{} ({id}, v{})",
                    entry.manifest.name, entry.manifest.version
                ),
                None => id.to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ExtensionIndexEntry, ExtensionManifest};

    #[test]
    fn matching_grammar_sources_remain_diagnosable() -> anyhow::Result<()> {
        let mut index = ExtensionIndex::default();
        for id in ["alpha", "beta"] {
            let manifest: ExtensionManifest = toml::from_str(&format!(
                "id = \"{id}\"\nname = \"{id}\"\nversion = \"1.0.0\"\nschema_version = 1\nauthors = []\n[grammars.shared]\nrepository = \"https://example.com/grammar\"\nrev = \"same-revision\"\n"
            ))?;
            index.extensions.insert(
                id.into(),
                ExtensionIndexEntry {
                    manifest: Arc::new(manifest),
                    dev: false,
                },
            );
        }
        let mut collisions = LanguageCollisions::default();
        collisions.update(
            &index,
            &BTreeMap::new(),
            &BTreeMap::from_iter([("shared".into(), "beta".into())]),
            &ExtensionHostProxy::new(),
        );
        assert_eq!(collisions.diagnostics.len(), 1);
        let diagnostic = collisions
            .diagnostics
            .values()
            .next()
            .ok_or_else(|| anyhow::anyhow!("missing duplicate diagnostic"))?;
        assert_eq!(diagnostic.resources.len(), 1);
        assert!(
            diagnostic
                .resources
                .iter()
                .all(|resource| resource.contains("active provider: beta"))
        );
        assert!(collisions.take_warnings().is_empty());
        assert!(
            collisions.warned.is_empty(),
            "diagnosing identical sources must not consume the session warning"
        );
        Ok(())
    }
}
