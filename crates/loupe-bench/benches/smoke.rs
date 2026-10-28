use criterion::{Criterion, criterion_group, criterion_main};
use loupe_core::BlobId;

/// Placeholder so `cargo bench` runs in CI from M0. Real benches arrive with M2.
fn blob_hash(c: &mut Criterion) {
    let data = vec![b'a'; 64 * 1024];
    c.bench_function("blake3_64k", |b| b.iter(|| BlobId::of(&data)));
}

criterion_group!(benches, blob_hash);
criterion_main!(benches);
