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

    #[error("rate limited")]
    RateLimited { retry_after: Option<u64> },

    #[error("checkpoint")]
    Checkpointed,
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

    pub fn rate_limited(retry_after: Option<u64>) -> Self {
        Self::RateLimited { retry_after }
    }

    pub fn checkpointed() -> Self {
        Self::Checkpointed
    }

    pub fn error_code(&self) -> &'static str {
        match self {
            Self::NoData(_) | Self::RateLimited { .. } | Self::Checkpointed => "C",
            Self::Parse { .. } => "P",
            Self::Http(_) | Self::Io(_) | Self::Json(_) | Self::Yaml(_) => "U",
            Self::Other(_) => "X",
        }
    }
}

pub type FacebedResult<T> = Result<T, FacebedError>;

#[cfg(test)]
mod tests {
    use super::FacebedError;

    #[test]
    fn rate_limit_and_checkpoint_render_as_c() {
        assert_eq!(FacebedError::rate_limited(Some(30)).error_code(), "C");
        assert_eq!(FacebedError::checkpointed().error_code(), "C");
    }
}
