use crate::faq::FaqEntry;
use crate::matcher::Match;
use std::collections::HashMap;
use std::time::{Duration, Instant};

const STALE_AFTER_MS: u64 = 10 * 60 * 1000;
const PENDING_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const MAX_CHOICES: usize = 5;
const MIN_WORDS: usize = 4;

#[derive(Debug, PartialEq)]
pub enum Decision {
	NoMatch,
	Answer(String),
	Choose(Vec<String>),
}

impl Decision {
	pub fn best(&self) -> Option<&str> {
		match self {
			Decision::NoMatch => None,
			Decision::Answer(id) => Some(id),
			Decision::Choose(ids) => ids.first().map(String::as_str),
		}
	}
}

pub fn decide<'a>(
	ranking: &[Match],
	threshold: f32,
	ambiguity_margin: f32,
	lookup: impl Fn(&str) -> Option<&'a FaqEntry>,
) -> Decision {
	let Some(top) = ranking.first().filter(|top| top.score >= threshold) else {
		return Decision::NoMatch;
	};
	let mut candidates: Vec<String> = ranking
		.iter()
		.take_while(|m| m.score >= threshold && top.score - m.score <= ambiguity_margin)
		.filter(|m| lookup(&m.id).is_some_and(|entry| !is_placeholder(&entry.answer)))
		.map(|m| m.id.clone())
		.take(MAX_CHOICES)
		.collect();
	match candidates.len() {
		0 => Decision::NoMatch,
		1 => Decision::Answer(candidates.remove(0)),
		_ => Decision::Choose(candidates),
	}
}

pub fn is_placeholder(answer: &str) -> bool {
	let answer = answer.trim();
	answer.is_empty() || answer.get(..4).is_some_and(|prefix| prefix.eq_ignore_ascii_case("todo"))
}

pub fn is_question(text: &str) -> bool {
	text.contains('?') && text.split_whitespace().count() >= MIN_WORDS
}

pub fn is_stale(origin_server_ts: u64, now_ms: u64) -> bool {
	now_ms.saturating_sub(origin_server_ts) > STALE_AFTER_MS
}

pub fn question_list(questions: &[&str]) -> String {
	questions
		.iter()
		.enumerate()
		.map(|(index, question)| format!("{}. {question}", index + 1))
		.collect::<Vec<_>>()
		.join("\n")
}

pub fn picked<'a>(body: &str, entry_ids: &'a [String]) -> Option<&'a str> {
	let number: usize = body.trim().parse().ok()?;
	entry_ids.get(number.checked_sub(1)?).map(String::as_str)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Message {
	pub room_id: String,
	pub event_id: String,
	pub sender: String,
	pub thread_root: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Pending {
	Choices { original: Message, entry_ids: Vec<String> },
	Prompt,
}

#[derive(Default)]
pub struct PendingMessages(HashMap<String, (Instant, Pending)>);

impl PendingMessages {
	pub fn insert(&mut self, event_id: String, pending: Pending, now: Instant) {
		self.0.retain(|_, (sent, _)| is_fresh(*sent, now));
		self.0.insert(event_id, (now, pending));
	}

	pub fn get(&self, event_id: &str, now: Instant) -> Option<&Pending> {
		self.0.get(event_id).filter(|(sent, _)| is_fresh(*sent, now)).map(|(_, pending)| pending)
	}
}

fn is_fresh(sent: Instant, now: Instant) -> bool {
	now.saturating_duration_since(sent) < PENDING_TTL
}

#[cfg(test)]
mod tests {
	use super::*;

	fn entry(id: &str, answer: &str) -> FaqEntry {
		FaqEntry {
			id: id.into(),
			question: format!("question for {id}"),
			paraphrases: Vec::new(),
			answer: answer.into(),
		}
	}

	fn ranking(pairs: &[(&str, f32)]) -> Vec<Match> {
		pairs.iter().map(|(id, score)| Match { id: (*id).into(), score: *score }).collect()
	}

	fn decide_with(
		entries: &[FaqEntry],
		ranking: &[Match],
		threshold: f32,
		margin: f32,
	) -> Decision {
		decide(ranking, threshold, margin, |id| entries.iter().find(|e| e.id == id))
	}

	fn choose(ids: &[&str]) -> Decision {
		Decision::Choose(ids.iter().map(|id| id.to_string()).collect())
	}

	fn entries() -> Vec<FaqEntry> {
		["install", "update", "ios", "linux"].iter().map(|id| entry(id, "An answer.")).collect()
	}

	#[test]
	fn answers_a_single_confident_match() {
		let ranked = ranking(&[("install", 0.9), ("ios", 0.4)]);
		assert_eq!(
			decide_with(&entries(), &ranked, 0.85, 0.02),
			Decision::Answer("install".into())
		);
		assert_eq!(decide_with(&entries(), &ranked, 0.9, 0.02), Decision::Answer("install".into()));
	}

	#[test]
	fn chooses_between_matches_within_the_margin() {
		let ranked = ranking(&[("install", 1.0), ("update", 0.875), ("ios", 0.75), ("linux", 0.5)]);
		assert_eq!(decide_with(&entries(), &ranked, 0.5, 0.125), choose(&["install", "update"]));
		assert_eq!(
			decide_with(&entries(), &ranked, 0.5, 0.25),
			choose(&["install", "update", "ios"])
		);
		assert_eq!(decide_with(&entries(), &ranked, 0.5, 0.0), Decision::Answer("install".into()));
	}

	#[test]
	fn lists_at_most_five_answerable_entries_in_rank_order() {
		let mut entries: Vec<FaqEntry> =
			["a", "b", "c", "d", "e", "f"].iter().map(|id| entry(id, "An answer.")).collect();
		entries.push(entry("todo", "TODO"));
		let ranked = ranking(&[
			("a", 0.97),
			("todo", 0.96),
			("b", 0.95),
			("c", 0.94),
			("d", 0.93),
			("e", 0.92),
			("f", 0.91),
		]);
		assert_eq!(decide_with(&entries, &ranked, 0.85, 0.1), choose(&["a", "b", "c", "d", "e"]));
	}

	#[test]
	fn best_is_the_answer_or_the_first_listed_entry() {
		assert_eq!(Decision::NoMatch.best(), None);
		assert_eq!(Decision::Answer("install".into()).best(), Some("install"));
		assert_eq!(choose(&["update", "install"]).best(), Some("update"));
	}

	#[test]
	fn candidates_must_clear_the_threshold() {
		let ranked = ranking(&[("install", 0.9), ("update", 0.875)]);
		assert_eq!(
			decide_with(&entries(), &ranked, 0.88, 0.125),
			Decision::Answer("install".into())
		);
	}

	#[test]
	fn below_threshold_is_no_match() {
		let ranked = ranking(&[("install", 0.84), ("update", 0.83)]);
		assert_eq!(decide_with(&entries(), &ranked, 0.85, 0.02), Decision::NoMatch);
	}

	#[test]
	fn placeholder_answers_are_excluded() {
		let entries =
			[entry("ios", "TODO:"), entry("install", "Run the installer."), entry("x", " ")];
		let ranked = ranking(&[("ios", 0.95), ("install", 0.94), ("x", 0.94)]);
		assert_eq!(decide_with(&entries, &ranked, 0.85, 0.02), Decision::Answer("install".into()));
		assert_eq!(decide_with(&entries, &ranked[..1], 0.85, 0.02), Decision::NoMatch);
		let far = ranking(&[("ios", 0.95), ("install", 0.9)]);
		assert_eq!(decide_with(&entries, &far, 0.85, 0.02), Decision::NoMatch);
	}

	#[test]
	fn empty_ranking_and_unknown_entries_are_no_match() {
		assert_eq!(decide_with(&[], &[], 0.0, 0.02), Decision::NoMatch);
		assert_eq!(decide_with(&[], &ranking(&[("gone", 0.99)]), 0.5, 0.02), Decision::NoMatch);
	}

	#[test]
	fn placeholder_answers_are_detected() {
		assert!(is_placeholder(""));
		assert!(is_placeholder("   \n"));
		assert!(is_placeholder("TODO:"));
		assert!(is_placeholder("  todo write this"));
		assert!(is_placeholder("ToDo"));
		assert!(!is_placeholder("Download it from the releases page."));
		assert!(!is_placeholder("Not TODO"));
		assert!(!is_placeholder("Tod"));
		assert!(!is_placeholder("Été"));
	}

	#[test]
	fn question_needs_four_words_and_a_question_mark() {
		assert!(is_question("how do I install it?"));
		assert!(is_question(" how\ndo  I\tinstall? "));
		assert!(!is_question("how do I install"));
		assert!(!is_question("how to install?"));
		assert!(!is_question("   "));
	}

	#[test]
	fn messages_delivered_over_ten_minutes_late_are_stale() {
		let now = 1_000_000_000;
		assert!(!is_stale(now, now));
		assert!(!is_stale(now - STALE_AFTER_MS, now));
		assert!(is_stale(now - STALE_AFTER_MS - 1, now));
		assert!(is_stale(0, now));
		assert!(!is_stale(now + 5_000, now));
	}

	#[test]
	fn question_list_is_a_numbered_markdown_list() {
		assert_eq!(
			question_list(&["How do I update?", "Where is the **changelog**?"]),
			"1. How do I update?\n2. Where is the **changelog**?"
		);
	}

	#[test]
	fn picks_parse_a_number_within_the_list() {
		let ids = vec!["install".to_string(), "update".to_string()];
		assert_eq!(picked(" 2 ", &ids), Some("update"));
		assert_eq!(picked("1", &ids), Some("install"));
		for invalid in ["0", "3", "two", "2.", "", "-1", "1 2"] {
			assert_eq!(picked(invalid, &ids), None, "{invalid:?}");
		}
	}

	fn choices() -> Pending {
		Pending::Choices {
			original: Message {
				room_id: "!support:example.org".into(),
				event_id: "$question".into(),
				sender: "@alice:example.org".into(),
				thread_root: None,
			},
			entry_ids: vec!["install".into(), "update".into()],
		}
	}

	#[test]
	fn lists_and_prompts_are_found_by_their_event_id_repeatedly() {
		let mut pending = PendingMessages::default();
		let now = Instant::now();
		pending.insert("$list".into(), choices(), now);
		pending.insert("$prompt".into(), Pending::Prompt, now);
		assert_eq!(pending.get("$list", now), Some(&choices()));
		assert_eq!(pending.get("$list", now), Some(&choices()));
		assert_eq!(pending.get("$prompt", now), Some(&Pending::Prompt));
		assert_eq!(pending.get("$other", now), None);
	}

	#[test]
	fn pending_messages_are_pruned_after_a_week_on_insert() {
		let mut pending = PendingMessages::default();
		let start = Instant::now();
		pending.insert("$old".into(), choices(), start);
		pending.insert("$recent".into(), Pending::Prompt, start + Duration::from_secs(60));
		pending.insert("$new".into(), Pending::Prompt, start + PENDING_TTL);
		assert!(!pending.0.contains_key("$old"));
		assert!(pending.0.contains_key("$recent"));
		assert!(pending.0.contains_key("$new"));
	}

	#[test]
	fn pending_messages_expire_after_a_week_without_inserts() {
		let mut pending = PendingMessages::default();
		let start = Instant::now();
		pending.insert("$list".into(), choices(), start);
		let almost = start + PENDING_TTL - Duration::from_secs(1);
		assert_eq!(pending.get("$list", almost), Some(&choices()));
		assert_eq!(pending.get("$list", start + PENDING_TTL), None);
	}
}
