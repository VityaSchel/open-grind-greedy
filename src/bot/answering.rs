use crate::faq::FaqEntry;
use crate::matcher::Match;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

pub const RAISED_HAND: &str = "🙋";
const STALE_AFTER_MS: u64 = 10 * 60 * 1000;
const PENDING_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const PENDING_TTL_ROOM_MESSAGES: u64 = 50;
const VARIATION_SELECTOR: char = '\u{fe0f}';
const COMBINING_KEYCAP: char = '\u{20e3}';
const MAX_CHOICES: usize = 5;
const MAX_QUESTIONS: usize = MAX_CHOICES;
const MIN_QUESTION_WORDS: usize = 2;
const MAX_SHORT_QUESTION_WORDS: usize = 3;

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

	fn from_entry_ids(mut entry_ids: Vec<String>) -> Self {
		match entry_ids.len() {
			0 => Decision::NoMatch,
			1 => Decision::Answer(entry_ids.remove(0)),
			_ => Decision::Choose(entry_ids),
		}
	}

	fn into_entry_ids(self) -> Vec<String> {
		match self {
			Decision::NoMatch => Vec::new(),
			Decision::Answer(id) => vec![id],
			Decision::Choose(ids) => ids,
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
	let candidates = ranking
		.iter()
		.take_while(|m| m.score >= threshold && top.score - m.score <= ambiguity_margin)
		.filter(|m| lookup(&m.id).is_some_and(|entry| !is_placeholder(&entry.answer)))
		.map(|m| m.id.clone())
		.take(MAX_CHOICES)
		.collect();
	Decision::from_entry_ids(candidates)
}

pub fn merge(decisions: Vec<Decision>) -> Decision {
	let mut entry_ids = Vec::new();
	for id in decisions.into_iter().flat_map(Decision::into_entry_ids) {
		if !entry_ids.contains(&id) {
			entry_ids.push(id);
		}
	}
	entry_ids.truncate(MAX_CHOICES);
	Decision::from_entry_ids(entry_ids)
}

pub fn questions(text: &str) -> Vec<&str> {
	let parts = split_questions(text);
	if parts.len() > 1 || parts.first().is_some_and(|part| is_short_question(part)) {
		parts
	} else {
		vec![text]
	}
}

fn split_questions(text: &str) -> Vec<&str> {
	split_after_question_marks_ending_a_sentence(text)
		.into_iter()
		.map(str::trim)
		.filter(|part| is_question(part) || !is_short(part))
		.take(MAX_QUESTIONS)
		.collect()
}

fn split_after_question_marks_ending_a_sentence(text: &str) -> Vec<&str> {
	let mut parts = Vec::new();
	let mut start = 0;
	let mut characters = text.char_indices().peekable();
	while let Some((index, character)) = characters.next() {
		let ends_sentence = characters.peek().is_none_or(|(_, next)| next.is_whitespace());
		if character == '?' && ends_sentence {
			let end = index + character.len_utf8();
			parts.push(&text[start..end]);
			start = end;
		}
	}
	parts.push(&text[start..]);
	parts
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Thresholds {
	pub question: f32,
	pub short_question: f32,
}

impl Thresholds {
	pub fn for_question(self, question: &str) -> f32 {
		if is_short_question(question) {
			self.question.max(self.short_question)
		} else {
			self.question
		}
	}
}

pub fn answer_markdown(entry: &FaqEntry) -> String {
	format!("**{}**\n\n{}", entry.question, entry.answer)
}

pub fn is_placeholder(answer: &str) -> bool {
	let answer = answer.trim();
	answer.is_empty() || answer.get(..4).is_some_and(|prefix| prefix.eq_ignore_ascii_case("todo"))
}

pub fn is_question(text: &str) -> bool {
	text.contains('?') && text.split_whitespace().count() >= MIN_QUESTION_WORDS
}

fn is_short(text: &str) -> bool {
	text.split_whitespace().count() <= MAX_SHORT_QUESTION_WORDS
}

fn is_short_question(text: &str) -> bool {
	is_question(text) && is_short(text)
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

pub fn keycap(number: usize) -> String {
	format!("{number}{VARIATION_SELECTOR}{COMBINING_KEYCAP}")
}

fn is_same_key(reacted: &str, expected: &str) -> bool {
	reacted.replace(VARIATION_SELECTOR, "") == expected.replace(VARIATION_SELECTOR, "")
}

#[derive(Clone, Debug, PartialEq)]
pub enum Pending {
	Choices { original: Message, entry_ids: Vec<String>, reaction_picks: HashSet<usize> },
	Prompt,
	Offer { message: Message, question: String },
}

#[derive(Debug, PartialEq)]
pub enum Reaction {
	Pick { original: Message, entry_id: String },
	Summon { message: Message, question: String },
}

struct Tracked {
	pending: Pending,
	room_id: String,
	sent: Instant,
	room_messages_since: u64,
}

impl Tracked {
	fn is_live(&self, now: Instant) -> bool {
		now.saturating_duration_since(self.sent) < PENDING_TTL
			&& self.room_messages_since < PENDING_TTL_ROOM_MESSAGES
	}
}

#[derive(Default)]
pub struct PendingMessages(HashMap<String, Tracked>);

impl PendingMessages {
	pub fn insert(&mut self, room_id: &str, event_id: String, pending: Pending, now: Instant) {
		self.0.retain(|_, tracked| tracked.is_live(now));
		let tracked =
			Tracked { pending, room_id: room_id.to_owned(), sent: now, room_messages_since: 0 };
		self.0.insert(event_id, tracked);
	}

	pub fn count_room_message(&mut self, room_id: &str, event_id: &str) {
		let others_in_room = self
			.0
			.iter_mut()
			.filter(|(tracked_id, tracked)| *tracked_id != event_id && tracked.room_id == room_id);
		for (_, tracked) in others_in_room {
			tracked.room_messages_since += 1;
		}
	}

	pub fn get(&self, event_id: &str, now: Instant) -> Option<&Pending> {
		self.0.get(event_id).filter(|tracked| tracked.is_live(now)).map(|tracked| &tracked.pending)
	}

	pub fn react(&mut self, event_id: &str, key: &str, now: Instant) -> Option<Reaction> {
		let tracked = self.0.get_mut(event_id).filter(|tracked| tracked.is_live(now))?;
		match &mut tracked.pending {
			Pending::Choices { original, entry_ids, reaction_picks } => {
				let (index, entry_id) = entry_ids
					.iter()
					.enumerate()
					.find(|(index, _)| is_same_key(key, &keycap(index + 1)))?;
				reaction_picks.insert(index).then(|| Reaction::Pick {
					original: original.clone(),
					entry_id: entry_id.clone(),
				})
			}
			Pending::Offer { message, question } if is_same_key(key, RAISED_HAND) => {
				let reaction =
					Reaction::Summon { message: message.clone(), question: question.clone() };
				self.0.remove(event_id);
				Some(reaction)
			}
			Pending::Offer { .. } | Pending::Prompt => None,
		}
	}
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

	fn answer(id: &str) -> Decision {
		Decision::Answer(id.into())
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
	fn merging_keeps_question_order_without_duplicates() {
		let decisions = vec![
			choose(&["update", "install"]),
			Decision::NoMatch,
			answer("install"),
			answer("ios"),
		];
		assert_eq!(merge(decisions), choose(&["update", "install", "ios"]));
	}

	#[test]
	fn merging_to_one_entry_answers_it() {
		assert_eq!(merge(vec![answer("install"), answer("install")]), answer("install"));
		assert_eq!(merge(vec![Decision::NoMatch, answer("install")]), answer("install"));
	}

	#[test]
	fn merging_nothing_is_no_match() {
		assert_eq!(merge(Vec::new()), Decision::NoMatch);
		assert_eq!(merge(vec![Decision::NoMatch, Decision::NoMatch]), Decision::NoMatch);
	}

	#[test]
	fn merging_lists_at_most_five_entries() {
		let decisions = vec![choose(&["a", "b", "c"]), answer("a"), choose(&["d", "e", "f"])];
		assert_eq!(merge(decisions), choose(&["a", "b", "c", "d", "e"]));
	}

	#[test]
	fn questions_end_at_each_question_mark() {
		assert_eq!(
			split_questions("How do I install it? Is there an iOS version?"),
			["How do I install it?", "Is there an iOS version?"]
		);
		assert_eq!(
			split_questions("How do I install it? Asking for a friend"),
			["How do I install it?", "Asking for a friend"]
		);
		assert_eq!(split_questions("How do I install it?"), ["How do I install it?"]);
		assert_eq!(split_questions("How do I install it"), ["How do I install it"]);
	}

	#[test]
	fn question_marks_inside_urls_do_not_end_a_question() {
		assert_eq!(
			split_questions("Why does this fail? https://example.org/cascade?page=2 returns 404"),
			["Why does this fail?", "https://example.org/cascade?page=2 returns 404"]
		);
		assert_eq!(
			split_questions("Is https://example.org/a?b the right link? It 404s for me"),
			["Is https://example.org/a?b the right link?", "It 404s for me"]
		);
	}

	#[test]
	fn question_parts_are_trimmed_and_need_two_words() {
		assert_eq!(split_questions("?? How do I install it?"), ["How do I install it?"]);
		assert_eq!(
			split_questions("  How do I install?\n Is there an iOS version ?  :) "),
			["How do I install?", "Is there an iOS version ?"]
		);
		assert_eq!(split_questions("How do I sign in? google"), ["How do I sign in?"]);
		assert_eq!(split_questions("Why? Is it free?"), ["Is it free?"]);
		assert!(split_questions(" ?! ").is_empty());
	}

	#[test]
	fn short_parts_are_questions_only_with_a_question_mark() {
		assert_eq!(
			split_questions("Who is Greedy? Is Open Grind iOS version available?"),
			["Who is Greedy?", "Is Open Grind iOS version available?"]
		);
		assert_eq!(
			split_questions("Anyone here? Can you help? How do I report a bug?"),
			["Anyone here?", "Can you help?", "How do I report a bug?"]
		);
		assert_eq!(split_questions("Who is Greedy? thanks a lot"), ["Who is Greedy?"]);
		assert_eq!(
			split_questions("Who is Greedy? thanks a lot everyone"),
			["Who is Greedy?", "thanks a lot everyone"]
		);
	}

	#[test]
	fn at_most_five_questions_are_split_off() {
		let text = "What about this one? ".repeat(6);
		assert_eq!(split_questions(&text), ["What about this one?"; 5]);
		assert_eq!(split_questions(&"Is it free? ".repeat(6)), ["Is it free?"; 5]);
	}

	#[test]
	fn a_message_with_one_question_part_or_none_is_one_question() {
		assert_eq!(questions("How do I install it? thanks"), ["How do I install it? thanks"]);
		assert_eq!(questions("Who is Greedy? lol"), ["Who is Greedy?"]);
		assert_eq!(questions("install plugins? lol ok"), ["install plugins?"]);
		assert_eq!(questions("Why? ok"), ["Why? ok"]);
		assert_eq!(
			questions("Who is Greedy? How do I install it?"),
			["Who is Greedy?", "How do I install it?"]
		);
	}

	#[test]
	fn short_questions_need_the_stricter_threshold() {
		let thresholds = |question| Thresholds { question, short_question: 0.97 };
		for short in ["install it?", "Who is Greedy?"] {
			assert_eq!(thresholds(0.88).for_question(short), 0.97, "{short}");
			assert_eq!(thresholds(0.99).for_question(short), 0.99, "{short}");
		}
		for other in ["Is Open Grind free?", "install?", "ios app", "donate"] {
			assert_eq!(thresholds(0.88).for_question(other), 0.88, "{other}");
		}
	}

	#[test]
	fn answer_markdown_puts_the_bold_question_above_the_answer() {
		let mut install = entry("install", "Download it from **the releases page**.");
		install.question = "How do I install it?".into();
		assert_eq!(
			answer_markdown(&install),
			"**How do I install it?**\n\nDownload it from **the releases page**."
		);
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
	fn question_needs_two_words_and_a_question_mark() {
		assert!(is_question("how do I install it?"));
		assert!(is_question(" how\ndo  I\tinstall? "));
		assert!(is_question("how to install?"));
		assert!(is_question("install it?"));
		assert!(!is_question("install?"));
		assert!(!is_question("how do I install"));
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

	const ROOM: &str = "!support:example.org";
	const OFFERED_QUESTION: &str = "can I install plugins too?";

	fn question_message() -> Message {
		Message {
			room_id: ROOM.into(),
			event_id: "$question".into(),
			sender: "@alice:example.org".into(),
			thread_root: None,
		}
	}

	fn choices() -> Pending {
		Pending::Choices {
			original: question_message(),
			entry_ids: vec!["install".into(), "update".into()],
			reaction_picks: HashSet::new(),
		}
	}

	fn offer() -> Pending {
		Pending::Offer { message: question_message(), question: OFFERED_QUESTION.into() }
	}

	fn pick(entry_id: &str) -> Option<Reaction> {
		Some(Reaction::Pick { original: question_message(), entry_id: entry_id.into() })
	}

	#[test]
	fn keycaps_are_the_digit_with_the_emoji_keycap() {
		assert_eq!(keycap(1), "1\u{fe0f}\u{20e3}");
		assert_eq!(keycap(5), "5\u{fe0f}\u{20e3}");
	}

	#[test]
	fn reaction_keys_match_with_or_without_the_variation_selector() {
		assert!(is_same_key("1\u{fe0f}\u{20e3}", &keycap(1)));
		assert!(is_same_key("1\u{20e3}", &keycap(1)));
		assert!(is_same_key("🙋\u{fe0f}", RAISED_HAND));
		assert!(is_same_key(RAISED_HAND, RAISED_HAND));
		assert!(!is_same_key("2\u{20e3}", &keycap(1)));
		assert!(!is_same_key("1", &keycap(1)));
		assert!(!is_same_key("🙋\u{200d}\u{2642}\u{fe0f}", RAISED_HAND));
	}

	#[test]
	fn lists_and_prompts_are_found_by_their_event_id_repeatedly() {
		let mut pending = PendingMessages::default();
		let now = Instant::now();
		pending.insert(ROOM, "$list".into(), choices(), now);
		pending.insert(ROOM, "$prompt".into(), Pending::Prompt, now);
		assert_eq!(pending.get("$list", now), Some(&choices()));
		assert_eq!(pending.get("$list", now), Some(&choices()));
		assert_eq!(pending.get("$prompt", now), Some(&Pending::Prompt));
		assert_eq!(pending.get("$other", now), None);
	}

	#[test]
	fn each_digit_reaction_on_a_list_picks_its_entry_once() {
		let mut pending = PendingMessages::default();
		let now = Instant::now();
		pending.insert(ROOM, "$list".into(), choices(), now);
		assert_eq!(pending.react("$list", "2\u{20e3}", now), pick("update"));
		assert_eq!(pending.react("$list", &keycap(2), now), None);
		assert_eq!(pending.react("$list", &keycap(1), now), pick("install"));
		for ignored in [keycap(1), keycap(3), "0\u{fe0f}\u{20e3}".into(), RAISED_HAND.into()] {
			assert_eq!(pending.react("$list", &ignored, now), None, "{ignored}");
		}
		assert_eq!(pending.react("$other", &keycap(1), now), None);
		assert!(pending.get("$list", now).is_some());
	}

	#[test]
	fn raised_hand_on_an_offer_summons_it_once() {
		let mut pending = PendingMessages::default();
		let now = Instant::now();
		pending.insert(ROOM, "$question".into(), offer(), now);
		pending.insert(ROOM, "$prompt".into(), Pending::Prompt, now);
		assert_eq!(pending.react("$question", "👍", now), None);
		assert_eq!(pending.react("$question", &keycap(1), now), None);
		assert_eq!(pending.react("$prompt", RAISED_HAND, now), None);
		let summon =
			Reaction::Summon { message: question_message(), question: OFFERED_QUESTION.into() };
		assert_eq!(pending.react("$question", RAISED_HAND, now), Some(summon));
		assert_eq!(pending.react("$question", RAISED_HAND, now), None);
		assert_eq!(pending.get("$question", now), None);
	}

	#[test]
	fn pending_messages_expire_after_fifty_further_messages_in_their_room() {
		let mut pending = PendingMessages::default();
		let now = Instant::now();
		pending.insert(ROOM, "$list".into(), choices(), now);
		pending.count_room_message(ROOM, "$list");
		pending.insert(ROOM, "$question".into(), offer(), now);
		for _ in 1..PENDING_TTL_ROOM_MESSAGES {
			pending.count_room_message(ROOM, "$chatter");
			pending.count_room_message("!other:example.org", "$elsewhere");
		}
		assert_eq!(pending.get("$list", now), Some(&choices()));
		assert_eq!(pending.get("$question", now), Some(&offer()));
		pending.count_room_message(ROOM, "$chatter");
		assert_eq!(pending.get("$list", now), None);
		assert_eq!(pending.react("$list", &keycap(1), now), None);
		assert_eq!(pending.react("$question", RAISED_HAND, now), None);
	}

	#[test]
	fn pending_messages_expire_after_a_day() {
		let mut pending = PendingMessages::default();
		let start = Instant::now();
		pending.insert(ROOM, "$list".into(), choices(), start);
		pending.insert(ROOM, "$question".into(), offer(), start);
		let almost = start + PENDING_TTL - Duration::from_secs(1);
		assert_eq!(pending.get("$list", almost), Some(&choices()));
		assert_eq!(pending.get("$list", start + PENDING_TTL), None);
		assert_eq!(pending.react("$list", &keycap(1), start + PENDING_TTL), None);
		assert_eq!(pending.react("$question", RAISED_HAND, start + PENDING_TTL), None);
	}

	#[test]
	fn expired_pending_messages_are_pruned_on_insert() {
		let mut pending = PendingMessages::default();
		let start = Instant::now();
		let later = start + Duration::from_secs(60);
		pending.insert(ROOM, "$old".into(), choices(), start);
		pending.insert(ROOM, "$busy".into(), Pending::Prompt, later);
		pending.insert("!quiet:example.org", "$quiet".into(), Pending::Prompt, later);
		for _ in 0..PENDING_TTL_ROOM_MESSAGES {
			pending.count_room_message(ROOM, "$chatter");
		}
		pending.insert(ROOM, "$new".into(), Pending::Prompt, start + PENDING_TTL);
		assert!(!pending.0.contains_key("$old"));
		assert!(!pending.0.contains_key("$busy"));
		assert!(pending.0.contains_key("$quiet"));
		assert!(pending.0.contains_key("$new"));
	}
}
