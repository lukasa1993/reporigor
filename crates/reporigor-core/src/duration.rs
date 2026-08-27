use std::time::Duration;

/// Error returned when floating-point seconds cannot form a useful timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "must be finite, greater than zero, fit the supported duration range, and round to at least one nanosecond"
)]
pub struct InvalidDurationSeconds;

/// Convert floating-point seconds into a positive, representable duration.
///
/// [`Duration::try_from_secs_f64`] rejects non-finite, negative, and overflowing
/// values. This wrapper additionally rejects zero and positive values that
/// round down to [`Duration::ZERO`], so every accepted value is a meaningful
/// timeout.
///
/// # Errors
///
/// Returns [`InvalidDurationSeconds`] when `seconds` cannot be represented as
/// a non-zero [`Duration`].
pub fn checked_duration_from_secs_f64(seconds: f64) -> Result<Duration, InvalidDurationSeconds> {
    let duration = Duration::try_from_secs_f64(seconds).map_err(|_| InvalidDurationSeconds)?;
    if duration.is_zero() {
        return Err(InvalidDurationSeconds);
    }
    Ok(duration)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_positive_representable_seconds() {
        let Ok(duration) = checked_duration_from_secs_f64(0.5) else {
            panic!("a half-second timeout must be representable");
        };
        assert_eq!(duration, Duration::from_millis(500));
    }

    #[test]
    fn rejects_every_unusable_duration_class_without_panicking() {
        for seconds in [
            f64::NEG_INFINITY,
            -1.0,
            -0.0,
            0.0,
            f64::from_bits(1),
            1.0e300,
            f64::INFINITY,
            f64::NAN,
        ] {
            assert!(
                checked_duration_from_secs_f64(seconds).is_err(),
                "{seconds:?} must not become a timeout"
            );
        }
    }
}
