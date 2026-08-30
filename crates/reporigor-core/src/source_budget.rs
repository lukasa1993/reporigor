use std::path::Path;

use crate::CoreError;

/// Immutable upper bound for one selected source file (64 MiB).
pub const MAX_SOURCE_BYTES_HARD_LIMIT: usize = 64 * 1024 * 1024;

/// Immutable upper bound for the number of selected source files.
pub const MAX_SELECTED_SOURCE_FILES: u64 = 100_000;

/// Immutable upper bound for aggregate selected source metadata (1 GiB).
pub const MAX_SELECTED_SOURCE_BYTES: u64 = 1024 * 1024 * 1024;

/// Validate the configurable per-source limit against immutable safety bounds.
///
/// # Errors
///
/// Returns a configuration message when the limit is zero or exceeds 64 MiB.
pub fn validate_max_source_bytes(max_source_bytes: usize) -> Result<(), String> {
    if max_source_bytes == 0 {
        return Err("max_source_bytes must be greater than zero".to_string());
    }
    if max_source_bytes > MAX_SOURCE_BYTES_HARD_LIMIT {
        return Err(format!(
            "max_source_bytes must not exceed the immutable {MAX_SOURCE_BYTES_HARD_LIMIT}-byte safety limit"
        ));
    }
    Ok(())
}

/// Running fail-closed budget for a deduplicated selected source set.
#[derive(Debug, Clone, Copy)]
pub struct SourceBudget {
    max_source_bytes: usize,
    selected_files: u64,
    selected_bytes: u64,
}

impl SourceBudget {
    /// Create a budget using the request's validated per-file limit.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Config`] when the requested limit is unsafe.
    pub fn new(max_source_bytes: usize) -> Result<Self, CoreError> {
        validate_max_source_bytes(max_source_bytes).map_err(CoreError::Config)?;
        Ok(Self {
            max_source_bytes,
            selected_files: 0,
            selected_bytes: 0,
        })
    }

    /// Account for one selected source before parsing or retaining its bytes.
    ///
    /// Callers must deduplicate physical files before observing them.
    ///
    /// # Errors
    ///
    /// Returns a typed per-file or aggregate source-budget error.
    pub fn observe(&mut self, path: &Path, actual_bytes: u64) -> Result<(), CoreError> {
        let max_source_bytes = u64::try_from(self.max_source_bytes).unwrap_or(u64::MAX);
        if actual_bytes > max_source_bytes {
            return Err(CoreError::source_too_large(
                path,
                actual_bytes,
                self.max_source_bytes,
            ));
        }

        let selected_files = self.selected_files.saturating_add(1);
        let selected_bytes = self.selected_bytes.saturating_add(actual_bytes);
        if selected_files > MAX_SELECTED_SOURCE_FILES || selected_bytes > MAX_SELECTED_SOURCE_BYTES {
            return Err(CoreError::SourceBudgetExceeded {
                path: path.display().to_string(),
                selected_files,
                max_files: MAX_SELECTED_SOURCE_FILES,
                selected_bytes,
                max_bytes: MAX_SELECTED_SOURCE_BYTES,
            });
        }
        self.selected_files = selected_files;
        self.selected_bytes = selected_bytes;
        Ok(())
    }

    /// Number of selected sources accounted for so far.
    #[must_use]
    pub const fn selected_files(self) -> u64 {
        self.selected_files
    }

    /// Aggregate metadata size accounted for so far.
    #[must_use]
    pub const fn selected_bytes(self) -> u64 {
        self.selected_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled_budget(max_source_bytes: usize, path: &str, actual_bytes: u64, count: u64) -> SourceBudget {
        let mut budget =
            SourceBudget::new(max_source_bytes).unwrap_or_else(|error| panic!("budget: {error}"));
        for index in 0..count {
            budget
                .observe(Path::new(path), actual_bytes)
                .unwrap_or_else(|error| panic!("source {index}: {error}"));
        }
        budget
    }

    fn assert_budget_exceeded(mut budget: SourceBudget, actual_bytes: u64) {
        let result = budget.observe(Path::new("overflow"), actual_bytes);
        assert!(matches!(result, Err(CoreError::SourceBudgetExceeded { .. })));
    }

    #[test]
    fn configurable_limit_cannot_exceed_immutable_ceiling() {
        assert!(SourceBudget::new(MAX_SOURCE_BYTES_HARD_LIMIT).is_ok());
        assert!(matches!(
            SourceBudget::new(MAX_SOURCE_BYTES_HARD_LIMIT + 1),
            Err(CoreError::Config(_))
        ));
    }

    #[test]
    fn aggregate_byte_limit_is_inclusive_and_typed() {
        let per_file = u64::try_from(MAX_SOURCE_BYTES_HARD_LIMIT).unwrap_or(u64::MAX);
        let budget = filled_budget(MAX_SOURCE_BYTES_HARD_LIMIT, "source", per_file, 16);
        assert_eq!(budget.selected_bytes(), MAX_SELECTED_SOURCE_BYTES);
        assert_budget_exceeded(budget, 1);
    }

    #[test]
    fn selected_file_count_is_bounded() {
        let budget = filled_budget(1, "empty", 0, MAX_SELECTED_SOURCE_FILES);
        assert_budget_exceeded(budget, 0);
    }
}
