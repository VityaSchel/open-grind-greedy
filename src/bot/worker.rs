use super::answering::{
	Decision, Message, Pending, PendingMessages, decide, is_question, is_stale, picked,
	question_list,
};
use super::calibration::Calibration;
use super::config::Config;
use super::matrix::{self, Event, Matrix, unix_millis};
use super::mentions::{mentions_user, summon_question};
use crate::faq::FaqEntry;
use crate::matcher::{Match, Matcher};
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

const HOW_MAY_I_HELP: &str = "How may I help?";
const PICK_NOTICE: &str = "Pick the question number in my message";
const NO_ANSWER_NOTICE: &str = "I don't have an FAQ answer for that message.";
const UNREADABLE_NOTICE: &str = "I couldn't read that message.";
const RAISED_HAND: &str = "🙋";
const MODERATOR_POWER_LEVEL: i64 = 50;

pub struct Worker {
	config: Arc<Config>,
	calibration: Calibration,
	matcher: Arc<Matcher>,
	matrix: Matrix,
	pending: PendingMessages,
	encrypted_rooms: HashSet<String>,
}

impl Worker {
	pub fn new(
		config: Arc<Config>,
		calibration: Calibration,
		matcher: Arc<Matcher>,
		matrix: Matrix,
	) -> Self {
		Self {
			config,
			calibration,
			matcher,
			matrix,
			pending: PendingMessages::default(),
			encrypted_rooms: HashSet::new(),
		}
	}

	pub async fn run(mut self, mut events: mpsc::Receiver<Event>) {
		while let Some(event) = events.recv().await {
			if let Err(e) = self.handle(event).await {
				warn!("{e:#}");
			}
		}
	}

	async fn handle(&mut self, event: Event) -> Result<()> {
		match event.event_type.as_str() {
			"m.room.message" => self.on_message(event).await?,
			"m.room.member"
				if event.state_key.as_ref() == Some(&self.config.app_service_user)
					&& event.content.membership.as_deref() == Some("invite") =>
			{
				info!(room = %event.room_id, inviter = %event.sender, "invited");
				self.matrix.spawn_join(event.room_id);
			}
			"m.room.encrypted" if self.encrypted_rooms.insert(event.room_id.clone()) => {
				warn!(room = %event.room_id, "room has encrypted messages, which the bot cannot read");
			}
			_ => {}
		}
		Ok(())
	}

	async fn on_message(&mut self, event: Event) -> Result<()> {
		let bot = self.config.app_service_user.as_str();
		let content = &event.content;
		if event.sender == bot
			|| !content.is_text()
			|| content.is_edit()
			|| is_stale(event.origin_server_ts, unix_millis())
		{
			debug!(room = %event.room_id, event = %event.event_id, "skipping message");
			return Ok(());
		}
		let message = Message {
			room_id: event.room_id.clone(),
			event_id: event.event_id.clone(),
			sender: event.sender.clone(),
			thread_root: content.thread_root().map(str::to_owned),
		};
		let body = content.text();
		let html = content.html();
		let target = content.reply_target();
		match target.and_then(|target| self.pending.get(target, Instant::now())).cloned() {
			Some(Pending::Choices { original, entry_ids }) => {
				let choice = summon_question(body, html, bot);
				return self.pick(&message, &choice, &original, &entry_ids).await;
			}
			Some(Pending::Prompt) => return self.summon_or_prompt(&message, body, html).await,
			None => {}
		}
		if mentions_user(content.mentioned_user_ids(), html, body, bot) {
			return match target {
				Some(target) => self.summon_on_reply(&message, target).await,
				None => self.summon_or_prompt(&message, body, html).await,
			};
		}
		if target.is_some() {
			debug!(room = %message.room_id, "reply without a mention");
			return Ok(());
		}
		self.auto(&message, body).await
	}

	async fn pick(
		&mut self,
		picker: &Message,
		choice: &str,
		original: &Message,
		entry_ids: &[String],
	) -> Result<()> {
		let Some(entry_id) = picked(choice, entry_ids) else {
			self.reply(picker, PICK_NOTICE).await?;
			info!(mode = "pick", room = %picker.room_id, picker = %picker.sender, "asked for a number");
			return Ok(());
		};
		self.answer("pick", original, entry_id, None).await
	}

	async fn summon_or_prompt(
		&mut self,
		message: &Message,
		body: &str,
		html: Option<&str>,
	) -> Result<()> {
		let question = summon_question(body, html, &self.config.app_service_user);
		if question.chars().any(char::is_alphanumeric) {
			return self.summon(message, message, question).await;
		}
		let prompt = self.reply(message, HOW_MAY_I_HELP).await?;
		info!(mode = "summon", room = %message.room_id, asker = %message.sender, "asked how to help");
		self.pending.insert(prompt, Pending::Prompt, Instant::now());
		Ok(())
	}

	async fn summon_on_reply(&mut self, summoner: &Message, target: &str) -> Result<()> {
		let replied_to: Option<Event> = self
			.matrix
			.event(&summoner.room_id, target)
			.await
			.inspect_err(|e| warn!("cannot read the replied-to message: {e:#}"))
			.ok();
		let Some(replied_to) = replied_to else {
			self.reply(summoner, UNREADABLE_NOTICE).await?;
			return Ok(());
		};
		if replied_to.sender == self.config.app_service_user {
			debug!(room = %summoner.room_id, "ping in a reply to the bot, ignoring");
			return Ok(());
		}
		let Some(question) = replied_to.latest_text() else {
			self.reply(summoner, UNREADABLE_NOTICE).await?;
			info!(mode = "summon", room = %summoner.room_id, event = target, "replied-to message is not text");
			return Ok(());
		};
		let answer_to = Message {
			room_id: summoner.room_id.clone(),
			event_id: target.to_owned(),
			sender: replied_to.sender.clone(),
			thread_root: replied_to
				.content
				.thread_root()
				.map(str::to_owned)
				.or_else(|| summoner.thread_root.clone()),
		};
		self.summon(summoner, &answer_to, question.to_owned()).await
	}

	async fn summon(
		&mut self,
		summoner: &Message,
		answer_to: &Message,
		question: String,
	) -> Result<()> {
		let ranking = self.rank(question).await?;
		let score = ranking.first().map_or(0.0, |top| top.score);
		match self.decide(&ranking, self.calibration.low_threshold) {
			Decision::Answer(id) => self.answer("summon", answer_to, &id, Some(score)).await,
			Decision::Choose(ids) => self.offer("summon", answer_to, ids, score).await,
			Decision::NoMatch => {
				self.reply(summoner, NO_ANSWER_NOTICE).await?;
				info!(mode = "summon", room = %summoner.room_id, score, "no confident match");
				Ok(())
			}
		}
	}

	async fn auto(&mut self, message: &Message, body: &str) -> Result<()> {
		if !is_question(body) {
			debug!(room = %message.room_id, "not a question");
			return Ok(());
		}
		if self.is_moderator(message).await {
			debug!(room = %message.room_id, sender = %message.sender, "moderator, skipping auto mode");
			return Ok(());
		}
		let ranking = self.rank(body.to_owned()).await?;
		let Some(top) = ranking.first() else {
			return Ok(());
		};
		match self.decide(&ranking, self.calibration.high_threshold) {
			Decision::Answer(id) => self.answer("auto", message, &id, Some(top.score)).await,
			Decision::Choose(ids) => self.offer("auto", message, ids, top.score).await,
			Decision::NoMatch => {
				let decision_if_summoned = self.decide(&ranking, self.calibration.low_threshold);
				let Some(entry) = decision_if_summoned.best() else {
					debug!(room = %message.room_id, entry = %top.id, score = top.score, "no match");
					return Ok(());
				};
				let reaction = matrix::reaction(&message.event_id, RAISED_HAND);
				self.matrix.send(&message.room_id, "m.reaction", &reaction).await?;
				info!(mode = "auto", room = %message.room_id, entry, score = top.score, "reacted");
				Ok(())
			}
		}
	}

	async fn is_moderator(&self, message: &Message) -> bool {
		self.matrix
			.power_level(&message.room_id, &message.sender)
			.await
			.inspect_err(|e| warn!("{e:#}; treating {} as a regular user", message.sender))
			.is_ok_and(|level| level >= MODERATOR_POWER_LEVEL)
	}

	async fn answer(
		&self,
		mode: &str,
		to: &Message,
		entry_id: &str,
		score: Option<f32>,
	) -> Result<()> {
		self.reply(to, &self.entry(entry_id)?.answer).await?;
		info!(mode, room = %to.room_id, entry = entry_id, score, answered = %to.event_id, "answered");
		Ok(())
	}

	async fn offer(
		&mut self,
		mode: &str,
		to: &Message,
		entry_ids: Vec<String>,
		score: f32,
	) -> Result<()> {
		let questions = entry_ids
			.iter()
			.map(|id| Ok(self.entry(id)?.question.as_str()))
			.collect::<Result<Vec<_>>>()?;
		let list = self.reply(to, &question_list(&questions)).await?;
		info!(mode, room = %to.room_id, entries = ?entry_ids, score, answered = %to.event_id, "listed questions");
		let pending = Pending::Choices { original: to.clone(), entry_ids };
		self.pending.insert(list, pending, Instant::now());
		Ok(())
	}

	async fn reply(&self, to: &Message, markdown: &str) -> Result<String> {
		let content = matrix::notice(markdown, &to.event_id, to.thread_root.as_deref(), &to.sender);
		self.matrix.send(&to.room_id, "m.room.message", &content).await
	}

	async fn rank(&self, text: String) -> Result<Vec<Match>> {
		let matcher = Arc::clone(&self.matcher);
		tokio::task::spawn_blocking(move || matcher.rank(&text))
			.await
			.context("FAQ ranking task failed")?
	}

	fn decide(&self, ranking: &[Match], threshold: f32) -> Decision {
		decide(ranking, threshold, self.calibration.ambiguity_margin, |id| self.matcher.entry(id))
	}

	fn entry(&self, id: &str) -> Result<&FaqEntry> {
		self.matcher.entry(id).with_context(|| format!("FAQ entry '{id}' vanished"))
	}
}
