use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FaqEntry {
	pub id: String,
	pub question: String,
	#[serde(default)]
	pub paraphrases: Vec<String>,
	pub answer: String,
}

pub fn load_faq(path: &Path) -> Result<Vec<FaqEntry>> {
	let text = std::fs::read_to_string(path)
		.with_context(|| format!("cannot read FAQ file '{}'", path.display()))?;
	let entries: Vec<FaqEntry> = serde_json::from_str(&text)
		.with_context(|| format!("malformed FAQ JSON in '{}'", path.display()))?;
	validate(&entries)?;
	Ok(entries)
}

pub fn validate(entries: &[FaqEntry]) -> Result<()> {
	let mut seen = HashSet::new();
	let mut duplicates: Vec<&str> = Vec::new();
	for entry in entries {
		if entry.id.trim().is_empty() {
			bail!("FAQ entry with empty id (question: {:?})", entry.question);
		}
		if !seen.insert(entry.id.as_str()) && !duplicates.contains(&entry.id.as_str()) {
			duplicates.push(&entry.id);
		}
	}
	if !duplicates.is_empty() {
		bail!("duplicate FAQ ids: {}", duplicates.join(", "));
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	fn entry(id: &str) -> FaqEntry {
		FaqEntry {
			id: id.into(),
			question: format!("question for {id}"),
			paraphrases: Vec::new(),
			answer: format!("answer for {id}"),
		}
	}

	#[test]
	fn accepts_unique_ids() {
		assert!(validate(&[entry("a"), entry("b")]).is_ok());
	}

	#[test]
	fn rejects_duplicate_ids_listing_them() {
		let err = validate(&[entry("a"), entry("b"), entry("a"), entry("b"), entry("a")])
			.unwrap_err()
			.to_string();
		assert!(err.contains("duplicate FAQ ids: a, b"), "got: {err}");
	}

	#[test]
	fn rejects_empty_id() {
		assert!(validate(&[entry("  ")]).is_err());
	}

	#[test]
	fn parses_bundled_faq_file() {
		let path = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/faq.json"));
		let entries = load_faq(path).expect("bundled faq.json must load");
		assert!(!entries.is_empty());
		assert!(entries.iter().all(|e| !e.question.trim().is_empty()));
	}

	#[test]
	fn paraphrases_are_optional_in_json() {
		let entries: Vec<FaqEntry> =
			serde_json::from_str(r#"[{"id": "x", "question": "q?", "answer": "a"}]"#).unwrap();
		assert!(entries[0].paraphrases.is_empty());
	}
}
