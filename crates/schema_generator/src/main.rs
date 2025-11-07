use anyhow::Result;
use clap::{Parser, ValueEnum};
use schemars::{schema_for, JsonSchema};
use settings::{KeymapFile, UserSettingsContent};
use task::{DebugTaskFile, TaskTemplates};
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

fn main() -> Result<()> {
    env_logger::init();

    let args = Args::parse();

    match args.schema_type {
        SchemaType::Theme => print_schema::<ThemeFamilyContent>()?,
        SchemaType::IconTheme => print_schema::<IconThemeFamilyContent>()?,
        SchemaType::Settings => print_schema::<UserSettingsContent>()?,
        SchemaType::Tasks => print_schema::<TaskTemplates>()?,
        SchemaType::Debug => print_schema::<DebugTaskFile>()?,
        SchemaType::Keymap => print_schema::<KeymapFile>()?,
    }

    Ok(())
}
