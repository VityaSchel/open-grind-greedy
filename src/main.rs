use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use greedy::{FastembedEmbedder, Matcher, bot, load_faq};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "greedy", version, about = "Open Grind's Matrix chat bot and FAQ matching tools")]
struct Cli {
	/// Suppress everything except results (for scripting)
	#[arg(long, global = true)]
	quiet: bool,
	#[command(subcommand)]
	command: Command,
}

#[derive(Args)]
struct CommonArgs {
	/// Path to the FAQ JSON file
	#[arg(long, default_value = "./faq.json")]
	faq: PathBuf,
}

#[derive(Subcommand)]
enum Command {
	/// One-shot match; prints JSON, exit code 0 if matched and 1 if not
	Match {
		#[command(flatten)]
		common: CommonArgs,
		/// Minimum similarity for a confident match
		#[arg(long, default_value_t = 0.75)]
		threshold: f32,
		/// Chat message to match against the FAQ
		#[arg(long)]
		query: String,
	},
	/// Score a file of real chat messages to help pick a threshold
	Calibrate {
		#[command(flatten)]
		common: CommonArgs,
		/// Minimum similarity for a confident match
		#[arg(long, default_value_t = 0.75)]
		threshold: f32,
		/// Plain-text file with one message per line (lines starting with '#' are ignored)
		#[arg(long)]
		samples: PathBuf,
	},
	/// Serve the Matrix appservice that answers FAQ questions
	Bot {
		/// Path to the deployment config YAML file
		#[arg(long, default_value = "./config.yaml")]
		config: PathBuf,
		#[command(flatten)]
		common: CommonArgs,
	},
	/// Print the Matrix appservice registration YAML
	Registration {
		/// Path to the deployment config YAML file
		#[arg(long, default_value = "./config.yaml")]
		config: PathBuf,
	},
}

fn main() -> ExitCode {
	let cli = Cli::parse();
	match run(cli) {
		Ok(code) => code,
		Err(err) => {
			eprintln!("error: {err:#}");
			ExitCode::from(2)
		}
	}
}

fn run(cli: Cli) -> Result<ExitCode> {
	match cli.command {
		Command::Match { common, threshold, query } => {
			one_shot_match(&common, cli.quiet, threshold, &query)
		}
		Command::Calibrate { common, threshold, samples } => {
			calibrate(&common, cli.quiet, threshold, &samples)?;
			Ok(ExitCode::SUCCESS)
		}
		Command::Bot { config, common } => {
			bot::run(&config, &common.faq, cli.quiet)?;
			Ok(ExitCode::SUCCESS)
		}
		Command::Registration { config } => {
			print!("{}", bot::registration_yaml(&bot::Config::load(&config)?));
			Ok(ExitCode::SUCCESS)
		}
	}
}

fn build_matcher(common: &CommonArgs, quiet: bool) -> Result<Matcher> {
	let entries = load_faq(&common.faq)?;
	if !quiet {
		let texts: usize = entries.iter().map(|e| 1 + e.paraphrases.len()).sum();
		eprintln!(
			"loaded {} FAQ entries ({texts} texts) from '{}'",
			entries.len(),
			common.faq.display()
		);
	}
	let embedder = FastembedEmbedder::load_or_download(quiet)?;
	Matcher::new(Box::new(embedder), entries)
}

fn one_shot_match(
	common: &CommonArgs,
	quiet: bool,
	threshold: f32,
	query: &str,
) -> Result<ExitCode> {
	let matcher = build_matcher(common, quiet)?;
	let ranked = matcher.rank(query)?;
	let top = ranked.first();
	let matched = top.is_some_and(|m| m.score >= threshold);
	let output = serde_json::json!({
		"matched": matched,
		"id": if matched { top.map(|m| m.id.as_str()) } else { None },
		"score": top.map_or(0.0, |m| m.score),
		"candidates": &ranked[..ranked.len().min(5)],
	});
	println!("{output}");
	Ok(if matched { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

fn calibrate(common: &CommonArgs, quiet: bool, threshold: f32, samples: &Path) -> Result<()> {
	let matcher = build_matcher(common, quiet)?;
	let text = std::fs::read_to_string(samples)
		.with_context(|| format!("cannot read samples file '{}'", samples.display()))?;
	let messages: Vec<&str> = text
		.lines()
		.map(str::trim)
		.filter(|line| !line.is_empty() && !line.starts_with('#'))
		.collect();
	if messages.is_empty() {
		bail!("no messages found in '{}'", samples.display());
	}
	let id_width = matcher.entries().iter().map(|e| e.id.len()).max().unwrap_or(6);
	let mut scores = Vec::with_capacity(messages.len());
	for message in &messages {
		match matcher.rank(message)?.first() {
			Some(top) => {
				if top.score >= threshold && message.contains("?") && message.split_whitespace().count() >= 4 && message.split("\n").count() <= 2 && !message.starts_with(">") {
					println!("{:.3}  {:<id_width$}  {}", top.score, top.id, display_text(message, 120));
					scores.push(top.score);
				}
			}
			None => println!("  -    {:<id_width$}  {}", "(none)", display_text(message, 60)),
		}
	}
	print_histogram(&scores);
	print_threshold_sweep(&scores);
	Ok(())
}

fn display_text(text: &str, max_chars: usize) -> String {
  let mut result = String::new();
  let mut line_len = 0;

  for word in text.split_whitespace() {
    let word_len = word.chars().count();

    if line_len == 0 {
      result.push_str(word);
      line_len = word_len;
    } else if line_len + 1 + word_len <= max_chars {
      result.push(' ');
      result.push_str(word);
      line_len += 1 + word_len;
    } else {
      result.push('\n');
      result.push('\t');
      result.push('\t');
      result.push('\t');
      result.push('\t');
      result.push(' ');
      result.push(' ');
      result.push_str(word);
      line_len = 1 + word_len; // account for the tab indentation
    }
  }

  result
}

fn print_histogram(scores: &[f32]) {
	let mut buckets = [0usize; 20];
	for &score in scores {
		buckets[(score.clamp(0.0, 0.999_9) * 20.0) as usize] += 1;
	}
	println!();
	println!("score histogram (top-1 scores, n={}):", scores.len());
	let first = buckets.iter().position(|&count| count > 0).unwrap_or(0);
	let last = buckets.iter().rposition(|&count| count > 0).unwrap_or(0);
	for (i, &count) in buckets.iter().enumerate().take(last + 1).skip(first) {
		let low = i as f32 * 0.05;
		println!("{low:.2}-{:.2}  {} {count}", low + 0.05, "#".repeat(count));
	}
}

fn print_threshold_sweep(scores: &[f32]) {
	println!();
	println!("messages at or above threshold:");
	let total = scores.len();
	for step in 0..=8 {
		let threshold = (50 + step * 5) as f32 / 100.0;
		let count = scores.iter().filter(|&&score| score >= threshold).count();
		println!(
			">= {threshold:.2}  {count}/{total}  ({:.1}%)",
			100.0 * count as f32 / total as f32
		);
	}
}
