use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use loupe_core::{Error, Result};
use sha2::{Digest, Sha256};

use crate::registry::{ModelFile, ModelSpec};

/// `$LOUPE_MODEL_DIR`, else `~/.cache/loupe/models`.
pub fn models_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("LOUPE_MODEL_DIR") {
        return PathBuf::from(d);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".cache").join("loupe").join("models")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelStatus {
    Missing,
    /// Files present but a checksum or size is wrong.
    Corrupt(String),
    Ready(PathBuf),
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
    move |e| Error::io(path, e)
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut f = fs::File::open(path).map_err(io(path))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).map_err(io(path))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn check_file(dir: &Path, f: &ModelFile) -> Result<Option<String>> {
    let p = dir.join(f.name);
    if !p.exists() {
        return Ok(Some(format!("{} is missing", f.name)));
    }
    let got = sha256_file(&p)?;
    if got != f.sha256 {
        return Ok(Some(format!(
            "{}: sha256 {got} does not match pinned {}",
            f.name, f.sha256
        )));
    }
    Ok(None)
}

/// Verify every file of `spec` under `root/<id>/` (reads and hashes them all).
pub fn verify(spec: &ModelSpec, root: &Path) -> Result<ModelStatus> {
    let dir = root.join(spec.id);
    if !dir.exists() {
        return Ok(ModelStatus::Missing);
    }
    for f in spec.files {
        if let Some(problem) = check_file(&dir, f)? {
            return Ok(if fs::read_dir(&dir).map_err(io(&dir))?.next().is_none() {
                ModelStatus::Missing
            } else {
                ModelStatus::Corrupt(problem)
            });
        }
    }
    Ok(ModelStatus::Ready(dir))
}

/// Cheap presence check (sizes only) for start-up; `verify` is the full check.
pub fn status(spec: &ModelSpec, root: &Path) -> ModelStatus {
    let dir = root.join(spec.id);
    for f in spec.files {
        match fs::metadata(dir.join(f.name)) {
            Ok(m) if m.len() == f.bytes => {}
            Ok(_) => return ModelStatus::Corrupt(format!("{} has the wrong size", f.name)),
            Err(_) => return ModelStatus::Missing,
        }
    }
    ModelStatus::Ready(dir)
}

/// Download `spec` into `root/<id>/` with `curl`, verifying each file's SHA-256 before it is
/// moved into place. Files already present and valid are skipped. Prints progress to stderr.
pub fn fetch(spec: &ModelSpec, root: &Path) -> Result<PathBuf> {
    let dir = root.join(spec.id);
    fs::create_dir_all(&dir).map_err(io(&dir))?;
    for f in spec.files {
        if check_file(&dir, f)?.is_none() {
            eprintln!("{}: already present and verified", f.name);
            continue;
        }
        let url = spec.url(f);
        let tmp = dir.join(format!("{}.part", f.name));
        eprintln!("downloading {} ({} bytes) from {url}", f.name, f.bytes);
        let st = Command::new("curl")
            .args([
                "--fail",
                "--location",
                "--silent",
                "--show-error",
                "--output",
            ])
            .arg(&tmp)
            .arg(&url)
            .status()
            .map_err(|e| Error::InvalidArgument(format!("could not run curl: {e}")))?;
        if !st.success() {
            let _ = fs::remove_file(&tmp);
            return Err(Error::InvalidArgument(format!(
                "curl failed for {url} ({st})"
            )));
        }
        let got = sha256_file(&tmp)?;
        if got != f.sha256 {
            let _ = fs::remove_file(&tmp);
            return Err(Error::InvalidArgument(format!(
                "checksum mismatch for {}: got {got}, pinned {}; file discarded",
                f.name, f.sha256
            )));
        }
        fs::rename(&tmp, dir.join(f.name)).map_err(io(&tmp))?;
        eprintln!("{}: sha256 verified", f.name);
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DATA: &[u8] = b"hello";
    // sha256("hello")
    const HELLO: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
    static FILES: &[ModelFile] = &[ModelFile {
        name: "a.bin",
        remote: "a.bin",
        sha256: HELLO,
        bytes: 5,
    }];
    static SPEC: ModelSpec = ModelSpec {
        id: "t",
        repo: "x/y",
        revision: "main",
        license: "MIT",
        dim: 4,
        files: FILES,
    };

    #[test]
    fn verify_detects_missing_corrupt_and_ready() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(verify(&SPEC, root.path()).unwrap(), ModelStatus::Missing);
        let d = root.path().join("t");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("a.bin"), b"hellO").unwrap();
        assert!(matches!(
            verify(&SPEC, root.path()).unwrap(),
            ModelStatus::Corrupt(_)
        ));
        fs::write(d.join("a.bin"), DATA).unwrap();
        assert_eq!(
            verify(&SPEC, root.path()).unwrap(),
            ModelStatus::Ready(d.clone())
        );
        assert_eq!(status(&SPEC, root.path()), ModelStatus::Ready(d));
    }

    #[test]
    fn registry_ids_resolve() {
        assert!(crate::lookup("minilm-l6-v2").is_some());
        assert!(crate::lookup("nope").is_none());
    }
}
