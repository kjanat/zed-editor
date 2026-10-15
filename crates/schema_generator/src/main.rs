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
    use std::path::Path;

    fn repository_root() -> &'static Path {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap()
    }

    fn committed_schema_text(file_name: &str) -> String {
        let path = repository_root()
            .join("crates/extension_api/schemas")
            .join(file_name);
        std::fs::read_to_string(&path).unwrap()
    }

    fn committed_schema(file_name: &str) -> serde_json::Value {
        serde_json::from_str(&committed_schema_text(file_name)).unwrap()
    }

    fn subdirectories(path: &Path) -> Vec<PathBuf> {
        let mut directories = std::fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.is_dir())
            .collect::<Vec<_>>();
        directories.sort();
        directories
    }

    fn assert_valid(schema: &serde_json::Value, label: &str, toml_content: &str) {
        let instance = toml::from_str::<serde_json::Value>(toml_content).unwrap();
        let errors = jsonschema::validator_for(schema)
            .unwrap()
            .iter_errors(&instance)
            .map(|error| error.to_string())
            .collect::<Vec<_>>();
        assert!(errors.is_empty(), "{label}: {errors:#?}");
    }

    fn assert_files_valid(schema: &serde_json::Value, paths: &[PathBuf]) {
        for path in paths {
            let content = std::fs::read_to_string(path).unwrap();
            assert_valid(schema, &path.display().to_string(), &content);
        }
    }

    #[test]
    fn test_committed_extension_schemas_match_the_generated_ones() {
        for (schema_type, file_name, argument) in [
            (SchemaType::Extension, "extension.schema.json", "extension"),
            (
                SchemaType::LanguageConfig,
                "language-config.schema.json",
                "language_config",
            ),
        ] {
            assert_eq!(
                committed_schema_text(file_name),
                schema_json(schema_type).unwrap(),
                "run `cargo run -p schema_generator -- {argument} --output crates/extension_api/schemas/{file_name}`"
            );
        }
    }

    #[test]
    fn test_repository_extensions_match_the_extension_schemas() {
        let extensions = subdirectories(&repository_root().join("extensions"));
        let manifests = extensions
            .iter()
            .map(|extension| extension.join("extension.toml"))
            .filter(|path| path.is_file())
            .collect::<Vec<_>>();
        let language_configs = extensions
            .iter()
            .map(|extension| extension.join("languages"))
            .filter(|path| path.is_dir())
            .flat_map(|languages| subdirectories(&languages))
            .chain(subdirectories(
                &repository_root().join("crates/grammars/src"),
            ))
            .map(|language| language.join("config.toml"))
            .filter(|path| path.is_file())
            .collect::<Vec<_>>();
        assert!(!manifests.is_empty());
        assert!(!language_configs.is_empty());

        assert_files_valid(&committed_schema("extension.schema.json"), &manifests);
        assert_files_valid(
            &committed_schema("language-config.schema.json"),
            &language_configs,
        );
    }

    #[test]
    fn test_language_config_schema_accepts_both_block_comment_forms() {
        let schema = generate(SchemaType::LanguageConfig).to_value();
        for config in [
            "name = \"C\"\nblock_comment = [\"/*\", \"*/\"]\n",
            "name = \"C\"\nblock_comment = { start = \"/*\", end = \"*/\", prefix = \"* \", tab_size = 0 }\n",
        ] {
            LanguageConfig::from_toml(config).unwrap();
            assert_valid(&schema, config, config);
        }
    }
}
