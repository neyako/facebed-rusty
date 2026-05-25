use thiserror::Error;

#[derive(Debug, Error)]
pub enum FacebedError {
    #[error("no data: {0}")]
    NoData(String),

    #[error("parse: {message}")]
    Parse {
        message: String,
        html: Option<String>,
        url: Option<String>,
    },

    #[error("http: {0}")]
    Http(#[from] reqwest::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("yaml: {0}")]
    Yaml(#[from] serde_yaml::Error),

    #[error("other: {0}")]
    Other(#[from] anyhow::Error),
}

impl FacebedError {
    pub fn parse(msg: impl Into<String>) -> Self {
        Self::Parse {
            message: msg.into(),
            html: None,
            url: None,
        }
    }

    pub fn parse_with(msg: impl Into<String>, html: String, url: String) -> Self {
        Self::Parse {
            message: msg.into(),
            html: Some(html),
            url: Some(url),
        }
    }

    pub fn no_data(msg: impl Into<String>) -> Self {
        Self::NoData(msg.into())
    }

    pub fn error_code(&self) -> &'static str {
        match self {
            Self::NoData(_) => "C",
            Self::Parse { .. } => "P",
            Self::Http(_) | Self::Io(_) | Self::Json(_) | Self::Yaml(_) => "U",
            Self::Other(_) => "X",
        }
    }
}

pub type FacebedResult<T> = Result<T, FacebedError>;
