use serde_json::Value;
use std::fmt;

/// An API or transport failure. Requests are never automatically retried.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ApiError {
    /// Stable error code, such as `RATE_LIMITED`, `NETWORK_ERROR` or `INVALID_RESPONSE`.
    pub code: String,
    /// Human readable message with bearer tokens redacted.
    pub message: String,
    /// HTTP status, or `None` when no response arrived.
    pub status: Option<u16>,
    /// Whether the API marked the failure as retryable. The client never retries on its own.
    pub retryable: bool,
    /// The API request ID, when the response carried one.
    pub request_id: Option<String>,
    /// Seconds the API asked you to wait before retrying, when given.
    pub retry_after_seconds: Option<u64>,
}

impl ApiError {
    pub(crate) fn new(code: &str, message: &str, retryable: bool) -> Self {
        ApiError {
            code: code.to_string(),
            message: message.to_string(),
            status: None,
            retryable,
            request_id: None,
            retry_after_seconds: None,
        }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

/// Every error this client returns.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Error {
    /// The API returned an error status, or the request could not complete.
    Api(ApiError),
    /// `wait_for_job` reached its deadline. The job keeps running on the server.
    JobTimeout {
        /// The last job response observed, or `None` if no poll completed.
        job: Option<Value>,
    },
    /// An argument was rejected before any request was sent.
    InvalidArgument(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Api(error) => fmt::Display::fmt(error, f),
            Error::JobTimeout { .. } => {
                f.write_str("The job is still running. Check it again using its ID.")
            }
            Error::InvalidArgument(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Api(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ApiError> for Error {
    fn from(error: ApiError) -> Self {
        Error::Api(error)
    }
}

pub(crate) fn invalid(message: &str) -> Error {
    Error::InvalidArgument(message.to_string())
}
