//! Host-side tool discovery for rootfs image operations.

use std::{
    env,
    ffi::OsStr,
    path::{Path, PathBuf},
};

use anyhow::{Context, bail};

pub(super) fn resolve(env_name: &str, tool_name: &str) -> anyhow::Result<PathBuf> {
    resolve_from(
        env::var_os(env_name).as_deref(),
        env::var_os("PATH").as_deref(),
        env_name,
        tool_name,
    )
}

fn resolve_from(
    configured: Option<&OsStr>,
    search_path: Option<&OsStr>,
    env_name: &str,
    tool_name: &str,
) -> anyhow::Result<PathBuf> {
    if let Some(configured) = configured.filter(|value| !value.is_empty()) {
        let configured = Path::new(configured);
        if let Some(path) = locate(configured, search_path) {
            return Ok(path);
        }
        bail!(
            "{env_name} points to an unavailable command: {}; set {env_name} to an executable \
             path or command name",
            configured.display()
        );
    }

    locate(Path::new(tool_name), search_path).with_context(|| {
        format!(
            "{tool_name} was not found in PATH; install it or set {env_name}=/path/to/{tool_name}"
        )
    })
}

fn locate(command: &Path, search_path: Option<&OsStr>) -> Option<PathBuf> {
    if command.components().count() > 1 {
        return is_executable(command).then(|| command.to_path_buf());
    }

    search_path.and_then(|paths| {
        env::split_paths(paths)
            .map(|directory| directory.join(command))
            .find(|candidate| is_executable(candidate))
    })
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }

    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;

        fs::write(path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn configured_path_takes_precedence_over_search_path() {
        let configured_dir = tempdir().unwrap();
        let path_dir = tempdir().unwrap();
        let configured = configured_dir.path().join("debugfs-custom");
        let path_tool = path_dir.path().join("debugfs");
        make_executable(&configured);
        make_executable(&path_tool);
        let search_path = env::join_paths([path_dir.path()]).unwrap();

        let resolved = resolve_from(
            Some(configured.as_os_str()),
            Some(search_path.as_os_str()),
            "DEBUGFS",
            "debugfs",
        )
        .unwrap();

        assert_eq!(resolved, configured);
    }

    #[cfg(unix)]
    #[test]
    fn configured_command_name_is_resolved_from_path() {
        let path_dir = tempdir().unwrap();
        let tool = path_dir.path().join("debugfs-custom");
        make_executable(&tool);
        let search_path = env::join_paths([path_dir.path()]).unwrap();

        let resolved = resolve_from(
            Some(OsStr::new("debugfs-custom")),
            Some(search_path.as_os_str()),
            "DEBUGFS",
            "debugfs",
        )
        .unwrap();

        assert_eq!(resolved, tool);
    }

    #[cfg(unix)]
    #[test]
    fn falls_back_to_path() {
        let path_dir = tempdir().unwrap();
        let tool = path_dir.path().join("resize2fs");
        make_executable(&tool);
        let search_path = env::join_paths([path_dir.path()]).unwrap();

        let resolved = resolve_from(
            None,
            Some(search_path.as_os_str()),
            "RESIZE2FS",
            "resize2fs",
        )
        .unwrap();

        assert_eq!(resolved, tool);
    }

    #[test]
    fn reports_the_environment_override_for_missing_tools() {
        let error = resolve_from(None, None, "E2FSCK", "e2fsck").unwrap_err();
        assert!(error.to_string().contains("E2FSCK=/path/to/e2fsck"));
    }
}
