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

type ProviderPair = (Provider, Provider);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LanguageCollision {
    pub providers: (String, String),
    pub resources: Vec<String>,
}

#[derive(Default)]
pub(crate) struct LanguageCollisions {
    active: BTreeMap<ProviderPair, LanguageCollision>,
    warned: BTreeSet<ProviderPair>,
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
                "language",
                name.as_ref(),
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
                "grammar",
                &name,
                &providers,
                native,
                winner,
            );
        }

        for (pair, collision) in &active {
            if self.active.get(pair) != Some(collision) {
                log::warn!(
                    "language/grammar collision between {} and {}: {}",
                    collision.providers.0,
                    collision.providers.1,
                    collision.resources.join("; ")
                );
            }
        }
        self.active = active;
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
        active: &mut BTreeMap<ProviderPair, LanguageCollision>,
        index: &ExtensionIndex,
        kind: &str,
        name: &str,
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
                active
                    .entry((first.clone(), second.clone()))
                    .or_insert_with(|| LanguageCollision {
                        providers: (Self::describe(index, first), Self::describe(index, second)),
                        resources: Vec::new(),
                    })
                    .resources
                    .push(format!("{kind} {name:?} (active provider: {winner})"));
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
