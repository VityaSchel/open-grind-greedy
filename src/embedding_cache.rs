use crate::embedder::cache_dir_fastembed_uses;
use crate::matcher::{EmbedReport, Embedder, dot, embed_all};
use anyhow::{Context, Result, ensure};
use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;

const MAGIC: &[u8; 8] = b"FAQEMBED";
const VERSION: u32 = 2;
const MODEL_FINGERPRINT_TEXT: &str = "How do I reset my password?";
const MIN_SAME_MODEL_FINGERPRINT_COSINE: f32 = 0.9999;

pub struct EmbeddingCache {
	path: PathBuf,
}

impl EmbeddingCache {
	pub fn new(path: impl Into<PathBuf>) -> Self {
		Self { path: path.into() }
	}

	pub fn in_cache_dir() -> Self {
		Self::new(cache_dir_fastembed_uses().join("faq-embeddings.bin"))
	}

	pub(crate) fn embed(
		&self,
		embedder: &dyn Embedder,
		unique_texts: &[&str],
	) -> Result<(Vec<Vec<f32>>, EmbedReport)> {
		let model_fingerprint =
			embed_all(embedder, &[MODEL_FINGERPRINT_TEXT])?.pop().unwrap_or_default();
		let mut report = EmbedReport::default();
		let mut known = self.load(&model_fingerprint).unwrap_or_else(|err| {
			report
				.cache_warnings
				.push(format!("ignoring embedding cache '{}': {err:#}", self.path.display()));
			HashMap::new()
		});
		let missing: Vec<&str> =
			unique_texts.iter().copied().filter(|text| !known.contains_key(*text)).collect();
		let unchanged = report.cache_warnings.is_empty()
			&& missing.is_empty()
			&& known.len() == unique_texts.len();
		known.extend(
			missing.iter().map(|text| text.to_string()).zip(embed_all(embedder, &missing)?),
		);
		report.cached_count = unique_texts.len() - missing.len();
		report.embedded_count = missing.len();
		let vectors: Vec<Vec<f32>> = unique_texts
			.iter()
			.map(|text| known.remove(*text))
			.collect::<Option<_>>()
			.context("duplicate text passed to the embedding cache")?;
		if !unchanged && let Err(err) = self.save(&model_fingerprint, unique_texts, &vectors) {
			report
				.cache_warnings
				.push(format!("cannot write embedding cache '{}': {err:#}", self.path.display()));
		}
		Ok((vectors, report))
	}

	fn load(&self, model_fingerprint: &[f32]) -> Result<HashMap<String, Vec<f32>>> {
		match fs::read(&self.path) {
			Ok(bytes) => decode(&bytes, model_fingerprint),
			Err(err) if err.kind() == ErrorKind::NotFound => Ok(HashMap::new()),
			Err(err) => Err(err.into()),
		}
	}

	fn save(&self, model_fingerprint: &[f32], texts: &[&str], vectors: &[Vec<f32>]) -> Result<()> {
		let bytes = encode(model_fingerprint, texts, vectors)?;
		let mut temp_of_this_process = self.path.clone().into_os_string();
		temp_of_this_process.push(format!(".{}.tmp", std::process::id()));
		let result = fs::write(&temp_of_this_process, bytes)
			.and_then(|()| fs::rename(&temp_of_this_process, &self.path));
		if result.is_err() {
			let _ = fs::remove_file(&temp_of_this_process);
		}
		Ok(result?)
	}
}

fn encode(model_fingerprint: &[f32], texts: &[&str], vectors: &[Vec<f32>]) -> Result<Vec<u8>> {
	let mut bytes = MAGIC.to_vec();
	bytes.extend(VERSION.to_le_bytes());
	put_len(&mut bytes, model_fingerprint.len())?;
	put_vector(&mut bytes, model_fingerprint);
	put_len(&mut bytes, texts.len())?;
	for (text, vector) in texts.iter().zip(vectors) {
		ensure!(
			vector.len() == model_fingerprint.len(),
			"embedder returned vectors of varying dimensions"
		);
		put_str(&mut bytes, text)?;
		put_vector(&mut bytes, vector);
	}
	Ok(bytes)
}

fn put_len(bytes: &mut Vec<u8>, len: usize) -> Result<()> {
	bytes.extend(u32::try_from(len).context("too large for the cache format")?.to_le_bytes());
	Ok(())
}

fn put_str(bytes: &mut Vec<u8>, text: &str) -> Result<()> {
	put_len(bytes, text.len())?;
	bytes.extend(text.as_bytes());
	Ok(())
}

fn put_vector(bytes: &mut Vec<u8>, vector: &[f32]) {
	bytes.extend(vector.iter().flat_map(|value| value.to_le_bytes()));
}

fn decode(bytes: &[u8], model_fingerprint: &[f32]) -> Result<HashMap<String, Vec<f32>>> {
	let mut reader = Reader(bytes);
	ensure!(reader.take(MAGIC.len())? == MAGIC, "not an embedding cache file");
	let version = reader.u32()?;
	ensure!(version == VERSION, "unsupported format version {version}");
	let dim = reader.len()?;
	ensure!(
		dim == model_fingerprint.len(),
		"made with {dim}-dimensional vectors, the model has {}",
		model_fingerprint.len()
	);
	ensure!(
		cosine(&reader.vector(dim)?, model_fingerprint) >= MIN_SAME_MODEL_FINGERPRINT_COSINE,
		"made with a different revision of the model"
	);
	let count = reader.len()?;
	let mut vectors = HashMap::new();
	for _ in 0..count {
		let text = reader.str()?;
		vectors.insert(text.to_owned(), reader.vector(dim)?);
	}
	ensure!(reader.0.is_empty(), "trailing bytes after {count} entries");
	Ok(vectors)
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
	dot(a, b) / (dot(a, a) * dot(b, b)).sqrt()
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
	fn take(&mut self, len: usize) -> Result<&'a [u8]> {
		let (head, tail) = self.0.split_at_checked(len).context("file is truncated")?;
		self.0 = tail;
		Ok(head)
	}

	fn u32(&mut self) -> Result<u32> {
		let (head, tail) = self.0.split_first_chunk().context("file is truncated")?;
		self.0 = tail;
		Ok(u32::from_le_bytes(*head))
	}

	fn len(&mut self) -> Result<usize> {
		Ok(usize::try_from(self.u32()?)?)
	}

	fn str(&mut self) -> Result<&'a str> {
		let len = self.len()?;
		std::str::from_utf8(self.take(len)?).context("text is not UTF-8")
	}

	fn vector(&mut self, dim: usize) -> Result<Vec<f32>> {
		let (chunks, _) = self.take(dim.checked_mul(4).context("vector too large")?)?.as_chunks();
		Ok(chunks.iter().map(|chunk| f32::from_le_bytes(*chunk)).collect())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::faq::FaqEntry;
	use crate::matcher::Matcher;
	use std::sync::{Arc, Mutex};
	use tempfile::TempDir;

	#[derive(Clone)]
	struct RecordingEmbedder {
		dim: usize,
		revision: f32,
		calls: Arc<Mutex<Vec<String>>>,
	}

	impl RecordingEmbedder {
		fn new() -> Self {
			Self { dim: 8, revision: 0.0, calls: Arc::default() }
		}

		fn take_calls(&self) -> Vec<String> {
			std::mem::take(&mut self.calls.lock().unwrap())
		}

		fn model_fingerprint(&self) -> Vec<f32> {
			mock_vector(MODEL_FINGERPRINT_TEXT, self.dim, self.revision)
		}
	}

	impl Embedder for RecordingEmbedder {
		fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
			self.calls.lock().unwrap().extend(texts.iter().map(|text| text.to_string()));
			Ok(texts.iter().map(|text| mock_vector(text, self.dim, self.revision)).collect())
		}
	}

	fn mock_vector(text: &str, dim: usize, revision: f32) -> Vec<f32> {
		let seed: f32 =
			text.bytes().enumerate().map(|(i, byte)| (i as f32 + 1.0) * f32::from(byte)).sum();
		(0..dim).map(|i| (seed * 0.001 * (i as f32 + 1.0) + revision).sin()).collect()
	}

	fn entry(id: &str, question: &str, paraphrases: &[&str]) -> FaqEntry {
		FaqEntry {
			id: id.into(),
			question: question.into(),
			paraphrases: paraphrases.iter().map(|p| p.to_string()).collect(),
			answer: format!("answer for {id}"),
		}
	}

	fn faq() -> Vec<FaqEntry> {
		vec![
			entry("install", "How do I install it?", &["setup help", "where to download"]),
			entry("update", "How do I update?", &["new version", "upgrade the app"]),
			entry("donate", "Can I donate?", &["support the project"]),
		]
	}

	fn texts(entries: &[FaqEntry]) -> Vec<String> {
		entries
			.iter()
			.flat_map(|e| std::iter::once(&e.question).chain(&e.paraphrases))
			.cloned()
			.collect()
	}

	fn sorted(mut texts: Vec<String>) -> Vec<String> {
		texts.sort();
		texts
	}

	fn cache_in(dir: &TempDir) -> EmbeddingCache {
		EmbeddingCache::new(dir.path().join("faq-embeddings.bin"))
	}

	fn build(
		embedder: &RecordingEmbedder,
		entries: Vec<FaqEntry>,
		cache: EmbeddingCache,
	) -> (Matcher, EmbedReport) {
		Matcher::with_cache(Box::new(embedder.clone()), entries, cache).unwrap()
	}

	fn cached_texts(dir: &TempDir, embedder: &RecordingEmbedder) -> Vec<String> {
		sorted(cache_in(dir).load(&embedder.model_fingerprint()).unwrap().into_keys().collect())
	}

	#[test]
	fn first_build_embeds_everything_and_writes_the_cache() {
		let dir = TempDir::new().unwrap();
		let embedder = RecordingEmbedder::new();
		let (_, report) = build(&embedder, faq(), cache_in(&dir));

		let mut expected = vec![MODEL_FINGERPRINT_TEXT.to_string()];
		expected.extend(texts(&faq()));
		assert_eq!(embedder.take_calls(), expected);
		assert_eq!(
			report,
			EmbedReport { cached_count: 0, embedded_count: 8, cache_warnings: Vec::new() }
		);
		assert_eq!(cached_texts(&dir, &embedder), sorted(texts(&faq())));
		assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1, "temporary file left behind");
	}

	#[test]
	fn second_build_embeds_only_the_model_fingerprint() {
		let dir = TempDir::new().unwrap();
		let embedder = RecordingEmbedder::new();
		build(&embedder, faq(), cache_in(&dir));
		embedder.take_calls();

		let (_, report) = build(&embedder, faq(), cache_in(&dir));
		assert_eq!(embedder.take_calls(), [MODEL_FINGERPRINT_TEXT]);
		assert_eq!(
			report,
			EmbedReport { cached_count: 8, embedded_count: 0, cache_warnings: Vec::new() }
		);
	}

	#[test]
	fn changed_paraphrase_is_the_only_text_embedded() {
		let dir = TempDir::new().unwrap();
		let embedder = RecordingEmbedder::new();
		build(&embedder, faq(), cache_in(&dir));
		embedder.take_calls();

		let mut entries = faq();
		entries[1].paraphrases[0] = "a brand new version".into();
		let (_, report) = build(&embedder, entries.clone(), cache_in(&dir));
		assert_eq!(embedder.take_calls(), [MODEL_FINGERPRINT_TEXT, "a brand new version"]);
		assert_eq!((report.cached_count, report.embedded_count), (7, 1));
		assert_eq!(cached_texts(&dir, &embedder), sorted(texts(&entries)));
	}

	#[test]
	fn removed_entry_leaves_the_cache() {
		let dir = TempDir::new().unwrap();
		let embedder = RecordingEmbedder::new();
		build(&embedder, faq(), cache_in(&dir));
		embedder.take_calls();

		let entries = faq()[..2].to_vec();
		build(&embedder, entries.clone(), cache_in(&dir));
		assert_eq!(embedder.take_calls(), [MODEL_FINGERPRINT_TEXT]);
		assert_eq!(cached_texts(&dir, &embedder), sorted(texts(&entries)));
	}

	#[test]
	fn duplicate_texts_are_embedded_once() {
		let dir = TempDir::new().unwrap();
		let embedder = RecordingEmbedder::new();
		let entries = vec![
			entry("a", "Same question?", &["shared"]),
			entry("b", "Other question?", &["shared", "Same question?"]),
		];
		let (matcher, report) = build(&embedder, entries, cache_in(&dir));

		assert_eq!(
			embedder.take_calls(),
			[MODEL_FINGERPRINT_TEXT, "Same question?", "shared", "Other question?"]
		);
		assert_eq!((report.cached_count, report.embedded_count), (0, 3));
		let ranked = matcher.rank("shared").unwrap();
		assert_eq!(ranked.len(), 2);
		assert!(ranked.iter().all(|m| (m.score - 1.0).abs() < 1e-5), "{ranked:?}");
	}

	#[test]
	fn unreadable_cache_falls_back_to_embedding_everything() {
		let dir = TempDir::new().unwrap();
		let embedder = RecordingEmbedder::new();
		build(&embedder, faq(), cache_in(&dir));
		let path = cache_in(&dir).path;
		let valid = fs::read(&path).unwrap();
		let mut bad_version = valid.clone();
		bad_version[MAGIC.len()] += 1;
		let corruptions = [
			Vec::new(),
			b"garbage".to_vec(),
			[b"NOTMAGIC".as_slice(), &valid[MAGIC.len()..]].concat(),
			bad_version,
			valid[..valid.len() / 2].to_vec(),
			valid[..valid.len() - 1].to_vec(),
			[valid.as_slice(), b"x"].concat(),
		];
		for corrupt in corruptions {
			fs::write(&path, corrupt).unwrap();
			embedder.take_calls();
			let (_, report) = build(&embedder, faq(), cache_in(&dir));
			assert_eq!(embedder.take_calls().len(), 9);
			assert_eq!((report.cached_count, report.embedded_count), (0, 8));
			assert_eq!(report.cache_warnings.len(), 1, "{:?}", report.cache_warnings);
			assert_eq!(fs::read(&path).unwrap(), valid);
		}
	}

	#[test]
	fn unreadable_cache_is_replaced_even_for_an_empty_faq() {
		let dir = TempDir::new().unwrap();
		let embedder = RecordingEmbedder::new();
		fs::write(cache_in(&dir).path, "garbage").unwrap();
		let (_, report) = build(&embedder, Vec::new(), cache_in(&dir));

		assert_eq!(report.cache_warnings.len(), 1, "{:?}", report.cache_warnings);
		assert_eq!(cached_texts(&dir, &embedder), Vec::<String>::new());
	}

	#[test]
	fn every_truncation_is_rejected() {
		let dir = TempDir::new().unwrap();
		let embedder = RecordingEmbedder::new();
		build(&embedder, faq(), cache_in(&dir));
		let valid = fs::read(cache_in(&dir).path).unwrap();

		assert!(decode(&valid, &embedder.model_fingerprint()).is_ok());
		for len in 0..valid.len() {
			assert!(
				decode(&valid[..len], &embedder.model_fingerprint()).is_err(),
				"accepted {len} bytes"
			);
		}
	}

	#[test]
	fn files_of_the_first_format_version_are_rejected() {
		let dir = TempDir::new().unwrap();
		let embedder = RecordingEmbedder::new();
		build(&embedder, faq(), cache_in(&dir));
		let mut first_version = fs::read(cache_in(&dir).path).unwrap();
		first_version[MAGIC.len()..][..4].copy_from_slice(&1u32.to_le_bytes());

		let error = decode(&first_version, &embedder.model_fingerprint()).unwrap_err();
		assert_eq!(error.to_string(), "unsupported format version 1");
	}

	#[test]
	fn cache_of_another_model_is_ignored() {
		let dir = TempDir::new().unwrap();
		build(&RecordingEmbedder::new(), faq(), cache_in(&dir));
		let path = cache_in(&dir).path;
		let valid = fs::read(&path).unwrap();
		let cases = [
			(RecordingEmbedder { dim: 9, ..RecordingEmbedder::new() }, "dimensional"),
			(RecordingEmbedder { revision: 1.0, ..RecordingEmbedder::new() }, "revision"),
		];
		for (embedder, reason) in cases {
			fs::write(&path, &valid).unwrap();
			let (_, report) = build(&embedder, faq(), cache_in(&dir));
			assert_eq!((report.cached_count, report.embedded_count), (0, 8));
			assert_eq!(report.cache_warnings.len(), 1, "{:?}", report.cache_warnings);
			assert!(report.cache_warnings[0].contains(reason), "{:?}", report.cache_warnings);
		}
	}

	#[test]
	fn unwritable_cache_still_builds_with_a_warning() {
		let dir = TempDir::new().unwrap();
		let embedder = RecordingEmbedder::new();
		let cache = EmbeddingCache::new(dir.path().join("missing/faq-embeddings.bin"));
		let (matcher, report) = build(&embedder, faq(), cache);

		assert_eq!((report.cached_count, report.embedded_count), (0, 8));
		assert_eq!(report.cache_warnings.len(), 1, "{:?}", report.cache_warnings);
		assert!(report.cache_warnings[0].contains("cannot write"), "{:?}", report.cache_warnings);
		assert_eq!(matcher.rank("How do I update?").unwrap()[0].id, "update");
	}

	#[test]
	fn cached_rankings_match_uncached_ones() {
		let dir = TempDir::new().unwrap();
		let embedder = RecordingEmbedder::new();
		let uncached = Matcher::new(Box::new(embedder.clone()), faq()).unwrap();
		build(&embedder, faq(), cache_in(&dir));
		let (cached, report) = build(&embedder, faq(), cache_in(&dir));

		assert_eq!(report.embedded_count, 0);
		for query in ["how to install", "upgrade the app", "can I donate money?"] {
			assert_eq!(cached.rank(query).unwrap(), uncached.rank(query).unwrap());
		}
	}
}
