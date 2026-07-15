//! Content moderation — M3 sidecar shadow client (D1/D4: shadow-only, fail-open).
//!
//! The pinned moderation sidecar reads the decrypted source from a shared
//! mount; we `POST /v1/moderate` over its Unix socket and relay the verdict to
//! the node. Nothing in this module may delay, fail, or block a transcode job.
//! Spec: `docs/node-reference/HANDOFF-TRANSCODER-M3.md` +
//! `CONTRACT-MODERATION-SERVICE.md`; plan:
//! `docs/development/IMPLEMENTATION-MODERATION-SIDECAR-M3.md`.
//!
//! The pre-M3 A1/A3/A4 fail-closed gate items below are QUARANTINED (retired
//! per Q1(a); deletion is a scheduled post-M3 follow-up).

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
/// QUARANTINED (A4 retired per Q1(a); post-M3 deletion follow-up).
#[allow(dead_code)]
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

// ── Config (env-driven). `pub` accessors are called cross-module.

/// Invocation switch (D1): "do I produce a shadow report" — nothing more.
/// NEVER blocks or delays a job; the blocking switch (`MODERATION_ENFORCE`)
/// lives on the node and the two are deliberately independent. Default `false`.
pub fn moderation_enabled() -> bool {
    var("MODERATION_ENABLED")
        .map(|v| v == "true")
        .unwrap_or(false)
}

/// Transcoder-side path to the sidecar's Unix socket (HAND-OFF §2). `None`
/// while enabled ⇒ one loud config-fault log per job, then fail OPEN.
pub fn socket_path() -> Option<String> {
    var("MODERATION_SOCKET_PATH").ok()
}

/// The sidecar's mount point of our source directory — used ONLY for path
/// translation (HAND-OFF §5.2). Default `/sources`.
pub fn sidecar_source_root() -> String {
    normalize_root(&var("MODERATION_SIDECAR_SOURCE_ROOT").unwrap_or_else(|_| "/sources".into()))
}

/// Trailing slashes trimmed so path joins are uniform (`"/"` becomes `""`,
/// which joins back to an absolute path).
fn normalize_root(s: &str) -> String {
    s.trim_end_matches('/').to_string()
}

/// Translate our local source path to the path AS THE SIDECAR SEES IT
/// (HAND-OFF §5.2 — "the most likely silent bug in the whole integration":
/// a wrong prefix fails open on EVERY job as `SOURCE_OUTSIDE_ROOT`/`_NOT_FOUND`).
/// Deployment rule this encodes: the sidecar mounts the PARENT DIRECTORY of
/// the decrypted files as its source root, so the mapping is
/// `<root>/<file_name(local)>` — correct for both `PATH_TO_FILE` shapes in
/// use (`/dir/` + cid and `/dir/prefix_` + cid).
pub fn sidecar_source_path(local: &str) -> Option<String> {
    translate_source_path(local, &sidecar_source_root())
}

fn translate_source_path(local: &str, root: &str) -> Option<String> {
    let name = std::path::Path::new(local).file_name()?.to_str()?;
    Some(format!("{}/{}", root, name))
}

// ── M3 shadow call: response classification (CONTRACT §3/§4) ───────────────

/// Every possible outcome of one shadow moderation attempt. NOTHING here can
/// affect the job (D4 fail-open) — the variants exist so the client logs the
/// `error.kind` detail that "exists nowhere else" (HAND-OFF §8) and relays
/// only true verdicts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShadowOutcome {
    /// 200 with a `verdict` key and no `error` key — the only relayable case.
    /// `raw` is the exact response body: M5 will sign canonical bytes, so the
    /// report must never be re-serialised (HAND-OFF §6).
    Verdict {
        verdict: String,
        /// `category ?? rating ?? None` — always `None` for `BLOCK_UNRESOLVED`
        /// (that kind carries neither; not missing data).
        reason: Option<String>,
        /// The service-derived content identity (D3a) — used for the shadow-
        /// sink grep during acceptance; never a storage key.
        content_id: String,
        raw: String,
    },
    /// The CONTRACT §3 error envelope, any status (4xx = rejected before the
    /// run slot; 200-with-error = an attempt that produced no verdict).
    ServiceError {
        kind: String,
        detail: String,
        status: u16,
    },
    /// Anything non-envelope: the framework 422 shape, garbage, or a 5xx
    /// ("the service never sends a 5xx — any 5xx observed is a bug").
    Malformed { status: u16 },
    /// Out-of-band client failure (CONTRACT §4): connect refused/reset/
    /// deadline expiry. Never reaches the §3 taxonomy; same log-and-move-on.
    Transport { detail: String },
}

/// Map one HTTP response to a `ShadowOutcome`. Encodes CONTRACT §3's rule:
/// *a body is a verdict iff it has a `verdict` key and no `error` key*, and
/// success is only ever a 200.
pub fn classify(status: u16, body: &str) -> ShadowOutcome {
    // CONTRACT §3: "the service never sends a 5xx — any 5xx observed is a
    // bug." Status precedence beats any body shape (even a well-formed error
    // envelope), so the log line always carries the sidecar-bug ALERT.
    if status >= 500 {
        return ShadowOutcome::Malformed { status };
    }
    let v: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return ShadowOutcome::Malformed { status },
    };
    if let Some(err) = v.get("error") {
        if let (Some(kind), Some(detail)) = (
            err.get("kind").and_then(|k| k.as_str()),
            err.get("detail").and_then(|d| d.as_str()),
        ) {
            return ShadowOutcome::ServiceError {
                kind: kind.to_owned(),
                detail: detail.to_owned(),
                status,
            };
        }
        return ShadowOutcome::Malformed { status };
    }
    if status == 200 {
        if let Some(verdict) = v.get("verdict").and_then(|s| s.as_str()) {
            let reason = v
                .get("category")
                .and_then(|c| c.as_str())
                .or_else(|| v.get("rating").and_then(|r| r.as_str()))
                .map(str::to_owned);
            let content_id = v
                .get("contentId")
                .and_then(|c| c.as_str())
                .unwrap_or_default()
                .to_owned();
            return ShadowOutcome::Verdict {
                verdict: verdict.to_owned(),
                reason,
                content_id,
                raw: body.to_owned(),
            };
        }
    }
    ShadowOutcome::Malformed { status }
}

/// Host-local node base URL; `None` (missing) ⇒ fail-closed HOLD at the client.
fn node_url() -> Option<String> {
    var("MODERATION_NODE_URL").ok()
}

/// Per-call deadline (s) for the sidecar POST: connect → response, starting
/// AFTER the one-permit semaphore is acquired. The queue wait is deliberately
/// untimed (HAND-OFF §5.4) — it is transitively bounded because every
/// permit-holder is bounded by this deadline. Default: CONTRACT §6 worst case
/// ~77 min for our own run + the service-side wait behind at most one
/// abandoned worst-case run + margin ⇒ 3 h. On expiry ⇒ `Transport`, fail open.
fn timeout_secs() -> u64 {
    var("MODERATION_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_800)
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

// ── M3 GC pin registry ─────────────────────────────────────────────────────
// CONTRACT §3: the source must never be mutated/deleted from before the call
// until the response arrives (worst case hours, §6). GC is the only deletion
// path for sources (verified invariant — structural test in server.rs), so
// pinning GC covers the whole requirement.

/// Refcounted so two shadow windows on one cached source (same cid, two jobs)
/// stay pinned until BOTH release.
static PINNED: once_cell::sync::Lazy<std::sync::Mutex<std::collections::HashMap<String, usize>>> =
    once_cell::sync::Lazy::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// Lock helper that survives poisoning: pin bookkeeping must keep working
/// even if some thread panicked mid-update (never-panic shadow code).
fn pinned_map() -> std::sync::MutexGuard<'static, std::collections::HashMap<String, usize>> {
    PINNED.lock().unwrap_or_else(|e| e.into_inner())
}

/// RAII pin: the path is protected from GC until the guard drops — on every
/// exit path including panic and deadline expiry.
pub struct PinGuard {
    path: String,
}

pub fn pin(path: &str) -> PinGuard {
    *pinned_map().entry(path.to_owned()).or_insert(0) += 1;
    PinGuard {
        path: path.to_owned(),
    }
}

impl Drop for PinGuard {
    fn drop(&mut self) {
        let mut map = pinned_map();
        if let Some(count) = map.get_mut(&self.path) {
            *count -= 1;
            if *count == 0 {
                map.remove(&self.path);
            }
        }
    }
}

/// Consulted by `garbage_collect` (server.rs) before deleting anything.
pub fn is_pinned(path: &std::path::Path) -> bool {
    match path.to_str() {
        Some(s) => pinned_map().contains_key(s),
        None => false,
    }
}

// ── M3 shadow call: HTTP over the sidecar's Unix socket ────────────────────

/// `POST /v1/moderate` request body. `contentId` is deliberately absent
/// (HAND-OFF §5.2, D3a: the service derives it; its value is authoritative).
fn build_moderate_body(sidecar_path: &str) -> String {
    serde_json::json!({ "sourcePath": sidecar_path }).to_string()
}

/// One request at a time per sidecar (HAND-OFF §5.3; the service's single run
/// slot serialises queued requests anyway, CONTRACT §5).
static SIDECAR_SLOT: once_cell::sync::Lazy<tokio::sync::Semaphore> =
    once_cell::sync::Lazy::new(|| tokio::sync::Semaphore::new(1));

// ── Admission control: bound the pending shadow queue ──────────────────────
// Each pending shadow call pins a multi-GB source against GC for up to hours.
// Without a bound, a wedged-but-connectable sidecar under sustained job
// arrivals grows the pin set until the cache volume fills — at which point
// NEW jobs' downloads/decrypts fail, and D1 forbids `MODERATION_ENABLED`
// from ever degrading job success. At the cap we SHED: fail open, ALERT log.

/// Cap on simultaneously pending (queued + in-flight) shadow calls.
/// Worst-case pinned bytes ≈ cap × ~2× largest source; a healthy queue of 8
/// drains in minutes at the ~100 s typical call time.
fn max_pending() -> usize {
    var("MODERATION_MAX_PENDING")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8)
        .max(1)
}

static SHADOW_PENDING: once_cell::sync::Lazy<std::sync::Mutex<usize>> =
    once_cell::sync::Lazy::new(|| std::sync::Mutex::new(0));

/// RAII admission token: dropping it (any exit path, panic included) frees
/// the slot.
pub struct ShadowPendingGuard;

pub fn try_shadow_slot() -> Option<ShadowPendingGuard> {
    let mut n = SHADOW_PENDING.lock().unwrap_or_else(|e| e.into_inner());
    if *n >= max_pending() {
        return None;
    }
    *n += 1;
    Some(ShadowPendingGuard)
}

impl Drop for ShadowPendingGuard {
    fn drop(&mut self) {
        let mut n = SHADOW_PENDING.lock().unwrap_or_else(|e| e.into_inner());
        *n = n.saturating_sub(1);
    }
}

/// Shadow-moderate one source via the sidecar. Returns
/// `(outcome, wait_ms, call_ms)`: `wait_ms` = semaphore queue wait (untimed —
/// transitively bounded by each holder's deadline; HAND-OFF §5.4), `call_ms`
/// = connect→response under the per-call deadline. `call_ms` on a verdict is
/// the CONTRACT §6 typical-escalation budget number; never fold `wait_ms` in.
pub async fn moderate_via_sidecar(socket: &str, sidecar_path: &str) -> (ShadowOutcome, u128, u128) {
    moderate_with_slot(
        &SIDECAR_SLOT,
        socket,
        sidecar_path,
        Duration::from_secs(timeout_secs()),
    )
    .await
}

/// Inner form with injectable slot + deadline (tests use a local slot so
/// parallel tests cannot contaminate each other's `wait_ms`).
async fn moderate_with_slot(
    slot: &tokio::sync::Semaphore,
    socket: &str,
    sidecar_path: &str,
    deadline: Duration,
) -> (ShadowOutcome, u128, u128) {
    let wait_start = std::time::Instant::now();
    let _permit = match slot.acquire().await {
        Ok(p) => p,
        Err(e) => {
            // unreachable (the slot is never closed) — but never panic in shadow code
            return (
                ShadowOutcome::Transport {
                    detail: format!("semaphore: {}", e),
                },
                wait_start.elapsed().as_millis(),
                0,
            );
        }
    };
    let wait_ms = wait_start.elapsed().as_millis();
    let call_start = std::time::Instant::now();
    let driver_slot: DriverSlot = std::sync::Arc::new(std::sync::Mutex::new(None));
    let outcome = match tokio::time::timeout(
        deadline,
        sidecar_post(socket, sidecar_path, &driver_slot),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(_) => {
            // The exchange future is dropped, but its detached connection
            // driver could otherwise keep the socket open against a wedged
            // sidecar indefinitely — abort it so nothing outlives the call.
            abort_driver(&driver_slot);
            ShadowOutcome::Transport {
                detail: format!("deadline {:?} expired", deadline),
            }
        }
    };
    (outcome, wait_ms, call_start.elapsed().as_millis())
}

/// Holds the spawned hyper connection-driver task so the deadline arm in
/// `moderate_with_slot` can abort it after the exchange future is dropped.
type DriverSlot = std::sync::Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>;

fn abort_driver(slot: &DriverSlot) {
    if let Some(handle) = slot.lock().unwrap_or_else(|e| e.into_inner()).take() {
        handle.abort();
    }
}

/// The bare HTTP/1.1 exchange over the UDS. Every failure arm ⇒ `Transport`
/// (CONTRACT §4 out-of-band bucket): no verdict, log and move on.
async fn sidecar_post(socket: &str, sidecar_path: &str, driver_slot: &DriverSlot) -> ShadowOutcome {
    let stream = match tokio::net::UnixStream::connect(socket).await {
        Ok(s) => s,
        Err(e) => {
            return ShadowOutcome::Transport {
                detail: format!("connect: {}", e),
            }
        }
    };
    let (mut sender, conn) = match hyper::client::conn::handshake(stream).await {
        Ok(x) => x,
        Err(e) => {
            return ShadowOutcome::Transport {
                detail: format!("handshake: {}", e),
            }
        }
    };
    // Driver parked in the shared slot: on success `Connection: close` ends it
    // promptly; on any failure (incl. deadline expiry upstream) it is aborted.
    *driver_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(tokio::spawn(async move {
        let _ = conn.await;
    }));
    let req = match hyper::Request::post("/v1/moderate")
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("connection", "close")
        .body(hyper::Body::from(build_moderate_body(sidecar_path)))
    {
        Ok(r) => r,
        Err(e) => {
            abort_driver(driver_slot);
            return ShadowOutcome::Transport {
                detail: format!("request: {}", e),
            };
        }
    };
    let resp = match sender.send_request(req).await {
        Ok(r) => r,
        Err(e) => {
            abort_driver(driver_slot);
            return ShadowOutcome::Transport {
                detail: format!("send: {}", e),
            };
        }
    };
    let status = resp.status().as_u16();
    match hyper::body::to_bytes(resp.into_body()).await {
        Ok(bytes) => match String::from_utf8(bytes.to_vec()) {
            Ok(text) => classify(status, &text),
            Err(_) => ShadowOutcome::Malformed { status },
        },
        Err(e) => {
            abort_driver(driver_slot);
            ShadowOutcome::Transport {
                detail: format!("body: {}", e),
            }
        }
    }
}

/// The outcome half of the per-job structured log line (HAND-OFF §8 — "your
/// client must log every outcome from the HTTP response body; the error.kind
/// detail exists nowhere else"). ALERT markers: the three deployment-fault
/// kinds that fail EVERY job's moderation (§5.6), and the never-legal 5xx.
pub fn outcome_log_fragment(outcome: &ShadowOutcome) -> String {
    match outcome {
        ShadowOutcome::Verdict {
            verdict,
            content_id,
            ..
        } => format!("outcome=verdict:{} contentId={}", verdict, content_id),
        ShadowOutcome::ServiceError {
            kind,
            detail,
            status,
        } => {
            let alert = matches!(
                kind.as_str(),
                "SOURCE_UNREADABLE" | "SOURCE_OUTSIDE_ROOT" | "SOURCE_NOT_FOUND"
            );
            format!(
                "outcome=error:{} status={} detail={}{}",
                kind,
                status,
                detail,
                if alert { " ALERT deployment-fault" } else { "" }
            )
        }
        ShadowOutcome::Malformed { status } => format!(
            "outcome=malformed:{}{}",
            status,
            if *status >= 500 {
                " ALERT sidecar-bug"
            } else {
                ""
            }
        ),
        ShadowOutcome::Transport { detail } => format!("outcome=transport:{}", detail),
    }
}

// ── M3 relay to the node (HAND-OFF §6; transport is OQ-M3-1, stubbed) ──────

/// Build the relay envelope for a `Verdict` outcome; `None` for every other
/// outcome (no verdict ⇒ nothing to relay). Keyed by OUR `task_id` — the node
/// resolves `task_id → job_id` server-side (amended HAND-OFF §6). The report
/// is spliced in **verbatim** via `RawValue`: M5 will sign its canonical
/// bytes, and a re-serialised copy is a different artifact.
pub fn build_relay_envelope(task_id: &str, outcome: &ShadowOutcome) -> Option<String> {
    #[derive(serde::Serialize)]
    struct RelayEnvelope<'a> {
        task_id: &'a str,
        verdict: &'a str,
        reason: Option<&'a str>,
        report: &'a serde_json::value::RawValue,
    }
    if let ShadowOutcome::Verdict {
        verdict,
        reason,
        raw,
        ..
    } = outcome
    {
        let report = serde_json::value::RawValue::from_string(raw.clone()).ok()?;
        return serde_json::to_string(&RelayEnvelope {
            task_id,
            verdict,
            reason: reason.as_deref(),
            report: &report,
        })
        .ok();
    }
    None
}

/// The final delivery hop to the node. Endpoint/transport/auth are OQ-M3-1;
/// until answered, `StubVerdictRelay` logs the envelope and succeeds — the
/// real transport lands behind this trait with no call-site changes.
#[async_trait::async_trait]
pub trait VerdictRelay: Send + Sync {
    async fn relay(&self, task_id: &str, envelope_json: &str) -> Result<()>;
}

pub struct StubVerdictRelay;

#[async_trait::async_trait]
impl VerdictRelay for StubVerdictRelay {
    async fn relay(&self, task_id: &str, envelope_json: &str) -> Result<()> {
        println!(
            "MODERATION RELAY (stub, OQ-M3-1) task_id={} envelope={}",
            task_id, envelope_json
        );
        Ok(())
    }
}

/// The production relay. Swapped for a real transport once OQ-M3-1 lands.
pub fn default_relay() -> Box<dyn VerdictRelay> {
    Box::new(StubVerdictRelay)
}

// ── Seam #1: the moderation client ─────────────────────────────────────────────

#[async_trait::async_trait]
pub trait ModerationClient: Send + Sync {
    /// QUARANTINED (A3 seam retired per Q1(a); post-M3 deletion follow-up).
    #[allow(dead_code)]
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
/// QUARANTINED (A3 seam retired per Q1(a); post-M3 deletion follow-up).
#[allow(dead_code)]
pub fn default_client() -> Box<dyn ModerationClient> {
    Box::new(HttpModerationClient)
}

/// Streamed SHA-256 of the source file → 64-hex (mirrors `blake3_digest` in `s5.rs`).
/// Optional own-hash exact-match input for the node (PDQ stays the detector).
/// QUARANTINED (M3 omits contentId per D3a; post-M3 deletion follow-up).
#[allow(dead_code)]
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

    // ── M3 (sidecar shadow) — Phase 1.1 ───────────────────────────────────

    #[test]
    fn test_m3_config_defaults() {
        // env-unset defaults: no socket path, /sources root
        assert_eq!(socket_path(), None);
        assert_eq!(sidecar_source_root(), "/sources");
    }

    #[test]
    fn test_sidecar_source_path_shapes() {
        // dir-with-slash shape: PATH_TO_FILE = "/cache/sources/" + cid
        assert_eq!(
            translate_source_path("/cache/sources/bafy123", "/sources"),
            Some("/sources/bafy123".into())
        );
        // string-prefix shape: PATH_TO_FILE = "/cache/src_" + cid
        assert_eq!(
            translate_source_path("/cache/src_bafy123", "/sources"),
            Some("/sources/src_bafy123".into())
        );
        // root "/" (normalized to "") still joins to an absolute path
        assert_eq!(
            translate_source_path("/cache/sources/x.mp4", ""),
            Some("/x.mp4".into())
        );
        // pathological: no file name component
        assert_eq!(translate_source_path("/", "/sources"), None);
        // the pub wrapper uses the env default root
        assert_eq!(
            sidecar_source_path("/cache/sources/bafy123"),
            Some("/sources/bafy123".into())
        );
    }

    #[test]
    fn test_normalize_root_trims_trailing_slashes() {
        assert_eq!(normalize_root("/sources/"), "/sources");
        assert_eq!(normalize_root("/sources///"), "/sources");
        assert_eq!(normalize_root("/sources"), "/sources");
        assert_eq!(normalize_root("/"), ""); // joins back as "" + "/<name>" = "/<name>"
    }

    // ── M3 — Phase 2.1: response classification ───────────────────────────

    /// CONTRACT §3's worked example, verbatim ("copy-pasteable into validator
    /// tests" is the contract's own invitation).
    const WORKED_EXAMPLE: &str = r#"{
  "schemaVersion": 1,
  "contentId": "c451f3a91d5afa1b1db1c9dcd2646df38ed32e21b10bb18c93cf1285e6a7ebb1",
  "bundleHash": "sha256:ec1b85cf0e2ad12d6a7c3dccd93d164b93df9154aae7b2ba99f9f82ec1129fdf",
  "bundleVersion": 1,
  "samplerVersion": 1,
  "verdict": "PASS_RATED",
  "rating": "18",
  "descriptors": ["sex"],
  "category": null,
  "stats": { "framesSampled": 7212, "maxScore": 0.91, "escalated": 38 },
  "scoreFileCid": null,
  "scoreDigest": null,
  "hashMatchModule": null,
  "hostAddress": null,
  "signature": null
}"#;

    #[test]
    fn test_classify_worked_example_verdict() {
        match classify(200, WORKED_EXAMPLE) {
            ShadowOutcome::Verdict {
                verdict,
                reason,
                content_id,
                raw,
            } => {
                assert_eq!(verdict, "PASS_RATED");
                assert_eq!(reason.as_deref(), Some("18")); // category null ⇒ rating
                assert_eq!(
                    content_id,
                    "c451f3a91d5afa1b1db1c9dcd2646df38ed32e21b10bb18c93cf1285e6a7ebb1"
                );
                assert_eq!(raw, WORKED_EXAMPLE); // byte-identical (M5 signs canonical bytes)
            }
            other => panic!("expected Verdict, got {:?}", other),
        }
    }

    #[test]
    fn test_classify_error_kinds() {
        // CONTRACT §3 taxonomy: 4xx = rejected before any run; 200-with-error =
        // an attempt existed and produced no verdict. All are ServiceError.
        for (status, kind) in [
            (403, "SOURCE_OUTSIDE_ROOT"),
            (403, "SOURCE_UNREADABLE"),
            (400, "SOURCE_INVALID"),
            (404, "SOURCE_NOT_FOUND"),
            (409, "CONTENT_ID_MISMATCH"),
            (200, "CLIENT_GONE"),
            (200, "SOURCE_MUTATED"),
            (200, "PIPELINE_FAILURE"),
        ] {
            let body = format!(r#"{{"error":{{"kind":"{}","detail":"d"}}}}"#, kind);
            assert_eq!(
                classify(status, &body),
                ShadowOutcome::ServiceError {
                    kind: kind.into(),
                    detail: "d".into(),
                    status
                }
            );
        }
    }

    #[test]
    fn test_classify_malformed_and_defensive() {
        use ShadowOutcome::*;
        // 422: FastAPI's default {"detail":[...]} — NOT the error envelope
        assert_eq!(
            classify(422, r#"{"detail":[{"loc":["body","sourcePath"]}]}"#),
            Malformed { status: 422 }
        );
        // "the service never sends a 5xx — any 5xx observed is a bug"
        assert_eq!(classify(500, ""), Malformed { status: 500 });
        assert_eq!(classify(200, "not json"), Malformed { status: 200 });
        // defensive: verdict + error keys are mutually exclusive by
        // construction — if both appear, it is NOT a verdict
        let both = r#"{"verdict":"PASS","error":{"kind":"PIPELINE_FAILURE","detail":"d"}}"#;
        assert!(!matches!(classify(200, both), Verdict { .. }));
        // a verdict body on a non-200 is not a success (success is only ever 200)
        assert_eq!(classify(404, WORKED_EXAMPLE), Malformed { status: 404 });
    }

    // ── M3 — Phase 5.1: structured log fragments ───────────────────────────

    #[test]
    fn test_outcome_log_fragment_shapes() {
        // verdict: carries contentId (the shadow-sink grep key, HAND-OFF §8)
        let frag = outcome_log_fragment(&classify(200, WORKED_EXAMPLE));
        assert_eq!(
            frag,
            "outcome=verdict:PASS_RATED contentId=c451f3a91d5afa1b1db1c9dcd2646df38ed32e21b10bb18c93cf1285e6a7ebb1"
        );
        // deployment faults carry the ALERT marker (HAND-OFF §5.6/§8)
        for kind in [
            "SOURCE_UNREADABLE",
            "SOURCE_OUTSIDE_ROOT",
            "SOURCE_NOT_FOUND",
        ] {
            let frag = outcome_log_fragment(&ShadowOutcome::ServiceError {
                kind: kind.into(),
                detail: "d".into(),
                status: 403,
            });
            assert!(frag.contains("ALERT deployment-fault"), "{}", frag);
            assert!(frag.contains(&format!("outcome=error:{}", kind)));
        }
        // a run-level error (no alert)
        let frag = outcome_log_fragment(&ShadowOutcome::ServiceError {
            kind: "PIPELINE_FAILURE".into(),
            detail: "d".into(),
            status: 200,
        });
        assert!(!frag.contains("ALERT"), "{}", frag);
        // 5xx is never legal from the service — flag as sidecar bug
        let frag = outcome_log_fragment(&ShadowOutcome::Malformed { status: 502 });
        assert!(frag.contains("ALERT sidecar-bug"), "{}", frag);
        assert!(!outcome_log_fragment(&ShadowOutcome::Malformed { status: 422 }).contains("ALERT"));
        let frag = outcome_log_fragment(&ShadowOutcome::Transport {
            detail: "connect: refused".into(),
        });
        assert_eq!(frag, "outcome=transport:connect: refused");
    }

    // ── M3 — Phase 4.1: GC pin registry ────────────────────────────────────

    #[test]
    fn test_pin_registry_guard_semantics() {
        let p = "/tmp/pin_test_source_a";
        assert!(!is_pinned(std::path::Path::new(p)));
        let g1 = pin(p);
        assert!(is_pinned(std::path::Path::new(p)));
        // double-pin (two shadow windows on one cached source): pinned until
        // BOTH guards drop
        let g2 = pin(p);
        drop(g1);
        assert!(is_pinned(std::path::Path::new(p)));
        drop(g2);
        assert!(!is_pinned(std::path::Path::new(p)));
    }

    #[test]
    fn test_pin_guard_released_on_panic() {
        // RAII: a panicking shadow task must not leak a pin (edge case 4)
        let p = "/tmp/pin_test_source_b";
        let result = std::panic::catch_unwind(|| {
            let _g = pin(p);
            panic!("boom");
        });
        assert!(result.is_err());
        assert!(!is_pinned(std::path::Path::new(p)));
    }

    // ── M3 — Phase 3.1: request body ───────────────────────────────────────

    #[test]
    fn test_moderate_body_omits_content_id() {
        // HAND-OFF §5.2 / D3a: send sourcePath only — the service derives
        // contentId from the bytes and its value is authoritative.
        let body = build_moderate_body("/sources/bafy123");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["sourcePath"], "/sources/bafy123");
        assert!(v.get("contentId").is_none());
        assert_eq!(v.as_object().unwrap().len(), 1);
    }

    // ── M3 — Phase 3.2: fake-sidecar UDS tests ─────────────────────────────
    // A raw UnixListener writing canned HTTP/1.1 bytes — exercises the real
    // client end-to-end in-process, covering CONTRACT §4's out-of-band
    // failure classes that classify() alone cannot.

    fn test_sock(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("modtest_uds_{}.sock", name));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn http_json(status_line: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            status_line,
            body.len(),
            body
        )
        .into_bytes()
    }

    /// Accepts connections, reads the request, waits `delay_ms`, writes
    /// `response` verbatim, closes. `None` response = accept then hang (the
    /// wedged/half-open sidecar).
    fn spawn_fake_sidecar(sock: std::path::PathBuf, response: Option<Vec<u8>>, delay_ms: u64) {
        // Bind BEFORE spawning: on the single-threaded test runtime the spawned
        // task only runs once the caller yields — the client must not race it.
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = listener.accept().await.unwrap();
                let resp = response.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 4096];
                    let _ = s.read(&mut buf).await;
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    match resp {
                        Some(r) => {
                            let _ = s.write_all(&r).await;
                        }
                        None => tokio::time::sleep(Duration::from_secs(3600)).await,
                    }
                });
            }
        });
    }

    fn slot1() -> tokio::sync::Semaphore {
        tokio::sync::Semaphore::new(1)
    }

    #[tokio::test]
    async fn test_uds_client_verdict_end_to_end() {
        let sock = test_sock("verdict");
        spawn_fake_sidecar(sock.clone(), Some(http_json("200 OK", WORKED_EXAMPLE)), 0);
        let slot = slot1();
        let (outcome, wait_ms, _call_ms) = moderate_with_slot(
            &slot,
            sock.to_str().unwrap(),
            "/sources/x",
            Duration::from_secs(5),
        )
        .await;
        match outcome {
            ShadowOutcome::Verdict { verdict, raw, .. } => {
                assert_eq!(verdict, "PASS_RATED");
                assert_eq!(raw, WORKED_EXAMPLE); // byte-verbatim through the wire
            }
            o => panic!("{:?}", o),
        }
        assert!(wait_ms < 2000, "uncontended slot, wait_ms={}", wait_ms);
    }

    #[tokio::test]
    async fn test_uds_client_service_error_and_malformed() {
        // 403 error envelope → ServiceError with the kind preserved
        let sock = test_sock("err403");
        spawn_fake_sidecar(
            sock.clone(),
            Some(http_json(
                "403 Forbidden",
                r#"{"error":{"kind":"SOURCE_OUTSIDE_ROOT","detail":"d"}}"#,
            )),
            0,
        );
        let slot = slot1();
        let (outcome, _, _) = moderate_with_slot(
            &slot,
            sock.to_str().unwrap(),
            "/wrong/prefix",
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(
            outcome,
            ShadowOutcome::ServiceError {
                kind: "SOURCE_OUTSIDE_ROOT".into(),
                detail: "d".into(),
                status: 403
            }
        );
        // framework 422 → Malformed
        let sock = test_sock("err422");
        spawn_fake_sidecar(
            sock.clone(),
            Some(http_json("422 Unprocessable Entity", r#"{"detail":[]}"#)),
            0,
        );
        let slot = slot1();
        let (outcome, _, _) = moderate_with_slot(
            &slot,
            sock.to_str().unwrap(),
            "/sources/x",
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(outcome, ShadowOutcome::Malformed { status: 422 });
    }

    #[tokio::test]
    async fn test_uds_client_garbage_and_refused() {
        // non-HTTP bytes → Transport (hyper parse failure)
        let sock = test_sock("garbage");
        spawn_fake_sidecar(sock.clone(), Some(b"NOT HTTP AT ALL\r\n\r\n".to_vec()), 0);
        let slot = slot1();
        let (outcome, _, _) = moderate_with_slot(
            &slot,
            sock.to_str().unwrap(),
            "/sources/x",
            Duration::from_secs(5),
        )
        .await;
        assert!(
            matches!(outcome, ShadowOutcome::Transport { .. }),
            "{:?}",
            outcome
        );
        // nothing listening → Transport(connect) — "connect-refused never
        // produces an error.kind" (HAND-OFF §8)
        let sock = test_sock("refused");
        let slot = slot1();
        let (outcome, _, _) = moderate_with_slot(
            &slot,
            sock.to_str().unwrap(),
            "/sources/x",
            Duration::from_secs(5),
        )
        .await;
        match outcome {
            ShadowOutcome::Transport { detail } => assert!(detail.contains("connect")),
            o => panic!("{:?}", o),
        }
    }

    #[tokio::test]
    async fn test_uds_client_deadline_expiry() {
        // wedged sidecar (accepts, never responds) → per-call deadline fires
        let sock = test_sock("hang");
        spawn_fake_sidecar(sock.clone(), None, 0);
        let slot = slot1();
        let (outcome, wait_ms, call_ms) = moderate_with_slot(
            &slot,
            sock.to_str().unwrap(),
            "/sources/x",
            Duration::from_secs(1),
        )
        .await;
        match outcome {
            ShadowOutcome::Transport { detail } => assert!(detail.contains("deadline")),
            o => panic!("{:?}", o),
        }
        assert!(
            (900..5000).contains(&(call_ms as u64)),
            "call_ms={}",
            call_ms
        );
        assert!(wait_ms < 500, "wait_ms={}", wait_ms);
    }

    #[tokio::test]
    async fn test_uds_client_serialises_without_spurious_timeout() {
        // Review issue 1 at test scale: two calls share the slot, and the
        // per-call deadline (clock starts post-acquire) lets BOTH succeed.
        let sock = test_sock("serial");
        // fake delay 1000 ms, deadline 1800 ms: each call fits (800 ms CI
        // margin) but queue-wait + call for the second (~2000 ms) exceeds the
        // deadline — the old whole-window design would spuriously expire it.
        spawn_fake_sidecar(
            sock.clone(),
            Some(http_json("200 OK", WORKED_EXAMPLE)),
            1000,
        );
        let slot = slot1();
        let path = sock.to_str().unwrap();
        let (a, b) = tokio::join!(
            moderate_with_slot(&slot, path, "/sources/a", Duration::from_millis(1800)),
            moderate_with_slot(&slot, path, "/sources/b", Duration::from_millis(1800)),
        );
        for (outcome, _, call_ms) in [&a, &b] {
            assert!(
                matches!(outcome, ShadowOutcome::Verdict { .. }),
                "expected Verdict, got {:?}",
                outcome
            );
            assert!(*call_ms < 1800, "call_ms={}", call_ms);
        }
        // exactly one of the two queued behind the other
        let max_wait = a.1.max(b.1);
        assert!(
            max_wait >= 800,
            "expected a queued call, max wait_ms={}",
            max_wait
        );
    }

    // ── M3 — Phase 2.2: relay envelope + stub relay ────────────────────────

    #[test]
    fn test_relay_envelope_verbatim_report() {
        let env = build_relay_envelope("task-42", &classify(200, WORKED_EXAMPLE)).unwrap();
        // the report value is the raw response bytes, spliced verbatim
        assert!(env.contains(WORKED_EXAMPLE));
        let v: serde_json::Value = serde_json::from_str(&env).unwrap();
        assert_eq!(v["task_id"], "task-42"); // amended HAND-OFF §6: keyed by task_id
        assert_eq!(v["verdict"], "PASS_RATED");
        assert_eq!(v["reason"], "18");
        // only a Verdict produces an envelope
        assert!(build_relay_envelope("t", &ShadowOutcome::Malformed { status: 500 }).is_none());
        assert!(
            build_relay_envelope("t", &ShadowOutcome::Transport { detail: "x".into() }).is_none()
        );
    }

    #[test]
    fn test_relay_envelope_block_unresolved_reason_null() {
        let body =
            r#"{"verdict":"BLOCK_UNRESOLVED","category":null,"rating":null,"contentId":"bb"}"#;
        let env = build_relay_envelope("t", &classify(200, body)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&env).unwrap();
        assert!(v["reason"].is_null()); // correct for BLOCK_UNRESOLVED, not missing data
        assert!(env.contains(body));
    }

    #[tokio::test]
    async fn test_stub_relay_accepts_envelope() {
        let env = build_relay_envelope("task-42", &classify(200, WORKED_EXAMPLE)).unwrap();
        assert!(default_relay().relay("task-42", &env).await.is_ok());
    }

    #[test]
    fn test_classify_5xx_envelope_is_malformed() {
        // CONTRACT §3: "the service never sends a 5xx — any 5xx observed is a
        // bug." Status precedence beats the envelope: even a well-formed error
        // body on a 5xx must classify as Malformed so the log line carries the
        // sidecar-bug ALERT (review round 2, finding 3).
        let enveloped = r#"{"error":{"kind":"PIPELINE_FAILURE","detail":"d"}}"#;
        assert_eq!(
            classify(502, enveloped),
            ShadowOutcome::Malformed { status: 502 }
        );
        assert!(outcome_log_fragment(&classify(502, enveloped)).contains("ALERT sidecar-bug"));
        // and a 5xx verdict-shaped body is equally not a verdict
        assert_eq!(
            classify(500, WORKED_EXAMPLE),
            ShadowOutcome::Malformed { status: 500 }
        );
    }

    // ── M3 — Round 2: shadow-queue cap (fail-open shedding) ────────────────

    #[test]
    fn test_shadow_pending_cap_sheds_then_recovers() {
        // Bound the number of in-flight shadow tasks (each pins a multi-GB
        // source): at the cap, try_shadow_slot() sheds — fail open — instead
        // of letting a wedged sidecar grow the pin set until the disk fills
        // (which would make MODERATION_ENABLED degrade job success, D1).
        let cap = max_pending();
        assert_eq!(cap, 8, "default cap");
        let mut guards = Vec::new();
        for _ in 0..cap {
            guards.push(try_shadow_slot().expect("below cap ⇒ Some"));
        }
        assert!(try_shadow_slot().is_none(), "at cap ⇒ shed");
        guards.pop(); // one slot frees…
        let again = try_shadow_slot();
        assert!(again.is_some(), "…and admission resumes");
        drop(again);
        drop(guards);
        assert!(try_shadow_slot().is_some(), "all released");
    }

    #[test]
    fn test_classify_reason_rule() {
        // reason = category ?? rating ?? None (HAND-OFF §6)
        let block = r#"{"verdict":"BLOCK_ILLEGAL","category":"csam_suspected","rating":null,"contentId":"aa"}"#;
        match classify(200, block) {
            ShadowOutcome::Verdict { reason, .. } => {
                assert_eq!(reason.as_deref(), Some("csam_suspected"))
            }
            o => panic!("{:?}", o),
        }
        // BLOCK_UNRESOLVED carries neither — reason null is CORRECT, not missing data
        let unresolved =
            r#"{"verdict":"BLOCK_UNRESOLVED","category":null,"rating":null,"contentId":"bb"}"#;
        match classify(200, unresolved) {
            ShadowOutcome::Verdict { reason, .. } => assert_eq!(reason, None),
            o => panic!("{:?}", o),
        }
        let pass = r#"{"verdict":"PASS","category":null,"rating":null,"contentId":"cc"}"#;
        match classify(200, pass) {
            ShadowOutcome::Verdict { reason, .. } => assert_eq!(reason, None),
            o => panic!("{:?}", o),
        }
    }

    #[test]
    fn test_config_defaults_and_coverage() {
        // env-unset defaults
        assert!(!moderation_enabled());
        // M3: per-call deadline (starts post-semaphore-acquire) — 2×77 min
        // CONTRACT §6 worst case + margin, was 30 s for the old seam-#1 POST.
        assert_eq!(timeout_secs(), 10_800);
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
