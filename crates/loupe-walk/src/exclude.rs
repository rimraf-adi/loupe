//! Built-in exclusions for build output, caches, dependencies and binary files.
//!
//! A 10 GB tree is almost always build output. These rules keep the walker from descending into
//! it, or opening binary files in it, at all. They are deliberately conservative: a directory
//! name that is also a common source directory (`build`, `dist`, `out`, `bin`, `vendor`) is *not*
//! excluded by name; those are handled by `.gitignore` / `.loupeignore` and, as a last resort, by
//! the size and file-count budgets in `Config`.

use std::path::Path;

/// Directory names that are never source, in any ecosystem.
const EXCLUDED_DIR_NAMES: &[&str] = &[
    "node_modules",
    "bower_components",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".tox",
    ".eggs",
    ".gradle",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".angular",
    ".turbo",
    ".parcel-cache",
    ".sass-cache",
    ".dart_tool",
    ".terraform",
    "DerivedData",
    "Pods",
    ".hg",
    ".svn",
];

/// Directory name prefixes of generated trees (`cmake-build-debug`, `bazel-out`, ...).
const EXCLUDED_DIR_PREFIXES: &[&str] = &["cmake-build-", "bazel-"];

/// Extensions of files that are binary or generated, never worth opening.
const SKIPPED_EXTENSIONS: &[&str] = &[
    // compiled / linked
    "o",
    "obj",
    "a",
    "lib",
    "so",
    "dylib",
    "dll",
    "exe",
    "rlib",
    "rmeta",
    "class",
    "pyc",
    "pyo",
    "wasm",
    "node",
    "pdb",
    "ilk",
    "pch",
    "gch",
    "bc",
    "dSYM",
    // archives and packages
    "zip",
    "gz",
    "tgz",
    "bz2",
    "xz",
    "zst",
    "7z",
    "rar",
    "jar",
    "war",
    "whl",
    "apk",
    "aar",
    "ipa",
    "deb",
    "rpm",
    "dmg",
    "iso",
    // media and fonts
    "png",
    "jpg",
    "jpeg",
    "gif",
    "webp",
    "ico",
    "bmp",
    "tiff",
    "psd",
    "mp3",
    "mp4",
    "mov",
    "avi",
    "mkv",
    "wav",
    "flac",
    "ogg",
    "woff",
    "woff2",
    "ttf",
    "otf",
    "eot",
    "pdf",
    // data blobs and generated text that dwarfs the code it describes
    "db",
    "sqlite",
    "sqlite3",
    "pack",
    "idx",
    "map",
    "tsbuildinfo",
];

/// True for a directory that must not be walked. Cheap name checks run first; the filesystem is
/// consulted only for the rules that need it.
pub fn is_excluded_dir(dir: &Path) -> bool {
    let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if EXCLUDED_DIR_NAMES.contains(&name)
        || EXCLUDED_DIR_PREFIXES.iter().any(|p| name.starts_with(p))
    {
        return true;
    }
    // Cargo / Maven / sbt output: `target` next to a build file.
    if name == "target"
        && let Some(parent) = dir.parent()
        && ["Cargo.toml", "pom.xml", "build.sbt"]
            .iter()
            .any(|f| parent.join(f).is_file())
    {
        return true;
    }
    // Python virtual environments, whatever they are called.
    if matches!(name, "venv" | ".venv" | "env" | "virtualenv") && dir.join("pyvenv.cfg").is_file() {
        return true;
    }
    // The Cache Directory Tagging standard: Cargo, CMake, pip, and others mark output this way.
    dir.join("CACHEDIR.TAG").is_file()
}

/// True if any directory above `rel` (relative to `root`) is excluded.
pub fn is_in_excluded_dir(root: &Path, rel: &Path) -> bool {
    let mut cur = root.to_path_buf();
    let Some(parent) = rel.parent() else {
        return false;
    };
    for comp in parent.components() {
        cur.push(comp);
        if is_excluded_dir(&cur) {
            return true;
        }
    }
    false
}

/// True for a path whose extension marks it binary or generated.
pub fn has_skipped_extension(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    let Some((_, ext)) = name.rsplit_once('.') else {
        return false;
    };
    SKIPPED_EXTENSIONS
        .iter()
        .any(|e| e.eq_ignore_ascii_case(ext))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn names_and_prefixes() {
        assert!(is_excluded_dir(Path::new("/r/node_modules")));
        assert!(is_excluded_dir(Path::new("/r/cmake-build-debug")));
        assert!(is_excluded_dir(Path::new("/r/DerivedData")));
        // Ambiguous names are left to gitignore.
        for n in ["build", "dist", "out", "bin", "vendor", "src", "target"] {
            assert!(
                !is_excluded_dir(Path::new(&format!("/definitely/not/{n}"))),
                "{n}"
            );
        }
    }

    #[test]
    fn target_needs_a_build_file_beside_it() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join("a/target")).unwrap();
        fs::create_dir_all(d.path().join("b/target")).unwrap();
        fs::write(d.path().join("a/Cargo.toml"), "").unwrap();
        assert!(is_excluded_dir(&d.path().join("a/target")));
        assert!(!is_excluded_dir(&d.path().join("b/target")));
    }

    #[test]
    fn markers_exclude_unknown_names() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join("weird-out")).unwrap();
        fs::write(
            d.path().join("weird-out/CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55",
        )
        .unwrap();
        fs::create_dir_all(d.path().join("venv")).unwrap();
        fs::write(d.path().join("venv/pyvenv.cfg"), "").unwrap();
        fs::create_dir_all(d.path().join("env")).unwrap(); // a real source dir named env
        assert!(is_excluded_dir(&d.path().join("weird-out")));
        assert!(is_excluded_dir(&d.path().join("venv")));
        assert!(!is_excluded_dir(&d.path().join("env")));
        assert!(is_in_excluded_dir(d.path(), Path::new("weird-out/x/y.rs")));
        assert!(!is_in_excluded_dir(d.path(), Path::new("env/y.rs")));
    }

    #[test]
    fn extensions() {
        assert!(has_skipped_extension("a/b/libfoo.RLIB"));
        assert!(has_skipped_extension("bundle.js.map"));
        assert!(!has_skipped_extension("src/main.rs"));
        assert!(!has_skipped_extension("Makefile"));
        assert!(!has_skipped_extension("dir.o/readme"));
    }
}
