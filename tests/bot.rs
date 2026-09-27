use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use greedy::bot::{self, Calibration, Config};
use greedy::{Embedder, FaqEntry, Matcher};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::net::TcpListener;
use tokio::sync::watch;

const ROOM: &str = "!general:test";
const V12_ROOM: &str = "!b2GcLXF4xhTrmSPXcdGk";
const BOT: &str = "@faq:test";
const ALICE: &str = "@alice:test";
const BOB: &str = "@bob:test";
const MODERATOR: &str = "@mod:test";
const CREATOR: &str = "@admin:test";
const HS_TOKEN: &str = "hs-secret";
const AS_TOKEN: &str = "as-secret";
const WAIT: Duration = Duration::from_secs(5);
const INSTALL_ANSWER: &str = "Download it from **the releases page**.";

struct MockEmbedder;

impl Embedder for MockEmbedder {
	fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
		Ok(texts.iter().map(|text| vector(text)).collect())
	}
}

fn vector(text: &str) -> Vec<f32> {
	match text {
		"How do I install it?" | "how do I install it?" | "how do I install it" | "install it?" => {
			vec![1.0, 0.0, 0.0, 0.0]
		}
		"How do I update on Android?" => vec![0.0, 1.0, 0.0, 0.0],
		"How do I update on Windows?" => vec![0.0, 0.0, 1.0, 0.0],
		"how do I update the app?" => vec![0.0, 1.0, 1.0, 0.0],
		"can I install plugins too?" => vec![1.0, 0.0, 0.0, 2.0],
		"Can I donate?" => vec![0.0, 0.0, 0.0, -1.0],
		"can I donate to the project?" => vec![0.6, 0.6, 0.6, -1.0],
		_ => vec![0.0, 0.0, 0.0, 1.0],
	}
}

fn faq() -> Vec<FaqEntry> {
	[
		("install", "How do I install it?", INSTALL_ANSWER),
		("update-android", "How do I update on Android?", "Open the app store."),
		("update-windows", "How do I update on Windows?", "Run the installer again."),
		("donate", "Can I donate?", "TODO"),
	]
	.into_iter()
	.map(|(id, question, answer)| FaqEntry {
		id: id.into(),
		question: question.into(),
		paraphrases: Vec::new(),
		answer: answer.into(),
	})
	.collect()
}

#[derive(Clone, Debug)]
struct Call {
	kind: &'static str,
	room_id: String,
	event_type: String,
	content: Value,
	authorization: Option<String>,
}

struct MockState {
	calls: watch::Sender<Vec<Call>>,
	events: Mutex<HashMap<String, Value>>,
	power_levels: Mutex<Option<Value>>,
}

impl MockState {
	fn record(&self, call: Call) -> usize {
		let mut count = 0;
		self.calls.send_modify(|calls| {
			calls.push(call);
			count = calls.len();
		});
		count
	}
}

fn authorization(headers: &HeaderMap) -> Option<String> {
	headers.get(header::AUTHORIZATION).and_then(|value| value.to_str().ok()).map(str::to_owned)
}

async fn send_event(
	State(state): State<Arc<MockState>>,
	Path((room_id, event_type, _txn_id)): Path<(String, String, String)>,
	headers: HeaderMap,
	Json(content): Json<Value>,
) -> Json<Value> {
	let call =
		Call { kind: "send", room_id, event_type, content, authorization: authorization(&headers) };
	let count = state.record(call);
	Json(json!({ "event_id": format!("$sent{count}") }))
}

async fn fetch_event(
	State(state): State<Arc<MockState>>,
	Path((_room_id, event_id)): Path<(String, String)>,
) -> Response {
	match state.events.lock().unwrap().get(&event_id) {
		Some(event) => Json(event.clone()).into_response(),
		None => (StatusCode::NOT_FOUND, Json(json!({ "errcode": "M_NOT_FOUND" }))).into_response(),
	}
}

async fn power_levels(State(state): State<Arc<MockState>>) -> Response {
	match state.power_levels.lock().unwrap().clone() {
		Some(content) => Json(content).into_response(),
		None => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "errcode": "M_UNKNOWN" })))
			.into_response(),
	}
}

async fn join_room(
	State(state): State<Arc<MockState>>,
	Path(room_id): Path<String>,
	headers: HeaderMap,
) -> Json<Value> {
	let call = Call {
		kind: "join",
		room_id: room_id.clone(),
		event_type: String::new(),
		content: Value::Null,
		authorization: authorization(&headers),
	};
	state.record(call);
	Json(json!({ "room_id": room_id }))
}

async fn serve(app: Router) -> String {
	let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let base = format!("http://{}", listener.local_addr().unwrap());
	tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
	base
}

struct TestBot {
	base: String,
	http: reqwest::Client,
	mock: Arc<MockState>,
}

impl TestBot {
	async fn start() -> Self {
		let _ = rustls::crypto::ring::default_provider().install_default();
		let mock = Arc::new(MockState {
			calls: watch::Sender::new(Vec::new()),
			events: Mutex::default(),
			power_levels: Mutex::new(Some(
				json!({ "users": { MODERATOR: 50 }, "users_default": 0 }),
			)),
		});
		let homeserver = serve(
			Router::new()
				.route(
					"/_matrix/client/v3/rooms/{room_id}/send/{event_type}/{txn_id}",
					put(send_event),
				)
				.route("/_matrix/client/v3/rooms/{room_id}/event/{event_id}", get(fetch_event))
				.route(
					"/_matrix/client/v3/rooms/{room_id}/state/m.room.power_levels/",
					get(power_levels),
				)
				.route("/_matrix/client/v3/join/{room_id}", post(join_room))
				.with_state(mock.clone()),
		)
		.await;
		let config = Config {
			homeserver_url: homeserver,
			app_service_user: BOT.into(),
			room_ids: vec![ROOM.into(), V12_ROOM.into()],
			host: "127.0.0.1".into(),
			port: 0,
			app_service_token: AS_TOKEN.into(),
			homeserver_token: HS_TOKEN.into(),
		};
		let calibration =
			Calibration { high_threshold: 0.7, low_threshold: 0.4, ambiguity_margin: 0.02 };
		let matcher = Matcher::new(Box::new(MockEmbedder), faq()).unwrap();
		let http = reqwest::Client::new();
		let (app, _worker) =
			bot::router(Arc::new(config), calibration, Arc::new(matcher), http.clone());
		Self { base: serve(app).await, http, mock }
	}

	fn serve_event(&self, event: Value) {
		let event_id = event["event_id"].as_str().unwrap().to_string();
		self.mock.events.lock().unwrap().insert(event_id, event);
	}

	async fn transaction(&self, events: Vec<Value>) {
		let txn_id = events[0]["event_id"].as_str().unwrap().replace('$', "");
		let response = self
			.http
			.put(format!("{}/_matrix/app/v1/transactions/{txn_id}", self.base))
			.bearer_auth(HS_TOKEN)
			.json(&json!({ "events": events }))
			.send()
			.await
			.unwrap();
		assert_eq!(response.status(), StatusCode::OK);
		assert_eq!(response.json::<Value>().await.unwrap(), json!({}));
	}

	async fn wait_for_calls(&self, count: usize) -> Vec<Call> {
		let mut receiver = self.mock.calls.subscribe();
		let reached = receiver.wait_for(|calls| calls.len() >= count);
		match tokio::time::timeout(WAIT, reached).await {
			Ok(Ok(calls)) => calls.clone(),
			_ => panic!("expected {count} calls, got {:#?}", self.mock.calls.borrow()),
		}
	}

	fn calls(&self) -> Vec<Call> {
		self.mock.calls.borrow().clone()
	}
}

fn now_ms() -> u64 {
	SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

fn message(event_id: &str, sender: &str, content: Value) -> Value {
	json!({
		"type": "m.room.message",
		"event_id": event_id,
		"room_id": ROOM,
		"sender": sender,
		"origin_server_ts": now_ms(),
		"content": content,
	})
}

fn text(event_id: &str, sender: &str, body: &str) -> Value {
	message(event_id, sender, json!({ "msgtype": "m.text", "body": body }))
}

fn reply(event_id: &str, sender: &str, body: &str, to: &str) -> Value {
	message(
		event_id,
		sender,
		json!({
			"msgtype": "m.text",
			"body": body,
			"m.relates_to": { "m.in_reply_to": { "event_id": to } },
		}),
	)
}

fn ping(event_id: &str, sender: &str) -> Value {
	message(
		event_id,
		sender,
		json!({
			"msgtype": "m.text",
			"body": "Faq",
			"format": "org.matrix.custom.html",
			"formatted_body": format!(r#"<a href="https://matrix.to/#/{BOT}">Faq</a>"#),
			"m.mentions": { "user_ids": [BOT] },
		}),
	)
}

fn ping_in_reply(event_id: &str, sender: &str, to: &str) -> Value {
	message(
		event_id,
		sender,
		json!({
			"msgtype": "m.text",
			"body": BOT,
			"m.mentions": { "user_ids": [BOT] },
			"m.relates_to": { "m.in_reply_to": { "event_id": to } },
		}),
	)
}

#[track_caller]
fn assert_notice(call: &Call, body: &str, reply_to: &str, mention: &str) {
	assert_eq!(
		(call.kind, call.room_id.as_str(), call.event_type.as_str()),
		("send", ROOM, "m.room.message")
	);
	assert_eq!(call.content["msgtype"], "m.notice");
	assert_eq!(call.content["body"], body);
	assert_eq!(call.content["format"], "org.matrix.custom.html");
	assert_eq!(call.content["m.relates_to"], json!({ "m.in_reply_to": { "event_id": reply_to } }));
	assert_eq!(call.content["m.mentions"], json!({ "user_ids": [mention] }));
	assert_eq!(call.authorization.as_deref(), Some("Bearer as-secret"));
}

#[tokio::test]
async fn auto_answers_questions_of_at_least_four_words() {
	let bot = TestBot::start().await;
	bot.transaction(vec![
		text("$short", ALICE, "install it?"),
		text("$statement", BOB, "how do I install it"),
		text("$q", ALICE, "how do I install it?"),
		text("$again", BOB, "how do I install it?"),
	])
	.await;
	let calls = bot.wait_for_calls(2).await;
	assert_notice(&calls[0], INSTALL_ANSWER, "$q", ALICE);
	let html = calls[0].content["formatted_body"].as_str().unwrap();
	assert!(html.contains("<strong>the releases page</strong>"), "{html}");
	assert_notice(&calls[1], INSTALL_ANSWER, "$again", BOB);
	assert_eq!(bot.calls().len(), 2);
}

#[tokio::test]
async fn reacts_between_the_thresholds() {
	let bot = TestBot::start().await;
	bot.transaction(vec![text("$q", ALICE, "can I install plugins too?")]).await;
	let calls = bot.wait_for_calls(1).await;
	assert_eq!((calls[0].kind, calls[0].event_type.as_str()), ("send", "m.reaction"));
	assert_eq!(
		calls[0].content,
		json!({ "m.relates_to": { "rel_type": "m.annotation", "event_id": "$q", "key": "🙋" } })
	);
}

#[tokio::test]
async fn no_reaction_when_only_a_placeholder_is_close() {
	let bot = TestBot::start().await;
	bot.transaction(vec![
		text("$placeholder", ALICE, "can I donate to the project?"),
		text("$q", ALICE, "can I install plugins too?"),
	])
	.await;
	let calls = bot.wait_for_calls(1).await;
	assert_eq!(calls[0].content["m.relates_to"]["event_id"], "$q");
	assert_eq!(bot.calls().len(), 1);
}

#[tokio::test]
async fn moderators_get_no_unprompted_answers_or_reactions() {
	let bot = TestBot::start().await;
	bot.transaction(vec![
		text("$answerable", MODERATOR, "how do I install it?"),
		text("$reactable", MODERATOR, "can I install plugins too?"),
		text("$q", ALICE, "how do I install it?"),
	])
	.await;
	let calls = bot.wait_for_calls(1).await;
	assert_notice(&calls[0], INSTALL_ANSWER, "$q", ALICE);
	assert_eq!(bot.calls().len(), 1);
}

#[tokio::test]
async fn unreadable_power_levels_leave_auto_mode_on() {
	let bot = TestBot::start().await;
	*bot.mock.power_levels.lock().unwrap() = None;
	bot.transaction(vec![text("$q", MODERATOR, "how do I install it?")]).await;
	let calls = bot.wait_for_calls(1).await;
	assert_notice(&calls[0], INSTALL_ANSWER, "$q", MODERATOR);
}

#[tokio::test]
async fn room_version_12_creators_count_as_moderators() {
	let bot = TestBot::start().await;
	bot.serve_event(json!({
		"type": "m.room.create",
		"event_id": format!("${}", &V12_ROOM[1..]),
		"room_id": V12_ROOM,
		"sender": CREATOR,
		"state_key": "",
		"content": { "room_version": "12", "additional_creators": [BOB] },
	}));
	let in_v12_room = |mut event: Value| {
		event["room_id"] = json!(V12_ROOM);
		event
	};
	bot.transaction(vec![
		in_v12_room(text("$creator", CREATOR, "how do I install it?")),
		in_v12_room(text("$additional", BOB, "how do I install it?")),
		in_v12_room(text("$q", ALICE, "how do I install it?")),
	])
	.await;
	let calls = bot.wait_for_calls(1).await;
	assert_eq!(
		(calls[0].room_id.as_str(), &calls[0].content["body"]),
		(V12_ROOM, &json!(INSTALL_ANSWER))
	);
	assert_eq!(calls[0].content["m.relates_to"]["m.in_reply_to"]["event_id"], "$q");
	assert_eq!(bot.calls().len(), 1);
}

async fn start_with_list() -> TestBot {
	let bot = TestBot::start().await;
	bot.transaction(vec![text("$q", ALICE, "how do I update the app?")]).await;
	let calls = bot.wait_for_calls(1).await;
	assert_notice(
		&calls[0],
		"1. How do I update on Android?\n2. How do I update on Windows?",
		"$q",
		ALICE,
	);
	let html = calls[0].content["formatted_body"].as_str().unwrap();
	assert!(html.contains("<ol>\n<li>How do I update on Android?</li>"), "{html}");
	bot
}

#[tokio::test]
async fn picks_from_a_question_list_answer_the_original_message() {
	let bot = start_with_list().await;
	let fallback =
		"> <@faq:test> 1. How do I update on Android?\n> 2. How do I update on Windows?\n\n2";
	bot.transaction(vec![reply("$pick2", BOB, fallback, "$sent1")]).await;
	let calls = bot.wait_for_calls(2).await;
	assert_notice(&calls[1], "Run the installer again.", "$q", ALICE);

	bot.transaction(vec![reply("$pick1", BOB, " 1 ", "$sent1")]).await;
	let calls = bot.wait_for_calls(3).await;
	assert_notice(&calls[2], "Open the app store.", "$q", ALICE);

	let pill = message(
		"$pill",
		BOB,
		json!({
			"msgtype": "m.text",
			"body": "Faq: 2",
			"format": "org.matrix.custom.html",
			"formatted_body": format!(r#"<a href="https://matrix.to/#/{BOT}">Faq</a>: 2"#),
			"m.mentions": { "user_ids": [BOT] },
			"m.relates_to": { "m.in_reply_to": { "event_id": "$sent1" } },
		}),
	);
	bot.transaction(vec![pill]).await;
	let calls = bot.wait_for_calls(4).await;
	assert_notice(&calls[3], "Run the installer again.", "$q", ALICE);
}

#[tokio::test]
async fn invalid_pick_asks_for_the_number() {
	let bot = start_with_list().await;
	bot.transaction(vec![reply("$pick", BOB, "two", "$sent1")]).await;
	let calls = bot.wait_for_calls(2).await;
	assert_notice(&calls[1], "Pick the question number in my message", "$pick", BOB);

	bot.transaction(vec![reply("$retry", BOB, "2", "$sent1")]).await;
	let calls = bot.wait_for_calls(3).await;
	assert_notice(&calls[2], "Run the installer again.", "$q", ALICE);
}

#[tokio::test]
async fn a_number_replied_to_an_answer_is_not_a_pick() {
	let bot = TestBot::start().await;
	bot.transaction(vec![text("$q", ALICE, "how do I install it?")]).await;
	let calls = bot.wait_for_calls(1).await;
	assert_notice(&calls[0], INSTALL_ANSWER, "$q", ALICE);
	bot.transaction(vec![reply("$number", BOB, "1", "$sent1")]).await;
	bot.transaction(vec![ping("$ping", BOB)]).await;
	let calls = bot.wait_for_calls(2).await;
	assert_notice(&calls[1], "How may I help?", "$ping", BOB);
	assert_eq!(bot.calls().len(), 2);
}

#[tokio::test]
async fn moderators_can_still_pick_and_summon() {
	let bot = start_with_list().await;
	bot.transaction(vec![reply("$pick", MODERATOR, "2", "$sent1")]).await;
	let calls = bot.wait_for_calls(2).await;
	assert_notice(&calls[1], "Run the installer again.", "$q", ALICE);

	bot.serve_event(text("$old", ALICE, "how do I install it?"));
	bot.transaction(vec![ping_in_reply("$summon", MODERATOR, "$old")]).await;
	let calls = bot.wait_for_calls(3).await;
	assert_notice(&calls[2], INSTALL_ANSWER, "$old", ALICE);

	bot.transaction(vec![ping("$ping", MODERATOR)]).await;
	let calls = bot.wait_for_calls(4).await;
	assert_notice(&calls[3], "How may I help?", "$ping", MODERATOR);
	bot.transaction(vec![reply("$question", MODERATOR, "how do I install it?", "$sent4")]).await;
	let calls = bot.wait_for_calls(5).await;
	assert_notice(&calls[4], INSTALL_ANSWER, "$question", MODERATOR);
}

#[tokio::test]
async fn ping_in_a_reply_answers_the_replied_to_message() {
	let bot = TestBot::start().await;
	bot.serve_event(text("$old", ALICE, "how do I install it?"));
	bot.transaction(vec![ping_in_reply("$summon", BOB, "$old")]).await;
	let calls = bot.wait_for_calls(1).await;
	assert_notice(&calls[0], INSTALL_ANSWER, "$old", ALICE);

	bot.transaction(vec![ping_in_reply("$lost", BOB, "$missing")]).await;
	let calls = bot.wait_for_calls(2).await;
	assert_notice(&calls[1], "I couldn't read that message.", "$lost", BOB);

	bot.serve_event(text("$chatter", ALICE, "nice weather today"));
	bot.transaction(vec![ping_in_reply("$unknown", BOB, "$chatter")]).await;
	let calls = bot.wait_for_calls(3).await;
	assert_notice(&calls[2], "I don't have an FAQ answer for that message.", "$unknown", BOB);
}

fn in_thread(mut event: Value, root: &str, reply_to: Option<&str>) -> Value {
	let mut relation =
		json!({ "rel_type": "m.thread", "event_id": root, "is_falling_back": false });
	if let Some(reply_to) = reply_to {
		relation["m.in_reply_to"] = json!({ "event_id": reply_to });
	}
	event["content"]["m.relates_to"] = relation;
	event
}

#[tokio::test]
async fn ping_in_a_reply_answers_in_the_thread_of_the_replied_to_message() {
	let bot = TestBot::start().await;
	bot.serve_event(in_thread(text("$threaded", ALICE, "how do I install it?"), "$root", None));
	bot.transaction(vec![ping_in_reply("$unthreaded", BOB, "$threaded")]).await;
	bot.serve_event(text("$root", ALICE, "how do I install it?"));
	let summon = in_thread(ping_in_reply("$in-thread", BOB, "$root"), "$root", Some("$root"));
	bot.transaction(vec![summon]).await;
	let calls = bot.wait_for_calls(2).await;
	for (call, answered) in calls.iter().zip(["$threaded", "$root"]) {
		assert_eq!(call.content["body"], INSTALL_ANSWER);
		assert_eq!(
			call.content["m.relates_to"],
			json!({
				"rel_type": "m.thread",
				"event_id": "$root",
				"is_falling_back": false,
				"m.in_reply_to": { "event_id": answered },
			})
		);
	}
}

#[tokio::test]
async fn ping_in_a_reply_to_the_bot_is_skipped() {
	let bot = TestBot::start().await;
	bot.serve_event(message(
		"$answer",
		BOT,
		json!({ "msgtype": "m.notice", "body": "An answer." }),
	));
	bot.transaction(vec![ping_in_reply("$summon", BOB, "$answer")]).await;
	bot.transaction(vec![ping("$ping", BOB)]).await;
	let calls = bot.wait_for_calls(1).await;
	assert_notice(&calls[0], "How may I help?", "$ping", BOB);
	assert_eq!(bot.calls().len(), 1);
}

#[tokio::test]
async fn bare_ping_asks_how_to_help_and_the_reply_is_answered() {
	let bot = TestBot::start().await;
	bot.transaction(vec![ping("$ping", BOB)]).await;
	let calls = bot.wait_for_calls(1).await;
	assert_notice(&calls[0], "How may I help?", "$ping", BOB);

	bot.transaction(vec![reply("$blank", BOB, "?", "$sent1")]).await;
	let calls = bot.wait_for_calls(2).await;
	assert_notice(&calls[1], "How may I help?", "$blank", BOB);

	bot.transaction(vec![reply("$question", BOB, "how do I install it?", "$sent1")]).await;
	let calls = bot.wait_for_calls(3).await;
	assert_notice(&calls[2], INSTALL_ANSWER, "$question", BOB);
}

#[tokio::test]
async fn own_stale_and_edited_messages_are_ignored() {
	let bot = TestBot::start().await;
	let mut stale = text("$stale", ALICE, "how do I install it?");
	stale["origin_server_ts"] = json!(now_ms() - 11 * 60 * 1000);
	let edit = message(
		"$edit",
		ALICE,
		json!({
			"msgtype": "m.text",
			"body": " * how do I install it?",
			"m.relates_to": { "rel_type": "m.replace", "event_id": "$stale" },
		}),
	);
	bot.transaction(vec![
		text("$own", BOT, "how do I install it?"),
		stale,
		edit,
		json!({ "type": "m.room.message", "event_id": 42 }),
		ping("$ping", BOB),
	])
	.await;
	let calls = bot.wait_for_calls(1).await;
	assert_notice(&calls[0], "How may I help?", "$ping", BOB);
	assert_eq!(bot.calls().len(), 1);
}

#[tokio::test]
async fn deeply_nested_events_do_not_fail_the_transaction() {
	let bot = TestBot::start().await;
	let mut nested = json!(0);
	for _ in 0..200 {
		nested = json!({ "x": nested });
	}
	let mut deep = text("$deep", ALICE, "how do I install it?");
	deep["content"]["x"] = nested;
	bot.transaction(vec![deep, ping("$ping", BOB)]).await;
	let calls = bot.wait_for_calls(2).await;
	assert_notice(&calls[0], INSTALL_ANSWER, "$deep", ALICE);
	assert_notice(&calls[1], "How may I help?", "$ping", BOB);
}

#[tokio::test]
async fn retried_transactions_are_processed_once() {
	let bot = TestBot::start().await;
	bot.transaction(vec![ping("$ping", BOB)]).await;
	bot.transaction(vec![ping("$ping", BOB)]).await;
	bot.transaction(vec![ping("$next", BOB)]).await;
	let calls = bot.wait_for_calls(2).await;
	assert_notice(&calls[0], "How may I help?", "$ping", BOB);
	assert_notice(&calls[1], "How may I help?", "$next", BOB);
	assert_eq!(bot.calls().len(), 2);
}

fn invite(event_id: &str, room_id: &str) -> Value {
	json!({
		"type": "m.room.member",
		"event_id": event_id,
		"room_id": room_id,
		"sender": ALICE,
		"state_key": BOT,
		"origin_server_ts": now_ms(),
		"content": { "membership": "invite" },
	})
}

#[tokio::test]
async fn invite_to_a_configured_room_is_joined() {
	let bot = TestBot::start().await;
	bot.transaction(vec![invite("$elsewhere", "!other:test")]).await;
	bot.transaction(vec![invite("$invite", ROOM)]).await;
	let calls = bot.wait_for_calls(1).await;
	assert_eq!((calls[0].kind, calls[0].room_id.as_str()), ("join", ROOM));
	assert_eq!(calls[0].authorization.as_deref(), Some("Bearer as-secret"));
}

#[tokio::test]
async fn requests_without_the_hs_token_are_rejected() {
	let bot = TestBot::start().await;
	let url = format!("{}/_matrix/app/v1/transactions/1", bot.base);
	let body = json!({ "events": [text("$q", ALICE, "how do I install it?")] });
	for request in [
		bot.http.put(&url).json(&body),
		bot.http.put(&url).bearer_auth("wrong").json(&body),
		bot.http.put(format!("{url}?access_token=wrong")).json(&body),
	] {
		let response = request.send().await.unwrap();
		assert_eq!(response.status(), StatusCode::FORBIDDEN);
		assert_eq!(response.json::<Value>().await.unwrap()["errcode"], "M_FORBIDDEN");
	}

	let appservice_ping = bot.http.post(format!("{}/_matrix/app/v1/ping", bot.base));
	let response = appservice_ping.bearer_auth(HS_TOKEN).json(&json!({})).send().await.unwrap();
	assert_eq!(response.status(), StatusCode::OK);
	let health = bot.http.get(format!("{}/health", bot.base)).send().await.unwrap();
	assert_eq!(health.status(), StatusCode::OK);
	let unknown =
		bot.http.get(format!("{}/_matrix/app/v1/users/x", bot.base)).send().await.unwrap();
	assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
	assert_eq!(unknown.json::<Value>().await.unwrap()["errcode"], "M_UNRECOGNIZED");

	bot.transaction(vec![ping("$ping", BOB)]).await;
	let calls = bot.wait_for_calls(1).await;
	assert_notice(&calls[0], "How may I help?", "$ping", BOB);
	assert_eq!(bot.calls().len(), 1);
}
