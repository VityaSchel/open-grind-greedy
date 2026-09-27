use super::config::Config;
use anyhow::{Context, Result};
use pulldown_cmark::{Event as Markdown, Parser};
use reqwest::{Client, RequestBuilder};
use serde::Deserialize;
use serde::de::{DeserializeOwned, IgnoredAny};
use serde_json::value::RawValue;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{error, info, warn};
use urlencoding::encode;

const HTML_FORMAT: &str = "org.matrix.custom.html";
const EXCERPT_LIMIT: usize = 200;
const JOIN_RETRY_DELAYS_SECS: [u64; 4] = [0, 2, 10, 60];
const UNLIMITED_POWER_LEVEL: i64 = i64::MAX;

#[derive(Deserialize)]
pub struct Transaction {
	#[serde(default)]
	pub events: Vec<Box<RawValue>>,
}

#[derive(Deserialize, Debug)]
pub struct Event {
	#[serde(rename = "type")]
	pub event_type: String,
	pub event_id: String,
	#[serde(default)]
	pub room_id: String,
	pub sender: String,
	pub state_key: Option<String>,
	#[serde(default)]
	pub origin_server_ts: u64,
	#[serde(default)]
	pub content: Content,
	#[serde(default)]
	pub unsigned: Unsigned,
}

#[derive(Deserialize, Debug, Default)]
pub struct Content {
	pub msgtype: Option<String>,
	pub body: Option<String>,
	pub format: Option<String>,
	pub formatted_body: Option<String>,
	pub membership: Option<String>,
	#[serde(rename = "m.relates_to")]
	pub relates_to: Option<RelatesTo>,
	#[serde(rename = "m.mentions")]
	pub mentions: Option<Mentions>,
	#[serde(rename = "m.new_content")]
	pub new_content: Option<Box<Content>>,
}

#[derive(Deserialize, Debug)]
pub struct RelatesTo {
	pub rel_type: Option<String>,
	pub event_id: Option<String>,
	#[serde(default)]
	pub is_falling_back: bool,
	#[serde(rename = "m.in_reply_to")]
	pub in_reply_to: Option<InReplyTo>,
}

#[derive(Deserialize, Debug)]
pub struct InReplyTo {
	pub event_id: String,
}

#[derive(Deserialize, Debug)]
pub struct Mentions {
	#[serde(default)]
	pub user_ids: Vec<String>,
}

#[derive(Deserialize, Debug, Default)]
pub struct Unsigned {
	#[serde(rename = "m.relations")]
	pub relations: Option<Relations>,
}

#[derive(Deserialize, Debug)]
pub struct Relations {
	#[serde(rename = "m.replace")]
	pub replace: Option<Edit>,
}

#[derive(Deserialize, Debug)]
pub struct Edit {
	#[serde(default)]
	pub sender: String,
	#[serde(default)]
	pub content: Content,
}

impl Event {
	pub fn latest_text(&self) -> Option<&str> {
		if self.event_type != "m.room.message" {
			return None;
		}
		self.bundled_edit_text().or_else(|| self.content.is_text().then(|| self.content.text()))
	}

	fn bundled_edit_text(&self) -> Option<&str> {
		let edit = self.unsigned.relations.as_ref()?.replace.as_ref()?;
		let relation = edit.content.relates_to.as_ref()?;
		let new_content = edit.content.new_content.as_deref()?;
		let replaces_this = relation.rel_type.as_deref() == Some("m.replace")
			&& relation.event_id.as_deref() == Some(self.event_id.as_str());
		if edit.sender != self.sender || !replaces_this || !new_content.is_text() {
			return None;
		}
		new_content.body.as_deref()
	}
}

impl Content {
	pub fn is_text(&self) -> bool {
		self.msgtype.as_deref() == Some("m.text")
	}

	pub fn is_edit(&self) -> bool {
		self.rel_type() == Some("m.replace")
	}

	pub fn reply_target(&self) -> Option<&str> {
		let relation = self.relates_to.as_ref()?;
		if relation.in_reply_to_only_serves_unthreaded_clients() {
			return None;
		}
		relation.in_reply_to.as_ref().map(|reply| reply.event_id.as_str())
	}

	pub fn thread_root(&self) -> Option<&str> {
		if self.rel_type() != Some("m.thread") {
			return None;
		}
		self.relates_to.as_ref()?.event_id.as_deref()
	}

	pub fn text(&self) -> &str {
		let body = self.body.as_deref().unwrap_or_default();
		match &self.relates_to {
			Some(relation) if relation.in_reply_to.is_some() => strip_reply_fallback(body),
			_ => body,
		}
	}

	pub fn html(&self) -> Option<&str> {
		self.formatted_body.as_deref().filter(|_| self.format.as_deref() == Some(HTML_FORMAT))
	}

	pub fn mentioned_user_ids(&self) -> &[String] {
		self.mentions.as_ref().map_or(&[], |mentions| &mentions.user_ids)
	}

	fn rel_type(&self) -> Option<&str> {
		self.relates_to.as_ref()?.rel_type.as_deref()
	}
}

impl RelatesTo {
	fn in_reply_to_only_serves_unthreaded_clients(&self) -> bool {
		self.rel_type.as_deref() == Some("m.thread") && self.is_falling_back
	}
}

pub fn strip_reply_fallback(body: &str) -> &str {
	let mut rest = body;
	let mut quoted = false;
	while rest.starts_with('>') {
		quoted = true;
		rest = rest.split_once('\n').map_or("", |(_, tail)| tail);
	}
	if quoted {
		rest = rest.strip_prefix('\n').or_else(|| rest.strip_prefix("\r\n")).unwrap_or(rest);
	}
	rest
}

pub fn notice(markdown: &str, reply_to: &str, thread_root: Option<&str>, mention: &str) -> Value {
	let in_reply_to = json!({ "event_id": reply_to });
	let relates_to = match thread_root {
		Some(root) => json!({
			"rel_type": "m.thread",
			"event_id": root,
			"is_falling_back": false,
			"m.in_reply_to": in_reply_to,
		}),
		None => json!({ "m.in_reply_to": in_reply_to }),
	};
	json!({
		"msgtype": "m.notice",
		"body": markdown,
		"format": HTML_FORMAT,
		"formatted_body": render_markdown(markdown),
		"m.relates_to": relates_to,
		"m.mentions": { "user_ids": [mention] },
	})
}

pub fn reaction(event_id: &str, key: &str) -> Value {
	json!({ "m.relates_to": { "rel_type": "m.annotation", "event_id": event_id, "key": key } })
}

fn render_markdown(markdown: &str) -> String {
	let events = Parser::new(markdown).map(|event| match event {
		Markdown::SoftBreak => Markdown::HardBreak,
		event => event,
	});
	let mut html = String::new();
	pulldown_cmark::html::push_html(&mut html, events);
	html
}

fn user_power_level(power_levels: &Value, user_id: &str) -> Option<i64> {
	let level = power_levels["users"].get(user_id).or_else(|| power_levels.get("users_default"));
	level.map_or(Some(0), level_from_int_or_pre_room_v10_string)
}

fn level_from_int_or_pre_room_v10_string(level: &Value) -> Option<i64> {
	level.as_i64().or_else(|| level.as_str()?.parse().ok())
}

fn room_v12_create_event_id(room_id: &str) -> Option<String> {
	let hash = room_id.strip_prefix('!').filter(|hash| !hash.contains(':'))?;
	Some(format!("${hash}"))
}

fn is_creator(create: &Value, user_id: &str) -> bool {
	create["sender"] == user_id
		|| create["content"]["additional_creators"]
			.as_array()
			.is_some_and(|creators| creators.iter().any(|creator| creator == user_id))
}

pub fn unix_millis() -> u64 {
	SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_millis() as u64)
}

fn next_txn_id() -> String {
	static STARTED: LazyLock<u64> = LazyLock::new(unix_millis);
	static SENT: AtomicU64 = AtomicU64::new(0);
	format!("{}.{}", *STARTED, SENT.fetch_add(1, Ordering::Relaxed))
}

#[derive(Clone)]
pub struct Matrix {
	http: Client,
	config: Arc<Config>,
}

impl Matrix {
	pub fn new(http: Client, config: Arc<Config>) -> Self {
		Self { http, config }
	}

	pub async fn send(&self, room_id: &str, event_type: &str, content: &Value) -> Result<String> {
		#[derive(Deserialize)]
		struct Sent {
			event_id: String,
		}
		let path = format!(
			"rooms/{}/send/{}/{}",
			encode(room_id),
			encode(event_type),
			encode(&next_txn_id())
		);
		let sent: Sent = self
			.request(self.http.put(self.url(&path)).json(content))
			.await
			.with_context(|| format!("sending {event_type} to {room_id}"))?;
		Ok(sent.event_id)
	}

	pub async fn event<T: DeserializeOwned>(&self, room_id: &str, event_id: &str) -> Result<T> {
		let path = format!("rooms/{}/event/{}", encode(room_id), encode(event_id));
		self.request(self.http.get(self.url(&path)))
			.await
			.with_context(|| format!("fetching {event_id:?} in {room_id}"))
	}

	pub async fn power_level(&self, room_id: &str, user_id: &str) -> Result<i64> {
		if let Some(create_event_id) = room_v12_create_event_id(room_id) {
			let create: Value = self.event(room_id, &create_event_id).await?;
			if is_creator(&create, user_id) {
				return Ok(UNLIMITED_POWER_LEVEL);
			}
		}
		let path = format!("rooms/{}/state/m.room.power_levels/", encode(room_id));
		let power_levels: Value = self
			.request(self.http.get(self.url(&path)))
			.await
			.with_context(|| format!("fetching power levels in {room_id}"))?;
		user_power_level(&power_levels, user_id)
			.with_context(|| format!("invalid power level for {user_id} in {room_id}"))
	}

	pub async fn join(&self, room_id: &str) -> Result<()> {
		let path = format!("join/{}", encode(room_id));
		let _: IgnoredAny = self
			.request(self.http.post(self.url(&path)).json(&json!({})))
			.await
			.with_context(|| format!("joining {room_id}"))?;
		Ok(())
	}

	pub fn spawn_join(&self, room_id: String) {
		let matrix = self.clone();
		tokio::spawn(async move {
			for delay in JOIN_RETRY_DELAYS_SECS.map(Duration::from_secs) {
				tokio::time::sleep(delay).await;
				match matrix.join(&room_id).await {
					Ok(()) => {
						info!(room = %room_id, "joined");
						return;
					}
					Err(e) => warn!("{e:#}"),
				}
			}
			error!(
				room = %room_id,
				"giving up joining; inviting {} again retries",
				matrix.config.app_service_user
			);
		});
	}

	async fn request<T: DeserializeOwned>(&self, request: RequestBuilder) -> Result<T> {
		let response = request
			.bearer_auth(&self.config.app_service_token)
			.send()
			.await
			.map_err(reqwest::Error::without_url)
			.context("request failed")?;
		let status = response.status();
		let body = response.bytes().await.map_err(reqwest::Error::without_url)?;
		anyhow::ensure!(
			status.is_success(),
			"{} {}",
			status.as_u16(),
			excerpt(&String::from_utf8_lossy(&body))
		);
		serde_json::from_slice(&body).context("unexpected response")
	}

	fn url(&self, path: &str) -> String {
		format!(
			"{}/_matrix/client/v3/{path}?user_id={}",
			self.config.homeserver_url,
			encode(&self.config.app_service_user)
		)
	}
}

fn excerpt(text: &str) -> String {
	let mut chars = text.chars().map(|c| if c.is_control() { ' ' } else { c });
	let mut out: String = chars.by_ref().take(EXCERPT_LIMIT).collect();
	if chars.next().is_some() {
		out.push('…');
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	fn content(value: Value) -> Content {
		serde_json::from_value(value).unwrap()
	}

	fn event(value: Value) -> Event {
		serde_json::from_value(value).unwrap()
	}

	#[test]
	fn strips_single_line_fallback() {
		assert_eq!(
			strip_reply_fallback("> <@alice:example.org> how do I install?\n\nthis one"),
			"this one"
		);
	}

	#[test]
	fn strips_multi_line_fallback_and_keeps_later_lines() {
		let body = "> <@alice:example.org> first line\n> second line\n> third\n\nmy reply\n> my own quote\nend";
		assert_eq!(strip_reply_fallback(body), "my reply\n> my own quote\nend");
	}

	#[test]
	fn strips_only_one_blank_line() {
		assert_eq!(strip_reply_fallback("> <@a:example.org> q\n\n\nreply"), "\nreply");
	}

	#[test]
	fn body_without_fallback_is_unchanged() {
		assert_eq!(strip_reply_fallback("plain message"), "plain message");
		assert_eq!(strip_reply_fallback("not > a quote"), "not > a quote");
		assert_eq!(strip_reply_fallback(""), "");
	}

	#[test]
	fn body_that_is_only_a_quote_becomes_empty() {
		assert_eq!(strip_reply_fallback("> <@alice:example.org> only a quote"), "");
		assert_eq!(strip_reply_fallback("> <@alice:example.org> line\n> line two\n"), "");
		assert_eq!(strip_reply_fallback("> <@alice:example.org> line\n\n"), "");
	}

	#[test]
	fn fallback_is_stripped_only_with_an_in_reply_to() {
		let body = "> quoted by hand\n\nmy question?";
		let reply = content(json!({
			"body": body,
			"m.relates_to": { "m.in_reply_to": { "event_id": "$replied:example.org" } },
		}));
		let bare_thread = content(json!({
			"body": body,
			"m.relates_to": { "rel_type": "m.thread", "event_id": "$root:example.org" },
		}));
		assert_eq!(reply.text(), "my question?");
		assert_eq!(bare_thread.text(), body);
		assert_eq!(content(json!({ "body": body })).text(), body);
		assert_eq!(content(json!({})).text(), "");
	}

	#[test]
	fn falling_back_thread_reply_is_fallback_stripped_but_not_a_reply() {
		let falling_back = content(json!({
			"body": "> <@bot:example.org> Download it from the releases page.\n\nthanks",
			"m.relates_to": {
				"rel_type": "m.thread",
				"event_id": "$root:example.org",
				"is_falling_back": true,
				"m.in_reply_to": { "event_id": "$answer:example.org" },
			},
		}));
		assert_eq!(falling_back.reply_target(), None);
		assert_eq!(falling_back.thread_root(), Some("$root:example.org"));
		assert_eq!(falling_back.text(), "thanks");
	}

	#[test]
	fn reply_target_from_reply_relation() {
		let reply = content(json!({
			"m.relates_to": { "m.in_reply_to": { "event_id": "$replied:example.org" } },
		}));
		assert_eq!(reply.reply_target(), Some("$replied:example.org"));
		assert_eq!(reply.thread_root(), None);
	}

	#[test]
	fn reply_target_from_thread_counts_only_without_fallback() {
		let genuine = content(json!({
			"m.relates_to": {
				"rel_type": "m.thread",
				"event_id": "$root:example.org",
				"is_falling_back": false,
				"m.in_reply_to": { "event_id": "$replied:example.org" },
			},
		}));
		let implicit = content(json!({
			"m.relates_to": {
				"rel_type": "m.thread",
				"event_id": "$root:example.org",
				"m.in_reply_to": { "event_id": "$replied:example.org" },
			},
		}));
		let bare = content(json!({
			"m.relates_to": { "rel_type": "m.thread", "event_id": "$root:example.org" },
		}));
		assert_eq!(genuine.reply_target(), Some("$replied:example.org"));
		assert_eq!(genuine.thread_root(), Some("$root:example.org"));
		assert_eq!(implicit.reply_target(), Some("$replied:example.org"));
		assert_eq!(bare.reply_target(), None);
		assert_eq!(content(json!({})).reply_target(), None);
		assert_eq!(content(json!({})).thread_root(), None);
	}

	#[test]
	fn edits_are_recognized() {
		let edit = content(json!({
			"msgtype": "m.text",
			"body": " * fixed",
			"m.relates_to": { "rel_type": "m.replace", "event_id": "$original:example.org" },
		}));
		assert!(edit.is_edit());
		assert!(!content(json!({ "msgtype": "m.text", "body": "hi" })).is_edit());
	}

	#[test]
	fn html_requires_the_matrix_format() {
		let html = content(json!({ "format": HTML_FORMAT, "formatted_body": "<b>hi</b>" }));
		let other = content(json!({ "format": "org.example", "formatted_body": "<b>hi</b>" }));
		assert_eq!(html.html(), Some("<b>hi</b>"));
		assert_eq!(other.html(), None);
		assert_eq!(content(json!({ "formatted_body": "<b>hi</b>" })).html(), None);
	}

	#[test]
	fn unknown_fields_are_ignored_and_missing_ones_default() {
		let event = event(json!({
			"type": "m.room.message",
			"event_id": "$1",
			"sender": "@alice:example.org",
			"extra": { "nested": true },
		}));
		assert_eq!(event.room_id, "");
		assert_eq!(event.origin_server_ts, 0);
		assert!(event.content.msgtype.is_none());
		assert!(event.content.mentioned_user_ids().is_empty());
	}

	fn message_json(sender: &str, content: Value) -> Value {
		json!({
			"type": "m.room.message",
			"event_id": "$replied:example.org",
			"room_id": "!support:example.org",
			"sender": sender,
			"origin_server_ts": 1,
			"content": content,
		})
	}

	fn edited_message(content: Value, editor: &str, new_body: &str) -> Event {
		let mut json = message_json("@alice:example.org", content);
		json["unsigned"] = json!({
			"m.relations": {
				"m.replace": {
					"type": "m.room.message",
					"event_id": "$edit:example.org",
					"sender": editor,
					"origin_server_ts": 2,
					"content": {
						"msgtype": "m.text",
						"body": format!(" * {new_body}"),
						"m.new_content": { "msgtype": "m.text", "body": new_body },
						"m.relates_to": { "rel_type": "m.replace", "event_id": "$replied:example.org" },
					},
				},
			},
		});
		event(json)
	}

	#[test]
	fn latest_text_is_the_edit_bundled_with_the_fetched_event() {
		let event = edited_message(
			json!({ "msgtype": "m.text", "body": "does it work on" }),
			"@alice:example.org",
			"does Open Grind work on iOS?",
		);
		assert_eq!(event.latest_text(), Some("does Open Grind work on iOS?"));
	}

	#[test]
	fn edit_of_a_reply_is_used_without_stripping() {
		let event = edited_message(
			json!({
				"msgtype": "m.text",
				"body": "> <@carol:example.org> hi\n\ndoes it work on",
				"m.relates_to": { "m.in_reply_to": { "event_id": "$earlier:example.org" } },
			}),
			"@alice:example.org",
			"> quoted by hand\n\ndoes it work on iOS?",
		);
		assert_eq!(event.latest_text(), Some("> quoted by hand\n\ndoes it work on iOS?"));
	}

	#[test]
	fn edit_by_another_sender_is_ignored() {
		let event = edited_message(
			json!({ "msgtype": "m.text", "body": "does it work on iOS?" }),
			"@mallory:example.org",
			"how do I get admin?",
		);
		assert_eq!(event.latest_text(), Some("does it work on iOS?"));
	}

	#[test]
	fn pre_spec_1_7_edit_without_content_falls_back_to_the_original() {
		let mut json =
			message_json("@alice:example.org", json!({ "msgtype": "m.text", "body": "original?" }));
		json["unsigned"] = json!({
			"m.relations": {
				"m.replace": {
					"event_id": "$edit:example.org",
					"origin_server_ts": 2,
					"sender": "@alice:example.org",
				},
			},
		});
		assert_eq!(event(json).latest_text(), Some("original?"));
	}

	#[test]
	fn latest_text_of_a_reply_is_fallback_stripped() {
		let event = event(message_json(
			"@alice:example.org",
			json!({
				"msgtype": "m.text",
				"body": "> <@carol:example.org> hi\n\nwhere can I download it?",
				"m.relates_to": { "m.in_reply_to": { "event_id": "$earlier:example.org" } },
			}),
		));
		assert_eq!(event.latest_text(), Some("where can I download it?"));
	}

	#[test]
	fn non_text_has_no_latest_text() {
		let notice = event(message_json(
			"@otherbot:example.org",
			json!({ "msgtype": "m.notice", "body": "beep" }),
		));
		assert_eq!(notice.latest_text(), None);
		let mut reaction = message_json("@alice:example.org", json!({ "body": "hi" }));
		reaction["type"] = json!("m.reaction");
		assert_eq!(event(reaction).latest_text(), None);
		let redacted = event(message_json("@alice:example.org", json!({})));
		assert_eq!(redacted.latest_text(), None);
	}

	#[test]
	fn notice_renders_markdown_and_replies_with_a_mention() {
		let markdown =
			"Get **it** from [releases](https://example.org/releases).\n\n1. one\n2. two";
		let content = notice(markdown, "$question:example.org", None, "@alice:example.org");
		assert_eq!(content["msgtype"], "m.notice");
		assert_eq!(content["body"], markdown);
		assert_eq!(content["format"], "org.matrix.custom.html");
		let html = content["formatted_body"].as_str().unwrap();
		assert!(html.contains("<strong>it</strong>"), "{html}");
		assert!(html.contains(r#"<a href="https://example.org/releases">releases</a>"#), "{html}");
		assert!(html.contains("<ol>\n<li>one</li>\n<li>two</li>\n</ol>"), "{html}");
		assert_eq!(
			content["m.relates_to"],
			json!({ "m.in_reply_to": { "event_id": "$question:example.org" } })
		);
		assert_eq!(content["m.mentions"], json!({ "user_ids": ["@alice:example.org"] }));
	}

	#[test]
	fn notice_in_a_thread_replies_inside_the_thread() {
		let content =
			notice("Releases page.", "$question:example.org", Some("$root:example.org"), "@a:b");
		assert_eq!(
			content["m.relates_to"],
			json!({
				"rel_type": "m.thread",
				"event_id": "$root:example.org",
				"is_falling_back": false,
				"m.in_reply_to": { "event_id": "$question:example.org" },
			})
		);
	}

	#[test]
	fn notice_line_breaks_are_kept() {
		let content = notice("first\nsecond", "$q", None, "@a:b");
		assert_eq!(content["formatted_body"], "<p>first<br />\nsecond</p>\n");
	}

	#[test]
	fn reaction_annotates_the_message() {
		assert_eq!(
			reaction("$question:example.org", "🙋"),
			json!({
				"m.relates_to": {
					"rel_type": "m.annotation",
					"event_id": "$question:example.org",
					"key": "🙋",
				},
			})
		);
	}

	#[test]
	fn power_level_is_the_user_entry_then_the_default_then_zero() {
		let levels = json!({
			"users": { "@mod:example.org": 50, "@pre_room_v10:example.org": "100" },
			"users_default": 10,
		});
		assert_eq!(user_power_level(&levels, "@mod:example.org"), Some(50));
		assert_eq!(user_power_level(&levels, "@pre_room_v10:example.org"), Some(100));
		assert_eq!(user_power_level(&levels, "@alice:example.org"), Some(10));
		assert_eq!(user_power_level(&json!({ "users_default": "-1" }), "@a:b"), Some(-1));
		assert_eq!(user_power_level(&json!({ "users": {} }), "@a:b"), Some(0));
		assert_eq!(user_power_level(&json!({}), "@a:b"), Some(0));
	}

	#[test]
	fn malformed_power_level_is_rejected() {
		for level in [json!(50.5), json!("high"), json!(null), json!(true)] {
			let levels = json!({ "users": { "@a:b": level }, "users_default": 0 });
			assert_eq!(user_power_level(&levels, "@a:b"), None, "{level}");
		}
		assert_eq!(user_power_level(&json!({ "users_default": "x" }), "@a:b"), None);
	}

	#[test]
	fn only_room_version_12_ids_name_their_create_event() {
		let room_id = "!b2GcLXF4xhTrmSPXcdGk-9UN40uEPnAz7NuH031cxLk";
		let event_id = "$b2GcLXF4xhTrmSPXcdGk-9UN40uEPnAz7NuH031cxLk";
		assert_eq!(room_v12_create_event_id(room_id).as_deref(), Some(event_id));
		assert_eq!(room_v12_create_event_id("!general:example.org"), None);
	}

	#[test]
	fn creators_are_the_sender_and_the_additional_creators() {
		let create = json!({
			"sender": "@admin:example.org",
			"content": { "additional_creators": ["@viktor:example.org"] },
		});
		assert!(is_creator(&create, "@admin:example.org"));
		assert!(is_creator(&create, "@viktor:example.org"));
		assert!(!is_creator(&create, "@alice:example.org"));
		let malformed = json!({ "content": { "additional_creators": "@alice:example.org" } });
		assert!(!is_creator(&malformed, "@alice:example.org"));
	}

	#[test]
	fn txn_ids_are_unique() {
		assert_ne!(next_txn_id(), next_txn_id());
	}

	#[test]
	fn excerpt_truncates_and_flattens_control_characters() {
		assert_eq!(excerpt("a\nb"), "a b");
		let long = "x".repeat(EXCERPT_LIMIT + 1);
		assert_eq!(excerpt(&long), format!("{}…", "x".repeat(EXCERPT_LIMIT)));
	}
}
