use crate::moderation::{self, PinGuard};
use dotenv::var;
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use tokio::sync::Semaphore;

// HashMap<task_id, Vec<progress for each format>>
pub static PROGRESS_MAP: Lazy<Mutex<HashMap<String, Vec<Option<i32>>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

pub static MAX_CONCURRENT: Lazy<usize> = Lazy::new(|| {
    var("MAX_CONCURRENT_TRANSCODES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3)
        .max(1)
});

pub static SEMAPHORE: Lazy<Semaphore> = Lazy::new(|| Semaphore::new(*MAX_CONCURRENT));

pub static ACTIVE_JOBS: AtomicUsize = AtomicUsize::new(0);
pub static QUEUED_JOBS: AtomicUsize = AtomicUsize::new(0);

/// The highest progress a running job can report. Clients read 100 as "finished"
/// and don't wait long for the result after that, so only `mark_finished` sets it.
const MAX_RUNNING_PROGRESS: i32 = 99;

/// Survives poisoning: a panic elsewhere must not stop other jobs reporting.
fn progress_map() -> MutexGuard<'static, HashMap<String, Vec<Option<i32>>>> {
    PROGRESS_MAP.lock().unwrap_or_else(|e| e.into_inner())
}

/// Updates the transcoding progress for a specific format of a given task in a global progress map.
/// If the task or format index does not exist, they are created. Progress is stored as a percentage,
/// held between 0 and 99 because FFmpeg's clock can run past the expected duration.
///
/// # Arguments
/// * `task_id` - Identifier for the transcoding task.
/// * `format_index` - Index of the format being transcoded.
/// * `progress` - Progress percentage of the transcoding task for the specified format.
///
pub fn update_progress(task_id: &str, format_index: usize, progress: i32) {
    let mut progress_map = progress_map();
    let progress_list = progress_map.entry(task_id.to_string()).or_default();

    // Ensure the vector is large enough to hold progress for all formats
    if progress_list.len() <= format_index {
        progress_list.resize(format_index + 1, None);
    }

    // Update the specific format's progress
    progress_list[format_index] = Some(progress.clamp(0, MAX_RUNNING_PROGRESS));
}

/// The job has finished and its result is available: the only way to reach 100.
pub fn mark_finished(task_id: &str) {
    let mut progress_map = progress_map();
    let progress_list = progress_map.entry(task_id.to_string()).or_default();
    if progress_list.is_empty() {
        progress_list.push(None);
    }
    progress_list.fill(Some(100));
}

pub fn forget_progress(task_id: &str) {
    progress_map().remove(task_id);
}

/// Calculates the overall progress for a given task by averaging the progress values stored in
/// `PROGRESS_MAP`. Returns 0 if the task ID is not found or if there are no progress values.
///
/// # Arguments
/// * `task_id` - The identifier for the task whose progress is being calculated.
///
pub fn calculate_overall_progress(task_id: &str) -> i32 {
    let progress_map = progress_map();
    if let Some(progress_list) = progress_map.get(task_id) {
        let sum: i32 = progress_list.iter().filter_map(|&p| p).sum();
        let count: i32 = progress_list.iter().filter_map(|&p| p).count() as i32;
        if count > 0 {
            sum / count
        } else {
            0
        }
    } else {
        0
    }
}

pub fn active_jobs() -> usize {
    ACTIVE_JOBS.load(Ordering::Relaxed)
}

pub fn queued_jobs() -> usize {
    QUEUED_JOBS.load(Ordering::Relaxed)
}

pub fn max_concurrent() -> usize {
    *MAX_CONCURRENT
}

pub fn increment_queued() {
    QUEUED_JOBS.fetch_add(1, Ordering::Relaxed);
}

/// Gives back the slot of a submission the queue refused.
pub fn decrement_queued() {
    let _ = QUEUED_JOBS.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
        Some(n.saturating_sub(1))
    });
}

/// Counts a job as active until the guard drops, which also happens on a panic.
pub struct ActiveJobGuard;

/// Moves a job from queued to active.
pub fn start_active_job() -> ActiveJobGuard {
    decrement_queued();
    ACTIVE_JOBS.fetch_add(1, Ordering::Relaxed);
    ActiveJobGuard
}

impl Drop for ActiveJobGuard {
    fn drop(&mut self) {
        let _ = ACTIVE_JOBS.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
            Some(n.saturating_sub(1))
        });
    }
}

/// A temporary file or directory belonging to one job: safe from garbage collection
/// while this exists, and deleted when it goes, which covers every way a job ends.
pub struct LocalFile {
    path: String,
    is_dir: bool,
    _pin: PinGuard,
}

impl LocalFile {
    pub fn file(path: &str) -> Self {
        LocalFile {
            path: path.to_string(),
            is_dir: false,
            _pin: moderation::pin(path),
        }
    }

    pub fn dir(path: &str) -> Self {
        LocalFile {
            path: path.to_string(),
            is_dir: true,
            _pin: moderation::pin(path),
        }
    }

    pub fn path(&self) -> &str {
        &self.path
    }
}

impl Drop for LocalFile {
    fn drop(&mut self) {
        // Usually already gone, and an error here has nowhere useful to go.
        let _ = if self.is_dir {
            std::fs::remove_dir_all(&self.path)
        } else {
            std::fs::remove_file(&self.path)
        };
    }
}

/// Everything that counts jobs mutates the same process-global atomics; the
/// parallel test runner can interleave them (one test's `store(0)` landing between
/// the other's increment and assert) — serialise them on a shared lock.
#[cfg(test)]
pub static COUNTER_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_max_concurrent_default() {
        assert_eq!(*MAX_CONCURRENT, 3);
    }

    fn fresh_task() -> String {
        format!("shared-test-{}", uuid::Uuid::new_v4())
    }

    #[test]
    fn test_a_local_file_is_pinned_while_held_and_deleted_after() {
        let dir = std::env::temp_dir().join(format!("local-file-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("inner")).unwrap();
        let file = dir.join("file");
        std::fs::write(&file, b"x").unwrap();
        let inner = dir.join("inner");
        std::fs::write(inner.join("f"), b"x").unwrap();

        let held_file = LocalFile::file(file.to_str().unwrap());
        let held_dir = LocalFile::dir(inner.to_str().unwrap());
        assert!(moderation::is_pinned(&file));
        assert!(moderation::is_pinned(&inner));
        assert_eq!(held_file.path(), file.to_str().unwrap());

        drop(held_file);
        drop(held_dir);
        assert!(!file.exists() && !inner.exists());
        assert!(!moderation::is_pinned(&file) && !moderation::is_pinned(&inner));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_a_local_file_that_was_never_created_is_not_a_problem() {
        drop(LocalFile::file("/nonexistent/never/created"));
    }

    /// A panic while the lock is held must not turn every later job's progress
    /// update into a panic of its own: one bad job would take the rest with it.
    #[test]
    fn test_progress_survives_a_poisoned_lock() {
        let poisoner = std::thread::spawn(|| {
            let _held = PROGRESS_MAP.lock().unwrap_or_else(|e| e.into_inner());
            panic!("poison the progress lock");
        });
        assert!(poisoner.join().is_err());

        let task = fresh_task();
        update_progress(&task, 0, 40);
        assert_eq!(calculate_overall_progress(&task), 40);
    }

    /// Clients treat 100 as "finished", and give up soon after if the result
    /// isn't there yet. A running job therefore has to stay below 100, however
    /// high FFmpeg's own clock runs.
    #[test]
    fn test_running_progress_stays_below_100_until_the_job_finishes() {
        let task = fresh_task();
        update_progress(&task, 0, 100);
        update_progress(&task, 1, 1000);
        assert_eq!(calculate_overall_progress(&task), 99);

        mark_finished(&task);
        assert_eq!(calculate_overall_progress(&task), 100);
    }

    #[test]
    fn test_mark_finished_covers_a_task_with_no_formats_recorded() {
        let task = fresh_task();
        mark_finished(&task);
        assert_eq!(calculate_overall_progress(&task), 100);
    }

    #[test]
    fn test_negative_progress_reads_as_zero() {
        let task = fresh_task();
        update_progress(&task, 0, -5);
        assert_eq!(calculate_overall_progress(&task), 0);
    }

    #[test]
    fn test_forget_progress_removes_the_task() {
        let task = fresh_task();
        update_progress(&task, 0, 50);
        forget_progress(&task);
        assert_eq!(calculate_overall_progress(&task), 0);
        assert!(!PROGRESS_MAP
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&task));
    }

    /// The counter is released however the job ends, a panic included.
    #[test]
    fn test_active_job_guard_releases_the_count_on_panic() {
        let _serial = COUNTER_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        ACTIVE_JOBS.store(0, Ordering::Relaxed);
        QUEUED_JOBS.store(1, Ordering::Relaxed);

        // Carry the counts out through the panic: an assert in here would panic
        // too, and read as the job blowing up as intended.
        let result = std::panic::catch_unwind(|| {
            let _active = start_active_job();
            std::panic::panic_any((active_jobs(), queued_jobs()));
        });

        let during = result.unwrap_err();
        assert_eq!(during.downcast_ref::<(usize, usize)>(), Some(&(1, 0)));
        assert_eq!(active_jobs(), 0);
    }

    /// A rejected submission gives back its queue slot.
    #[test]
    fn test_decrement_queued_undoes_a_failed_submission() {
        let _serial = COUNTER_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        QUEUED_JOBS.store(0, Ordering::Relaxed);
        increment_queued();
        decrement_queued();
        assert_eq!(queued_jobs(), 0);
    }

    #[test]
    fn test_active_jobs_counter() {
        let _serial = COUNTER_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        ACTIVE_JOBS.store(0, Ordering::Relaxed);
        QUEUED_JOBS.store(0, Ordering::Relaxed);

        increment_queued();
        let active = start_active_job();
        assert_eq!(active_jobs(), 1);
        assert_eq!(queued_jobs(), 0);

        drop(active);
        assert_eq!(active_jobs(), 0);

        ACTIVE_JOBS.store(0, Ordering::Relaxed);
        QUEUED_JOBS.store(0, Ordering::Relaxed);
    }

    #[test]
    fn test_queued_jobs_counter() {
        let _serial = COUNTER_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        ACTIVE_JOBS.store(0, Ordering::Relaxed);
        QUEUED_JOBS.store(0, Ordering::Relaxed);

        increment_queued();
        increment_queued();
        increment_queued();
        assert_eq!(queued_jobs(), 3);

        let active = start_active_job();
        assert_eq!(queued_jobs(), 2);
        drop(active);

        ACTIVE_JOBS.store(0, Ordering::Relaxed);
        QUEUED_JOBS.store(0, Ordering::Relaxed);
    }
}
