//! Async client for the [Transcript Dock API](https://www.transcriptdock.com/docs).
//!
//! The client never follows redirects and never retries a request on its own.
//! Responses are kept as [`serde_json::Map`] values, so fields added to the API
//! later are never dropped. Calls need a Tokio runtime with timers enabled.
//!
//! ```no_run
//! use serde_json::json;
//! use std::time::Duration;
//! use transcriptdock::{Error, TranscriptDock};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Error> {
//!     let api_key = std::env::var("TRANSCRIPTDOCK_API_KEY").expect("set TRANSCRIPTDOCK_API_KEY");
//!     let client = TranscriptDock::new(api_key)?;
//!
//!     // Generate one key per intended operation and save it with the job ID.
//!     // Reuse the same key and exact body if a submission's response is lost.
//!     let body = json!({
//!         "source": {"url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ"},
//!         "mode": "captions_only",
//!         "max_credits": 1
//!     });
//!     let job = client.create_job(&body, "replace-with-a-unique-operation-key").await?;
//!     let id = job["id"].as_str().unwrap_or_default();
//!     let waited = client
//!         .wait_for_job(id, Duration::from_secs(120), Duration::from_secs(3))
//!         .await;
//!     match waited {
//!         Ok(job) if job["status"] == "succeeded" => {
//!             let result_id = job["result_id"].as_str().unwrap_or_default();
//!             println!("{:?}", client.get_transcript(result_id).await?);
//!             println!("{}", client.export_transcript(result_id, "srt").await?);
//!         }
//!         Ok(job) => println!("{} {:?}", job["status"], job.get("error")),
//!         Err(Error::JobTimeout { job }) => println!("Still running {job:?}"),
//!         Err(error) => return Err(error),
//!     }
//!     Ok(())
//! }
//! ```
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod error;

pub use error::{ApiError, Error};
pub use serde_json::{Map, Value};

use error::invalid;
use reqwest::header::{HeaderValue, ACCEPT, AUTHORIZATION};
use reqwest::redirect::Policy;
use reqwest::Method;
use serde_json::json;
use std::fmt;
use std::time::{Duration, Instant};
use url::{form_urlencoded, Host, Url};

/// A JSON object as returned by the API, with every field kept.
pub type Object = Map<String, Value>;

/// The default API origin.
pub const DEFAULT_BASE_URL: &str = "https://www.transcriptdock.com";

const USER_AGENT: &str = concat!("transcriptdock-rust/", env!("CARGO_PKG_VERSION"));
const METHODS: [&str; 7] = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];
const SUBMISSIONS: [&str; 4] = [
    "/v1/jobs",
    "/v1/batches",
    "/v1/video-discoveries",
    "/v1/language-discoveries",
];
const TERMINAL: [&str; 3] = ["succeeded", "failed", "cancelled"];
const EXPORT_FORMATS: [&str; 4] = ["json", "txt", "srt", "vtt"];
const REDACTED: &str = "[REDACTED]";
const HEX_DIGITS: &[u8; 16] = b"0123456789ABCDEF";

const KEY_MESSAGE: &str = "Provide an API key without whitespace or control characters.";
const ORIGIN_MESSAGE: &str = "Use an HTTPS origin, or HTTP on loopback for local development.";
const ORIGIN_PARTS_MESSAGE: &str =
    "The API origin cannot contain credentials, a path, query or fragment.";
const METHOD_MESSAGE: &str = "Provide a supported uppercase HTTP method.";
const PATH_MESSAGE: &str = "Use a /v1/ path and pass query parameters separately.";
const PATH_CHARS_MESSAGE: &str = "API paths cannot contain whitespace or control characters.";
const TRAVERSAL_MESSAGE: &str = "API paths cannot contain traversal segments.";
const NEEDS_KEY_MESSAGE: &str = "Provide an idempotency key for this submission.";
const IDEMPOTENCY_MESSAGE: &str =
    "Idempotency keys must contain 8 to 128 printable ASCII characters.";
const NETWORK_MESSAGE: &str =
    "Could not reach the API. Check the job before retrying a submission.";

/// Options for [`TranscriptDock::request`] and [`TranscriptDock::request_text`].
///
/// ```
/// use transcriptdock::RequestOptions;
/// let options = RequestOptions::default().query("limit", 10).query("cursor", "next");
/// assert_eq!(options.query.len(), 2);
/// ```
#[derive(Debug, Clone, Default)]
pub struct RequestOptions<'a> {
    /// JSON request body.
    pub body: Option<&'a Value>,
    /// Query parameters, encoded by the client. Leave out a parameter to omit it.
    pub query: Vec<(&'a str, String)>,
    /// Idempotency key. Required for POST submissions.
    pub idempotency_key: Option<&'a str>,
    /// Timeout for this request, replacing the client default.
    pub timeout: Option<Duration>,
}

impl<'a> RequestOptions<'a> {
    /// Set the JSON request body.
    pub fn body(mut self, body: &'a Value) -> Self {
        self.body = Some(body);
        self
    }

    /// Add a query parameter.
    pub fn query(mut self, name: &'a str, value: impl ToString) -> Self {
        self.query.push((name, value.to_string()));
        self
    }

    /// Set the idempotency key.
    pub fn idempotency_key(mut self, key: &'a str) -> Self {
        self.idempotency_key = Some(key);
        self
    }

    /// Set the timeout for this request.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

/// Builder for a [`TranscriptDock`] client with a custom origin or timeout.
#[derive(Clone)]
pub struct TranscriptDockBuilder {
    api_key: String,
    base_url: String,
    timeout: Duration,
}

impl fmt::Debug for TranscriptDockBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TranscriptDockBuilder")
            .field("base_url", &self.base_url)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl TranscriptDockBuilder {
    /// Use another API origin. It must be HTTPS, or HTTP on loopback for local development.
    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Set the default timeout for each request. The default is 30 seconds.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Validate the settings and create the client.
    pub fn build(self) -> Result<TranscriptDock, Error> {
        if !valid_api_key(&self.api_key) {
            return Err(invalid(KEY_MESSAGE));
        }
        let base_url = parse_origin(&self.base_url)?;
        let timeout = positive(self.timeout, "timeout")?;
        let http = reqwest::Client::builder()
            .redirect(Policy::none())
            .user_agent(USER_AGENT)
            .build()
            .map_err(|_| setup_error())?;
        Ok(TranscriptDock {
            api_key: self.api_key,
            base_url,
            timeout,
            http,
        })
    }
}

/// Async API client. Responses keep every field returned by the API.
#[derive(Clone)]
pub struct TranscriptDock {
    api_key: String,
    base_url: String,
    timeout: Duration,
    http: reqwest::Client,
}

impl fmt::Debug for TranscriptDock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TranscriptDock")
            .field("base_url", &self.base_url)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl TranscriptDock {
    /// Create a client for the default origin with a 30 second timeout.
    pub fn new(api_key: impl Into<String>) -> Result<Self, Error> {
        Self::builder(api_key).build()
    }

    /// Start a builder to set the origin or timeout.
    pub fn builder(api_key: impl Into<String>) -> TranscriptDockBuilder {
        TranscriptDockBuilder {
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
            timeout: Duration::from_secs(30),
        }
    }

    /// Call a public `/v1/` endpoint and return its JSON object.
    ///
    /// The method must be uppercase. Redirects are not followed and nothing is retried.
    pub async fn request(
        &self,
        method: &str,
        path: &str,
        options: RequestOptions<'_>,
    ) -> Result<Object, Error> {
        let raw = self.send(method, path, options).await?;
        let message = match serde_json::from_str::<Value>(&raw) {
            Ok(Value::Object(object)) => return Ok(object),
            Ok(_) => "The API did not return a JSON object.",
            Err(_) => "The API did not return JSON.",
        };
        Err(invalid_response(message))
    }

    /// Call a public `/v1/` endpoint and return the response body as text.
    pub async fn request_text(
        &self,
        method: &str,
        path: &str,
        options: RequestOptions<'_>,
    ) -> Result<String, Error> {
        self.send(method, path, options).await
    }

    async fn send(
        &self,
        method: &str,
        path: &str,
        options: RequestOptions<'_>,
    ) -> Result<String, Error> {
        if !METHODS.contains(&method) {
            return Err(invalid(METHOD_MESSAGE));
        }
        check_path(path)?;
        let submission = SUBMISSIONS.contains(&path) || path.ends_with("/retry");
        if method == "POST" && submission && options.idempotency_key.is_none() {
            return Err(invalid(NEEDS_KEY_MESSAGE));
        }
        let key = options.idempotency_key;
        if key.is_some_and(|key| !valid_idempotency_key(key)) {
            return Err(invalid(IDEMPOTENCY_MESSAGE));
        }
        let timeout = match options.timeout {
            Some(timeout) => positive(timeout, "timeout")?,
            None => self.timeout,
        };
        let mut address = format!("{}{}", self.base_url, path);
        if !options.query.is_empty() {
            let pairs = options.query.iter().map(|(name, value)| (*name, value));
            let mut encoder = form_urlencoded::Serializer::new(String::new());
            address.push('?');
            address.push_str(&encoder.extend_pairs(pairs).finish());
        }
        let Ok(url) = Url::parse(&address) else {
            return Err(invalid(PATH_MESSAGE));
        };
        let Ok(method) = Method::from_bytes(method.as_bytes()) else {
            return Err(invalid(METHOD_MESSAGE));
        };
        let bearer = format!("Bearer {}", self.api_key);
        let Ok(mut authorization) = HeaderValue::from_str(&bearer) else {
            return Err(invalid(KEY_MESSAGE));
        };
        authorization.set_sensitive(true);

        let mut request = self
            .http
            .request(method, url)
            .timeout(timeout)
            .header(AUTHORIZATION, authorization)
            .header(ACCEPT, "application/json");
        if let Some(key) = key {
            request = request.header("Idempotency-Key", key);
        }
        if let Some(body) = options.body {
            request = request.json(body);
        }
        let response = request.send().await.map_err(|_| network_error())?;
        let status = response.status().as_u16();
        let raw = response.bytes().await.map_err(|_| network_error())?;
        if !(200..300).contains(&status) {
            return Err(Error::Api(self.api_error(status, &raw)));
        }
        match String::from_utf8(raw.to_vec()) {
            Ok(text) => Ok(text),
            Err(_) => Err(invalid_response("The API response is not UTF-8 text.")),
        }
    }

    fn api_error(&self, status: u16, raw: &[u8]) -> ApiError {
        let envelope = match serde_json::from_slice::<Value>(raw) {
            Ok(Value::Object(envelope)) => envelope,
            _ => Object::new(),
        };
        let error = match envelope.get("error") {
            Some(Value::Object(error)) => error.clone(),
            _ => Object::new(),
        };
        let code = truthy_text(error.get("code"));
        let message = truthy_text(error.get("message"));
        let fallback = format!("Request failed with status {status}.");
        let retryable = error.get("retryable").and_then(Value::as_bool);
        let retry_after = error.get("retry_after_seconds").and_then(Value::as_u64);
        let request_id = truthy_text(envelope.get("request_id"));
        ApiError {
            code: self.redact(code.as_deref().unwrap_or("HTTP_ERROR")),
            message: self.redact(message.as_deref().unwrap_or(&fallback)),
            status: Some(status),
            retryable: retryable.unwrap_or(status == 429 || status >= 500),
            request_id: request_id.map(|id| self.redact(&id)),
            retry_after_seconds: retry_after,
        }
    }

    fn redact(&self, value: &str) -> String {
        redact_tokens(&value.replace(&self.api_key, REDACTED))
    }

    async fn call(&self, method: &str, path: &str) -> Result<Object, Error> {
        let options = RequestOptions::default();
        self.request(method, path, options).await
    }

    async fn post(&self, path: &str, body: &Value, key: Option<&str>) -> Result<Object, Error> {
        let mut options = RequestOptions::default().body(body);
        options.idempotency_key = key;
        self.request("POST", path, options).await
    }

    /// `GET /v1/capabilities`
    pub async fn capabilities(&self) -> Result<Object, Error> {
        self.call("GET", "/v1/capabilities").await
    }

    /// `GET /v1/usage`
    pub async fn usage(&self) -> Result<Object, Error> {
        self.call("GET", "/v1/usage").await
    }

    /// `POST /v1/jobs`. Can spend credits. Save the idempotency key with the job ID.
    pub async fn create_job(&self, body: &Value, idempotency_key: &str) -> Result<Object, Error> {
        self.post("/v1/jobs", body, Some(idempotency_key)).await
    }

    /// `GET /v1/jobs/{id}`. `wait` asks the server to hold the response up to that many seconds.
    pub async fn get_job(&self, job_id: &str, wait: u32) -> Result<Object, Error> {
        let path = format!("/v1/jobs/{}", encode_id(job_id)?);
        let options = RequestOptions::default().query("wait", wait);
        self.request("GET", &path, options).await
    }

    /// `GET /v1/jobs`
    pub async fn list_jobs(&self, limit: u32, cursor: Option<&str>) -> Result<Object, Error> {
        let mut options = RequestOptions::default().query("limit", limit);
        if let Some(cursor) = cursor {
            options = options.query("cursor", cursor);
        }
        self.request("GET", "/v1/jobs", options).await
    }

    /// `POST /v1/jobs/{id}/cancel`
    pub async fn cancel_job(&self, job_id: &str) -> Result<Object, Error> {
        let path = format!("/v1/jobs/{}/cancel", encode_id(job_id)?);
        self.call("POST", &path).await
    }

    /// `POST /v1/jobs/{id}/retry`. Can spend credits.
    pub async fn retry_job(
        &self,
        job_id: &str,
        idempotency_key: &str,
        max_credits: Option<u64>,
    ) -> Result<Object, Error> {
        let path = format!("/v1/jobs/{}/retry", encode_id(job_id)?);
        let body = match max_credits {
            Some(credits) => json!({ "max_credits": credits }),
            None => json!({}),
        };
        self.post(&path, &body, Some(idempotency_key)).await
    }

    /// Poll a job until it succeeds, fails or is cancelled.
    ///
    /// Returns any terminal job, so check `status` before using `result_id`. When
    /// `timeout` passes first, returns [`Error::JobTimeout`] with the last job seen.
    /// The job keeps running on the server.
    pub async fn wait_for_job(
        &self,
        job_id: &str,
        timeout: Duration,
        poll_interval: Duration,
    ) -> Result<Object, Error> {
        positive(timeout, "timeout")?;
        positive(poll_interval, "poll_interval")?;
        let path = format!("/v1/jobs/{}", encode_id(job_id)?);
        let Some(deadline) = Instant::now().checked_add(timeout) else {
            return Err(invalid("timeout is too large."));
        };
        let mut last: Option<Object> = None;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(job_timeout(last));
            }
            let limit = remaining.min(self.timeout);
            let options = RequestOptions::default().timeout(limit);
            let job = self.request("GET", &path, options).await?;
            let status = job.get("status").and_then(Value::as_str).unwrap_or("");
            if TERMINAL.contains(&status) {
                return Ok(job);
            }
            last = Some(job);
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(job_timeout(last));
            }
            tokio::time::sleep(poll_interval.min(remaining)).await;
        }
    }

    /// `GET /v1/transcripts/{id}`. Timing fields are returned unchanged.
    pub async fn get_transcript(&self, transcript_id: &str) -> Result<Object, Error> {
        let path = format!("/v1/transcripts/{}", encode_id(transcript_id)?);
        self.call("GET", &path).await
    }

    /// `GET /v1/transcripts/{id}/export` as text. `format` is `json`, `txt`, `srt` or `vtt`.
    pub async fn export_transcript(
        &self,
        transcript_id: &str,
        format: &str,
    ) -> Result<String, Error> {
        if !EXPORT_FORMATS.contains(&format) {
            return Err(invalid("Choose json, txt, srt or vtt."));
        }
        let path = format!("/v1/transcripts/{}/export", encode_id(transcript_id)?);
        let options = RequestOptions::default().query("format", format);
        self.request_text("GET", &path, options).await
    }

    /// `DELETE /v1/transcripts/{id}`
    pub async fn delete_transcript(&self, transcript_id: &str) -> Result<Object, Error> {
        let path = format!("/v1/transcripts/{}", encode_id(transcript_id)?);
        self.call("DELETE", &path).await
    }

    /// `POST /v1/batches`. Can spend credits.
    pub async fn create_batch(&self, body: &Value, idempotency_key: &str) -> Result<Object, Error> {
        self.post("/v1/batches", body, Some(idempotency_key)).await
    }

    /// `GET /v1/batches/{id}`
    pub async fn get_batch(&self, batch_id: &str) -> Result<Object, Error> {
        let path = format!("/v1/batches/{}", encode_id(batch_id)?);
        self.call("GET", &path).await
    }

    /// `POST /v1/video-discoveries`
    pub async fn create_video_discovery(
        &self,
        body: &Value,
        idempotency_key: &str,
    ) -> Result<Object, Error> {
        let key = Some(idempotency_key);
        self.post("/v1/video-discoveries", body, key).await
    }

    /// `GET /v1/video-discoveries/{id}`
    pub async fn get_video_discovery(&self, discovery_id: &str) -> Result<Object, Error> {
        let path = format!("/v1/video-discoveries/{}", encode_id(discovery_id)?);
        self.call("GET", &path).await
    }

    /// `POST /v1/language-discoveries`
    pub async fn create_language_discovery(
        &self,
        body: &Value,
        idempotency_key: &str,
    ) -> Result<Object, Error> {
        let key = Some(idempotency_key);
        self.post("/v1/language-discoveries", body, key).await
    }

    /// `GET /v1/language-discoveries/{id}`
    pub async fn get_language_discovery(&self, discovery_id: &str) -> Result<Object, Error> {
        let path = format!("/v1/language-discoveries/{}", encode_id(discovery_id)?);
        self.call("GET", &path).await
    }

    /// `POST /v1/uploads`. Request an upload URL, then send the file bytes there
    /// separately without the Transcript Dock API key.
    pub async fn create_upload(&self, body: &Value) -> Result<Object, Error> {
        self.post("/v1/uploads", body, None).await
    }

    /// `POST /v1/uploads/{id}/complete`
    pub async fn complete_upload(&self, upload_id: &str) -> Result<Object, Error> {
        let path = format!("/v1/uploads/{}/complete", encode_id(upload_id)?);
        self.call("POST", &path).await
    }

    /// `GET /v1/webhook-endpoints`
    pub async fn list_webhook_endpoints(&self) -> Result<Object, Error> {
        self.call("GET", "/v1/webhook-endpoints").await
    }

    /// `POST /v1/webhook-endpoints`. The response holds a signing secret; store it privately.
    pub async fn create_webhook_endpoint(&self, body: &Value) -> Result<Object, Error> {
        self.post("/v1/webhook-endpoints", body, None).await
    }

    /// `DELETE /v1/webhook-endpoints/{id}`
    pub async fn delete_webhook_endpoint(&self, endpoint_id: &str) -> Result<Object, Error> {
        let path = format!("/v1/webhook-endpoints/{}", encode_id(endpoint_id)?);
        self.call("DELETE", &path).await
    }
}

fn job_timeout(job: Option<Object>) -> Error {
    let job = job.map(Value::Object);
    Error::JobTimeout { job }
}

fn network_error() -> Error {
    Error::Api(ApiError::new("NETWORK_ERROR", NETWORK_MESSAGE, true))
}

fn setup_error() -> Error {
    let message = "Could not set up the HTTP client.";
    Error::Api(ApiError::new("NETWORK_ERROR", message, false))
}

fn invalid_response(message: &str) -> Error {
    Error::Api(ApiError::new("INVALID_RESPONSE", message, false))
}

fn positive(value: Duration, name: &str) -> Result<Duration, Error> {
    if value.is_zero() {
        let message = format!("{name} must be a positive duration.");
        return Err(Error::InvalidArgument(message));
    }
    Ok(value)
}

fn valid_api_key(key: &str) -> bool {
    !key.is_empty() && key.bytes().all(|byte| (33..=126).contains(&byte))
}

fn valid_idempotency_key(key: &str) -> bool {
    (8..=128).contains(&key.len()) && key.bytes().all(|byte| (32..=126).contains(&byte))
}

fn parse_origin(base_url: &str) -> Result<String, Error> {
    let Ok(url) = Url::parse(base_url) else {
        return Err(invalid("Provide a valid API origin."));
    };
    let loopback = match url.host() {
        Some(Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    };
    let secure = url.scheme() == "https" || (url.scheme() == "http" && loopback);
    if !secure || url.host().is_none() {
        return Err(invalid(ORIGIN_MESSAGE));
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err(invalid(ORIGIN_PARTS_MESSAGE));
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

fn check_path(path: &str) -> Result<(), Error> {
    if !path.starts_with("/v1/") || path.contains(['?', '#', '\\']) {
        return Err(invalid(PATH_MESSAGE));
    }
    if path.bytes().any(|byte| byte <= b' ' || byte == 0x7f) {
        return Err(invalid(PATH_CHARS_MESSAGE));
    }
    let decoded = percent_decode(path);
    let traversal = |segment: &[u8]| segment == b"." || segment == b"..";
    if decoded.split(|byte| *byte == b'/').any(traversal) {
        return Err(invalid(TRAVERSAL_MESSAGE));
    }
    Ok(())
}

fn percent_decode(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        let decoded = match (byte, bytes.get(index + 1), bytes.get(index + 2)) {
            (b'%', Some(&high), Some(&low)) => hex(high).zip(hex(low)),
            _ => None,
        };
        match decoded {
            Some((high, low)) => {
                out.push(high * 16 + low);
                index += 3;
            }
            None => {
                out.push(byte);
                index += 1;
            }
        }
    }
    out
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn encode_id(id: &str) -> Result<String, Error> {
    if id.is_empty() || id == "." || id == ".." {
        return Err(invalid("Provide a nonempty ID."));
    }
    let mut out = String::with_capacity(id.len());
    for byte in id.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(char::from(HEX_DIGITS[usize::from(byte >> 4)]));
            out.push(char::from(HEX_DIGITS[usize::from(byte & 15)]));
        }
    }
    Ok(out)
}

/// Text for a field that is present and not empty, zero, false or null.
fn truthy_text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::Null | Value::Bool(false) => None,
        Value::String(text) if text.is_empty() => None,
        Value::String(text) => Some(text.clone()),
        Value::Array(items) if items.is_empty() => None,
        Value::Object(fields) if fields.is_empty() => None,
        Value::Number(number) if number.as_i64() == Some(0) => None,
        other => Some(other.to_string()),
    }
}

/// Replace `td_live_...` keys and `Bearer ...` credentials, ignoring case.
fn redact_tokens(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < chars.len() {
        match token_end(&chars, index) {
            Some(end) => {
                out.push_str(REDACTED);
                index = end;
            }
            None => {
                out.push(chars[index]);
                index += 1;
            }
        }
    }
    out
}

fn token_end(chars: &[char], start: usize) -> Option<usize> {
    if starts_with_ignore_case(chars, start, "td_live_") {
        let begin = start + "td_live_".len();
        let end = scan(chars, begin, key_char);
        if end > begin {
            return Some(end);
        }
    }
    if starts_with_ignore_case(chars, start, "bearer") {
        let begin = start + "bearer".len();
        let spaces = scan(chars, begin, char::is_whitespace);
        let end = scan(chars, spaces, |c| !c.is_whitespace());
        if spaces > begin && end > spaces {
            return Some(end);
        }
    }
    None
}

fn key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

fn scan(chars: &[char], start: usize, accept: impl Fn(char) -> bool) -> usize {
    let mut index = start;
    while index < chars.len() && accept(chars[index]) {
        index += 1;
    }
    index
}

fn starts_with_ignore_case(chars: &[char], start: usize, prefix: &str) -> bool {
    let mut index = start;
    for expected in prefix.chars() {
        match chars.get(index) {
            Some(c) if c.to_ascii_lowercase() == expected => index += 1,
            _ => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins() {
        let default = parse_origin(DEFAULT_BASE_URL).unwrap();
        assert_eq!(default, DEFAULT_BASE_URL);
        let local = parse_origin("http://127.0.0.1:8080/").unwrap();
        assert_eq!(local, "http://127.0.0.1:8080");
        assert!(parse_origin("http://localhost:3000").is_ok());
        assert!(parse_origin("http://[::1]:3000").is_ok());
        for bad in [
            "http://example.com",
            "https://user:pass@example.com",
            "https://example.com/path",
            "https://example.com?x=1",
            "https://example.com#top",
            "ftp://127.0.0.1",
            "https://example.com:99999",
            "not a url",
        ] {
            assert!(parse_origin(bad).is_err(), "{bad} was accepted");
        }
    }

    #[test]
    fn api_keys() {
        assert!(valid_api_key("td_live_abc"));
        assert!(!valid_api_key(""));
        assert!(!valid_api_key("bad key"));
        assert!(!valid_api_key("bad\nkey"));
        assert!(!valid_api_key("caf\u{e9}"));
    }

    #[test]
    fn idempotency_keys() {
        assert!(valid_idempotency_key("fixture-replay-01"));
        assert!(valid_idempotency_key("with space ok"));
        assert!(!valid_idempotency_key("short"));
        assert!(!valid_idempotency_key("bad\nreplay"));
        assert!(valid_idempotency_key(&"x".repeat(128)));
        assert!(!valid_idempotency_key(&"x".repeat(129)));
    }

    #[test]
    fn paths() {
        assert!(check_path("/v1/jobs").is_ok());
        assert!(check_path("/v1/jobs/a%2Fb").is_ok());
        assert!(check_path("/v1/jobs/...").is_ok());
        for bad in [
            "/v2/jobs",
            "https://evil.example/v1/jobs",
            "/v1/jobs?limit=1",
            "/v1/jobs#x",
            "/v1/jobs\\x",
            "/v1/../secret",
            "/v1/%2e%2e/secret",
            "/v1/%2E/secret",
            "/v1/jobs/..",
            "/v1/jobs/\n../x",
            "/v1/jobs/a b",
        ] {
            assert!(check_path(bad).is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn ids() {
        assert_eq!(encode_id("id/other?x").unwrap(), "id%2Fother%3Fx");
        assert_eq!(encode_id("a b~c").unwrap(), "a%20b~c");
        assert_eq!(encode_id("caf\u{e9}").unwrap(), "caf%C3%A9");
        assert!(encode_id("").is_err());
        assert!(encode_id(".").is_err());
        assert!(encode_id("..").is_err());
    }

    #[test]
    fn redaction() {
        assert_eq!(redact_tokens("a td_live_x-1 b"), "a [REDACTED] b");
        assert_eq!(redact_tokens("TD_LIVE_X"), "[REDACTED]");
        assert_eq!(redact_tokens("bearer   secret ok"), "[REDACTED] ok");
        assert_eq!(redact_tokens("Bearer"), "Bearer");
        assert_eq!(redact_tokens("td_live_"), "td_live_");
        assert_eq!(redact_tokens("caf\u{e9} fine"), "caf\u{e9} fine");
    }

    #[test]
    fn error_envelopes() {
        let client = TranscriptDock::builder("custom-key-1")
            .base_url("http://127.0.0.1:9")
            .build()
            .unwrap();
        let body = json!({
            "error": {
                "code": "RATE_LIMITED",
                "message": "custom-key-1",
                "retryable": true,
                "retry_after_seconds": 4
            },
            "request_id": "req"
        });
        let error = client.api_error(429, &serde_json::to_vec(&body).unwrap());
        assert_eq!(error.code, "RATE_LIMITED");
        assert_eq!(error.message, "[REDACTED]");
        assert_eq!(error.retry_after_seconds, Some(4));
        assert_eq!(error.request_id.as_deref(), Some("req"));
        assert!(error.retryable);

        let error = client.api_error(502, b"<html>gateway failed</html>");
        assert_eq!(error.code, "HTTP_ERROR");
        assert_eq!(error.message, "Request failed with status 502.");
        assert!(error.retryable);
        assert_eq!(client.api_error(302, b"").status, Some(302));
        assert!(!client.api_error(404, b"[]").retryable);
    }

    #[test]
    fn durations() {
        assert!(positive(Duration::ZERO, "timeout").is_err());
        assert!(positive(Duration::from_millis(1), "timeout").is_ok());
        let builder = TranscriptDock::builder("key-1").timeout(Duration::ZERO);
        assert!(builder.build().is_err());
    }

    #[test]
    fn debug_hides_the_key() {
        let client = TranscriptDock::new("td_live_secret").unwrap();
        assert!(!format!("{client:?}").contains("td_live_secret"));
        let builder = TranscriptDock::builder("td_live_secret");
        assert!(!format!("{builder:?}").contains("td_live_secret"));
    }
}
