use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WindowKind {
    Terminal,
    Web { url: String },
}

impl Default for WindowKind {
    fn default() -> Self {
        Self::Terminal
    }
}

impl WindowKind {
    pub fn is_web(&self) -> bool {
        matches!(self, Self::Web { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TabKind {
    Terminal,
    Web { url: String },
}

impl From<&WindowKind> for TabKind {
    fn from(kind: &WindowKind) -> Self {
        match kind {
            WindowKind::Terminal => Self::Terminal,
            WindowKind::Web { url } => Self::Web { url: url.clone() },
        }
    }
}
