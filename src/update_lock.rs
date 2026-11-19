use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

pub(crate) struct FileLock { path: String, pid: u32 }

impl FileLock {
    pub(crate) fn acquire(path: &str) -> std::io::Result<Option<Self>> {
        let path_ref = Path::new(path);
        if let Some(parent) = path_ref.parent() { fs::create_dir_all(parent)?; }
        for _ in 0..3 {
            match OpenOptions::new().write(true).create_new(true).open(path_ref) {
                Ok(mut file) => {
                    let pid = std::process::id();
                    if let Err(error) = writeln!(file, "{pid}").and_then(|_| file.sync_all()) {
                        let _ = fs::remove_file(path_ref);
                        return Err(error);
                    }
                    return Ok(Some(Self { path: path.to_owned(), pid }));
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let mut text = String::new();
                    if let Ok(mut file) = fs::File::open(path_ref) { let _ = file.read_to_string(&mut text); }
                    let Some(pid) = text.trim().parse::<u32>().ok() else {
                        let recent = fs::metadata(path_ref).and_then(|m| m.modified())
                            .ok().and_then(|t| t.elapsed().ok())
                            .map(|age| age < Duration::from_secs(30)).unwrap_or(false);
                        if recent { return Ok(None); }
                        match fs::remove_file(path_ref) {
                            Ok(()) => continue,
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                            Err(e) => return Err(e),
                        }
                    };
                    if is_xkeen_sub(pid) { return Ok(None); }
                    match fs::remove_file(path_ref) {
                        Ok(()) => continue,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(e) => return Err(e),
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Ok(None)
    }
}
fn is_xkeen_sub(pid: u32) -> bool {
    matches!(fs::read_to_string(format!("/proc/{pid}/comm")), Ok(s) if s.trim() == "xkeen-sub")
}
impl Drop for FileLock {
    fn drop(&mut self) {
        let owner = fs::read_to_string(&self.path).ok().and_then(|s| s.trim().parse::<u32>().ok());
        if owner == Some(self.pid) { let _ = fs::remove_file(&self.path); }
    }
}