use thiserror::Error;

#[derive(Error, Debug)]
pub enum HoyoverseError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("API error (retcode={retcode}): {message}")]
    Api { retcode: i64, message: String },

    #[error("Authentication required (expired/invalid cookies)")]
    Auth,

    #[error("Target player's data is not public")]
    DataNotPublic,

    #[error("Deserialization failed: {0}")]
    Deserialize(String),

    #[error("{0}")]
    Other(String),
}

impl From<serde_json::Error> for HoyoverseError {
    fn from(e: serde_json::Error) -> Self {
        HoyoverseError::Deserialize(e.to_string())
    }
}
