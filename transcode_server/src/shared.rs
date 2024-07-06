use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::sync::RwLock;

// HashMap<task_id, Vec<progress for each format>>
pub static PROGRESS_MAP: Lazy<Mutex<HashMap<String, Vec<Option<i32>>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

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
