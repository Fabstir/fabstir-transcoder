use dotenv::var;
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
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

/// Updates the transcoding progress for a specific format of a given task in a global progress map.
/// If the task or format index does not exist, they are created. Progress is stored as a percentage.
///
/// # Arguments
/// * `task_id` - Identifier for the transcoding task.
/// * `format_index` - Index of the format being transcoded.
/// * `progress` - Progress percentage of the transcoding task for the specified format.
///
pub fn update_progress(task_id: &str, format_index: usize, progress: i32) {
    let mut progress_map = PROGRESS_MAP.lock().unwrap();
    let progress_list = progress_map
        .entry(task_id.to_string())
        .or_insert_with(Vec::new);

    // Ensure the vector is large enough to hold progress for all formats
    if progress_list.len() <= format_index {
        progress_list.resize(format_index + 1, None);
    }

    // Update the specific format's progress
    progress_list[format_index] = Some(progress);
}

/// Calculates the overall progress for a given task by averaging the progress values stored in
/// `PROGRESS_MAP`. Returns 0 if the task ID is not found or if there are no progress values.
///
/// # Arguments
/// * `task_id` - The identifier for the task whose progress is being calculated.
///
pub fn calculate_overall_progress(task_id: &str) -> i32 {
    let progress_map = PROGRESS_MAP.lock().unwrap();
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

pub fn decrement_queued_increment_active() {
    QUEUED_JOBS.fetch_sub(1, Ordering::Relaxed);
    ACTIVE_JOBS.fetch_add(1, Ordering::Relaxed);
}

pub fn decrement_active() {
    ACTIVE_JOBS.fetch_sub(1, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_max_concurrent_default() {
        assert_eq!(*MAX_CONCURRENT, 3);
    }

    #[test]
    fn test_active_jobs_counter() {
        ACTIVE_JOBS.store(0, Ordering::Relaxed);
        QUEUED_JOBS.store(0, Ordering::Relaxed);

        increment_queued();
        decrement_queued_increment_active();
        assert_eq!(active_jobs(), 1);
        assert_eq!(queued_jobs(), 0);

        decrement_active();
        assert_eq!(active_jobs(), 0);

        ACTIVE_JOBS.store(0, Ordering::Relaxed);
        QUEUED_JOBS.store(0, Ordering::Relaxed);
    }

    #[test]
    fn test_queued_jobs_counter() {
        ACTIVE_JOBS.store(0, Ordering::Relaxed);
        QUEUED_JOBS.store(0, Ordering::Relaxed);

        increment_queued();
        increment_queued();
        increment_queued();
        assert_eq!(queued_jobs(), 3);

        decrement_queued_increment_active();
        assert_eq!(queued_jobs(), 2);

        ACTIVE_JOBS.store(0, Ordering::Relaxed);
        QUEUED_JOBS.store(0, Ordering::Relaxed);
    }
}
