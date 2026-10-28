//! Durable file primitives: write + fsync, directory fsync, atomic replace.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use loupe_core::{Error, Result};

/// Test hook: when built with debug assertions and `LOUPE_CRASH=<point>@<n>` is set, abort the
/// process (no destructors, like a kill) the n-th time `point` is reached. Lets tests die at an
/// exact step of a durable write instead of hoping a random kill lands there. Compiled out of
/// release builds.
#[cfg(debug_assertions)]
pub fn crash_point(point: &str) {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static HITS: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();
    let Ok(spec) = std::env::var("LOUPE_CRASH") else {
        return;
    };
    let Some((name, nth)) = spec.split_once('@') else {
        return;
    };
    if name != point {
        return;
    }
    let mut hits = HITS.get_or_init(Default::default).lock().unwrap();
    let count = hits.entry(point.to_string()).or_default();
    *count += 1;
    if *count == nth.parse().unwrap_or(1) {
        eprintln!("crash injected at {point}#{count}");
        std::process::abort();
    }
}

#[cfg(not(debug_assertions))]
#[inline(always)]
pub fn crash_point(_point: &str) {}

pub fn io<T>(path: &Path, r: std::io::Result<T>) -> Result<T> {
    r.map_err(|e| Error::io(path, e))
}

/// Flush a directory's entries (new files, renames) to disk. No-op where unsupported.
pub fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        io(dir, File::open(dir).and_then(|f| f.sync_all()))
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

pub fn write_synced(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = io(path, File::create(path))?;
    io(path, f.write_all(bytes))?;
    io(path, f.sync_all())
}

pub fn tmp_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    s.into()
}

/// Replace `path` so that readers see either the old or the new contents, never a mix.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = tmp_path(path);
    write_synced(&tmp, bytes)?;
    crash_point(match path.extension().and_then(|e| e.to_str()) {
        Some("json") => "manifest.before_rename",
        _ => "snapshot.before_rename",
    });
    io(path, fs::rename(&tmp, path))?;
    match path.parent() {
        Some(parent) => sync_dir(parent),
        None => Ok(()),
    }
}
