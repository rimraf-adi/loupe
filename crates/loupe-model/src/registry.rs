//! Known models. Every file is pinned by SHA-256; a mismatch is an error, never a warning.

#[derive(Clone, Copy, Debug)]
pub struct ModelFile {
    pub name: &'static str,
    /// Path under the repository's `resolve/<revision>/` URL.
    pub remote: &'static str,
    pub sha256: &'static str,
    pub bytes: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct ModelSpec {
    /// Directory name under the models dir; also the `--embedder` value.
    pub id: &'static str,
    pub repo: &'static str,
    /// Pinned commit, so the bytes cannot change under a fixed checksum.
    pub revision: &'static str,
    pub license: &'static str,
    pub dim: usize,
    pub files: &'static [ModelFile],
}

impl ModelSpec {
    pub fn url(&self, f: &ModelFile) -> String {
        format!(
            "https://huggingface.co/{}/resolve/{}/{}",
            self.repo, self.revision, f.remote
        )
    }
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.bytes).sum()
    }
}

pub const REGISTRY: &[ModelSpec] = &[ModelSpec {
    id: "minilm-l6-v2",
    repo: "sentence-transformers/all-MiniLM-L6-v2",
    revision: "1110a243fdf4706b3f48f1d95db1a4f5529b4d41",
    license: "Apache-2.0",
    dim: 384,
    files: &[
        ModelFile {
            name: "model.safetensors",
            remote: "model.safetensors",
            sha256: "53aa51172d142c89d9012cce15ae4d6cc0ca6895895114379cacb4fab128d9db",
            bytes: 90_868_376,
        },
        ModelFile {
            name: "tokenizer.json",
            remote: "tokenizer.json",
            sha256: "be50c3628f2bf5bb5e3a7f17b1f74611b2561a3a27eeab05e5aa30f411572037",
            bytes: 466_247,
        },
        ModelFile {
            name: "config.json",
            remote: "config.json",
            sha256: "953f9c0d463486b10a6871cc2fd59f223b2c70184f49815e7efbcab5d8908b41",
            bytes: 612,
        },
    ],
}];

pub fn lookup(id: &str) -> Option<&'static ModelSpec> {
    REGISTRY.iter().find(|m| m.id == id)
}
