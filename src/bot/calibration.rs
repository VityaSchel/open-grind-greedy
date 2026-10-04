use super::answering::Thresholds;
use super::parse_yaml;
use anyhow::{Context, Result, ensure};
use serde::Deserialize;

const EMBEDDED: &str = include_str!("../../calibration.yaml");

#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Calibration {
	pub high_threshold: f32,
	pub short_question_threshold: f32,
	pub low_threshold: f32,
	pub mention_threshold: f32,
	pub ambiguity_margin: f32,
}

impl Calibration {
	pub fn embedded() -> Result<Self> {
		Self::parse(EMBEDDED).context("invalid calibration.yaml built into this binary")
	}

	fn parse(yaml: &str) -> Result<Self> {
		let calibration: Self = parse_yaml(yaml)?;
		let Self {
			high_threshold,
			short_question_threshold,
			low_threshold,
			mention_threshold,
			ambiguity_margin,
		} = calibration;
		for (key, value) in [
			("high_threshold", high_threshold),
			("short_question_threshold", short_question_threshold),
			("low_threshold", low_threshold),
			("mention_threshold", mention_threshold),
		] {
			ensure!((0.0..=1.0).contains(&value), "{key} must be between 0 and 1, got {value}");
		}
		ensure!(
			low_threshold <= high_threshold,
			"low_threshold ({low_threshold}) must not exceed high_threshold ({high_threshold})"
		);
		ensure!(
			mention_threshold <= low_threshold,
			"mention_threshold ({mention_threshold}) must not exceed low_threshold ({low_threshold})"
		);
		ensure!(
			short_question_threshold >= high_threshold,
			"short_question_threshold ({short_question_threshold}) must be at least high_threshold ({high_threshold})"
		);
		ensure!(
			ambiguity_margin.is_finite() && ambiguity_margin >= 0.0,
			"ambiguity_margin must be a non-negative number, got {ambiguity_margin}"
		);
		Ok(calibration)
	}

	pub fn unprompted(&self) -> Thresholds {
		Thresholds { question: self.high_threshold, short_question: self.short_question_threshold }
	}

	pub fn raised_hand(&self) -> Thresholds {
		Thresholds { question: self.low_threshold, short_question: self.short_question_threshold }
	}

	pub fn mention(&self) -> Thresholds {
		Thresholds { question: self.mention_threshold, short_question: self.mention_threshold }
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::bot::edited;

	const VALID: &str = "high_threshold: 0.9
short_question_threshold: 0.95
low_threshold: 0.8
mention_threshold: 0.7
ambiguity_margin: 0.05
";

	fn parse_error(yaml: &str) -> String {
		Calibration::parse(yaml).err().map(|e| format!("{e:#}")).unwrap_or_default()
	}

	fn is_rejected(key: &str, value: &str) -> bool {
		Calibration::parse(&edited(VALID, key, Some(value))).is_err()
	}

	#[test]
	fn parses_every_key() {
		assert_eq!(
			Calibration::parse(VALID).unwrap(),
			Calibration {
				high_threshold: 0.9,
				short_question_threshold: 0.95,
				low_threshold: 0.8,
				mention_threshold: 0.7,
				ambiguity_margin: 0.05,
			}
		);
	}

	#[test]
	fn embedded_calibration_is_valid() {
		Calibration::embedded().unwrap();
	}

	#[test]
	fn rejects_a_missing_key() {
		for key in [
			"high_threshold",
			"short_question_threshold",
			"low_threshold",
			"mention_threshold",
			"ambiguity_margin",
		] {
			let error = parse_error(&edited(VALID, key, None));
			assert!(error.contains(&format!("missing field `{key}`")), "{error}");
		}
	}

	#[test]
	fn rejects_an_unknown_key() {
		let error = parse_error(&format!("{VALID}min_words: 4\n"));
		assert!(error.contains("unknown field `min_words`"), "{error}");
	}

	#[test]
	fn rejects_invalid_thresholds() {
		for value in [".nan", ".inf", "-0.1", "1.01", "high"] {
			assert!(is_rejected("high_threshold", value), "{value}");
			assert!(is_rejected("short_question_threshold", value), "{value}");
			assert!(is_rejected("low_threshold", value), "{value}");
			assert!(is_rejected("mention_threshold", value), "{value}");
		}
		assert_eq!(
			parse_error(&edited(VALID, "low_threshold", Some("0.95"))),
			"low_threshold (0.95) must not exceed high_threshold (0.9)"
		);
		assert_eq!(
			parse_error(&edited(VALID, "mention_threshold", Some("0.85"))),
			"mention_threshold (0.85) must not exceed low_threshold (0.8)"
		);
		assert_eq!(
			parse_error(&edited(VALID, "short_question_threshold", Some("0.85"))),
			"short_question_threshold (0.85) must be at least high_threshold (0.9)"
		);
	}

	#[test]
	fn short_question_threshold_may_equal_high_threshold() {
		Calibration::parse(&edited(VALID, "short_question_threshold", Some("0.9"))).unwrap();
	}

	#[test]
	fn only_mentions_hold_short_questions_to_their_own_threshold() {
		let calibration = Calibration::parse(VALID).unwrap();
		assert_eq!(calibration.unprompted().for_question("install it?"), 0.95);
		assert_eq!(calibration.raised_hand().for_question("install it?"), 0.95);
		assert_eq!(calibration.mention().for_question("install it?"), 0.7);
		assert_eq!(calibration.raised_hand().for_question("how do I install it?"), 0.8);
	}

	#[test]
	fn rejects_an_invalid_margin() {
		for value in ["-0.01", ".nan", ".inf", "wide"] {
			assert!(is_rejected("ambiguity_margin", value), "{value}");
		}
	}
}
