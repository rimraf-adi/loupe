/// Detect a language by file name / extension, falling back to a `#!` line in `head`.
///
/// Returns a lowercase language id, or `None` for unknown files. Unknown files are still
/// indexed for text and embeddings; only symbol extraction needs a language.
pub fn detect_language(path: &str, head: &[u8]) -> Option<&'static str> {
    let name = path.rsplit('/').next().unwrap_or(path);

    match name {
        "Makefile" | "makefile" | "GNUmakefile" => return Some("make"),
        "Dockerfile" => return Some("dockerfile"),
        "CMakeLists.txt" => return Some("cmake"),
        _ => {}
    }

    if let Some((_, ext)) = name.rsplit_once('.') {
        let lang = match ext.to_ascii_lowercase().as_str() {
            "rs" => "rust",
            "py" | "pyi" => "python",
            "js" | "mjs" | "cjs" | "jsx" => "javascript",
            "ts" | "mts" | "cts" => "typescript",
            "tsx" => "tsx",
            "go" => "go",
            "c" | "h" => "c",
            "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => "cpp",
            "java" => "java",
            "kt" | "kts" => "kotlin",
            "swift" => "swift",
            "rb" => "ruby",
            "php" => "php",
            "cs" => "csharp",
            "scala" => "scala",
            "lua" => "lua",
            "sh" | "bash" | "zsh" => "shell",
            "sql" => "sql",
            "html" | "htm" => "html",
            "css" | "scss" => "css",
            "json" => "json",
            "toml" => "toml",
            "yaml" | "yml" => "yaml",
            "md" | "markdown" => "markdown",
            _ => "",
        };
        if !lang.is_empty() {
            return Some(lang);
        }
    }

    shebang_language(head)
}

fn shebang_language(head: &[u8]) -> Option<&'static str> {
    let rest = head.strip_prefix(b"#!")?;
    let line = rest.split(|&b| b == b'\n').next()?;
    let line = std::str::from_utf8(line).ok()?;
    let mut words = line.split_whitespace();
    let mut prog = words.next()?.rsplit('/').next()?;
    if prog == "env" {
        // `#!/usr/bin/env -S python3 -u`: skip flags to find the interpreter.
        prog = words.find(|w| !w.starts_with('-'))?;
    }
    let lang = if prog.starts_with("python") {
        "python"
    } else if prog.starts_with("node") || prog == "deno" || prog == "bun" {
        "javascript"
    } else if prog.starts_with("ruby") {
        "ruby"
    } else if matches!(prog, "sh" | "bash" | "zsh" | "dash" | "ksh") {
        "shell"
    } else if prog.starts_with("php") {
        "php"
    } else if prog.starts_with("lua") {
        "lua"
    } else {
        return None;
    };
    Some(lang)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn by_extension() {
        assert_eq!(detect_language("src/lib.rs", b""), Some("rust"));
        assert_eq!(detect_language("a/b/App.TSX", b""), Some("tsx"));
        assert_eq!(detect_language("x.unknown", b""), None);
    }

    #[test]
    fn by_name() {
        assert_eq!(detect_language("sub/Makefile", b""), Some("make"));
        assert_eq!(detect_language("Dockerfile", b""), Some("dockerfile"));
    }

    #[test]
    fn by_shebang() {
        assert_eq!(
            detect_language("bin/run", b"#!/usr/bin/env python3\nprint()"),
            Some("python")
        );
        assert_eq!(
            detect_language("bin/run", b"#!/usr/bin/env -S node --flag\n"),
            Some("javascript")
        );
        assert_eq!(detect_language("bin/run", b"#!/bin/bash\n"), Some("shell"));
        assert_eq!(detect_language("bin/run", b"#!/usr/bin/perl\n"), None);
        assert_eq!(detect_language("bin/run", b"no shebang"), None);
    }

    #[test]
    fn extension_wins_over_shebang() {
        assert_eq!(detect_language("a.rb", b"#!/bin/sh\n"), Some("ruby"));
    }
}
