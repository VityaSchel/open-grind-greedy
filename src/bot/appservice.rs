use super::calibration::Calibration;
use super::config::Config;
use super::matrix::{Event, Matrix, Transaction};
use super::worker::Worker;
use crate::matcher::Matcher;
use axum::Router;
use axum::extract::{DefaultBodyLimit, Json, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use serde_json::value::RawValue;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tracing::warn;

const BODY_LIMIT: usize = 16 * 1024 * 1024;
const RECENT_IDS_CAPACITY: usize = 4096;
const QUEUE_CAPACITY: usize = 1024;

struct AppState {
	config: Arc<Config>,
	events: mpsc::Sender<Event>,
	recent_ids: Mutex<RecentIds>,
}

#[derive(Default)]
struct RecentIds(VecDeque<String>);

impl RecentIds {
	fn contains(&self, id: &str) -> bool {
		self.0.iter().any(|recent| recent == id)
	}

	fn insert(&mut self, id: String) {
		if self.0.len() >= RECENT_IDS_CAPACITY {
			self.0.pop_front();
		}
		self.0.push_back(id);
	}
}

pub fn router(
	config: Arc<Config>,
	calibration: Calibration,
	matcher: Arc<Matcher>,
	http: reqwest::Client,
) -> (Router, JoinHandle<()>) {
	let (events, queue) = mpsc::channel(QUEUE_CAPACITY);
	let matrix = Matrix::new(http, config.clone());
	let worker = tokio::spawn(Worker::new(config.clone(), calibration, matcher, matrix).run(queue));
	let state = Arc::new(AppState { config, events, recent_ids: Mutex::new(RecentIds::default()) });
	let protected = Router::new()
		.route("/_matrix/app/v1/transactions/{txn_id}", put(transactions))
		.route("/_matrix/app/v1/ping", post(ping))
		.layer(middleware::from_fn_with_state(state.clone(), auth))
		.layer(DefaultBodyLimit::max(BODY_LIMIT));
	let router = Router::new()
		.merge(protected)
		.route("/health", get(health))
		.fallback(not_found)
		.with_state(state);
	(router, worker)
}

pub fn spawn_room_joins(config: Arc<Config>, http: reqwest::Client) {
	let matrix = Matrix::new(http, config.clone());
	for room_id in &config.room_ids {
		matrix.spawn_join(room_id.clone());
	}
}

async fn health() -> Json<Value> {
	Json(json!({ "ok": true }))
}

async fn ping() -> Json<Value> {
	Json(json!({}))
}

async fn not_found() -> Response {
	(StatusCode::NOT_FOUND, Json(json!({ "errcode": "M_UNRECOGNIZED" }))).into_response()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
	let difference =
		a.iter().zip(b).fold(0, |difference, (x, y)| std::hint::black_box(difference | (x ^ y)));
	difference == 0 && a.len() == b.len()
}

fn query_token_matches(req: &Request, token: &str) -> Option<bool> {
	let value =
		req.uri().query()?.split('&').find_map(|pair| pair.strip_prefix("access_token="))?;
	Some(
		urlencoding::decode(value)
			.is_ok_and(|decoded| constant_time_eq(decoded.as_bytes(), token.as_bytes())),
	)
}

fn header_token_matches(req: &Request, token: &str) -> Option<bool> {
	let value = req.headers().get(header::AUTHORIZATION)?.to_str().ok()?;
	Some(
		value
			.strip_prefix("Bearer ")
			.is_some_and(|value| constant_time_eq(value.as_bytes(), token.as_bytes())),
	)
}

fn authorized(token: &str, req: &Request) -> bool {
	match (query_token_matches(req, token), header_token_matches(req, token)) {
		(Some(query_ok), Some(header_ok)) => query_ok && header_ok,
		(Some(ok), None) | (None, Some(ok)) => ok,
		(None, None) => false,
	}
}

async fn auth(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
	if authorized(&state.config.homeserver_token, &req) {
		next.run(req).await
	} else {
		let error = json!({ "errcode": "M_FORBIDDEN", "error": "Bad hs_token" });
		(StatusCode::FORBIDDEN, Json(error)).into_response()
	}
}

fn parse_each_event_skipping_malformed(
	raw_events: Vec<Box<RawValue>>,
) -> impl Iterator<Item = Event> {
	raw_events.into_iter().filter_map(|event| {
		serde_json::from_str(event.get())
			.inspect_err(|e| warn!("skipping malformed event: {e}"))
			.ok()
	})
}

async fn transactions(
	State(state): State<Arc<AppState>>,
	Json(transaction): Json<Transaction>,
) -> Response {
	let mut recent_ids = state.recent_ids.lock().await;
	for event in parse_each_event_skipping_malformed(transaction.events) {
		if !state.config.room_ids.contains(&event.room_id) || recent_ids.contains(&event.event_id) {
			continue;
		}
		let event_id = event.event_id.clone();
		if state.events.send(event).await.is_err() {
			let error = json!({ "errcode": "M_UNKNOWN", "error": "shutting down" });
			return (StatusCode::SERVICE_UNAVAILABLE, Json(error)).into_response();
		}
		recent_ids.insert(event_id);
	}
	Json(json!({})).into_response()
}

#[cfg(test)]
mod tests {
	use super::*;
	use axum::body::Body;

	fn request(uri: &str, authorization: Option<&str>) -> Request {
		let mut builder = Request::builder().uri(uri);
		if let Some(authorization) = authorization {
			builder = builder.header("authorization", authorization);
		}
		builder.body(Body::empty()).unwrap()
	}

	#[test]
	fn authorized_accepts_query_and_bearer_rejects_others() {
		assert!(authorized("secret", &request("/x?access_token=secret", None)));
		assert!(authorized("secret", &request("/x?a=b&access_token=sec%72et", None)));
		assert!(authorized("secret", &request("/x", Some("Bearer secret"))));
		assert!(authorized("secret", &request("/x?access_token=secret", Some("Bearer secret"))));
		assert!(!authorized("secret", &request("/x?access_token=nope", None)));
		assert!(!authorized("secret", &request("/x", Some("Bearer nope"))));
		assert!(!authorized("secret", &request("/x", Some("secret"))));
		assert!(!authorized("secret", &request("/x", None)));
		assert!(!authorized("secret", &request("/x?access_token=nope", Some("Bearer secret"))));
		assert!(!authorized("secret", &request("/x?access_token=secret", Some("Bearer nope"))));
	}

	#[test]
	fn constant_time_eq_compares_whole_values() {
		assert!(constant_time_eq(b"secret", b"secret"));
		assert!(constant_time_eq(b"", b""));
		assert!(!constant_time_eq(b"secret", b"secreT"));
		assert!(!constant_time_eq(b"secret", b"secret2"));
		assert!(!constant_time_eq(b"secret", b""));
	}

	fn parsed_ids(events: &str) -> Vec<String> {
		let transaction: Transaction =
			serde_json::from_str(&format!(r#"{{"events":[{events}]}}"#)).unwrap();
		parse_each_event_skipping_malformed(transaction.events)
			.map(|event| event.event_id)
			.collect()
	}

	#[test]
	fn skips_malformed_events_so_the_homeserver_does_not_retry_forever() {
		let events = [
			json!({ "type": "m.room.message", "event_id": "$1", "sender": "@a:b" }),
			json!({ "type": "m.room.message", "event_id": 2, "sender": "@a:b" }),
			json!("not an event"),
			json!({ "type": "m.room.message", "event_id": "$3", "sender": "@a:b", "content": { "body": 3 } }),
			json!({ "type": "m.room.member", "event_id": "$4", "sender": "@a:b", "state_key": "@c:b" }),
		];
		let events = events.map(|event| event.to_string()).join(",");
		assert_eq!(parsed_ids(&events), ["$1", "$4"]);
	}

	#[test]
	fn deeply_nested_event_does_not_hit_the_recursion_limit() {
		let deep = format!("{}0{}", r#"{"x":"#.repeat(400), "}".repeat(400));
		let events = format!(
			r#"{{"type":"m.room.message","event_id":"$deep","sender":"@a:b","content":{{"body":"hi","x":{deep}}}}},{{"type":"m.room.message","event_id":"$2","sender":"@a:b"}}"#
		);
		assert_eq!(parsed_ids(&events), ["$deep", "$2"]);
	}

	#[test]
	fn recent_ids_evict_oldest_beyond_capacity() {
		let mut ids = RecentIds::default();
		assert!(!ids.contains("$0"));
		for i in 0..=RECENT_IDS_CAPACITY {
			ids.insert(format!("${i}"));
		}
		assert!(!ids.contains("$0"));
		assert!(ids.contains("$1"));
		assert!(ids.contains(&format!("${RECENT_IDS_CAPACITY}")));
		assert_eq!(ids.0.len(), RECENT_IDS_CAPACITY);
	}
}
