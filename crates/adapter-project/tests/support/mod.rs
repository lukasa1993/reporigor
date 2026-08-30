use std::fs;
use std::path::{Path, PathBuf};

pub(crate) fn write_fixtures(root: &Path, files: &[(&str, &str)]) {
    for (relative, contents) in files {
        let path = root.join(relative);
        create_parent(&path);
        fs::write(&path, contents).unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
    }
}

fn create_parent(path: &Path) {
    let parent = path
        .parent()
        .unwrap_or_else(|| panic!("fixture path has no parent"));
    fs::create_dir_all(parent).unwrap_or_else(|error| panic!("create {}: {error}", parent.display()));
}

pub(crate) fn fixture_executable(root: &Path, relative: &str) -> PathBuf {
    let path = root.join(relative);
    write_fixtures(root, &[(relative, "fixture executable")]);
    make_executable(&path);
    path
}

pub(crate) fn make_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
            .unwrap_or_else(|error| panic!("permissions for {}: {error}", path.display()));
    }
    #[cfg(not(unix))]
    let _ = path;
}
