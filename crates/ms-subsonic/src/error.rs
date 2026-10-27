//! Subsonic error codes. A failed call is still HTTP 200, with
//! `status="failed"` and an `<error>` element, as the protocol requires.

use crate::encode::Node;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiError {
    pub code: u16,
    pub message: String,
}

impl ApiError {
    pub fn new(code: u16, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }

    pub fn generic(message: impl Into<String>) -> Self {
        Self::new(0, message)
    }

    pub fn missing(param: &str) -> Self {
        Self::new(10, format!("Required parameter is missing: {param}"))
    }

    pub fn bad_param(param: &str) -> Self {
        Self::new(10, format!("Invalid value for parameter: {param}"))
    }

    pub fn wrong_credentials() -> Self {
        Self::new(40, "Wrong username or password")
    }

    pub fn not_authorized() -> Self {
        Self::new(50, "User is not authorized for the given operation")
    }

    pub fn not_found(what: &str) -> Self {
        Self::new(70, format!("{what} not found"))
    }

    pub fn node(&self) -> Node {
        Node::new("error").i("code", self.code).s("message", self.message.clone())
    }
}

impl From<ms_common::Error> for ApiError {
    fn from(e: ms_common::Error) -> Self {
        Self::generic(e.message())
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
