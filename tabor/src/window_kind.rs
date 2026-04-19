use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[derive(Default)]
pub enum WindowKind {
    #[default]
    Terminal,
    Web {
        url: String,
    },
    Image {
        source: String,
    },
    Pdf {
        source: String,
    },
}

impl WindowKind {
    pub fn is_web(&self) -> bool {
        matches!(self, Self::Web { .. })
    }

    pub fn is_image(&self) -> bool {
        matches!(self, Self::Image { .. })
    }

    pub fn is_pdf(&self) -> bool {
        matches!(self, Self::Pdf { .. })
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Terminal)
    }

    pub fn has_status_bar(&self) -> bool {
        !self.is_terminal()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TabKind {
    Terminal,
    Web { url: String },
    Image { source: String },
    Pdf { source: String },
}

impl From<&WindowKind> for TabKind {
    fn from(kind: &WindowKind) -> Self {
        match kind {
            WindowKind::Terminal => Self::Terminal,
            WindowKind::Web { url } => Self::Web { url: url.clone() },
            WindowKind::Image { source } => Self::Image { source: source.clone() },
            WindowKind::Pdf { source } => Self::Pdf { source: source.clone() },
        }
    }
}
