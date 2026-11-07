use std::{
    io,
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

use crate::{config::utils::tilde_expand, shared::env::ENV};

#[cfg(debug_assertions)]
const CONFIG_NAME: &str = "config.debug.ron";
#[cfg(not(debug_assertions))]
const CONFIG_NAME: &str = "config.ron";
const CONFIG_DIR: &str = "rspc";

pub fn runtime_dir() -> PathBuf {
    ENV.var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .map_or_else(
            || {
                std::env::temp_dir()
                    .join(format!("rspc-ui-{}", rustix::process::geteuid().as_raw()))
            },
            |path| path.join("rspc-ui"),
        )
}

pub fn create_runtime_dir() -> io::Result<PathBuf> {
    let path = runtime_dir();
    private_dir(&path)?;
    Ok(path)
}

pub fn log_dir() -> io::Result<PathBuf> {
    let state = ENV
        .var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| home_dir().map(|home| home.join(".local/state")))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "cannot determine rspc state directory",
            )
        })?;
    let path = state.join("rspc/logs");
    private_dir(&path)?;
    Ok(path)
}

fn private_dir(path: &Path) -> io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    let metadata = path.symlink_metadata()?;
    if !metadata.is_dir() || metadata.uid() != rustix::process::geteuid().as_raw() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "rspc directory must be owned by the current user and not be a symlink",
        ));
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

pub fn home_dir() -> Option<PathBuf> {
    ENV.var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

pub fn config_dir() -> Option<PathBuf> {
    ENV.var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home_dir().map(|home| home.join(".config")))
        .map(|p| p.join(CONFIG_DIR))
}

pub fn config_paths(cli_arg_config_path: Option<&Path>) -> Vec<PathBuf> {
    if let Some(path) = cli_arg_config_path {
        return vec![path.to_path_buf()];
    }

    let mut result = Vec::new();
    match config_dir() {
        Some(config_dir) => result.push(config_dir.join(CONFIG_NAME)),
        None => log::warn!("Could not determine configuration directory"),
    }

    if let Some(home) = home_dir() {
        result.push(home.join(CONFIG_DIR).join(CONFIG_NAME));
    }

    result
}

pub fn theme_paths(
    cli_arg_theme: Option<&Path>,
    config_path: &Path,
    theme_name: &str,
) -> Vec<PathBuf> {
    if let Some(path) = cli_arg_theme {
        return vec![path.to_path_buf()];
    }

    let config_dir = config_path.parent().unwrap_or_else(|| {
        panic!(
            "Expected config path to have parent directory. Path: '{}'",
            config_path.display()
        )
    });

    vec![
        config_dir.join("themes").join(format!("{theme_name}.ron")),
        config_dir.join("themes").join(theme_name),
        config_dir.join(format!("{theme_name}.ron")),
        config_dir.join(theme_name),
        PathBuf::from(tilde_expand(theme_name).into_owned()),
    ]
}

#[cfg(test)]
mod privacy_tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn private_directories_restrict_access_and_reject_symlinks() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let path = temp.path().join("private");
        private_dir(&path).expect("create private directory");
        assert_eq!(
            path.metadata().expect("metadata").permissions().mode() & 0o777,
            0o700
        );
        let link = temp.path().join("link");
        symlink(&path, &link).expect("symlink");
        assert_eq!(
            private_dir(&link).expect_err("reject symlink").kind(),
            io::ErrorKind::PermissionDenied
        );
    }
}
