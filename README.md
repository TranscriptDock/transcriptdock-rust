# Transcript Dock for Rust

An async client for the [Transcript Dock API](https://www.transcriptdock.com/docs).
Rust 1.85 or newer. Built on `reqwest` with rustls and runs on Tokio.

Install from crates.io:

```sh
cargo add transcriptdock
cargo add tokio --features macros,rt-multi-thread
cargo add serde_json
```

## Submit and read a transcript

Set `TRANSCRIPTDOCK_API_KEY` in your environment. Keep it out of source control.

```rust
use serde_json::json;
use std::time::Duration;
use transcriptdock::{Error, TranscriptDock};

#[tokio::main]
async fn main() -> Result<(), Error> {
    let api_key = std::env::var("TRANSCRIPTDOCK_API_KEY").expect("set TRANSCRIPTDOCK_API_KEY");
    let client = TranscriptDock::new(api_key)?;
    println!("{:?}", client.capabilities().await?);

    // Generate once per intended operation. Save this key and the returned job ID.
    // Reuse the same key and exact body if a submission's response is lost.
    let submission_key = "replace-with-a-unique-operation-key";
    let body = json!({
        "source": {"url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ"},
        "mode": "captions_only",
        "max_credits": 1
    });
    let job = client.create_job(&body, submission_key).await?;
    let id = job["id"].as_str().unwrap_or_default();

    let waited = client
        .wait_for_job(id, Duration::from_secs(120), Duration::from_secs(3))
        .await;
    match waited {
        Ok(job) if job["status"] == "succeeded" => {
            let result_id = job["result_id"].as_str().unwrap_or_default();
            println!("{:?}", client.get_transcript(result_id).await?);
            println!("{}", client.export_transcript(result_id, "srt").await?);
        }
        Ok(job) => println!("{} {:?}", job["status"], job.get("error")),
        Err(Error::JobTimeout { job }) => println!("Still running {job:?}"),
        Err(error) => return Err(error),
    }
    Ok(())
}
```

Creating a job can spend credits. This example caps the request at one credit
and requests captions only. It does not enable AI fallback. Check capabilities
and your plan before selecting other modes. Never blindly resubmit after a
network failure: check the job ID or replay the same body with the same
idempotency key. The client does not retry requests automatically.

## Supported operations

All responses are `serde_json::Map<String, Value>` objects (aliased as
`transcriptdock::Object`) that keep every field the API returns. Request bodies
are `serde_json::Value` with the API's exact field names; the server validates
supported options. See the
[OpenAPI definition](https://www.transcriptdock.com/docs/openapi.yaml) for bodies.
Every method is `async` and returns `Result<_, transcriptdock::Error>`.

* `capabilities()`, `usage()`
* `create_job(&body, idempotency_key)`, `get_job(id, wait)`,
  `list_jobs(limit, cursor)`, `cancel_job(id)`,
  `retry_job(id, idempotency_key, max_credits)`, `wait_for_job(id, timeout, poll_interval)`
* `get_transcript(id)`, `export_transcript(id, format)`, `delete_transcript(id)`
* `create_batch(&body, idempotency_key)`, `get_batch(id)`
* `create_video_discovery(&body, idempotency_key)`, `get_video_discovery(id)`
* `create_language_discovery(&body, idempotency_key)`, `get_language_discovery(id)`
* `create_upload(&body)`, `complete_upload(id)`
* `list_webhook_endpoints()`, `create_webhook_endpoint(&body)`, `delete_webhook_endpoint(id)`

`get_job` takes `wait` in seconds; pass `0` to return at once. `list_jobs` takes
a limit such as `20` and an optional cursor.

`export_transcript` returns a `String` for `json`, `txt`, `srt` and `vtt`,
preserving the export response. `get_transcript` preserves raw timing fields
without converting units. `wait_for_job` returns succeeded, failed or cancelled
jobs; inspect `status` before using `result_id`. Timeout leaves the job running
and returns `Error::JobTimeout { job }` with the last observed response (or
`None` if no poll was possible).

`create_upload` creates an upload slot only. Transfer bytes to the returned signed
upload URL separately following the API instructions, without sending your
Transcript Dock API key to the storage host. Then call `complete_upload`.
Webhook secrets are sensitive; store them privately and verify callback
signatures in your receiver according to the API documentation.

`request(method, "/v1/...", options)` and `request_text(...)` support public
endpoints added later. Build the options with `RequestOptions::default()` and
its `body`, `query`, `idempotency_key` and `timeout` methods. The method must be
an uppercase name such as `"GET"`. These calls reject absolute URLs, traversal
paths and redirects. Custom API origins must use HTTPS, except loopback HTTP for
local development:

```rust
use std::time::Duration;
use transcriptdock::TranscriptDock;

let client = TranscriptDock::builder(api_key)
    .base_url("http://127.0.0.1:3000")
    .timeout(Duration::from_secs(10))
    .build()?;
```

The default origin is `https://www.transcriptdock.com`; the apex domain redirects.

## Errors

```rust
use transcriptdock::Error;

match client.list_jobs(10, None).await {
    Ok(jobs) => println!("{jobs:?}"),
    Err(Error::Api(error)) => {
        println!("{} {:?} {:?}", error.code, error.status, error.request_id);
        // retryable and retry_after_seconds describe the API response.
        // Decide whether to retry using the operation's idempotency rules.
    }
    Err(error) => println!("{error}"),
}
```

Transport failures and timeouts use `NETWORK_ERROR`; malformed responses use
`INVALID_RESPONSE`. A redirect is returned as an `ApiError` with its 3xx status.
Arguments rejected before sending use `Error::InvalidArgument`. API messages
redact bearer tokens. Error responses do not expose raw HTML pages. The client
`Debug` output never shows the API key.

## Development and release

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
python3 .ci/check.py
cargo publish --dry-run
```

Dependencies are pinned to exact versions and `Cargo.lock` is committed.
`python3 .ci/check.py` starts a loopback fixture server and runs
`examples/client_check.rs` against it. Tests spend no credits and never call
production.

Before publishing, review the packaged files with `cargo package --list`. The
founder handles registry sign-in. Never put registry or API credentials in this
directory. A local build is not a release.

The first release is published by hand, because crates.io trusted publishing can
only be configured for a crate that already exists:

1. Sign in to crates.io with GitHub, verify the account email, and create an API
   token with the `publish-new` scope.
2. Run `cargo login`, paste the token, then run `cargo publish` from a clean
   checkout of `main`.
3. In the crate's Settings on crates.io, add a Trusted Publisher with these fields,
   then revoke the API token.

| Field | Value |
| --- | --- |
| Publisher | GitHub |
| Repository owner | `TranscriptDock` |
| Repository name | `transcriptdock-rust` |
| Workflow filename | `release.yml` |
| Environment | `crates-io` |

For later releases, bump the version, then run the GitHub Actions workflow on
`main` with `publish` enabled. The publish job exchanges a GitHub OIDC token for
a short lived crates.io token; no repository secret is needed. The normal push
workflow checks and packages only. docs.rs builds the documentation
automatically after each release.
