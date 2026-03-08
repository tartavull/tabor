use serde::Serialize;
use tabor_config_derive::ConfigDeserialize;

#[derive(ConfigDeserialize, Serialize, Default, Clone, Debug, PartialEq)]
pub struct Browser {
    /// Render a browser viewport into multiple columns.
    pub multi_column: MultiColumnBrowserConfig,
}

#[derive(ConfigDeserialize, Serialize, Clone, Debug, PartialEq)]
pub struct MultiColumnBrowserConfig {
    /// Target logical width for folded browser columns, in device-independent pixels.
    pub target_width_px: usize,
}

impl Default for MultiColumnBrowserConfig {
    fn default() -> Self {
        Self { target_width_px: 900 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_column_browser_target_width_defaults_to_900() {
        assert_eq!(Browser::default().multi_column.target_width_px, 900);
    }
}
