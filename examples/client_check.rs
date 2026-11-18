//! Contract checks against the local fixture server in `.ci/fixture.py`.
//!
//! Run them with `python3 .ci/check.py`. They only call the loopback fixture,
//! never production, and spend no credits.

use serde_json::json;
use std::fmt::Debug;
use std::time::Duration;
use transcriptdock::{ApiError, Error, RequestOptions, TranscriptDock};

const KEY: &str = "td_live_fixture_only";
const REPLAY: &str = "fixture-replay-01";
const BASE_URL: &str = "TRANSCRIPTDOCK_TEST_BASE_URL";

fn check(ok: bool, message: &str) {
    assert!(ok, "{message}");
}

fn api_error<T: Debug>(result: Result<T, Error>) -> ApiError {
    match result {
        Err(Error::Api(error)) => error,
        other => panic!("Expected ApiError, got {other:?}"),
    }
}

fn invalid<T: Debug>(result: Result<T, Error>) {
    match result {
        Err(Error::InvalidArgument(_)) => {}
        other => panic!("Expected InvalidArgument, got {other:?}"),
    }
}

fn none<'a>() -> RequestOptions<'a> {
    RequestOptions::default()
}

#[tokio::main]
async fn main() {
    let base = std::env::var(BASE_URL).expect("set the fixture URL");
    let builder = TranscriptDock::builder(KEY).base_url(base);
    let client = builder.build().unwrap();
    let seconds = Duration::from_secs;

    let body = json!({
        "source": {"url": "https://example.com/video"},
        "mode": "captions_only",
        "max_credits": 1
    });
    for _ in 0..2 {
        let out = client.create_job(&body, REPLAY).await.unwrap();
        check(out["future_field"] == 42, "future response field lost");
        let request = &out["request"];
        check(request["body"] == body, "request body changed");
        let replayed = request["idempotency_key"] == REPLAY;
        let authorized = request["authorized"] == true;
        check(replayed && authorized, "headers changed");
    }

    let options = none().query("cursor", "a/b?c&d").query("limit", 7);
    let out = client.request("GET", "/v1/jobs", options).await.unwrap();
    let cursor = &out["request"]["query"]["cursor"];
    check(*cursor == json!(["a/b?c&d"]), "cursor encoding changed");

    let out = client.get_job("id/other?x", 0).await.unwrap();
    let path = out["request"]["path"].as_str().unwrap_or_default();
    let encoded = path.ends_with("id%2Fother%3Fx");
    check(encoded, "ID path encoding changed");

    let out = client.get_transcript("fixture").await.unwrap();
    check(out["words"][0]["start_ms"] == 123, "timing unit changed");

    let text = client.export_transcript("fixture", "srt").await.unwrap();
    check(text.contains("fixture"), "export lost");

    for status in ["succeeded", "failed", "cancelled"] {
        let waited = client.wait_for_job(status, seconds(120), seconds(3)).await;
        let job = waited.unwrap();
        check(job["status"] == status, "terminal status lost");
    }

    let short = Duration::from_millis(150);
    match client.wait_for_job("processing", short, seconds(1)).await {
        Err(Error::JobTimeout { job: Some(job) }) => {
            check(job["status"] == "processing", "timeout lost last job");
        }
        other => panic!("Expected JobTimeout, got {other:?}"),
    }

    let error = api_error(client.request("GET", "/v1/rate-limit", none()).await);
    let fields = error.status == Some(429)
        && error.retryable
        && error.retry_after_seconds == Some(4)
        && error.request_id.as_deref() == Some("fixture-request");
    check(fields, "API error fields lost");
    let leaked = error.message.contains(KEY) || error.to_string().contains(KEY);
    check(!leaked, "key in error");

    let error = api_error(client.request("GET", "/v1/redirect", none()).await);
    check(error.status == Some(302), "redirect followed");

    for path in [
        "/v1/gateway",
        "/v1/error-shape",
        "/v1/malformed",
        "/v1/array",
    ] {
        api_error(client.request("GET", path, none()).await);
    }

    let empty = json!({});
    let fixture = json!({ "fixture": true });
    for path in [
        "/v1/batches",
        "/v1/video-discoveries",
        "/v1/language-discoveries",
    ] {
        invalid(client.request("POST", path, none().body(&empty)).await);
        let options = none().body(&fixture).idempotency_key("fixture-generic-01");
        let out = client.request("POST", path, options).await.unwrap();
        let method = &out["request"]["method"];
        check(*method == "POST", "generic submission failed");
    }

    let path = "/v1/transcripts/fixture";
    let out = client.request("DELETE", path, none()).await.unwrap();
    let method = &out["request"]["method"];
    check(*method == "DELETE", "delete dispatch failed");

    for url in [
        "http://example.com",
        "https://user:pass@example.com",
        "https://example.com/path",
        "https://example.com?x=1",
    ] {
        let builder = TranscriptDock::builder("fixture-key");
        invalid(builder.base_url(url).build());
    }
    invalid(TranscriptDock::new("bad\nkey"));
    invalid(client.create_job(&empty, "bad\nreplay").await);
    invalid(client.request("GET", "/v1/../secret", none()).await);
    // A Duration cannot be NaN, so zero is the invalid polling timeout here.
    let zero = Duration::ZERO;
    invalid(client.wait_for_job("fixture", zero, seconds(3)).await);

    let options = none().timeout(Duration::from_millis(30));
    let error = api_error(client.request("GET", "/v1/slow", options).await);
    let timed_out = error.code == "NETWORK_ERROR";
    check(timed_out, "transport timeout not handled");

    let options = none().body(&empty);
    invalid(client.request("post", "/v1/jobs", options).await);
    invalid(client.request("GET", "/v1/%2e%2e/secret", none()).await);

    for (path, status, code) in [
        ("/v1/budget", 402, "BUDGET_EXCEEDED"),
        ("/v1/conflict", 409, "IDEMPOTENCY_CONFLICT"),
        ("/v1/jobs/not-owned", 404, "NOT_FOUND"),
    ] {
        let error = api_error(client.request("GET", path, none()).await);
        let matched = error.status == Some(status) && error.code == code;
        check(
            matched && !error.retryable,
            "budget, replay conflict or inaccessible job error lost",
        );
    }

    println!("Rust client checks passed");
}
