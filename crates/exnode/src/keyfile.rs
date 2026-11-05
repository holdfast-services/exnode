//! The node's private key file.  Created 0600; refused if anyone else can read it.

use anyhow::{bail, Context, Result};

pub fn write_key_file(path: &str, content: &str) -> Result<()> {
    if std::path::Path::new(path).exists() {
        bail!("{path} already exists; refusing to overwrite a node key");
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true).create_new(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("create {path}"))?;
        f.write_all(content.as_bytes())?;
    }
    #[cfg(not(unix))]
    std::fs::write(path, content)?;
    Ok(())
}

/// The key file's permission bits and its owner's uid, for `exnode doctor`.
#[cfg(unix)]
pub fn key_file_mode(path: &str) -> Result<(u32, u32)> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let m = std::fs::metadata(path).with_context(|| format!("cannot read key file {path}"))?;
    Ok((m.permissions().mode() & 0o777, m.uid()))
}

/// What `exnode doctor` makes of a key file's mode: `None` for 0600, else
/// the problem and whether it is fatal (the agent refuses a key that group
/// or others can read).
pub fn key_mode_problem(mode: u32) -> Option<(String, bool)> {
    match mode & 0o777 {
        0o600 => None,
        m if m & 0o077 != 0 => Some((format!("mode {m:04o}: other users can read the node's private key; run chmod 600"), true)),
        m => Some((format!("mode {m:04o}, expected 0600"), false)),
    }
}

pub fn read_key_file(path: &str) -> Result<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)
            .with_context(|| format!("cannot read key file {path}"))?
            .permissions().mode();
        if mode & 0o077 != 0 {
            bail!("{path} is readable by other users (mode {:o}); run: chmod 600 {path}", mode & 0o777);
        }
    }
    let raw = std::fs::read_to_string(path).with_context(|| format!("cannot read key file {path}"))?;
    Ok(raw.trim().to_string())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn doctor_wants_0600() {
        assert_eq!(key_mode_problem(0o600), None);
        assert!(key_mode_problem(0o644).is_some_and(|(_, fatal)| fatal));
        assert!(key_mode_problem(0o640).is_some_and(|(_, fatal)| fatal));
        assert!(key_mode_problem(0o400).is_some_and(|(_, fatal)| !fatal));
    }

    #[test]
    fn key_file_is_private_and_checked() {
        let dir  = std::env::temp_dir().join(format!("exnode-key-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("node.key");
        let p    = path.to_str().unwrap();
        write_key_file(p, "secret").unwrap();
        assert_eq!(std::fs::metadata(p).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(read_key_file(p).unwrap(), "secret");
        assert!(write_key_file(p, "other").is_err(), "must not overwrite");

        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_key_file(p).is_err(), "world-readable key must be refused");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
