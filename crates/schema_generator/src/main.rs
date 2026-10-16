use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, ValueEnum};
use extension::ExtensionManifest;
use language_core::LanguageConfig;
use schemars::{Schema, schema_for};
use settings::ProjectSettingsContent;
use theme::IconThemeFamilyContent;
use theme_settings::ThemeFamilyContent;

#[derive(Parser, Debug)]
pub struct Args {
    #[arg(value_enum)]
    pub schema_type: SchemaType,

    /// The path to write the output to.
    #[arg(long, short)]
    pub output: Option<PathBuf>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
#[clap(rename_all = "snake_case")]
pub enum SchemaType {
    Theme,
    IconTheme,
    Project,
    Extension,
    LanguageConfig,
}

fn generate(schema_type: SchemaType) -> Schema {
    match schema_type {
        SchemaType::Theme => schema_for!(ThemeFamilyContent),
        SchemaType::IconTheme => schema_for!(IconThemeFamilyContent),
        SchemaType::Project => schema_for!(ProjectSettingsContent),
        SchemaType::Extension => schema_for!(ExtensionManifest),
        SchemaType::LanguageConfig => schema_for!(LanguageConfig),
    }
}

fn schema_json(schema_type: SchemaType) -> serde_json::Result<String> {
    json_schema_sort::to_string_sorted(&serde_json::to_string(&generate(schema_type))?)
}

fn main() -> Result<()> {
    env_logger::init();

    let args = Args::parse();

    let schema_json = schema_json(args.schema_type)?;

    if let Some(output_path) = args.output {
        let mut file = File::create(output_path)?;
        file.write_all(schema_json.as_bytes())?;
    } else {
        print!("{}", schema_json);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context as _;
    use extension::SchemaVersion;
    use std::path::Path;

    fn repository_root() -> Result<&'static Path> {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .context("schema_generator is not two directories below the repository root")
    }

    fn committed_schema_text(file_name: &str) -> Result<String> {
        let path = repository_root()?
            .join("crates/extension_api/schemas")
            .join(file_name);
        std::fs::read_to_string(&path).with_context(|| format!("reading {path:?}"))
    }

    fn committed_schema(file_name: &str) -> Result<serde_json::Value> {
        serde_json::from_str(&committed_schema_text(file_name)?)
            .with_context(|| format!("parsing {file_name}"))
    }

    fn subdirectories(path: &Path) -> Result<Vec<PathBuf>> {
        let mut directories = Vec::new();
        for entry in std::fs::read_dir(path).with_context(|| format!("listing {path:?}"))? {
            let path = entry.with_context(|| format!("listing {path:?}"))?.path();
            if path.is_dir() {
                directories.push(path);
            }
        }
        directories.sort();
        Ok(directories)
    }

    fn validation_errors(schema: &serde_json::Value, toml_content: &str) -> Result<Vec<String>> {
        let instance = toml::from_str::<serde_json::Value>(toml_content)
            .with_context(|| format!("parsing TOML:\n{toml_content}"))?;
        Ok(jsonschema::validator_for(schema)
            .context("compiling the schema")?
            .iter_errors(&instance)
            .map(|error| error.to_string())
            .collect())
    }

    fn assert_valid(schema: &serde_json::Value, label: &str, toml_content: &str) -> Result<()> {
        let errors = validation_errors(schema, toml_content)?;
        assert!(errors.is_empty(), "{label}: {errors:#?}");
        Ok(())
    }

    fn assert_invalid(schema: &serde_json::Value, toml_content: &str) -> Result<()> {
        assert!(
            !validation_errors(schema, toml_content)?.is_empty(),
            "the schema accepted:\n{toml_content}"
        );
        Ok(())
    }

    fn assert_files_valid(schema: &serde_json::Value, paths: &[PathBuf]) -> Result<()> {
        for path in paths {
            let content =
                std::fs::read_to_string(path).with_context(|| format!("reading {path:?}"))?;
            assert_valid(schema, &path.display().to_string(), &content)?;
        }
        Ok(())
    }

    fn manifest(schema_version: i32, grammar: &str) -> String {
        format!(
            "id = \"gleam\"\nname = \"Gleam\"\nversion = \"0.1.0\"\nschema_version = {schema_version}\n\n[grammars.gleam]\nrepository = \"https://github.com/gleam-lang/tree-sitter-gleam\"\n{grammar}"
        )
    }

    #[test]
    fn test_committed_extension_schemas_match_the_generated_ones() -> Result<()> {
        for (schema_type, file_name, argument) in [
            (SchemaType::Extension, "extension.schema.json", "extension"),
            (
                SchemaType::LanguageConfig,
                "language-config.schema.json",
                "language_config",
            ),
        ] {
            assert_eq!(
                committed_schema_text(file_name)?,
                schema_json(schema_type)?,
                "run `cargo run -p schema_generator -- {argument} --output crates/extension_api/schemas/{file_name}`"
            );
        }
        Ok(())
    }

    #[test]
    fn test_repository_extensions_match_the_extension_schemas() -> Result<()> {
        let repository_root = repository_root()?;
        let extensions = subdirectories(&repository_root.join("extensions"))?;
        let manifests = extensions
            .iter()
            .map(|extension| extension.join("extension.toml"))
            .filter(|path| path.is_file())
            .collect::<Vec<_>>();
        let mut language_directories =
            subdirectories(&repository_root.join("crates/grammars/src"))?;
        for extension in &extensions {
            let languages = extension.join("languages");
            if languages.is_dir() {
                language_directories.extend(subdirectories(&languages)?);
            }
        }
        let language_configs = language_directories
            .into_iter()
            .map(|language| language.join("config.toml"))
            .filter(|path| path.is_file())
            .collect::<Vec<_>>();
        assert!(!manifests.is_empty());
        assert!(!language_configs.is_empty());

        assert_files_valid(&committed_schema("extension.schema.json")?, &manifests)?;
        assert_files_valid(
            &committed_schema("language-config.schema.json")?,
            &language_configs,
        )
    }

    #[test]
    fn test_language_config_schema_accepts_both_block_comment_forms() -> Result<()> {
        let schema = generate(SchemaType::LanguageConfig).to_value();
        for config in [
            "name = \"C\"\nblock_comment = [\"/*\", \"*/\"]\n",
            "name = \"C\"\nblock_comment = { start = \"/*\", end = \"*/\", prefix = \"* \", tab_size = 0 }\n",
        ] {
            LanguageConfig::from_toml(config)?;
            assert_valid(&schema, config, config)?;
        }
        Ok(())
    }

    #[test]
    fn test_extension_schemas_accept_serde_aliases() -> Result<()> {
        let manifest_schema = generate(SchemaType::Extension).to_value();
        for grammar in ["rev = \"abc\"\n", "commit = \"abc\"\n"] {
            let manifest = manifest(1, grammar);
            toml::from_str::<ExtensionManifest>(&manifest)?;
            assert_valid(&manifest_schema, &manifest, &manifest)?;
        }
        for grammar in ["", "rev = \"abc\"\ncommit = \"abc\"\n"] {
            let manifest = manifest(1, grammar);
            assert!(toml::from_str::<ExtensionManifest>(&manifest).is_err());
            assert_invalid(&manifest_schema, &manifest)?;
        }

        let language_schema = generate(SchemaType::LanguageConfig).to_value();
        let documentation = "{ start = \"/**\", end = \"*/\", prefix = \"* \", tab_size = 1 }";
        for config in [
            format!("name = \"C\"\ndocumentation = {documentation}\n"),
            format!("name = \"C\"\ndocumentation_comment = {documentation}\n"),
            "name = \"C\"\nsoft_wrap = \"preferred_line_length\"\n".to_string(),
            "name = \"C\"\nsoft_wrap = \"bounded\"\n".to_string(),
        ] {
            LanguageConfig::from_toml(&config)?;
            assert_valid(&language_schema, &config, &config)?;
        }
        let both_names = format!(
            "name = \"C\"\ndocumentation = {documentation}\ndocumentation_comment = {documentation}\n"
        );
        assert!(LanguageConfig::from_toml(&both_names).is_err());
        assert_invalid(&language_schema, &both_names)
    }

    #[test]
    fn test_extension_schema_rejects_schema_versions_above_the_current_one() -> Result<()> {
        let schema = generate(SchemaType::Extension).to_value();
        let grammar = "rev = \"abc\"\n";
        let current = manifest(SchemaVersion::CURRENT.0, grammar);
        assert_valid(&schema, &current, &current)?;
        assert_invalid(&schema, &manifest(SchemaVersion::CURRENT.0 + 1, grammar))
    }
}
