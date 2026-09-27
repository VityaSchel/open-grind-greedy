mod answering;
mod appservice;
mod calibration;
mod config;
mod matrix;
mod mentions;
mod registration;
mod worker;

pub use appservice::{router, spawn_room_joins};
pub use calibration::Calibration;
pub use config::Config;
pub use registration::registration_yaml;

use crate::embedder::FastembedEmbedder;
use crate::faq::load_faq;
use crate::matcher::Matcher;
use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use std::io::IsTerminal;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

const WORKER_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

pub fn run(config_path: &Path, faq_path: &Path, quiet: bool) -> Result<()> {
	tracing_subscriber::fmt()
		.with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
		.with_ansi(std::io::stdout().is_terminal())
		.init();
	let config = Config::load(config_path)?;
	let calibration = Calibration::embedded()?;
	let runtime = tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.build()
		.context("cannot start the async runtime")?;
	let listener = runtime.block_on(bind_before_slow_faq_embedding(&config))?;
	let matcher = load_matcher(faq_path, quiet)?;
	runtime.block_on(serve(config, calibration, matcher, listener))
}

async fn bind_before_slow_faq_embedding(config: &Config) -> Result<TcpListener> {
	TcpListener::bind((config.host.as_str(), config.port))
		.await
		.with_context(|| format!("binding {}:{}", config.host, config.port))
}

fn parse_yaml<T: DeserializeOwned>(yaml: &str) -> Result<T> {
	let single_line_errors = serde_saphyr::options! { with_snippet: false };
	Ok(serde_saphyr::from_str_with_options(yaml, single_line_errors)?)
}

#[cfg(test)]
fn edited(valid: &str, key: &str, value: Option<&str>) -> String {
	valid
		.lines()
		.filter_map(|line| match line.split_once(": ") {
			Some((name, _)) if name == key => value.map(|value| format!("{key}: {value}\n")),
			_ => Some(format!("{line}\n")),
		})
		.collect()
}

fn load_matcher(faq_path: &Path, quiet: bool) -> Result<Matcher> {
	let entries = load_faq(faq_path)?;
	let placeholders =
		entries.iter().filter(|entry| answering::is_placeholder(&entry.answer)).count();
	info!(
		path = %faq_path.display(),
		entries = entries.len(),
		placeholders,
		"FAQ loaded; placeholder answers are never posted"
	);
	let embedder = FastembedEmbedder::load_or_download(quiet)?;
	let matcher = Matcher::new(Box::new(embedder), entries)?;
	info!("FAQ embedded");
	Ok(matcher)
}

async fn serve(
	config: Config,
	calibration: Calibration,
	matcher: Matcher,
	listener: TcpListener,
) -> Result<()> {
	rustls::crypto::ring::default_provider()
		.install_default()
		.map_err(|_| anyhow::anyhow!("failed to install rustls ring crypto provider"))?;
	let config = Arc::new(config);
	let http = reqwest::Client::builder()
		.connect_timeout(Duration::from_secs(10))
		.timeout(Duration::from_secs(30))
		.build()
		.context("building HTTP client")?;

	info!(
		address = %listener.local_addr()?,
		app_service_user = %config.app_service_user,
		rooms = %config.room_ids.join(","),
		"greedy appservice listening"
	);

	let (app, worker) = router(config.clone(), calibration, Arc::new(matcher), http.clone());
	spawn_room_joins(config, http);

	axum::serve(listener, app)
		.with_graceful_shutdown(shutdown_signal())
		.await
		.context("server error")?;

	match tokio::time::timeout(WORKER_DRAIN_TIMEOUT, worker).await {
		Ok(Ok(())) => {}
		Ok(Err(e)) => error!("worker task failed: {e}"),
		Err(_) => warn!("worker did not finish within {WORKER_DRAIN_TIMEOUT:?}"),
	}
	Ok(())
}

async fn shutdown_signal() {
	let interrupt = async {
		if let Err(e) = tokio::signal::ctrl_c().await {
			error!("cannot listen for ctrl-c: {e}");
			std::future::pending::<()>().await;
		}
	};
	#[cfg(unix)]
	let terminate = async {
		use tokio::signal::unix::{SignalKind, signal};
		match signal(SignalKind::terminate()) {
			Ok(mut terminate) => {
				terminate.recv().await;
			}
			Err(e) => {
				error!("cannot listen for SIGTERM: {e}");
				std::future::pending::<()>().await;
			}
		}
	};
	#[cfg(not(unix))]
	let terminate = std::future::pending::<()>();
	tokio::select! {
		() = interrupt => {}
		() = terminate => {}
	}
	info!("shutting down");
}
