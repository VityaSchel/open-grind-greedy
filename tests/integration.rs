use greedy::{FaqEntry, FastembedEmbedder, Matcher};

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
