use serde::{Deserialize, Deserializer, Serialize, de};
use toml::Value;

use tabor_config_derive::{ConfigDeserialize, SerdeReplace};
use tabor_terminal::term::Osc52;

use crate::config::ui_config::{Program, StringVisitor};

#[derive(ConfigDeserialize, Serialize, Default, Clone, Debug, PartialEq)]
pub struct Terminal {
    /// OSC52 support mode.
    pub osc52: SerdeOsc52,
    /// Path to a shell program to run on startup.
    pub shell: Option<Program>,
    /// Render a narrow logical terminal into multiple columns.
    pub multi_column: MultiColumnTerminalConfig,
}

#[derive(ConfigDeserialize, Serialize, Clone, Debug, PartialEq)]
pub struct MultiColumnTerminalConfig {
    /// Target logical width for folded terminal strips.
    pub target_columns: usize,
    /// Gap between folded strips, in cells.
    pub gutter_columns: usize,
    /// Visual strip ordering for folded terminal rendering.
    pub order: MultiColumnOrder,
}

impl Default for MultiColumnTerminalConfig {
    fn default() -> Self {
        Self { target_columns: 100, gutter_columns: 1, order: MultiColumnOrder::default() }
    }
}

#[derive(SerdeReplace, Serialize, Deserialize, Default, Copy, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MultiColumnOrder {
    EndLeft,
    #[default]
    StartLeft,
}

#[derive(SerdeReplace, Serialize, Default, Copy, Clone, Debug, PartialEq)]
pub struct SerdeOsc52(pub Osc52);

impl<'de> Deserialize<'de> for SerdeOsc52 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = deserializer.deserialize_str(StringVisitor)?;
        Osc52::deserialize(Value::String(value)).map(SerdeOsc52).map_err(de::Error::custom)
    }
}
