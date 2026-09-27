use crate::matcher::Embedder;
use anyhow::{Context, Result, anyhow};
use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const MODEL: EmbeddingModel = EmbeddingModel::BGESmallENV15;

pub struct FastembedEmbedder {
	model: Mutex<TextEmbedding>,
}

impl FastembedEmbedder {
	pub fn load_or_download(quiet: bool) -> Result<Self> {
		let cache_dir = cache_dir_fastembed_uses();
		if !quiet && !model_is_cached(&cache_dir) {
			eprintln!(
				"first run: downloading model {MODEL:?} into '{}' (later runs are fully offline)",
				cache_dir.display()
			);
		}
		let options = TextInitOptions::new(MODEL)
			.with_cache_dir(cache_dir)
			.with_show_download_progress(!quiet);
		let text_embedding = TextEmbedding::try_new(options).with_context(|| {
			format!("failed to load embedding model {MODEL:?} (the first run needs network access to download it)")
		})?;
		Ok(Self { model: Mutex::new(text_embedding) })
	}
}

impl Embedder for FastembedEmbedder {
	fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
		let mut model = self.model.lock().map_err(|_| anyhow!("embedding model lock poisoned"))?;
		model.embed(texts, None).context("embedding failed")
	}
}

fn cache_dir_fastembed_uses() -> PathBuf {
	std::env::var("HF_HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(fastembed::get_cache_dir()))
}

fn model_is_cached(cache_dir: &Path) -> bool {
	TextEmbedding::get_model_info(&MODEL)
		.map(|info| {
			cache_dir.join(format!("models--{}", info.model_code.replace('/', "--"))).is_dir()
		})
		.unwrap_or(false)
}
