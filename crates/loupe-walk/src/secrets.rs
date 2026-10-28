/// True if the file name looks like it holds credentials or key material.
///
/// This is a safety default, not a security boundary: indexed text is returned to an LLM, so
/// obvious secret files are left out unless `Config::index_secrets` is set.
pub fn is_secret_path(rel_path: &str) -> bool {
    let name = rel_path.rsplit('/').next().unwrap_or(rel_path);
    let lower = name.to_ascii_lowercase();

    if lower == ".env" || lower.starts_with(".env.") {
        // Templates are meant to be committed and contain no real values.
        return !matches!(
            lower.as_str(),
            ".env.example" | ".env.sample" | ".env.template" | ".env.dist"
        );
    }

    const EXACT: &[&str] = &[
        "id_rsa",
        "id_dsa",
        "id_ecdsa",
        "id_ed25519",
        ".netrc",
        ".pgpass",
        ".npmrc",
        ".pypirc",
        "credentials",
        "credentials.json",
        "service-account.json",
        "secrets.json",
        "secrets.yml",
        "secrets.yaml",
    ];
    if EXACT.contains(&lower.as_str()) {
        return true;
    }

    const SUFFIXES: &[&str] = &[".pem", ".key", ".p12", ".pfx", ".jks", ".keystore", ".kdbx"];
    SUFFIXES.iter().any(|s| lower.ends_with(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_secret_files() {
        for p in [
            ".env",
            "app/.env.local",
            ".env.production",
            "keys/server.pem",
            "id_rsa",
            "home/.ssh/id_ed25519",
            "tls.KEY",
            "config/credentials.json",
        ] {
            assert!(is_secret_path(p), "{p} should be secret");
        }
    }

    #[test]
    fn allows_ordinary_files() {
        for p in [
            ".env.example",
            "src/main.rs",
            "environment.rs",
            "keyboard.rs",
            "docs/credentials-guide.md",
            "id_rsa.pub",
        ] {
            assert!(!is_secret_path(p), "{p} should be allowed");
        }
    }
}
