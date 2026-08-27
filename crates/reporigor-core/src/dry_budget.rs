/// Default maximum number of minimum-size token windows in one DRY run.
pub const DRY_DEFAULT_MAX_TOTAL_WINDOWS: usize = 1_000_000;

/// Immutable ceiling for minimum-size token windows in one DRY run.
pub const DRY_HARD_MAX_TOTAL_WINDOWS: usize = 2_000_000;

/// Default maximum number of distinct rolling-fingerprint buckets.
pub const DRY_DEFAULT_MAX_FINGERPRINT_BUCKETS: usize = 500_000;

/// Immutable ceiling for distinct rolling-fingerprint buckets.
pub const DRY_HARD_MAX_FINGERPRINT_BUCKETS: usize = 1_000_000;

/// Default exact-comparison and extension work units in one DRY run.
pub const DRY_DEFAULT_MAX_CANDIDATE_WORK: usize = 10_000_000;

/// Immutable ceiling for exact-comparison and extension work units.
pub const DRY_HARD_MAX_CANDIDATE_WORK: usize = 25_000_000;

/// Validate one repository-configurable DRY work limit against its immutable
/// compiled ceiling.
///
/// # Errors
///
/// Returns a human-readable configuration error when `provided` is zero or
/// exceeds `hard_limit`.
pub fn validate_dry_work_limit(name: &str, provided: usize, hard_limit: usize) -> Result<(), String> {
    if provided == 0 {
        return Err(format!("{name} must be greater than zero"));
    }
    if provided > hard_limit {
        return Err(format!(
            "{name} must not exceed the immutable {hard_limit} safety limit"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configurable_limits_are_positive_and_cannot_raise_hard_ceilings() {
        assert!(validate_dry_work_limit("limit", 1, 2).is_ok());
        assert!(validate_dry_work_limit("limit", 0, 2).is_err());
        assert!(validate_dry_work_limit("limit", 3, 2).is_err());
    }
}
