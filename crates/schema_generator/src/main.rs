use anyhow::Result;
use clap::{Parser, ValueEnum};
use schemars::{schema_for, JsonSchema};
use settings::{KeymapFile, UserSettingsContent};
use task::{AdapterSchemas, DebugTaskFile, TaskTemplates};
use theme::{IconThemeFamilyContent, ThemeFamilyContent};

#[derive(Parser, Debug)]
pub struct Args {
    #[arg(value_enum)]
    pub schema_type: SchemaType,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
#[clap(rename_all = "snake_case")]
pub enum SchemaType {
    Theme,
    IconTheme,
    Settings,
    Tasks,
    Debug,
    Keymap,
}

fn print_schema<T: JsonSchema>() -> Result<()> {
    let schema = schema_for!(T);
    println!("{}", serde_json::to_string_pretty(&schema)?);
    Ok(())
}

fn print_schema_value(schema: serde_json::Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(&schema)?);
    Ok(())
}

fn main() -> Result<()> {
    env_logger::init();

    let args = Args::parse();

    match args.schema_type {
        SchemaType::Theme => print_schema::<ThemeFamilyContent>()?,
        SchemaType::IconTheme => print_schema::<IconThemeFamilyContent>()?,
        SchemaType::Settings => {
            // Settings schema requires runtime initialization for fonts/themes/languages
            // Generate with minimal runtime data
            print_schema::<UserSettingsContent>()?;
            eprintln!("WARNING: Settings schema generated without runtime fonts/themes/languages.");
            eprintln!("For complete schema with autocomplete, use the runtime schema from json_schema_store.");
        }
        SchemaType::Tasks => {
            // Use the proper schema generation method which includes custom transforms
            let schema = TaskTemplates::generate_json_schema();
            print_schema_value(schema)?;
        }
        SchemaType::Debug => {
            // Use the proper schema generation method with default (empty) adapter schemas
            // This ensures BuildTaskDefinition has the correct schema (label is optional)
            // Note: This generates a base schema without adapter-specific validation.
            // At runtime, zed dynamically adds adapter-specific schemas from DapRegistry.
            let schema = DebugTaskFile::generate_json_schema(&AdapterSchemas::default());
            print_schema_value(schema)?;
            eprintln!("WARNING: Debug schema generated without adapter-specific schemas.");
            eprintln!("For complete schema with adapter autocomplete, use the runtime schema from json_schema_store.");
        }
        SchemaType::Keymap => {
            // Keymap schema requires all actions to be registered
            // Initialize zed to register all actions
            zed::stdout_is_a_pty(); // Ensures all actions are registered

            // Generate keymap schema with all registered actions
            let schema = gpui::App::new().update(|cx| {
                KeymapFile::generate_json_schema_for_registered_actions(cx)
            })?;
            print_schema_value(schema)?;
        }
    }

    Ok(())
}
