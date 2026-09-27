use std::ops::Range;

const MATRIX_TO_PREFIX: &str = "matrix.to/#/";
const HTML_REPLY_FALLBACK_END: &str = "</mx-reply>";

pub fn mentions_user(
	mentioned_user_ids: &[String],
	formatted_html: Option<&str>,
	body_without_reply_fallback: &str,
	user_id: &str,
) -> bool {
	mentioned_user_ids.iter().any(|id| id == user_id)
		|| formatted_html
			.is_some_and(|html| links_to_user(strip_html_reply_fallback(html), user_id))
		|| body_without_reply_fallback.contains(user_id)
}

pub fn summon_question(
	body_without_reply_fallback: &str,
	formatted_html: Option<&str>,
	user_id: &str,
) -> String {
	let spans = mention_spans(body_without_reply_fallback, formatted_html, user_id);
	remove_spans(body_without_reply_fallback, spans)
		.split_whitespace()
		.collect::<Vec<_>>()
		.join(" ")
}

fn strip_html_reply_fallback(html: &str) -> &str {
	match html.find(HTML_REPLY_FALLBACK_END) {
		Some(end) if html.trim_start().starts_with("<mx-reply>") => {
			&html[end + HTML_REPLY_FALLBACK_END.len()..]
		}
		_ => html,
	}
}

fn links_to_user(html: &str, user_id: &str) -> bool {
	html.match_indices(MATRIX_TO_PREFIX).any(|(index, _)| {
		let rest = &html[index + MATRIX_TO_PREFIX.len()..];
		let end = rest
			.find(|c: char| matches!(c, '"' | '\'' | '?' | '<' | '>' | '/') || c.is_whitespace())
			.unwrap_or(rest.len());
		urlencoding::decode(&rest[..end]).is_ok_and(|target| target == user_id)
	})
}

fn mention_spans(body: &str, formatted_html: Option<&str>, user_id: &str) -> Vec<Range<usize>> {
	let raw_id_spans: Vec<_> =
		body.match_indices(user_id).map(|(start, _)| start..start + user_id.len()).collect();
	let client_writes_pills_as_raw_ids = !raw_id_spans.is_empty();
	if client_writes_pills_as_raw_ids {
		return raw_id_spans;
	}
	formatted_html
		.map(|html| pill_display_name_spans(body, strip_html_reply_fallback(html), user_id))
		.unwrap_or_default()
}

fn pill_display_name_spans(body: &str, html: &str, user_id: &str) -> Vec<Range<usize>> {
	let (text, pills) = render_html(html, user_id);
	pills
		.into_iter()
		.filter_map(|pill| {
			let name = &text[pill.clone()];
			let occurrence = text.match_indices(name).position(|(start, _)| start == pill.start)?;
			body.match_indices(name).nth(occurrence).map(|(start, _)| start..start + name.len())
		})
		.collect()
}

fn render_html(html: &str, user_id: &str) -> (String, Vec<Range<usize>>) {
	let mut text = String::with_capacity(html.len());
	let mut pills = Vec::new();
	let mut open_pill = None;
	let mut rest = html;
	while let Some(tag_start) = rest.find('<') {
		text.push_str(&unescape_html(&rest[..tag_start]));
		let tag = &rest[tag_start..];
		let tag = tag.find('>').map_or(tag, |end| &tag[..=end]);
		if tag.starts_with("<a ") {
			open_pill = links_to_user(tag, user_id).then_some(text.len());
		} else if tag == "</a>"
			&& let Some(start) = open_pill.take()
		{
			let inner = &text[start..];
			let start = start + inner.len() - inner.trim_start().len();
			let end = start + inner.trim().len();
			if start < end {
				pills.push(start..end);
			}
		}
		rest = &rest[tag_start + tag.len()..];
	}
	text.push_str(&unescape_html(rest));
	(text, pills)
}

fn remove_spans(text: &str, mut spans: Vec<Range<usize>>) -> String {
	spans.sort_by_key(|span| span.start);
	let mut kept = String::with_capacity(text.len());
	let mut cursor = 0;
	for span in spans {
		if span.start < cursor {
			continue;
		}
		kept.push_str(&text[cursor..span.start]);
		kept.push(' ');
		cursor = span.end;
		if text[cursor..].starts_with([':', ',']) {
			cursor += 1;
		}
	}
	kept.push_str(&text[cursor..]);
	kept
}

fn unescape_html(text: &str) -> String {
	text.replace("&lt;", "<")
		.replace("&gt;", ">")
		.replace("&quot;", "\"")
		.replace("&#39;", "'")
		.replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::bot::matrix::strip_reply_fallback;

	const BOT: &str = "@bot:example.org";

	fn ids(ids: &[&str]) -> Vec<String> {
		ids.iter().map(|id| id.to_string()).collect()
	}

	#[test]
	fn mention_via_m_mentions() {
		assert!(mentions_user(&ids(&[BOT]), None, "hello", BOT));
		assert!(!mentions_user(&ids(&["@alice:example.org"]), None, "hello", BOT));
	}

	#[test]
	fn mention_via_raw_matrix_to_link() {
		let html =
			r#"<a href="https://matrix.to/#/@bot:example.org">Greedy</a>: how do I install?"#;
		assert!(mentions_user(&[], Some(html), "Greedy: how do I install?", BOT));
	}

	#[test]
	fn mention_via_percent_encoded_matrix_to_link() {
		let upper = r#"<a href="https://matrix.to/#/%40bot%3Aexample.org">Greedy</a> help"#;
		let lower =
			r#"<a href="https://matrix.to/#/%40bot%3aexample.org?via=example.org">Greedy</a> help"#;
		assert!(mentions_user(&[], Some(upper), "Greedy help", BOT));
		assert!(mentions_user(&[], Some(lower), "Greedy help", BOT));
	}

	#[test]
	fn link_to_another_user_is_not_a_mention() {
		let html = r#"<a href="https://matrix.to/#/@bot:example.org.evil">x</a> <a href="https://matrix.to/#/@botty:example.org">y</a>"#;
		assert!(!mentions_user(&[], Some(html), "x y", BOT));
	}

	#[test]
	fn mention_via_raw_id_in_body() {
		assert!(mentions_user(&[], None, "@bot:example.org how do I install?", BOT));
	}

	#[test]
	fn display_name_alone_is_not_a_mention() {
		assert!(!mentions_user(&[], Some("<b>bot</b> help"), "bot help", BOT));
	}

	#[test]
	fn id_only_inside_reply_fallback_is_not_a_mention() {
		let body = strip_reply_fallback(
			"> <@bot:example.org> Download it from the releases page.\n\nthanks!",
		);
		let html = r#"<mx-reply><blockquote><a href="https://matrix.to/#/!room:example.org/$event">In reply to</a> <a href="https://matrix.to/#/@bot:example.org">@bot:example.org</a><br>Download it</blockquote></mx-reply>thanks!"#;
		assert_eq!(body, "thanks!");
		assert!(!mentions_user(&[], Some(html), body, BOT));
	}

	#[test]
	fn summon_question_drops_bot_id() {
		assert_eq!(
			summon_question("@bot:example.org how do I install?", None, BOT),
			"how do I install?"
		);
		assert_eq!(
			summon_question("@bot:example.org: how do I install?", None, BOT),
			"how do I install?"
		);
		assert_eq!(summon_question("@bot:example.org", None, BOT), "");
	}

	#[test]
	fn summon_question_drops_element_pill_display_names() {
		let html =
			r#"<a href="https://matrix.to/#/@bot:example.org">Greedy</a>: how do I install it?"#;
		assert_eq!(
			summon_question("Greedy: how do I install it?", Some(html), BOT),
			"how do I install it?"
		);
		let encoded = r#"is there an iOS version, <a href="https://matrix.to/#/%40bot%3Aexample.org">Tom &amp; Jerry</a>?"#;
		assert_eq!(
			summon_question("is there an iOS version, Tom & Jerry?", Some(encoded), BOT),
			"is there an iOS version, ?"
		);
		assert_eq!(
			summon_question(
				"Greedy",
				Some(r#"<a href="https://matrix.to/#/@bot:example.org">Greedy</a>"#),
				BOT
			),
			""
		);
	}

	#[test]
	fn summon_question_keeps_other_pills_and_words_matching_the_name() {
		let html = r#"<a href="https://matrix.to/#/@alice:example.org">Alice</a>: <a href="https://matrix.to/#/@bot:example.org">Greedy</a>: is greedy mode on?"#;
		assert_eq!(
			summon_question("Alice: Greedy: is greedy mode on?", Some(html), BOT),
			"Alice: is greedy mode on?"
		);
		let raw =
			r#"<a href="https://matrix.to/#/@bot:example.org">bot</a> how does the bot work?"#;
		assert_eq!(
			summon_question("@bot:example.org how does the bot work?", Some(raw), BOT),
			"how does the bot work?"
		);
	}

	#[test]
	fn summon_question_removes_only_the_pill_occurrence() {
		let pill =
			|name: &str| format!(r#"<a href="https://matrix.to/#/@bot:example.org">{name}</a>"#);
		assert_eq!(
			summon_question(
				"Does Open Grind run on iOS? Open Grind",
				Some(&format!("Does Open Grind run on iOS? {}", pill("Open Grind"))),
				BOT
			),
			"Does Open Grind run on iOS?"
		);
		assert_eq!(
			summon_question(
				"why does Botanist crash? Bot",
				Some(&format!("why does Botanist crash? {}", pill("Bot"))),
				BOT
			),
			"why does Botanist crash?"
		);
		assert_eq!(
			summon_question(
				"Open Grind: does Open Grind run on iOS?",
				Some(&format!("{}: does Open Grind run on iOS?", pill("Open Grind"))),
				BOT
			),
			"does Open Grind run on iOS?"
		);
	}

	#[test]
	fn summon_question_removes_every_pill_and_ignores_the_reply_fallback() {
		let html = r#"<mx-reply><blockquote><a href="https://matrix.to/#/@bot:example.org">Greedy</a> Greedy</blockquote></mx-reply><a href="https://matrix.to/#/@bot:example.org">Greedy</a>, <strong>Greedy</strong> mode <a href="https://matrix.to/#/@bot:example.org"> Greedy </a>?"#;
		assert_eq!(
			summon_question("Greedy, **Greedy** mode Greedy?", Some(html), BOT),
			"**Greedy** mode ?"
		);
	}

	#[test]
	fn summon_question_keeps_text_when_the_pill_is_not_in_the_body() {
		let html = r#"<a href="https://matrix.to/#/@bot:example.org">Greedy</a> how do I install?"#;
		assert_eq!(summon_question("how do I install?", Some(html), BOT), "how do I install?");
	}
}
