//! Content moderation — M3 sidecar shadow client (D1/D4: shadow-only, fail-open).
//!
//! The pinned moderation sidecar reads the decrypted source from a shared
//! mount; we `POST /v1/moderate` over its Unix socket and relay the verdict to
//! the node. Nothing in this module may delay, fail, or block a transcode job.
//! Spec: `docs/node-reference/HANDOFF-TRANSCODER-M3.md` +
//! `CONTRACT-MODERATION-SERVICE.md`; plan:
//! `docs/development/IMPLEMENTATION-MODERATION-SIDECAR-M3.md`.
//!
//! The A1/A3/A4 fail-closed gate items below were quarantined by M3 and are
//! REVIVED as WP-T's three-state publish gate (`MODERATION_GATE`), which is
//! separate from M3's invocation-only `MODERATION_ENABLED`. Plan:
//! `docs/development/IMPLEMENTATION-MODERATION-FRAMES-GATE-WPT.md`.

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

/// THE gate invariant: publish is allowed ONLY on a clean `Cleared`.
/// Revived for WP-T (IMPLEMENTATION-MODERATION-FRAMES-GATE-WPT.md).
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

/// WP-T's three-state publish gate — **deliberately NOT `MODERATION_ENABLED`**.
///
/// M3's D1 re-semanticed `MODERATION_ENABLED` to invocation-only ("do I produce
/// a shadow report"), with the rule that nothing may make job success depend on
/// moderation output. This gate is the opposite: it withholds the transcoded
/// outputs. Sharing one switch would arm a fail-closed hold every time the
/// shadow switch was flipped for data collection (Q1(a)).
///
/// Three states rather than a bool because Milestone 1 ("OPERATING, DARK")
/// requires POST + record + **publish anyway**, which a boolean cannot express.
/// Same ladder as the node and SDK gates, so all three flip in the same order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateMode {
    /// Default. No tap, no POST, no gate — byte-identical to a pre-WP-T build.
    Off,
    /// Tap + POST + verdict recorded + `WOULD-HOLD` logged, publishes anyway.
    Dark,
    /// Fail-closed: publish only on `cleared`.
    Enforce,
}

impl GateMode {
    /// Does this mode tap keyframes and POST them? (`Dark` and `Enforce`.)
    pub fn moderates(&self) -> bool {
        matches!(self, GateMode::Dark | GateMode::Enforce)
    }

    /// Does a non-`cleared` verdict actually withhold the outputs? (`Enforce`
    /// only.) The dark-publishes-anyway rule lives here and nowhere else — the
    /// wiring calls this, never `match`es the enum inline.
    pub fn holds(&self) -> bool {
        matches!(self, GateMode::Enforce)
    }
}

/// Pure parser. Blank/whitespace ⇒ `Off` (no typo is involved in a default);
/// anything else unrecognised ⇒ `Err` carrying the bad value and the valid set.
/// Q1(b): that `Err` is boot-fatal, not a job-time decision.
pub fn parse_gate_mode(raw: &str) -> Result<GateMode, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "off" => Ok(GateMode::Off),
        "dark" => Ok(GateMode::Dark),
        "enforce" => Ok(GateMode::Enforce),
        other => Err(format!(
            "MODERATION_GATE={:?} is not a recognised value (expected one of: off, dark, enforce)",
            other
        )),
    }
}

/// Boot-time validation (Q1(b)). Called from `main()` BEFORE either listener
/// binds; on `Err` the container dies with a message naming the bad value.
/// Static misconfiguration belongs at deploy time, when someone is watching —
/// not at job time, where failing open silently disarms the gate and failing
/// closed is a total publish outage.
pub fn validate_gate_mode() -> Result<(), String> {
    parse_gate_mode(&var("MODERATION_GATE").unwrap_or_default()).map(|_| ())
}

/// The gate mode for this job.
///
/// The unparseable arm is UNREACHABLE once `validate_gate_mode()` runs at boot
/// (Task 1.1.5) — it is deliberate defence-in-depth for a future call path that
/// bypasses boot validation, and it fails CLOSED so a gate can never be
/// silently disarmed by a typo. It is **not** the Q1(b) policy; boot-fatal is.
pub fn gate_mode() -> GateMode {
    let raw = var("MODERATION_GATE").unwrap_or_default();
    match parse_gate_mode(&raw) {
        Ok(m) => m,
        Err(e) => {
            eprintln!(
                "MODERATION GATE ALERT config-fault: {} — failing closed to enforce. \
                 This should be unreachable; boot validation must have been bypassed.",
                e
            );
            GateMode::Enforce
        }
    }
}

/// May the M3 shadow path relay its verdict to the node? (Q2.)
///
/// **This is a safety interlock, not a tidy-up.** Both moderation paths write to
/// the node's `VerdictStore` under the same `job_id`, and verdicts there are
/// MONOTONIC: the node's `set_if_not_downgrade` rejects only a `cleared`
/// arriving over a non-`cleared`, so a `blocked` over a `cleared` lands
/// **permanently**. The M3 shadow path is the OQ-12-blocked ShieldGemma policy
/// scan, known to misfire on lawful content; WP-T's frames path is
/// deterministic hash matching. Left unguarded, one VLM false positive would
/// permanently poison a job that Track-1 had cleared, with no way back.
///
/// So whenever the gate is armed, the *relay hop* is suppressed — never the
/// scan. The sidecar still runs and still writes its own JSONL shadow sink, so
/// no shadow evidence is lost; only the node-side write is withheld, leaving
/// exactly one authoritative writer per job.
pub fn should_relay() -> bool {
    !gate_mode().moderates()
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

/// Per-call deadline (s) for the **seam-#1 frames POST to the node** — a
/// hash-match round trip, not a VLM run. Default 30 s.
///
/// DELIBERATELY SEPARATE from `timeout_secs()` above (WP-T Task 1.2.3): that
/// one is M3's 3 h *sidecar* deadline. The frames path briefly shared it while
/// A3 was quarantined, which would have made a wedged node hold every job for
/// three hours. `test_frames_timeout_is_decoupled_from_the_sidecar_deadline`
/// fails if the two are ever merged back together.
fn frames_timeout_secs() -> u64 {
    var("MODERATION_FRAMES_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30)
}

/// Maximum keyframes per frames POST (PART-A §3.2 amendment, 2026-07-31).
///
/// The node caps a request body at 20 MB. A full-coverage sample under the Q7
/// numbers reaches ~1800 frames on a 6 h title, which exceeds that once base64
/// inflates the PNGs by ~33% — so an unbatched POST would 413 on long titles
/// only: fail-closed, but silent, and it would also press the 30 s deadline
/// while the node decoded 1800 PNGs in one request.
///
/// Deliberately a constant, not an env var. It is a property of the node's body
/// cap, not a per-deployment preference, and the failure mode of setting it too
/// high (413 ⇒ every long title holds) is exactly the silent one this exists to
/// remove. Raising it requires the node's cap to change first.
const FRAMES_BATCH_MAX: usize = 200;

/// Shared secret sent as the `ingestToken` **body** field (PART-A §3.2, REQUIRED).
/// `None` when unset, so the field is omitted entirely and the node's own
/// "an unset secret never means accept all" 401 path is what speaks.
/// NEVER log this value, and never log the request body that carries it.
fn ingest_token() -> Option<String> {
    var("MODERATION_INGEST_TOKEN").ok()
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

/// Keyframe budget — a coverage *resolution* knob (NOT front-truncation), clamped
/// `.max(1)` so the `effective_interval` divisor is never zero. Raised 300 → 1000
/// for Q7: at 300 the budget hits the §0.5.11 interval cap at only ~75 min, so
/// every feature-length title was sampled coarser than the mandate allows.
pub fn keyframe_max() -> usize {
    var("MODERATION_KEYFRAME_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000)
        .max(1)
}

/// PART-A §0.5.11's upper bound on the sampling interval: "never coarser than
/// ~10–15 s even on long content". 12 s sits in the middle of that band.
const DEFAULT_MAX_INTERVAL_SECS: f64 = 12.0;

/// Pure resolution of the cap, so the clamp is testable without touching a
/// process-global env var (which would race the coverage table test).
fn resolve_max_interval(configured: Option<f64>, floor: f64) -> f64 {
    configured.unwrap_or(DEFAULT_MAX_INTERVAL_SECS).max(floor)
}

/// Sampling interval CAP (s). Clamped `.max(floor)` so a cap misconfigured below
/// the floor can never invert the two and produce an empty range.
fn max_interval_secs() -> f64 {
    resolve_max_interval(
        var("MODERATION_MAX_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse::<f64>().ok()),
        sample_interval_secs(),
    )
}

/// Even full-duration sampling interval (s) — `min(max(floor, d/budget), cap)`.
///
/// The budget widens the interval on long content so the WHOLE duration is
/// sampled (no front-loaded truncation, no unscanned tail); the cap then bounds
/// how coarse that widening may get, which is PART-A §0.5.11's mandate and what
/// the pre-Q7 code was missing — it degraded linearly (90 min ⇒ 18 s, 6 h ⇒ 72 s)
/// and did so silently, coverage just thinning.
///
/// Above `cap × budget` (~3 h 20 m at the defaults) the cap wins and the frame
/// count exceeds the budget — 1800 frames for a 6 h source. That is deliberate:
/// §0.5.11 says favour coverage over payload. It is also why `-frames:v` must
/// come from [`frames_cap`] rather than `keyframe_max` (see there).
///
/// Auditable residual: the worst-case gap between consecutive sampled frames is
/// exactly this return value, i.e. never more than `max_interval_secs()`.
pub fn effective_interval(duration_secs: f64) -> f64 {
    sample_interval_secs()
        .max(duration_secs / keyframe_max() as f64)
        .min(max_interval_secs())
}

/// The `-frames:v` backstop for the tap output.
///
/// A flat `keyframe_max` here would front-truncate any source longer than
/// `cap × budget` — chopping a 6 h title at the 3 h 20 m mark and leaving the
/// tail unscanned, the exact failure §0.5.11 forbids. So when the duration probe
/// succeeded we derive the bound from the frame count we actually expect, with
/// margin for `fps`-filter rounding; only an unprobeable source (`0.0`) falls
/// back to the flat budget, preserving As-Built Deviation 1's OOM/disk guard for
/// the one case where the adaptive interval cannot widen.
pub fn frames_cap(duration_secs: f64) -> usize {
    // `!is_finite()` is load-bearing, not defensive noise: ffprobe emits `nan`
    // for some malformed inputs and `"nan".parse::<f64>()` SUCCEEDS, so a NaN
    // duration reaches here. `NaN <= 0.0` is false, and `NaN as usize`
    // saturates to 0 — which would silently cap the tap at ~10 frames for a
    // whole title and let the gate POST that as a complete scan.
    if !duration_secs.is_finite() || duration_secs <= 0.0 {
        return keyframe_max();
    }
    let expected = (duration_secs / effective_interval(duration_secs)).ceil() as usize;
    // +10% +10: comfortably above any rounding overshoot, still finite.
    (expected as f64 * 1.1).ceil() as usize + 10
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
    /// Revived for WP-T (IMPLEMENTATION-MODERATION-FRAMES-GATE-WPT.md).
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

/// Build the seam-#1 JSON body (PART-A §3.2 shape) from in-memory keyframe bytes.
///
/// `ingest_token` is REQUIRED by the node but `Option` here on purpose: when it
/// is `None` the field is OMITTED rather than sent empty, so the node's own
/// "an unset secret never means accept all" rule produces the 401 — we never
/// invent a credential. §3.2's optional `audio` (Track-2) is deliberately never
/// emitted: "omit at launch".
///
/// **This value is a shared secret. The returned body must never be logged.**
///
/// Shaped so WP-N3's `audioOnly: true` is a one-line addition here (see the
/// empty-set guard in `HttpModerationClient::moderate`).
fn build_request_body(
    task_id: &str,
    keyframes: &[Vec<u8>],
    source_sha256: Option<String>,
    ingest_token: Option<String>,
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
    if let Some(token) = ingest_token {
        body["ingestToken"] = serde_json::Value::String(token);
    }
    Ok(body)
}

/// The response's optional `reason`, for LOGGING ONLY.
///
/// Deliberately separate from [`outcome_from_response`] so the fail-closed
/// mapping stays untouched: this is an OPAQUE display string that is changing
/// upstream, and nothing anywhere may compare it. Only `verdict` is
/// load-bearing.
fn reason_from_response(body: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct ReasonBody {
        reason: Option<String>,
    }
    serde_json::from_str::<ReasonBody>(body).ok()?.reason
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
        // Q5 interim (b): never POST an empty keyframe set. Under the V2 wire an
        // empty `keyframes_png_base64` is a 400 that writes NO verdict, so the
        // call is pure cost with a guaranteed hold. Reached by an audio-only
        // source or a tap that produced nothing — and for the latter,
        // fail-closed is mandatory: a video source we could not scan must never
        // be recorded as clean.
        //
        // THIS GUARD BELONGS HERE, NOT ON THE TRAIT. `StubModerationClient` is
        // called with an empty slice throughout the gate tests; a trait-level
        // guard (or a trait default) would short-circuit them and the gate
        // matrix would pass vacuously.
        //
        // PLANNED INVERSION, not a weakening: when the node ships WP-N3 this
        // becomes `audioOnly: true` + the empty array, and the node records an
        // explicit `cleared` / "audio-only-out-of-track1-scope". Adding that is
        // one optional bool in `build_request_body`. Skipping the POST entirely
        // was considered and is WRONG — the SDK gate holds on an absent verdict,
        // so it would strand audio-only jobs at seam 3 instead, harder to
        // diagnose.
        if keyframes.is_empty() {
            println!(
                "MODERATION FRAMES task_id={} skip: empty keyframe set \
                 (audio-only or tap failure) — holding fail-closed, no POST made",
                task_id
            );
            return ModerationOutcome::Unavailable;
        }
        let url = match node_url() {
            Some(u) => u,
            None => return ModerationOutcome::Unavailable, // config error = HOLD
        };
        // 30 s frames deadline — never M3's 3 h sidecar one (WP-T Task 1.2.3).
        let timeout = frames_timeout_secs();
        // Own the inputs for the `'static` blocking closure; this also keeps the
        // CPU-bound base64 (`build_request_body`) OFF the async reactor.
        let task_id = task_id.to_owned();
        let frames: Vec<Vec<u8>> = keyframes.to_vec();
        let token = ingest_token();
        let join = tokio::task::spawn_blocking(move || {
            let client = match reqwest::Client::builder()
                .timeout(Duration::from_secs(timeout))
                .build()
            {
                Ok(c) => c,
                Err(_) => return ModerationOutcome::Unavailable,
            };
            // ── Batching (PART-A §3.2 amendment, 2026-07-31) ──────────────────
            // The node caps a request body at 20 MB. A full-coverage sample —
            // up to ~1800 frames past 3 h 20 m under the Q7 numbers — exceeds
            // that once base64 inflates the PNGs by a third, so long titles
            // would 413. That still holds (fail-closed) but silently, and it
            // would push the 30 s deadline while the node decoded 1800 PNGs in
            // one request. So: sequential batches of at most FRAMES_BATCH_MAX,
            // each an ordinary §3.2 POST with the same taskId and the same
            // sourceSha256. The deadline above is PER BATCH.
            let batches: Vec<&[Vec<u8>]> = frames.chunks(FRAMES_BATCH_MAX).collect();
            let total = batches.len();
            for (i, batch) in batches.iter().enumerate() {
                let body =
                    match build_request_body(&task_id, batch, source_sha256.clone(), token.clone())
                    {
                        Ok(b) => b,
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
                let outcome = batch_outcome(status, &text, &url, &task_id, i + 1, total);
                // AGGREGATION — fail-closed, and order-independent by
                // construction. The job may publish only if EVERY batch
                // cleared; any non-cleared verdict or non-200 on any batch
                // holds the whole job. Returning on the first non-cleared is
                // what makes a later `cleared` unable to override an earlier
                // `blocked`/`flagged` — the node's store is monotonic and has
                // already recorded the block, so continuing would only risk us
                // disagreeing with it locally.
                if !may_publish(&outcome) {
                    println!(
                        "MODERATION FRAMES task_id={} HELD at batch {}/{} outcome={:?} \
                         — remaining batches not sent",
                        task_id,
                        i + 1,
                        total,
                        outcome
                    );
                    return outcome;
                }
            }
            println!(
                "MODERATION FRAMES task_id={} cleared across {} batch(es) of <={} keyframes",
                task_id, total, FRAMES_BATCH_MAX
            );
            ModerationOutcome::Cleared
        })
        .await;
        join.unwrap_or(ModerationOutcome::Unavailable) // spawn_blocking JoinError ⇒ HOLD
    }
}

/// One batch's HTTP response → outcome, with the status-specific ALERT lines.
/// Split out of the batch loop so the logging stays one concern; the mapping
/// itself is still `outcome_from_response`, unchanged and fail-closed.
fn batch_outcome(
    status: u16,
    text: &str,
    url: &str,
    task_id: &str,
    batch: usize,
    total: usize,
) -> ModerationOutcome {
    // Status-specific ALERTs: the response body is the only place this detail
    // exists, and both of these are permanent faults rather than transient
    // ones — they must not read as just another hold.
    // NOTE: log status + URL + verdict/reason ONLY. The request body carries
    // the ingest token and must never appear here.
    match status {
        401 => eprintln!(
            "MODERATION FRAMES ALERT config-fault: 401 unauthorised from {} — the \
             ingestToken is missing or wrong on this transcoder, or unset on the node \
             (an unset node secret rejects every request). EVERY job holds until fixed",
            url
        ),
        400 => eprintln!(
            "MODERATION FRAMES ALERT: 400 malformed request to {} — single attempt \
             by design (one POST, one decision); this job holds",
            url
        ),
        413 => eprintln!(
            "MODERATION FRAMES ALERT: 413 payload too large from {} — a single batch \
             exceeded the node's 20 MB body cap; FRAMES_BATCH_MAX is too high for this \
             keyframe size. This job holds",
            url
        ),
        _ => {}
    }
    let outcome = outcome_from_response(status, text);
    // `reason` is an opaque display string — logged, never compared. A cleared
    // response omits it entirely rather than sending null; both parse to None.
    println!(
        "MODERATION FRAMES task_id={} batch={}/{} status={} outcome={:?} reason={:?}",
        task_id,
        batch,
        total,
        status,
        outcome,
        reason_from_response(text)
    );
    outcome
}

/// The production client. Swapped for `StubModerationClient` in tests.
/// Revived for WP-T (IMPLEMENTATION-MODERATION-FRAMES-GATE-WPT.md).
pub fn default_client() -> Box<dyn ModerationClient> {
    Box::new(HttpModerationClient)
}

/// Streamed SHA-256 of the source file → 64-hex (mirrors `blake3_digest` in `s5.rs`).
/// Optional own-hash exact-match input for the node (PDQ stays the detector).
/// Revived for WP-T (IMPLEMENTATION-MODERATION-FRAMES-GATE-WPT.md) — the
/// own-hash exact match Milestone 1a depends on.
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
        assert_eq!(keyframe_max(), 1000, "raised 300 -> 1000 for Q7");
        // full-coverage on a 2 h source: even sample, no unscanned tail, within budget
        let n = effective_interval(7200.0);
        assert!(n >= sample_interval_secs(), "interval must be >= the floor");
        assert!(
            (7200.0 / n).ceil() as usize <= keyframe_max(),
            "frame count must stay within the budget"
        );
        // Above cap x budget (~3 h 20 m) the budget deliberately goes SOFT: the
        // §0.5.11 interval cap wins and the count exceeds the budget rather than
        // letting coverage thin (Q7, "favour coverage over payload").
        let long = effective_interval(21600.0);
        assert_eq!(long, 12.0, "6 h source pinned at the cap");
        assert!((21600.0 / long).ceil() as usize > keyframe_max());
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
        let body = build_request_body("task-42", &frames, None, None).unwrap();
        assert_eq!(body["taskId"], "task-42");
        let arr = body["keyframes_png_base64"].as_array().unwrap();
        assert_eq!(arr, &vec!["AQID", "BAU="]); // STANDARD-padded base64, in order
        assert!(body.get("sourceSha256").is_none());
        // sourceSha256 present only when supplied
        let with_sha = build_request_body("t", &frames, Some("abc".into()), None).unwrap();
        assert_eq!(with_sha["sourceSha256"], "abc");
        // empty slice → empty array (audio-only path)
        let empty = build_request_body("t", &[], None, None).unwrap();
        assert_eq!(empty["keyframes_png_base64"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_transport_error_holds() {
        // SUPERSEDED by `test_empty_set_never_connects_and_dead_node_holds`,
        // which owns MODERATION_NODE_URL and exercises the same `.send()` Err
        // arm plus the config-fault arm. Kept as a pure-predicate check so the
        // coverage name survives; it must NOT touch the env var, because doing
        // so would race the owning test under the parallel harness.
        assert!(!may_publish(&ModerationOutcome::Unavailable));
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

    // ── WP-T Sub-phase 1.2 ─────────────────────────────────────────────────

    /// ANTI-TIDY TEST — the entire point of Sub-phase 1.2 (plan Task 1.2.1).
    ///
    /// `timeout_secs()` is M3's *sidecar* deadline: 3 h, sized for a ~77 min
    /// ShieldGemma run plus a queue wait. `frames_timeout_secs()` is the *node*
    /// frames POST: a hash-match round trip that must fail closed in seconds.
    /// They look like duplicate accessors and a simplifier will try to merge
    /// them — merged, a wedged node holds every job for three hours. If this
    /// assertion ever fails, restore the split; do not relax the test.
    #[test]
    fn test_frames_timeout_is_decoupled_from_the_sidecar_deadline() {
        assert_eq!(frames_timeout_secs(), 30, "frames POST default");
        assert_eq!(timeout_secs(), 10_800, "M3 sidecar default (3 h)");
        assert_ne!(
            frames_timeout_secs(),
            timeout_secs(),
            "the frames POST must never share M3's 3 h sidecar deadline"
        );
        // Two DIFFERENT accessors existing is not the property that matters —
        // the property is that the frames CLIENT calls the right one. Without
        // this, swapping the client back to `timeout_secs()` (the precise bug
        // Sub-phase 1.2 exists to fix: a wedged node holding every job for
        // three hours) passes the whole suite, caught only by a shell grep.
        let src = include_str!("moderation.rs");
        let start = src
            .find("impl ModerationClient for HttpModerationClient")
            .expect("frames client present");
        let client = &src[start..];
        let client = &client[..client.find("\n}").unwrap_or(client.len())];
        assert!(
            client.contains("frames_timeout_secs()"),
            "the frames client must use the 30 s frames deadline"
        );
        assert!(
            !client.contains(concat!(" timeout_", "secs()")),
            "the frames client must NOT use M3's 3 h sidecar deadline"
        );
        // ...and it is independently overridable (same test to avoid a
        // cross-test race on a process-global env var).
        std::env::set_var("MODERATION_FRAMES_TIMEOUT_SECS", "7");
        let overridden = frames_timeout_secs();
        std::env::remove_var("MODERATION_FRAMES_TIMEOUT_SECS");
        assert_eq!(overridden, 7);
    }

    // ── WP-T Sub-phase 2.1: ingestToken + the documented error shapes ──────

    #[test]
    fn test_request_body_carries_ingest_token_as_a_body_field() {
        let frames = vec![vec![1u8, 2, 3]];
        let with = build_request_body("t", &frames, None, Some("s3cret".into())).unwrap();
        assert_eq!(with["ingestToken"], "s3cret", "top-level BODY field (§3.2)");
        // Absent ⇒ OMITTED, so the node's own "an unset secret never means
        // accept all" 401 is what speaks — never a bare "" we invented.
        let without = build_request_body("t", &frames, None, None).unwrap();
        assert!(without.get("ingestToken").is_none());
        // §3.2's optional Track-2 field is explicitly "omit at launch".
        assert!(with.get("audio").is_none());
        assert!(without.get("audio").is_none());
    }

    /// V2 §5's first named sharp edge, plus the secret-handling rule: the token
    /// travels in the body, and the body never reaches a log.
    #[test]
    fn test_token_is_a_body_field_and_the_body_is_never_logged() {
        let src = include_str!("moderation.rs");
        // needles via concat! so this test's own literals don't self-match —
        // and so checklist #3's file-wide grep stays clean.
        assert!(
            !src.contains(concat!("Author", "ization")),
            "the token is a body field, never an auth header (V2 §5)"
        );
        assert!(!src.to_lowercase().contains(concat!("bear", "er")), "ditto");
        let start = src
            .find("impl ModerationClient for HttpModerationClient")
            .expect("frames client present");
        let client = &src[start..];
        let client = &client[..client.find("\n}").unwrap_or(client.len())];
        // The per-batch response handler logs on behalf of the client, so it is
        // part of the same secret-handling surface and must be checked too.
        let bstart = src
            .find("fn batch_outcome(")
            .expect("batch handler present");
        let batch = &src[bstart..];
        let batch = &batch[..batch.find("\n}").unwrap_or(batch.len())];
        // The request body variable carries the shared secret. The lazy
        // implementation logs it on failure; these are what that looks like.
        for (region, name) in [(client, "client"), (batch, "batch_outcome")] {
            assert!(
                !region.contains(", body)"),
                "{}: request body must never be logged",
                name
            );
            assert!(
                !region.contains(", &body)"),
                "{}: request body must never be logged",
                name
            );
        }
        // Pin the intended log shape: status + batch position + verdict/reason,
        // nothing else. If the token or body is ever added to the outcome line,
        // this format string changes and the test fails.
        assert!(
            batch.contains("status={} outcome={:?} reason={:?}"),
            "the outcome line must log only status/outcome/reason"
        );
        // Stronger than "does not log the token": the batch handler must never
        // RECEIVE it, so it cannot leak it however it is later edited. Check
        // the parameter list only — the 401 message legitimately mentions
        // `ingestToken` by name, which is the reason a body-wide scan is the
        // wrong assertion here.
        let params = &batch[..batch.find(')').expect("signature closes")];
        assert!(
            !params.contains("token"),
            "batch_outcome must not receive the ingest token"
        );
    }

    #[test]
    fn test_documented_error_shapes_all_hold() {
        // The real PART-A §3.2 responses. Every one of them holds.
        for (status, body) in [
            (401, r#"{"error":"unauthorised"}"#),
            (404, r#"{"error":"unknown task"}"#),
            (400, r#"{"error":"malformed"}"#),
            (403, r#"{"error":"forbidden"}"#),
            (500, r#"{"error":"boom"}"#),
            (503, ""),
            (200, r#"{"verdict":"bogus"}"#),
            (200, "not json"),
            (200, ""),
        ] {
            let o = outcome_from_response(status, body);
            assert_eq!(
                o,
                ModerationOutcome::Unavailable,
                "status {} body {:?} must hold",
                status,
                body
            );
            assert!(!may_publish(&o));
        }
        // ...and the only shapes that are not a hold:
        assert_eq!(
            outcome_from_response(200, r#"{"verdict":"cleared"}"#),
            ModerationOutcome::Cleared
        );
        assert_eq!(
            outcome_from_response(200, r#"{"verdict":"blocked"}"#),
            ModerationOutcome::Blocked
        );
        assert_eq!(
            outcome_from_response(200, r#"{"verdict":"flagged"}"#),
            ModerationOutcome::Flagged
        );
    }

    #[test]
    fn test_client_names_401_as_a_config_fault() {
        let src = include_str!("moderation.rs");
        // A 401 is ALWAYS a secret-pinning fault on one side or the other,
        // never transient — it must not read as just another hold in the log.
        assert!(
            src.contains("MODERATION FRAMES ALERT config-fault: 401"),
            "401 needs its own named ALERT line"
        );
        assert!(
            src.contains("MODERATION FRAMES ALERT"),
            "400 needs a named line too"
        );
    }

    /// Task 2.1.4 — `reason` is captured for the log and is OPAQUE. The blocked
    /// reason string is changing upstream, so nothing may compare it; these use
    /// deliberately neutral values to prove the outcome does not depend on it.
    #[test]
    fn test_reason_is_captured_for_logging_but_never_branched_on() {
        assert_eq!(
            reason_from_response(r#"{"verdict":"blocked","reason":"reason-one"}"#).as_deref(),
            Some("reason-one")
        );
        // A `cleared` response OMITS `reason` entirely rather than sending it
        // as null (node dev, 2026-07-31). Both spellings must parse to None —
        // serde maps a missing `Option` field to None, and an explicit null too.
        assert!(reason_from_response(r#"{"verdict":"cleared"}"#).is_none());
        assert!(reason_from_response(r#"{"verdict":"cleared","reason":null}"#).is_none());
        assert!(reason_from_response("not json").is_none());
        // ...and neither shape may disturb the verdict.
        assert_eq!(
            outcome_from_response(200, r#"{"verdict":"cleared"}"#),
            ModerationOutcome::Cleared
        );
        assert_eq!(
            outcome_from_response(200, r#"{"verdict":"cleared","reason":null}"#),
            ModerationOutcome::Cleared
        );
        let a = outcome_from_response(200, r#"{"verdict":"blocked","reason":"reason-one"}"#);
        let b = outcome_from_response(200, r#"{"verdict":"blocked","reason":"reason-two"}"#);
        let c = outcome_from_response(200, r#"{"verdict":"blocked"}"#);
        assert_eq!(a, b, "the reason string must not change the outcome");
        assert_eq!(b, c, "an absent reason must not change the outcome");
    }

    // ── WP-T: frames batching (PART-A §3.2 amendment, 2026-07-31) ──────────

    /// Serialises every test that mutates `MODERATION_NODE_URL` — it is
    /// process-global and the harness runs tests in parallel threads. Mirrors
    /// `shared.rs`'s `COUNTER_TEST_LOCK`, including poison tolerance.
    static NODE_URL_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A minimal node that answers `POST /v1/moderate/frames`, records each
    /// request body, and serves one verdict per request in order (the last
    /// repeats). `Connection: close` is deliberate: it forces one TCP
    /// connection per request so a connection count equals a request count.
    struct FakeNode {
        port: u16,
        bodies: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl FakeNode {
        fn start(verdicts: &'static [&'static str]) -> FakeNode {
            use std::io::{Read, Write};
            use std::sync::atomic::Ordering::SeqCst;
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            listener.set_nonblocking(true).unwrap();
            let bodies = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (b, s) = (bodies.clone(), stop.clone());
            let handle = std::thread::spawn(move || {
                let mut n = 0usize;
                while !s.load(SeqCst) {
                    let (mut conn, _) = match listener.accept() {
                        Ok(c) => c,
                        Err(_) => {
                            std::thread::sleep(std::time::Duration::from_millis(2));
                            continue;
                        }
                    };
                    conn.set_read_timeout(Some(std::time::Duration::from_secs(2)))
                        .ok();
                    // Read headers, then exactly Content-Length bytes of body.
                    let mut buf: Vec<u8> = Vec::new();
                    let mut tmp = [0u8; 16384];
                    let mut need: Option<usize> = None;
                    loop {
                        match conn.read(&mut tmp) {
                            Ok(0) => break,
                            Ok(k) => {
                                buf.extend_from_slice(&tmp[..k]);
                                if need.is_none() {
                                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                                        let head =
                                            String::from_utf8_lossy(&buf[..p]).to_lowercase();
                                        let cl = head
                                            .lines()
                                            .find_map(|l| l.strip_prefix("content-length:"))
                                            .and_then(|v| v.trim().parse::<usize>().ok())
                                            .unwrap_or(0);
                                        need = Some(p + 4 + cl);
                                    }
                                }
                                if need.is_some_and(|t| buf.len() >= t) {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        b.lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(String::from_utf8_lossy(&buf[p + 4..]).to_string());
                    }
                    let verdict = verdicts[n.min(verdicts.len() - 1)];
                    n += 1;
                    let body = format!("{{\"verdict\":\"{}\"}}", verdict);
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = conn.write_all(resp.as_bytes());
                    let _ = conn.flush();
                }
            });
            FakeNode {
                port,
                bodies,
                stop,
                handle: Some(handle),
            }
        }
        fn url(&self) -> String {
            format!("http://127.0.0.1:{}", self.port)
        }
        fn requests(&self) -> Vec<String> {
            self.bodies
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }
    }

    impl Drop for FakeNode {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    #[tokio::test]
    async fn test_frames_are_batched_and_every_batch_carries_identity() {
        let _serial = NODE_URL_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let node = FakeNode::start(&["cleared"]);
        std::env::set_var("MODERATION_NODE_URL", node.url());
        // 450 frames ⇒ 3 POSTs at FRAMES_BATCH_MAX = 200.
        let frames: Vec<Vec<u8>> = (0..450u32).map(|i| vec![i as u8; 2]).collect();
        let outcome = HttpModerationClient
            .moderate("task-9", &frames, Some("deadbeef".into()))
            .await;
        std::env::remove_var("MODERATION_NODE_URL");

        assert_eq!(
            outcome,
            ModerationOutcome::Cleared,
            "every batch cleared ⇒ the job clears"
        );
        let reqs = node.requests();
        assert_eq!(reqs.len(), 3, "450 frames must split into 3 POSTs of <=200");
        let mut total = 0usize;
        for r in &reqs {
            let v: serde_json::Value = serde_json::from_str(r).expect("each batch is valid JSON");
            assert_eq!(v["taskId"], "task-9", "same taskId on every batch");
            assert_eq!(
                v["sourceSha256"], "deadbeef",
                "sourceSha256 must ride EVERY batch, not just the first"
            );
            let n = v["keyframes_png_base64"].as_array().unwrap().len();
            assert!(n <= FRAMES_BATCH_MAX, "batch of {} exceeds the cap", n);
            total += n;
        }
        assert_eq!(total, 450, "every keyframe sent exactly once, none dropped");
    }

    /// The aggregation rule the node dev called mandatory rather than advisory.
    #[tokio::test]
    async fn test_a_later_cleared_never_overrides_an_earlier_block() {
        let _serial = NODE_URL_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // cleared, then BLOCKED, then a batch that would have cleared.
        let node = FakeNode::start(&["cleared", "blocked", "cleared"]);
        std::env::set_var("MODERATION_NODE_URL", node.url());
        let frames: Vec<Vec<u8>> = (0..450u32).map(|_| vec![7u8; 2]).collect();
        let outcome = HttpModerationClient.moderate("t", &frames, None).await;
        std::env::remove_var("MODERATION_NODE_URL");

        assert_eq!(
            outcome,
            ModerationOutcome::Blocked,
            "a blocked batch holds the whole job — a later cleared must not wash it out"
        );
        assert!(!may_publish(&outcome));
        assert_eq!(
            node.requests().len(),
            2,
            "sending must stop at the blocked batch; the node has already recorded it"
        );
    }

    #[tokio::test]
    async fn test_a_non_200_on_any_batch_holds_the_job() {
        let _serial = NODE_URL_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // A verdict string the node never sends ⇒ unknown ⇒ Unavailable, which
        // stands in for the 4xx/5xx/413 family: all non-cleared, all hold.
        let node = FakeNode::start(&["cleared", "kaboom"]);
        std::env::set_var("MODERATION_NODE_URL", node.url());
        let frames: Vec<Vec<u8>> = (0..450u32).map(|_| vec![1u8; 2]).collect();
        let outcome = HttpModerationClient.moderate("t", &frames, None).await;
        std::env::remove_var("MODERATION_NODE_URL");

        assert_eq!(outcome, ModerationOutcome::Unavailable);
        assert!(!may_publish(&outcome));
        assert_eq!(node.requests().len(), 2, "stops at the first non-cleared");
    }

    #[test]
    fn test_batch_cap_matches_the_node_body_limit() {
        // A constant, not an env var: it tracks the node's 20 MB body cap, and
        // setting it too high reintroduces the silent 413-on-long-titles-only
        // failure this batching exists to remove.
        assert_eq!(FRAMES_BATCH_MAX, 200);
        // Q7's worst case (~1800 frames on a 6 h title) must split into a
        // sane number of POSTs rather than one oversized body.
        // (Longhand ceiling division: `usize::div_ceil` is 1.73+ and the
        // production builder is rust:1.72.)
        let worst = 1800_usize;
        assert_eq!((worst + FRAMES_BATCH_MAX - 1) / FRAMES_BATCH_MAX, 9);
        // And the chunking the client actually uses agrees with that.
        assert_eq!(vec![0u8; worst].chunks(FRAMES_BATCH_MAX).count(), 9);
    }

    // ── WP-T Sub-phase 2.2: never POST an empty keyframe set (Q5) ──────────

    /// Single owner of `MODERATION_NODE_URL` (process-global env, parallel
    /// harness). Replaces the older `test_transport_error_holds`, covering
    /// strictly more: that test set the same var, so a second one would race.
    ///
    /// Two properties, and the second needs a real socket to mean anything:
    ///  1. an unreachable/dead node HOLDS (the `.send()` Err arm), and
    ///  2. an EMPTY keyframe set opens NO connection at all (Q5).
    ///
    /// (2) cannot be shown by the return value — with the guard disabled the
    /// call still returns `Unavailable`, just via the transport error instead.
    /// Both spellings "pass" unless you actually watch the socket, which is why
    /// the guard previously had no real coverage. The `>= 1` assertion on the
    /// non-empty case is the discriminator: it proves the 0 above means
    /// "short-circuited", not "nothing ever connects here".
    #[tokio::test]
    async fn test_empty_set_never_connects_and_dead_node_holds() {
        let _serial = NODE_URL_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as O};
        use std::sync::Arc;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (h, s) = (hits.clone(), stop.clone());
        // Accept and immediately drop: the peer sees EOF/reset, so the client's
        // transport arm fires fast instead of waiting out the 30 s deadline.
        let acceptor = std::thread::spawn(move || {
            while !s.load(O::SeqCst) {
                if let Ok((conn, _)) = listener.accept() {
                    h.fetch_add(1, O::SeqCst);
                    drop(conn);
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        });

        std::env::set_var("MODERATION_NODE_URL", format!("http://127.0.0.1:{}", port));
        let empty = HttpModerationClient.moderate("t", &[], None).await;
        std::thread::sleep(std::time::Duration::from_millis(150));
        let after_empty = hits.load(O::SeqCst);

        let frames = vec![vec![0u8; 4]];
        let real = HttpModerationClient.moderate("t", &frames, None).await;
        std::thread::sleep(std::time::Duration::from_millis(150));
        let after_real = hits.load(O::SeqCst);

        // Third arm, safe to test here because this test owns the variable:
        // an unset node URL is a config fault and must also HOLD.
        std::env::remove_var("MODERATION_NODE_URL");
        let unset = HttpModerationClient.moderate("t", &frames, None).await;
        stop.store(true, O::SeqCst);
        let _ = acceptor.join();

        assert_eq!(
            unset,
            ModerationOutcome::Unavailable,
            "an unset MODERATION_NODE_URL is a config fault and must HOLD"
        );
        assert_eq!(empty, ModerationOutcome::Unavailable);
        assert!(!may_publish(&empty));
        assert_eq!(
            after_empty, 0,
            "the Q5 empty-set guard must short-circuit BEFORE any network work"
        );
        assert_eq!(
            real,
            ModerationOutcome::Unavailable,
            "a node that accepts then drops must fail closed"
        );
        assert!(
            after_real >= 1,
            "a non-empty set must actually attempt the POST — otherwise the \
             zero above proves nothing"
        );
    }

    /// The guard must short-circuit BEFORE any network setup — that is the
    /// difference between "we chose not to POST" and "we POSTed and it failed".
    /// Structural because the two are indistinguishable from the return value.
    #[test]
    fn test_empty_set_guard_precedes_any_network_work() {
        let src = include_str!("moderation.rs");
        let start = src
            .find("impl ModerationClient for HttpModerationClient")
            .expect("frames client present");
        let client = &src[start..];
        let client = &client[..client.find("\n}").unwrap_or(client.len())];
        let guard = client
            .find("empty keyframe set")
            .expect("empty-set guard must live in the client");
        let url = client.find("node_url()").expect("url read present");
        assert!(
            guard < url,
            "the empty-set guard must return before any network setup"
        );
    }

    /// Placement matters as much as existence: all four recovered gate tests
    /// call `moderate(task, &[], None)` on the STUB. A guard on the trait (or as
    /// a trait default) would short-circuit them, and the whole gate matrix
    /// would pass vacuously while asserting nothing.
    #[tokio::test]
    async fn test_empty_set_guard_is_not_on_the_trait() {
        for (outcome, expect) in [
            (ModerationOutcome::Cleared, true),
            (ModerationOutcome::Blocked, false),
        ] {
            let stub = StubModerationClient { outcome };
            assert_eq!(
                may_publish(&stub.moderate("t", &[], None).await),
                expect,
                "the stub must still honour its configured outcome on an empty set"
            );
        }
    }

    // ── WP-T Sub-phase 1.4: keyframe coverage (Q7 / PART-A §0.5.11) ────────

    #[test]
    fn test_max_interval_resolution_and_clamp() {
        // Pure, so it needs no env var and cannot race the table test below.
        let floor = 2.0;
        assert_eq!(resolve_max_interval(None, floor), 12.0, "default cap");
        assert_eq!(resolve_max_interval(Some(10.0), floor), 10.0, "honoured");
        // A cap configured BELOW the floor must never invert the two.
        assert_eq!(resolve_max_interval(Some(0.5), floor), floor, "clamped");
        assert_eq!(max_interval_secs(), 12.0, "env-unset default");
    }

    /// Asserts PART-A §0.5.11's MANDATE, not the formula: "never coarser than
    /// ~10–15 s even on long content", with the floor as the lower bound. Stated
    /// this way, a later tweak to `effective_interval` cannot quietly re-break
    /// compliance — which is exactly how the as-built code drifted out of it.
    #[test]
    fn test_effective_interval_honours_the_coverage_mandate() {
        let floor = sample_interval_secs();
        for (label, duration) in [
            ("30 min", 1800.0),
            ("75 min", 4500.0),
            ("90 min", 5400.0),
            ("2 h", 7200.0),
            ("3 h", 10800.0),
            ("6 h", 21600.0),
            ("24 h", 86400.0),
        ] {
            let interval = effective_interval(duration);
            assert!(
                interval <= 15.0,
                "{}: interval {}s is coarser than the ~10-15s mandate",
                label,
                interval
            );
            assert!(
                interval >= floor,
                "{}: interval {}s is below the floor {}s",
                label,
                interval,
                floor
            );
        }
    }

    /// The `-frames:v` backstop must bound an UNPROBEABLE source without ever
    /// front-truncating a probed one. Above cap x budget (~3 h 20 m) the budget
    /// deliberately goes soft, so a flat `keyframe_max` cap would chop a 6 h
    /// source at the 3 h 20 m mark — the exact behaviour §0.5.11 forbids.
    #[test]
    fn test_frames_cap_never_truncates_a_probed_source() {
        for duration in [1800.0, 5400.0, 10800.0, 21600.0, 86400.0] {
            let expected = (duration / effective_interval(duration)).ceil() as usize;
            assert!(
                frames_cap(duration) > expected,
                "duration {}s: cap {} would truncate the expected {} frames",
                duration,
                frames_cap(duration),
                expected
            );
        }
        // Probe failure (0.0) keeps the flat OOM guard — the adaptive interval
        // cannot widen without a duration, so this is the only thing bounding
        // PNG production (As-Built Deviation 1).
        assert_eq!(frames_cap(0.0), keyframe_max());
        assert_eq!(frames_cap(-1.0), keyframe_max());
        // A NaN duration is REACHABLE: ffprobe emits `nan` for some malformed
        // inputs and Rust parses that successfully. Without an is_finite guard
        // `NaN as usize` saturates to 0 and the tap is capped at ~10 frames for
        // an entire title — an unscanned video POSTed as if complete.
        assert_eq!(frames_cap(f64::NAN), keyframe_max());
        assert_eq!(frames_cap(f64::INFINITY), keyframe_max());
        // The interval itself is already NaN-safe (f64::max ignores NaN), but
        // pin it so a refactor cannot regress into `fps=NaN`.
        assert!(effective_interval(f64::NAN).is_finite());
        assert!(effective_interval(f64::NAN) >= sample_interval_secs());
    }

    // ── WP-T Sub-phase 1.1: GateMode ───────────────────────────────────────

    #[test]
    fn test_parse_gate_mode_valid_values() {
        assert_eq!(parse_gate_mode("off").unwrap(), GateMode::Off);
        assert_eq!(parse_gate_mode("dark").unwrap(), GateMode::Dark);
        assert_eq!(parse_gate_mode("enforce").unwrap(), GateMode::Enforce);
        // case- and whitespace-insensitive
        assert_eq!(parse_gate_mode("  ENFORCE  ").unwrap(), GateMode::Enforce);
        assert_eq!(parse_gate_mode("Dark").unwrap(), GateMode::Dark);
        // blank is the same non-typo default as unset
        assert_eq!(parse_gate_mode("").unwrap(), GateMode::Off);
        assert_eq!(parse_gate_mode("   ").unwrap(), GateMode::Off);
    }

    /// Q1(b) — a set-but-unrecognised value is BOOT-FATAL, and this message is
    /// the entire user-facing value of that rule. An operator reading a
    /// crash-loop at deploy time must see what they typed and what was allowed.
    #[test]
    fn test_parse_gate_mode_typo_names_the_bad_value_and_the_valid_set() {
        let err = parse_gate_mode("enfroce").unwrap_err();
        assert!(err.contains("enfroce"), "must echo the bad value: {}", err);
        assert!(err.contains("off"), "must list off: {}", err);
        assert!(err.contains("dark"), "must list dark: {}", err);
        assert!(err.contains("enforce"), "must list enforce: {}", err);
    }

    #[test]
    fn test_gate_mode_predicates() {
        // The dark-publishes-anyway rule lives in exactly ONE place: these two
        // predicates. The wiring must never match the enum inline (Task 1.1.3).
        assert!(!GateMode::Off.moderates());
        assert!(GateMode::Dark.moderates());
        assert!(GateMode::Enforce.moderates());
        assert!(!GateMode::Off.holds());
        assert!(
            !GateMode::Dark.holds(),
            "dark logs WOULD-HOLD and publishes"
        );
        assert!(GateMode::Enforce.holds());
    }

    /// Single owner of `MODERATION_GATE` — env is process-global and the test
    /// harness runs in parallel threads, so splitting this would race.
    #[test]
    fn test_gate_mode_and_boot_validation_from_env() {
        assert_eq!(gate_mode(), GateMode::Off, "unset ⇒ Off");
        assert!(validate_gate_mode().is_ok(), "unset is not a typo");

        // Q2: the M3 relay hop is live ONLY while the gate is disarmed.
        assert!(
            should_relay(),
            "gate off ⇒ the shadow relay is the only writer"
        );

        std::env::set_var("MODERATION_GATE", "dark");
        let dark = gate_mode();
        let dark_ok = validate_gate_mode();
        let dark_relay = should_relay();

        std::env::set_var("MODERATION_GATE", "enforce");
        let enforce_relay = should_relay();

        std::env::set_var("MODERATION_GATE", "enfroce");
        let typo_validate = validate_gate_mode();
        let typo_runtime = gate_mode();
        let typo_relay = should_relay();

        std::env::remove_var("MODERATION_GATE");

        assert_eq!(dark, GateMode::Dark);
        assert!(dark_ok.is_ok());
        // Suppressed in BOTH armed modes: `dark` records Track-1 verdicts
        // node-side too, so the OQ-12 VLM path could still poison them.
        assert!(!dark_relay, "dark arms the gate ⇒ relay suppressed");
        assert!(!enforce_relay, "enforce arms the gate ⇒ relay suppressed");
        // The fail-closed backstop mode must also suppress — it is `Enforce`.
        assert!(!typo_relay);
        // Boot validation rejects it (Q1(b), Task 1.1.5)...
        assert!(typo_validate.is_err());
        // ...and the unreachable runtime backstop fails CLOSED (Task 1.1.6),
        // never silently to Off.
        assert_eq!(typo_runtime, GateMode::Enforce);
    }

    #[test]
    fn test_ingest_token_unset_is_none() {
        // Unset ⇒ None so the field is OMITTED and the node's own
        // "unset secret ⇒ 401" path speaks, rather than a bare "" we invented.
        assert!(ingest_token().is_none());
        std::env::set_var("MODERATION_INGEST_TOKEN", "s3cret");
        let got = ingest_token();
        std::env::remove_var("MODERATION_INGEST_TOKEN");
        assert_eq!(got.as_deref(), Some("s3cret"));
    }
}
