# Embedder spike (STEP 2)

Goal: replace the stand-in `HashingEmbedder` (worse than keyword search) with a small local
neural model behind the `Embedder` trait, if one is worth its cost. Constraints: weights < ~300 MB,
CPU only, pure Rust runtime preferred, weights fetched explicitly by the user.

## Candidates

License/size/format facts come from the Hugging Face API (checked 2026-11-06). Only the model I
actually ran has measured throughput; the rest are not measured, and the table says so.

| Model | License | Weights | Format available | Runtime fit | Measured here |
|---|---|---|---|---|---|
| `sentence-transformers/all-MiniLM-L6-v2` (22M params, 6 layers, 384-d) | Apache-2.0 | 91 MB safetensors | safetensors, ONNX | candle `BertModel` loads it as is | **yes**: 76 chunks/s, see below |
| `BAAI/bge-small-en-v1.5` (33M, 12 layers, 384-d) | MIT | 133 MB safetensors | safetensors, ONNX | candle `BertModel` | earlier trial (progress.md "Neural embedder trial"): ~18 chunks/s, 3.3 GB peak; not re-run |
| `codesage/codesage-small-v2` (~130M, code-trained) | Apache-2.0 | 256 MB `pytorch_model.bin` only | no safetensors, no ONNX | custom architecture; no candle implementation, would need a converter and a port | no |
| `jinaai/jina-embeddings-v2-base-code` (code-trained, 8k ctx) | Apache-2.0 | 322 MB safetensors; 162 MB int8 ONNX | safetensors, ONNX | JinaBert is in candle-transformers, but the fp32 file is over the 300 MB budget and a 161M-param model is ~5x MiniLM's compute | no |

Runtime: **candle** (pure Rust, optional Apple Accelerate BLAS) over `ort`. `ort` needs a
downloaded or bundled native ONNX Runtime binary at build time, which breaks the "build offline
from a lockfile" property the rest of the workspace has. Tokenization uses the `tokenizers` crate
(its `onig` regex backend is a C dependency; the only non-Rust piece).

## Choice: `all-MiniLM-L6-v2`

Chosen on cost, not on code quality. It is the only candidate that is both small enough to index a
real tree in tens of minutes and loadable by candle with no porting. It is a general English
sentence model (trained on 1B pairs that include some code/StackExchange data), **not** a code
model, so the code-specific candidates (CodeSage, Jina-code) remain the likely quality upgrade and
are the open item: they need either a converter (CodeSage) or a larger compute budget (Jina-code).

Settings: mean pooling, L2-normalised, input truncated to 256 tokens (part of the model id, so
changing it rebuilds embeddings), length-sorted sub-batches of 16 inside the pipeline's batches of
64, then the existing int8 quantisation. Model id: `minilm-l6-v2+t256`.

## Throughput (M1, 8 cores, Accelerate, release build)

Indexing a 1,484-file subset of the cargo registry (1,364 blobs, about 22.7k unique chunks):

| Embedder | Wall time | Peak RSS | Index size |
|---|---|---|---|
| hashing (256-d) | 2.6 s | 172 MB | 31.4 MB |
| minilm-l6-v2 (384-d) | 298 s | 2.17 GB | 50.6 MB |

About **76 chunks/s** (about 4x the 18/s bge-small trial). Query embedding is a single short
forward pass: end-to-end `query` p50 is 7-14 ms vs 1-9 ms with hashing on the eval repos.
