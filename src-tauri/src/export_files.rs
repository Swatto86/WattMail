//! Save automatically named exports without overwriting an existing file.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Reserve each name atomically: concurrent exports and dangling symlinks are
/// collisions too. Callers supply a sanitized single-component stem/extension.
pub fn write_unique(dir: &Path, stem: &str, ext: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    let mut number = 1u32;
    loop {
        let name = if number == 1 {
            format!("{stem}{ext}")
        } else {
            format!("{stem} ({number}){ext}")
        };
        let path = dir.join(name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                let result = file.write_all(bytes).and_then(|()| file.sync_all());
                drop(file);
                if let Err(error) = result {
                    let _ = std::fs::remove_file(&path);
                    return Err(error);
                }
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                number = number
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("too many exports with the same filename"))?;
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    fn directory(label: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("wattmail-export-{label}-{}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn concurrent_exports_preserve_every_payload_and_existing_file() {
        let dir = directory("concurrent");
        std::fs::write(dir.join("subject.eml"), b"existing").unwrap();
        let barrier = Arc::new(Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|n| {
                let dir = dir.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let body = format!("message {n}");
                    barrier.wait();
                    let path = write_unique(&dir, "subject", ".eml", body.as_bytes()).unwrap();
                    assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
                    path
                })
            })
            .collect();
        let paths: std::collections::HashSet<_> =
            threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(paths.len(), 8);
        assert_eq!(std::fs::read(dir.join("subject.eml")).unwrap(), b"existing");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn attachment_collisions_keep_extension_and_surface_io_errors() {
        let dir = directory("attachment");
        let first = write_unique(&dir, "report", ".pdf", b"first").unwrap();
        let second = write_unique(&dir, "report", ".pdf", b"second").unwrap();
        assert_eq!(first.file_name().unwrap(), "report.pdf");
        assert_eq!(second.file_name().unwrap(), "report (2).pdf");
        assert_eq!(std::fs::read(first).unwrap(), b"first");
        assert!(write_unique(&dir.join("missing"), "report", ".pdf", b"x").is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_is_a_collision_not_an_export_destination() {
        let dir = directory("symlink");
        let target = dir.join("outside");
        std::os::unix::fs::symlink(&target, dir.join("report.pdf")).unwrap();
        let path = write_unique(&dir, "report", ".pdf", b"export").unwrap();
        assert_eq!(path.file_name().unwrap(), "report (2).pdf");
        assert!(!target.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
