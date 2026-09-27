use crate::embedding_cache::EmbeddingCache;
use crate::faq::FaqEntry;
use anyhow::{Result, anyhow, ensure};
use serde::Serialize;
use std::collections::HashMap;

const MAX_QUERY_CHARS_WORTH_TOKENIZING: usize = 4096;

pub trait Embedder: Send + Sync {
	fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Match {
	pub id: String,
	pub score: f32,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct EmbedReport {
	pub cached_count: usize,
	pub embedded_count: usize,
	pub cache_warnings: Vec<String>,
}

struct PhrasingVector {
	entry_index: usize,
	normalized: Vec<f32>,
}

pub struct Matcher {
	embedder: Box<dyn Embedder>,
	entries: Vec<FaqEntry>,
	phrasing_vectors: Vec<PhrasingVector>,
}

impl Matcher {
	pub fn new(embedder: Box<dyn Embedder>, entries: Vec<FaqEntry>) -> Result<Self> {
		Self::build(embedder, entries, None).map(|(matcher, _)| matcher)
	}

	pub fn with_cache(
		embedder: Box<dyn Embedder>,
		entries: Vec<FaqEntry>,
		cache: EmbeddingCache,
	) -> Result<(Self, EmbedReport)> {
		Self::build(embedder, entries, Some(cache))
	}

	fn build(
		embedder: Box<dyn Embedder>,
		entries: Vec<FaqEntry>,
		cache: Option<EmbeddingCache>,
	) -> Result<(Self, EmbedReport)> {
		let (phrasing_vectors, report) =
			embed_entries(embedder.as_ref(), &entries, cache.as_ref())?;
		Ok((Self { embedder, entries, phrasing_vectors }, report))
	}

	pub fn entries(&self) -> &[FaqEntry] {
		&self.entries
	}

	pub fn entry(&self, id: &str) -> Option<&FaqEntry> {
		self.entries.iter().find(|e| e.id == id)
	}

	pub fn rank(&self, query: &str) -> Result<Vec<Match>> {
		let query = truncate_chars(query.trim(), MAX_QUERY_CHARS_WORTH_TOKENIZING);
		if query.is_empty() {
			return Ok(Vec::new());
		}
		let mut vectors = self.embedder.embed(&[query])?;
		let mut query_vector = vectors
			.pop()
			.ok_or_else(|| anyhow!("embedder returned no vector for the query"))?;
		l2_normalize(&mut query_vector);

		let mut max_score_per_entry = vec![f32::NEG_INFINITY; self.entries.len()];
		for phrasing in &self.phrasing_vectors {
			let cosine = dot(&query_vector, &phrasing.normalized);
			if cosine > max_score_per_entry[phrasing.entry_index] {
				max_score_per_entry[phrasing.entry_index] = cosine;
			}
		}
		let mut matches: Vec<Match> = self
			.entries
			.iter()
			.zip(&max_score_per_entry)
			.map(|(entry, score)| Match { id: entry.id.clone(), score: *score })
			.collect();
		matches.sort_by(|a, b| b.score.total_cmp(&a.score));
		Ok(matches)
	}
}

fn embed_entries(
	embedder: &dyn Embedder,
	entries: &[FaqEntry],
	cache: Option<&EmbeddingCache>,
) -> Result<(Vec<PhrasingVector>, EmbedReport)> {
	let mut unique_texts = Vec::new();
	let mut unique_text_slots = HashMap::new();
	let mut phrasing_slots = Vec::new();
	for (entry_index, entry) in entries.iter().enumerate() {
		for phrasing in std::iter::once(&entry.question).chain(&entry.paraphrases) {
			let slot = *unique_text_slots.entry(phrasing.as_str()).or_insert_with(|| {
				unique_texts.push(phrasing.as_str());
				unique_texts.len() - 1
			});
			phrasing_slots.push((entry_index, slot));
		}
	}
	let (mut vectors, report) = match cache {
		Some(cache) => cache.embed(embedder, &unique_texts)?,
		None => {
			let report =
				EmbedReport { embedded_count: unique_texts.len(), ..EmbedReport::default() };
			(embed_all(embedder, &unique_texts)?, report)
		}
	};
	for vector in &mut vectors {
		l2_normalize(vector);
	}
	let phrasing_vectors = phrasing_slots
		.into_iter()
		.map(|(entry_index, slot)| PhrasingVector {
			entry_index,
			normalized: vectors[slot].clone(),
		})
		.collect();
	Ok((phrasing_vectors, report))
}

pub(crate) fn embed_all(embedder: &dyn Embedder, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
	if texts.is_empty() {
		return Ok(Vec::new());
	}
	let vectors = embedder.embed(texts)?;
	ensure!(
		vectors.len() == texts.len(),
		"embedder returned {} vectors for {} texts",
		vectors.len(),
		texts.len()
	);
	Ok(vectors)
}

pub fn dot(a: &[f32], b: &[f32]) -> f32 {
	a.iter().zip(b).map(|(x, y)| x * y).sum()
}

pub fn l2_normalize(vector: &mut [f32]) {
	let norm = dot(vector, vector).sqrt();
	if norm > 0.0 {
		for value in vector {
			*value /= norm;
		}
	}
}

fn truncate_chars(text: &str, max_chars: usize) -> &str {
	match text.char_indices().nth(max_chars) {
		Some((byte_index, _)) => &text[..byte_index],
		None => text,
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::collections::HashMap;

	struct MockEmbedder {
		vectors: HashMap<String, Vec<f32>>,
	}

	impl MockEmbedder {
		fn new(pairs: &[(&str, Vec<f32>)]) -> Self {
			let vectors = pairs.iter().map(|(t, v)| (t.to_string(), v.clone())).collect();
			Self { vectors }
		}
	}

	impl Embedder for MockEmbedder {
		fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
			texts
				.iter()
				.map(|t| {
					self.vectors
						.get(*t)
						.cloned()
						.ok_or_else(|| anyhow!("no mock vector for {t:?}"))
				})
				.collect()
		}
	}

	fn entry(id: &str, question: &str, paraphrases: &[&str]) -> FaqEntry {
		FaqEntry {
			id: id.into(),
			question: question.into(),
			paraphrases: paraphrases.iter().map(|p| p.to_string()).collect(),
			answer: format!("answer for {id}"),
		}
	}

	fn assert_close(actual: f32, expected: f32) {
		assert!((actual - expected).abs() < 1e-5, "expected {expected}, got {actual}");
	}

	#[test]
	fn dot_of_normalized_vectors_is_cosine() {
		assert_close(dot(&[1.0, 0.0], &[0.0, 1.0]), 0.0);
		assert_close(dot(&[0.6, 0.8], &[0.6, 0.8]), 1.0);
		assert_close(dot(&[1.0, 0.0], &[-1.0, 0.0]), -1.0);
	}

	#[test]
	fn l2_normalize_produces_unit_norm() {
		let mut vector = vec![3.0, 4.0];
		l2_normalize(&mut vector);
		assert_close(vector[0], 0.6);
		assert_close(vector[1], 0.8);
		assert_close(dot(&vector, &vector), 1.0);
	}

	#[test]
	fn l2_normalize_leaves_zero_vector_untouched() {
		let mut vector = vec![0.0, 0.0];
		l2_normalize(&mut vector);
		assert_eq!(vector, vec![0.0, 0.0]);
	}

	#[test]
	fn entry_score_is_max_over_question_and_paraphrases() {
		let diagonal = std::f32::consts::FRAC_1_SQRT_2;
		let query_vector = vec![0.0, 1.0];
		let orthogonal_to_query = vec![1.0, 0.0];
		let embedder = MockEmbedder::new(&[
			("how do I reset my password?", orthogonal_to_query),
			("i forgot my login", query_vector.clone()),
			("how do I install it?", vec![diagonal, diagonal]),
			("forgot login help", query_vector),
		]);
		let entries = vec![
			entry("password-reset", "how do I reset my password?", &["i forgot my login"]),
			entry("install", "how do I install it?", &[]),
		];
		let matcher = Matcher::new(Box::new(embedder), entries).unwrap();
		let ranked = matcher.rank("forgot login help").unwrap();

		assert_eq!(ranked.len(), 2);
		assert_eq!(ranked[0].id, "password-reset");
		assert_close(ranked[0].score, 1.0);
		assert_eq!(ranked[1].id, "install");
		assert_close(ranked[1].score, std::f32::consts::FRAC_1_SQRT_2);
	}

	#[test]
	fn empty_and_whitespace_queries_rank_nothing() {
		let embedder = MockEmbedder::new(&[("q?", vec![1.0, 0.0])]);
		let matcher = Matcher::new(Box::new(embedder), vec![entry("x", "q?", &[])]).unwrap();
		assert!(matcher.rank("").unwrap().is_empty());
		assert!(matcher.rank(" \t\n ").unwrap().is_empty());
	}

	#[test]
	fn truncate_chars_respects_char_boundaries() {
		assert_eq!(truncate_chars("héllo", 2), "hé");
		assert_eq!(truncate_chars("short", 100), "short");
	}
}
