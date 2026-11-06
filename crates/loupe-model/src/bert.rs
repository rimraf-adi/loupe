//! A candle BERT sentence embedder (mean pooling, L2-normalised).

use std::path::Path;

use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config};
use loupe_core::{Error, Result};
use loupe_embed::Embedder;
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams};

use crate::registry::ModelSpec;

pub struct BertEmbedder {
    id: String,
    dim: usize,
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
}

impl std::fmt::Debug for BertEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BertEmbedder")
            .field("id", &self.id)
            .finish()
    }
}

fn bad<E: std::fmt::Display>(what: &str) -> impl FnOnce(E) -> Error + '_ {
    move |e| Error::InvalidArgument(format!("{what}: {e}"))
}

impl BertEmbedder {
    /// Truncation length in tokens; part of the model id because it changes the vectors.
    pub const MAX_TOKENS: usize = 256;

    /// Load from `dir` (as laid out by [`crate::fetch`]); call [`crate::verify`] first if the
    /// files have not been checked.
    pub fn load(spec: &ModelSpec, dir: &Path) -> Result<Self> {
        let device = Device::Cpu;
        let cfg: Config = serde_json::from_slice(
            &std::fs::read(dir.join("config.json")).map_err(|e| Error::io(dir, e))?,
        )
        .map_err(bad("config.json"))?;
        let weights =
            std::fs::read(dir.join("model.safetensors")).map_err(|e| Error::io(dir, e))?;
        let vb = VarBuilder::from_buffered_safetensors(weights, DType::F32, &device)
            .map_err(bad("weights"))?;
        let model = BertModel::load(vb, &cfg).map_err(bad("model"))?;
        let mut tokenizer =
            Tokenizer::from_file(dir.join("tokenizer.json")).map_err(bad("tokenizer"))?;
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: Self::MAX_TOKENS,
                ..Default::default()
            }))
            .map_err(bad("truncation"))?;
        tokenizer.with_padding(Some(PaddingParams {
            strategy: PaddingStrategy::BatchLongest,
            ..Default::default()
        }));
        Ok(BertEmbedder {
            id: format!("{}+t{}", spec.id, Self::MAX_TOKENS),
            dim: spec.dim,
            model,
            tokenizer,
            device,
        })
    }

    fn run(&self, texts: &[&str]) -> candle_core::Result<Vec<Vec<f32>>> {
        let enc = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| candle_core::Error::Msg(e.to_string()))?;
        let rows = |f: &dyn Fn(&tokenizers::Encoding) -> Vec<u32>| -> candle_core::Result<Tensor> {
            let n = enc.len();
            let len = enc[0].len();
            let flat: Vec<u32> = enc.iter().flat_map(f).collect();
            Tensor::from_vec(flat, (n, len), &self.device)
        };
        let ids = rows(&|e| e.get_ids().to_vec())?;
        let mask = rows(&|e| e.get_attention_mask().to_vec())?;
        let types = ids.zeros_like()?;
        let out = self.model.forward(&ids, &types, Some(&mask))?; // (n, len, dim)
        let m = mask.to_dtype(DType::F32)?.unsqueeze(2)?;
        let summed = out.broadcast_mul(&m)?.sum(1)?;
        let counts = m.sum(1)?.clamp(1e-9, f64::MAX)?;
        let mean = summed.broadcast_div(&counts)?;
        let norm = mean.sqr()?.sum_keepdim(1)?.sqrt()?.clamp(1e-12, f64::MAX)?;
        mean.broadcast_div(&norm)?.to_vec2::<f32>()
    }
}

impl Embedder for BertEmbedder {
    fn model_id(&self) -> String {
        self.id.clone()
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        // Length-sorted sub-batches keep padding (and wasted compute) small.
        let mut order: Vec<usize> = (0..texts.len()).collect();
        order.sort_by_key(|&i| texts[i].len());
        let mut out = vec![Vec::new(); texts.len()];
        for group in order.chunks(16) {
            let batch: Vec<&str> = group.iter().map(|&i| texts[i]).collect();
            let vecs = self.run(&batch).map_err(bad("embedding"))?;
            for (&i, v) in group.iter().zip(vecs) {
                out[i] = v;
            }
        }
        Ok(out)
    }
}
