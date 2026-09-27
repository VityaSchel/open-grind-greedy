use greedy::matcher::{dot, l2_normalize};
use greedy::{Embedder, FaqEntry, FastembedEmbedder, Matcher};

fn entry(id: &str, question: &str) -> FaqEntry {
	FaqEntry {
		id: id.into(),
		question: question.into(),
		paraphrases: Vec::new(),
		answer: format!("answer for {id}"),
	}
}

#[test]
#[ignore = "requires the embedding model (downloaded on first run)"]
fn login_query_ranks_password_reset_first() {
	let entries = vec![
		entry("password-reset", "How do I reset my password?"),
		entry("install-plugin", "How do I install the plugin?"),
		entry("system-requirements", "What are the system requirements?"),
	];
	let embedder = FastembedEmbedder::load_or_download(true).expect("model must load");
	let matcher = Matcher::new(Box::new(embedder), entries).expect("embedding the FAQ must work");

	let ranked = matcher.rank("i forgot my login, help").expect("ranking must work");
	assert_eq!(ranked.first().map(|m| m.id.as_str()), Some("password-reset"));
}

#[test]
#[ignore = "requires the embedding model (downloaded on first run)"]
fn batch_embedding_keeps_input_order() {
	let topics = ["password reset", "plugin install", "refund policy", "dark mode", "data export"];
	let texts: Vec<String> = (0..70)
		.map(|i| {
			format!(
				"{} {}",
				topics[i % topics.len()],
				"please help me with this ".repeat(i * 7 % 11)
			)
		})
		.collect();
	let texts: Vec<&str> = texts.iter().map(String::as_str).collect();
	let embedder = FastembedEmbedder::load_or_download(true).expect("model must load");

	let batched = embedder.embed(&texts).expect("batch embedding must work");
	assert_eq!(batched.len(), texts.len());
	for (text, mut vector) in texts.iter().zip(batched) {
		let mut single = embedder.embed(&[text]).expect("embedding must work").remove(0);
		l2_normalize(&mut vector);
		l2_normalize(&mut single);
		let cosine = dot(&vector, &single);
		assert!(cosine > 0.9999, "{text:?}: cosine {cosine}");
	}
}
