use sha2::{Digest, Sha256};

/// Whether `value` is one canonical lowercase SHA-256 digest.
#[must_use]
pub fn is_lowercase_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Return a canonical repository-relative path with `/` separators.
///
/// # Errors
///
/// Returns an error for empty paths, absolute paths, Windows drive-prefixed
/// paths, or paths containing a parent-directory traversal.
pub fn normalize_repository_path(path: &str) -> Result<String, String> {
    let normalized = path.replace('\\', "/");
    validate_relative_path_prefix(path, &normalized)?;
    normalize_path_components(path, &normalized)
}

fn validate_relative_path_prefix(original: &str, normalized: &str) -> Result<(), String> {
    if normalized.is_empty() {
        return Err("repository-relative path must not be empty".to_string());
    }
    if normalized.starts_with('/')
        || normalized.starts_with("//")
        || normalized.as_bytes().get(1) == Some(&b':')
    {
        return Err(format!("repository path must be relative: {original}"));
    }
    Ok(())
}

fn normalize_path_components(original: &str, normalized: &str) -> Result<String, String> {
    let mut components = Vec::new();
    for component in normalized.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                return Err(format!(
                    "repository path must not contain parent traversal: {original}"
                ));
            }
            value => components.push(value),
        }
    }
    if components.is_empty() {
        return Err("repository-relative path must identify a file or module".to_string());
    }
    Ok(components.join("/"))
}

/// Derive a stable lowercase SHA-256 identifier from structural evidence.
///
/// Callers must pass a repository-relative path and evidence that excludes
/// volatile locations, timestamps, durations, and scheduler-dependent data.
/// Path separators and redundant current-directory components are normalized.
#[must_use]
pub fn stable_id(rule: &str, path: &str, symbol: &str, normalized_evidence: &str) -> String {
    let path = normalize_repository_path(path).unwrap_or_else(|_| path.replace('\\', "/"));
    let mut hasher = Sha256::new();
    for component in [rule, path.as_str(), symbol, normalized_evidence] {
        let length = u64::try_from(component.len()).unwrap_or(u64::MAX);
        hasher.update(length.to_be_bytes());
        hasher.update(component.as_bytes());
    }
    let digest = hasher.finalize();
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{normalize_repository_path, stable_id};

    #[test]
    fn stable_ids_normalize_repository_path_separators() {
        let unix = stable_id("kiss.complexity", "src/lib.rs", "crate::run", "if|match");
        let windows = stable_id("kiss.complexity", r"src\lib.rs", "crate::run", "if|match");
        let dotted = stable_id("kiss.complexity", "./src//lib.rs", "crate::run", "if|match");
        assert_eq!(unix, windows);
        assert_eq!(unix, dotted);
    }

    #[test]
    fn stable_ids_ignore_absolute_roots_and_line_movement_when_callers_use_structural_inputs() {
        let first_root = Path::new("/checkout/one");
        let second_root = Path::new("/different/root");
        let relative = Path::new("src/lib.rs");
        let first = first_root.join(relative);
        let second = second_root.join(relative);
        let first_relative = first.strip_prefix(first_root).unwrap_or(relative);
        let second_relative = second.strip_prefix(second_root).unwrap_or(relative);

        // There is deliberately no line-number input. Unrelated lines may move
        // while the normalized structural evidence remains the same.
        let before = stable_id(
            "dry.clone",
            &first_relative.to_string_lossy(),
            "crate::run",
            "IDENT ( LOCAL )",
        );
        let after = stable_id(
            "dry.clone",
            &second_relative.to_string_lossy(),
            "crate::run",
            "IDENT ( LOCAL )",
        );
        assert_eq!(before, after);
    }

    #[test]
    fn repository_paths_reject_absolute_and_traversing_inputs() {
        assert!(normalize_repository_path("src/lib.rs").is_ok());
        assert!(normalize_repository_path("/src/lib.rs").is_err());
        assert!(normalize_repository_path(r"C:\src\lib.rs").is_err());
        assert!(normalize_repository_path("src/../lib.rs").is_err());
    }
}
