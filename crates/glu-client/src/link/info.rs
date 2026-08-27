//! `install-info` side effects for linked Texinfo files.
//!
//! Homebrew source oracle: `Pathname#install_info`,
//! `Pathname#uninstall_info`, and `Pathname#which_install_info` in
//! `/opt/homebrew/Library/Homebrew/extend/pathname.rb` at 5b90e281d.

use glu_core::Prefix;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub(crate) fn install_info(prefix: &Prefix, dst: &Path) {
    run_install_info(prefix, dst, &[]);
}

pub(crate) fn uninstall_info(prefix: &Prefix, dst: &Path) {
    run_install_info(prefix, dst, &["--delete"]);
}

fn run_install_info(prefix: &Prefix, dst: &Path, leading_args: &[&str]) {
    let Some(cmd) = which_install_info(prefix) else {
        return;
    };
    let Some(dir) = dst.parent() else {
        return;
    };
    let _ = Command::new(cmd)
        .args(leading_args)
        .arg("--quiet")
        .arg(dst)
        .arg(dir.join("dir"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn which_install_info(prefix: &Prefix) -> Option<PathBuf> {
    let system = PathBuf::from("/usr/bin/install-info");
    if is_executable_file(&system) {
        return Some(system);
    }
    let texinfo = prefix.0.join("opt/texinfo/bin/install-info");
    if is_executable_file(&texinfo) {
        return Some(texinfo);
    }
    None
}

pub(crate) fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn sh_quote(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
    }

    fn fake_install_info(prefix: &Prefix, log: &Path) {
        let script = prefix.0.join("opt/texinfo/bin/install-info");
        fs::create_dir_all(script.parent().unwrap()).unwrap();
        fs::write(
            &script,
            format!("#!/bin/sh\nprintf '%s\\n' \"$@\" >> {}\n", sh_quote(log)),
        )
        .unwrap();
        let mut perms = fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms).unwrap();
    }

    #[test]
    fn missing_install_info_is_noop() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().join("prefix"));
        let dst = prefix.0.join("share/info/missing.info");
        install_info(&prefix, &dst);
        uninstall_info(&prefix, &dst);
    }

    #[test]
    fn texinfo_opt_install_info_is_used_when_system_binary_is_absent() {
        if is_executable_file(Path::new("/usr/bin/install-info")) {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().join("prefix"));
        let log = tmp.path().join("install-info.log");
        fake_install_info(&prefix, &log);
        let dst = prefix.0.join("share/info/one.info");
        fs::create_dir_all(dst.parent().unwrap()).unwrap();
        fs::write(&dst, b"info").unwrap();

        install_info(&prefix, &dst);
        uninstall_info(&prefix, &dst);

        let calls = fs::read_to_string(log).unwrap();
        assert!(calls.contains("--quiet"));
        assert!(calls.contains("--delete"));
        assert!(calls.contains("one.info"));
        assert!(calls.contains("share/info/dir"));
    }
}
