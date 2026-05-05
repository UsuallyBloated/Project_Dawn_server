//! Server-side error type for auth handlers. Maps cleanly to
//! `protocol::auth::ErrorCode` for wire responses.

use protocol::auth::ErrorCode;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("username already taken")]
    NameTaken,
    #[error("authentication failed")]
    AuthFailed,
    #[error("client version {client} below required {required}")]
    VersionMismatch { client: String, required: String },
    #[error("session token expired or unknown")]
    SessionExpired,
    #[error("account is banned: {0}")]
    Banned(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("not found")]
    NotFound,
    #[error("internal: {0}")]
    Internal(#[from] anyhow::Error),
}

impl AuthError {
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::NameTaken => ErrorCode::NameTaken,
            Self::AuthFailed => ErrorCode::AuthFailed,
            Self::VersionMismatch { .. } => ErrorCode::VersionMismatch,
            Self::SessionExpired => ErrorCode::SessionExpired,
            Self::Banned(_) => ErrorCode::Banned,
            Self::InvalidInput(_) => ErrorCode::InvalidInput,
            Self::NotFound => ErrorCode::NotFound,
            Self::Internal(_) => ErrorCode::Internal,
        }
    }

    pub fn user_msg(&self) -> String {
        match self {
            Self::Internal(_) => "internal server error".into(),
            other => other.to_string(),
        }
    }
}

// Convenience conversions from sqlx errors.
impl From<sqlx::Error> for AuthError {
    fn from(e: sqlx::Error) -> Self {
        AuthError::Internal(anyhow::Error::from(e))
    }
}

impl From<argon2::password_hash::Error> for AuthError {
    fn from(e: argon2::password_hash::Error) -> Self {
        AuthError::Internal(anyhow::anyhow!("password hash error: {e}"))
    }
}

pub type AuthResult<T> = Result<T, AuthError>;
