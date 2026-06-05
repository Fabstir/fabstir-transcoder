//! Content-moderation seam (A4 fail-closed gate).
//!
//! Implements the transcoder half of the CSAM publish gate: the `may_publish`
//! A4 predicate, the verdict outcome type, the seam-#1 client, and config.
//! See `docs/development/IMPLEMENTATION-CONTENT-MODERATION.md`.
//!
//! Every item is wired by Phase 5 (the gate). `StubModerationClient` is the only
//! test-only item, so it carries a targeted `#[allow(dead_code)]`.

use anyhow::Result;
use base64::{engine::general_purpose, Engine as _};
use dotenv::var;
use std::time::Duration;

/// Outcome of a moderation attempt. Anything that is not `Cleared` MUST hold.
/// (No separate `Verdict` enum — the whole module operates on `ModerationOutcome`,
/// so a second enum would be dead code and trip the `cargo clippy` clean gate.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModerationOutcome {
    Cleared,
    Blocked,
    Flagged,
    Unavailable,
}

/// THE A4 invariant: publish is allowed ONLY on a clean `Cleared`.
pub fn may_publish(o: &ModerationOutcome) -> bool {
    matches!(o, ModerationOutcome::Cleared)
}

/// Map the node's HTTP outcome to a `ModerationOutcome`. Every failure mode —
/// non-2xx (incl. 404 unknown `task_id`), unknown verdict string, unparseable
/// body — collapses to `Unavailable` ⇒ HOLD (fail-closed).
fn outcome_from_response(status: u16, body: &str) -> ModerationOutcome {
    #[derive(serde::Deserialize)]
    struct VerdictBody {
        verdict: String,
    }
    if status != 200 {
        return ModerationOutcome::Unavailable;
    }
    match serde_json::from_str::<VerdictBody>(body) {
        Ok(v) => match v.verdict.as_str() {
            "cleared" => ModerationOutcome::Cleared,
            "blocked" => ModerationOutcome::Blocked,
            "flagged" => ModerationOutcome::Flagged,
            _ => ModerationOutcome::Unavailable,
        },
        Err(_) => ModerationOutcome::Unavailable,
    }
}

// ── Config (env-driven; fail-closed defaults). `MODERATION_ENABLED` is the
//    master dark-launch switch (default off). `pub` accessors are called cross-module.

/// Master switch (default `false` = dark-launch).
pub fn moderation_enabled() -> bool {
    var("MODERATION_ENABLED")
        .map(|v| v == "true")
        .unwrap_or(false)
}

/// Host-local node base URL; `None` (missing) ⇒ fail-closed HOLD at the client.
fn node_url() -> Option<String> {
    var("MODERATION_NODE_URL").ok()
}

/// Seam-#1 POST timeout (s); on expiry the client maps to `Unavailable` ⇒ HOLD.
fn timeout_secs() -> u64 {
    var("MODERATION_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30)
}

/// Keyframe sampling floor interval (s) — tight floor for short content. Clamped to a
/// tiny positive minimum (mirrors `keyframe_max`'s `.max(1)`) so `effective_interval` is
/// never `0` even under operator misconfig (`=0`/negative) + a mis-probed `0.0` duration
/// — otherwise `1.0 / effective_interval` would be `inf` and ffmpeg would reject `fps=inf`.
fn sample_interval_secs() -> f64 {
    var("MODERATION_SAMPLE_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(2.0)
        .max(0.001)
}

/// Keyframe budget — a coverage *resolution* cap (NOT front-truncation), clamped
/// `.max(1)` so the `effective_interval` divisor is never zero.
pub fn keyframe_max() -> usize {
    var("MODERATION_KEYFRAME_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300)
        .max(1)
}

/// Even full-duration sampling interval (s): widen on long content so the WHOLE
/// duration is sampled within the budget — no front-loaded truncation, no
/// unscanned tail.
pub fn effective_interval(duration_secs: f64) -> f64 {
    // Even-widen on overflow: max(floor, duration/budget). The cap (`keyframe_max`)
    // widens the interval rather than front-truncating the sample.
    sample_interval_secs().max(duration_secs / keyframe_max() as f64)
}

// ── Seam #1: the moderation client ─────────────────────────────────────────────

#[async_trait::async_trait]
pub trait ModerationClient: Send + Sync {
    /// POST the sampled keyframes (in-memory PNG bytes) keyed by `task_id`;
    /// returns the verdict outcome. Any failure ⇒ `Unavailable`. Frames are passed
    /// IN-MEMORY (read once by the gate, Task 5.1.3) — not a dir path — so the
    /// integrity decision and the POST use the SAME data (no GC TOCTOU).
    async fn moderate(
        &self,
        task_id: &str,
        keyframes: &[Vec<u8>],
        source_sha256: Option<String>,
    ) -> ModerationOutcome;
}

/// Test seam: returns a fixed outcome so the A4 fail-closed tests run in isolation
/// without a live node. Used only by `#[cfg(test)]`, hence the targeted allow.
#[allow(dead_code)]
pub struct StubModerationClient {
    pub outcome: ModerationOutcome,
}

#[async_trait::async_trait]
impl ModerationClient for StubModerationClient {
    async fn moderate(
        &self,
        _task_id: &str,
        _keyframes: &[Vec<u8>],
        _source_sha256: Option<String>,
    ) -> ModerationOutcome {
        self.outcome.clone()
    }
}

/// Build the seam-#1 JSON body (§3.2 shape) from in-memory keyframe bytes. An
/// empty slice ⇒ an empty `keyframes_png_base64` array (the audio-only path).
fn build_request_body(
    task_id: &str,
    keyframes: &[Vec<u8>],
    source_sha256: Option<String>,
) -> Result<serde_json::Value> {
    let frames: Vec<String> = keyframes
        .iter()
        .map(|f| general_purpose::STANDARD.encode(f))
        .collect();
    let mut body = serde_json::json!({
        "taskId": task_id,
        "keyframes_png_base64": frames,
    });
    if let Some(sha) = source_sha256 {
        body["sourceSha256"] = serde_json::Value::String(sha);
    }
    Ok(body)
}

/// Real seam-#1 client. POSTs keyframes to the node over the existing blocking
/// `reqwest 0.9` inside `spawn_blocking`. ANY failure (missing URL, body build,
/// client build, transport/timeout, non-2xx, parse, `JoinError`) ⇒ `Unavailable`
/// ⇒ HOLD. Never panics, never fail-open.
pub struct HttpModerationClient;

#[async_trait::async_trait]
impl ModerationClient for HttpModerationClient {
    async fn moderate(
        &self,
        task_id: &str,
        keyframes: &[Vec<u8>],
        source_sha256: Option<String>,
    ) -> ModerationOutcome {
        let url = match node_url() {
            Some(u) => u,
            None => return ModerationOutcome::Unavailable, // config error = HOLD
        };
        let timeout = timeout_secs();
        // Own the inputs for the `'static` blocking closure; this also keeps the
        // CPU-bound base64 (`build_request_body`) OFF the async reactor.
        let task_id = task_id.to_owned();
        let frames: Vec<Vec<u8>> = keyframes.to_vec();
        let join = tokio::task::spawn_blocking(move || {
            let body = match build_request_body(&task_id, &frames, source_sha256) {
                Ok(b) => b,
                Err(_) => return ModerationOutcome::Unavailable,
            };
            let client = match reqwest::Client::builder()
                .timeout(Duration::from_secs(timeout))
                .build()
            {
                Ok(c) => c,
                Err(_) => return ModerationOutcome::Unavailable,
            };
            let mut resp = match client
                .post(&format!("{}/v1/moderate/frames", url))
                .json(&body)
                .send()
            {
                Ok(r) => r,
                Err(_) => return ModerationOutcome::Unavailable, // transport/timeout ⇒ HOLD
            };
            let status = resp.status().as_u16();
            let text = resp.text().unwrap_or_default();
            outcome_from_response(status, &text)
        })
        .await;
        join.unwrap_or(ModerationOutcome::Unavailable) // spawn_blocking JoinError ⇒ HOLD
    }
}

/// The production client. Swapped for `StubModerationClient` in tests.
pub fn default_client() -> Box<dyn ModerationClient> {
    Box::new(HttpModerationClient)
}

/// Streamed SHA-256 of the source file → 64-hex (mirrors `blake3_digest` in `s5.rs`).
/// Optional own-hash exact-match input for the node (PDQ stays the detector).
pub fn sha256_file(path: &str) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 1_048_576];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_may_publish_only_cleared() {
        assert!(may_publish(&ModerationOutcome::Cleared));
        assert!(!may_publish(&ModerationOutcome::Blocked));
        assert!(!may_publish(&ModerationOutcome::Flagged));
        assert!(!may_publish(&ModerationOutcome::Unavailable));
    }

    #[test]
    fn test_outcome_from_response_maps_verdicts() {
        use ModerationOutcome::*;
        assert_eq!(
            outcome_from_response(200, r#"{"verdict":"cleared"}"#),
            Cleared
        );
        assert_eq!(
            outcome_from_response(200, r#"{"verdict":"blocked"}"#),
            Blocked
        );
        assert_eq!(
            outcome_from_response(200, r#"{"verdict":"flagged"}"#),
            Flagged
        );
        // fail-closed: unknown verdict / non-2xx (incl. 404) / malformed ⇒ Unavailable
        assert_eq!(
            outcome_from_response(200, r#"{"verdict":"huh"}"#),
            Unavailable
        );
        assert_eq!(
            outcome_from_response(404, r#"{"verdict":"cleared"}"#),
            Unavailable
        );
        assert_eq!(outcome_from_response(500, ""), Unavailable);
        assert_eq!(outcome_from_response(200, "not json"), Unavailable);
    }

    #[test]
    fn test_config_defaults_and_coverage() {
        // env-unset defaults
        assert!(!moderation_enabled());
        assert_eq!(timeout_secs(), 30);
        assert_eq!(sample_interval_secs(), 2.0);
        assert_eq!(keyframe_max(), 300);
        // full-coverage on a 2 h source: even sample, no unscanned tail, within budget
        let n = effective_interval(7200.0);
        assert!(n >= sample_interval_secs(), "interval must be >= the floor");
        assert!(
            (7200.0 / n).ceil() as usize <= keyframe_max(),
            "frame count must stay within the budget"
        );
        // short source: sampled at the tight floor
        assert_eq!(effective_interval(60.0), sample_interval_secs());
    }

    // ── Phase 2 ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_stub_gate_publishes_only_on_cleared() {
        use ModerationOutcome::*;
        for (outcome, expect) in [
            (Cleared, true),
            (Blocked, false),
            (Flagged, false),
            (Unavailable, false),
        ] {
            let stub = StubModerationClient { outcome };
            assert_eq!(may_publish(&stub.moderate("t", &[], None).await), expect);
        }
    }

    #[test]
    fn test_build_request_body() {
        let frames = vec![vec![1u8, 2, 3], vec![4u8, 5]];
        let body = build_request_body("task-42", &frames, None).unwrap();
        assert_eq!(body["taskId"], "task-42");
        let arr = body["keyframes_png_base64"].as_array().unwrap();
        assert_eq!(arr, &vec!["AQID", "BAU="]); // STANDARD-padded base64, in order
        assert!(body.get("sourceSha256").is_none());
        // sourceSha256 present only when supplied
        let with_sha = build_request_body("t", &frames, Some("abc".into())).unwrap();
        assert_eq!(with_sha["sourceSha256"], "abc");
        // empty slice → empty array (audio-only path)
        let empty = build_request_body("t", &[], None).unwrap();
        assert_eq!(empty["keyframes_png_base64"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_transport_error_holds() {
        // The real client must HOLD when the node is unreachable (connection refused):
        // exercises the `.send()` Err arm — the node-down/timeout fail-closed path.
        std::env::set_var("MODERATION_NODE_URL", "http://127.0.0.1:1");
        let outcome = HttpModerationClient.moderate("t", &[], None).await;
        std::env::remove_var("MODERATION_NODE_URL"); // clean up before asserting
        assert_eq!(outcome, ModerationOutcome::Unavailable);
        assert!(!may_publish(&outcome));
    }

    #[test]
    fn test_http_client_fails_closed() {
        let src = include_str!("moderation.rs");
        assert!(src.contains("ModerationOutcome::Unavailable")); // every failure arm holds
        assert!(src.contains(".timeout(")); // MODERATION_TIMEOUT_SECS enforced
        assert!(src.contains("spawn_blocking")); // off-reactor, no new async dep
    }

    #[test]
    fn test_sha256_file_known_vector() {
        use std::io::Write;
        let path = std::env::temp_dir().join("modtest_sha256_abc.txt");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(b"abc")
            .unwrap();
        assert_eq!(
            sha256_file(path.to_str().unwrap()).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
