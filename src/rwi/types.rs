use crate::rwi::proto::{ResponseStatus, RwiError, RwiErrorCode, RwiResponse};
use axum::{
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Clone, Deserialize)]
pub struct RwiRequestEnvelope {
    #[serde(rename = "rwi")]
    pub version: Option<String>,
    pub action_id: Option<String>,
    pub action: Option<String>,
    #[serde(default)]
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AuthError {
    #[error("missing token")]
    MissingToken,
    #[error("invalid authorization header")]
    InvalidAuthorizationHeader,
    #[error("invalid token")]
    InvalidToken,
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        (
            StatusCode::UNAUTHORIZED,
            [(
                header::WWW_AUTHENTICATE,
                r#"Bearer realm="rwi", error="invalid_token""#,
            )],
        )
            .into_response()
    }
}

impl AuthError {
    fn into_rwi_response(self) -> RwiResponse {
        let code = match self {
            AuthError::MissingToken
            | AuthError::InvalidAuthorizationHeader
            | AuthError::InvalidToken => RwiErrorCode::Forbidden,
        };

        RwiResponse {
            action_id: String::new(),
            response: ResponseStatus::Error,
            data: None,
            error: Some(RwiError::new(code, self.to_string())),
        }
    }
}

#[derive(Debug, Error)]
#[error("{message}")]
pub struct HandleTextMessageError {
    action_id: String,
    code: RwiErrorCode,
    message: String,
}

impl HandleTextMessageError {
    pub fn new(
        action_id: impl Into<String>,
        code: RwiErrorCode,
        message: impl Into<String>,
    ) -> Self {
        Self {
            action_id: action_id.into(),
            code,
            message: message.into(),
        }
    }

    pub fn into_rwi_response(self) -> RwiResponse {
        RwiResponse {
            action_id: self.action_id,
            response: ResponseStatus::Error,
            data: None,
            error: Some(RwiError::new(self.code, self.message)),
        }
    }

    pub fn from_command(
        action_id: impl Into<String>,
        error: crate::rwi::processor::CommandError,
    ) -> Self {
        let code = error.rwi_code();
        Self::new(action_id, code, error.to_string())
    }
}

impl From<HandleTextMessageError> for RwiResponse {
    fn from(value: HandleTextMessageError) -> Self {
        value.into_rwi_response()
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error(transparent)]
    Message(#[from] HandleTextMessageError),
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        match self {
            Error::Auth(error) => error.into_response(),
            Error::Message(error) => {
                (StatusCode::BAD_REQUEST, error.to_string()).into_response()
            }
        }
    }
}

impl From<Error> for RwiResponse {
    fn from(value: Error) -> Self {
        match value {
            Error::Auth(error) => error.into_rwi_response(),
            Error::Message(error) => error.into_rwi_response(),
        }
    }
}

impl From<serde_json::Error> for Error {
    fn from(value: serde_json::Error) -> Self {
        HandleTextMessageError::new("", RwiErrorCode::ParseError, value.to_string()).into()
    }
}
