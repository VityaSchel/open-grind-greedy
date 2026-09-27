use super::parse_yaml;
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

pub struct Config {
	pub homeserver_url: String,
	pub app_service_user: String,
	pub room_ids: Vec<String>,
	pub host: String,
	pub port: u16,
	pub app_service_token: String,
	pub homeserver_token: String,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
	homeserver_url: String,
	app_service_user: String,
	room_ids: Vec<String>,
}

const DEFAULT_HOST: &str = "127.0.0.1";
const CREDENTIALS_DIRECTORY: &str = "CREDENTIALS_DIRECTORY";
const CREDENTIAL_VARIABLES: [&str; 2] = ["APP_SERVICE_TOKEN", "HOMESERVER_TOKEN"];

impl Config {
	pub fn load(path: &Path) -> Result<Self> {
		Self::from_file_and_env(ConfigFile::load(path)?, |key| std::env::var(key).ok())
	}

	fn from_file_and_env(file: ConfigFile, env: impl Fn(&str) -> Option<String>) -> Result<Self> {
		let credentials = env(CREDENTIALS_DIRECTORY)
			.filter(|directory| !directory.is_empty())
			.map(|directory| read_credentials(Path::new(&directory)))
			.transpose()?;
		let lookup = |key: &str| match &credentials {
			Some(credentials) if CREDENTIAL_VARIABLES.contains(&key) => {
				credentials.get(key).cloned()
			}
			_ => env(key),
		};
		let optional = |key: &str| lookup(key).filter(|value| !value.is_empty());
		let required = |key: &str| {
			optional(key).with_context(|| format!("{key} environment variable is required"))
		};

		let ConfigFile { homeserver_url, app_service_user, room_ids } = file;
		Ok(Self {
			homeserver_url,
			app_service_user,
			room_ids,
			host: optional("HOST").unwrap_or_else(|| DEFAULT_HOST.to_string()),
			port: required("PORT")?.parse().context("PORT must be a number between 0 and 65535")?,
			app_service_token: required("APP_SERVICE_TOKEN")?,
			homeserver_token: required("HOMESERVER_TOKEN")?,
		})
	}

	pub fn app_service_localpart(&self) -> &str {
		split_user_id(&self.app_service_user).map_or("", |(localpart, _)| localpart)
	}

	pub fn server_name(&self) -> &str {
		split_user_id(&self.app_service_user).map_or("", |(_, server_name)| server_name)
	}
}

impl ConfigFile {
	fn load(path: &Path) -> Result<Self> {
		let yaml = std::fs::read_to_string(path)
			.with_context(|| format!("cannot read config file '{}'", path.display()))?;
		Self::parse(&yaml).with_context(|| format!("invalid config file '{}'", path.display()))
	}

	fn parse(yaml: &str) -> Result<Self> {
		let mut file: Self = parse_yaml(yaml)?;
		file.homeserver_url = file.homeserver_url.trim_end_matches('/').to_string();
		ensure!(!file.homeserver_url.is_empty(), "homeserver_url must not be empty");
		ensure!(
			split_user_id(&file.app_service_user).is_some(),
			"app_service_user must be a user id like @localpart:server, got {:?}",
			file.app_service_user
		);
		ensure!(!file.room_ids.is_empty(), "room_ids must list at least one room id");
		for room_id in &mut file.room_ids {
			*room_id = room_id.trim().to_string();
			ensure!(
				room_id.len() > 1 && room_id.starts_with('!'),
				"room_ids must hold internal room ids starting with '!', got {room_id:?}"
			);
		}
		Ok(file)
	}
}

fn split_user_id(user_id: &str) -> Option<(&str, &str)> {
	let (localpart, server_name) = user_id.strip_prefix('@')?.split_once(':')?;
	(!localpart.is_empty() && !server_name.is_empty()).then_some((localpart, server_name))
}

fn read_credentials(directory: &Path) -> Result<HashMap<&'static str, String>> {
	CREDENTIAL_VARIABLES
		.into_iter()
		.map(|variable| Ok((variable, read_credential(directory, variable)?)))
		.collect()
}

fn read_credential(directory: &Path, variable: &str) -> Result<String> {
	let name = variable.to_ascii_lowercase().replace('_', "-");
	let path = directory.join(&name);
	let value = std::fs::read_to_string(&path).with_context(|| {
		format!(
			"reading credential {}: is LoadCredential={name} missing from the unit?",
			path.display()
		)
	})?;
	let value = value.trim_end_matches('\n');
	ensure!(!value.is_empty(), "{} is empty", path.display());
	Ok(value.to_string())
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::bot::edited;
	use std::cell::RefCell;
	use tempfile::TempDir;

	const VALID: &str = r#"homeserver_url: https://matrix.test/
app_service_user: "@faq:test"
room_ids: ["!general:test"]
"#;

	fn parse_error(yaml: &str) -> String {
		ConfigFile::parse(yaml).err().map(|e| format!("{e:#}")).unwrap_or_default()
	}

	fn is_rejected(key: &str, value: &str) -> bool {
		ConfigFile::parse(&edited(VALID, key, Some(value))).is_err()
	}

	#[test]
	fn parses_every_key() {
		assert_eq!(
			ConfigFile::parse(VALID).unwrap(),
			ConfigFile {
				homeserver_url: "https://matrix.test".into(),
				app_service_user: "@faq:test".into(),
				room_ids: vec!["!general:test".into()],
			}
		);
	}

	#[test]
	fn repository_config_parses() {
		ConfigFile::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("config.yaml")).unwrap();
	}

	#[test]
	fn rejects_a_missing_key() {
		let error = parse_error(&edited(VALID, "room_ids", None));
		assert!(error.contains("missing field `room_ids`"), "{error}");
	}

	#[test]
	fn rejects_an_unknown_key() {
		let error = parse_error(&format!("{VALID}high_threshold: 0.95\n"));
		assert!(error.contains("unknown field `high_threshold`"), "{error}");
	}

	#[test]
	fn syntax_errors_fit_on_one_journal_line() {
		let error = parse_error(&edited(VALID, "app_service_user", Some("[@faq:test")));
		assert!(error.contains("line 2"), "{error}");
		assert!(!error.contains('\n'), "{error}");
	}

	#[test]
	fn rejects_malformed_values() {
		assert_eq!(
			parse_error(&edited(VALID, "homeserver_url", Some("/"))),
			"homeserver_url must not be empty"
		);
		for user in ["faq", r#""@faq:""#, r#""@:test""#] {
			assert!(is_rejected("app_service_user", user), "{user}");
		}
		assert_eq!(
			parse_error(&edited(VALID, "room_ids", Some("[]"))),
			"room_ids must list at least one room id"
		);
		assert_eq!(
			parse_error(&edited(VALID, "room_ids", Some(r##"["!a:test", "#alias:test"]"##))),
			r##"room_ids must hold internal room ids starting with '!', got "#alias:test""##
		);
		for room_ids in [r#"["!"]"#, r#""!a:test,!b:test""#] {
			assert!(is_rejected("room_ids", room_ids), "{room_ids}");
		}
	}

	#[test]
	fn accepts_room_version_12_ids_without_a_server_part() {
		let room_id = "!b2GcLXF4xhTrmSPXcdGk-9UN40uEPnAz7NuH031cxLk";
		let yaml = edited(VALID, "room_ids", Some(&format!(r#"["{room_id}"]"#)));
		assert_eq!(ConfigFile::parse(&yaml).unwrap().room_ids, [room_id]);
	}

	#[test]
	fn trims_whitespace_inside_quoted_room_ids() {
		let yaml = edited(VALID, "room_ids", Some(r#"["!a:test ", " !b:test"]"#));
		assert_eq!(ConfigFile::parse(&yaml).unwrap().room_ids, ["!a:test", "!b:test"]);
	}

	#[test]
	fn load_errors_name_the_file() {
		let dir = TempDir::new().unwrap();
		let path = dir.path().join("config.yaml");
		let missing = ConfigFile::load(&path).unwrap_err();
		assert_eq!(missing.to_string(), format!("cannot read config file '{}'", path.display()));
		std::fs::write(&path, edited(VALID, "room_ids", Some("[]"))).unwrap();
		let invalid = ConfigFile::load(&path).unwrap_err();
		assert_eq!(invalid.to_string(), format!("invalid config file '{}'", path.display()));
	}

	fn valid_vars() -> Vec<(&'static str, &'static str)> {
		vec![("PORT", "8090"), ("APP_SERVICE_TOKEN", "as"), ("HOMESERVER_TOKEN", "hs")]
	}

	fn with_var<'a>(key: &'static str, value: &'a str) -> Vec<(&'static str, &'a str)> {
		let mut vars = valid_vars();
		vars.retain(|(k, _)| *k != key);
		vars.push((key, value));
		vars
	}

	fn without_var(key: &str) -> Vec<(&'static str, &'static str)> {
		let mut vars = valid_vars();
		vars.retain(|(k, _)| *k != key);
		vars
	}

	fn load(vars: &[(&str, &str)]) -> Result<Config> {
		let map: HashMap<String, String> =
			vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
		Config::from_file_and_env(ConfigFile::parse(VALID)?, |key| map.get(key).cloned())
	}

	fn load_error(vars: &[(&str, &str)]) -> String {
		load(vars).err().map(|e| e.to_string()).unwrap_or_default()
	}

	#[test]
	fn combines_the_file_with_the_environment() {
		let config = load(&valid_vars()).unwrap();
		assert_eq!(config.homeserver_url, "https://matrix.test");
		assert_eq!(config.app_service_user, "@faq:test");
		assert_eq!(config.app_service_localpart(), "faq");
		assert_eq!(config.server_name(), "test");
		assert_eq!(config.room_ids, ["!general:test"]);
		assert_eq!(config.host, "127.0.0.1");
		assert_eq!(config.port, 8090);
		assert_eq!(config.app_service_token, "as");
		assert_eq!(config.homeserver_token, "hs");
	}

	#[test]
	fn host_defaults_when_unset_or_empty() {
		assert_eq!(load(&with_var("HOST", "0.0.0.0")).unwrap().host, "0.0.0.0");
		assert_eq!(load(&with_var("HOST", "")).unwrap().host, "127.0.0.1");
	}

	#[test]
	fn reads_only_deployment_variables() {
		let vars: HashMap<&str, &str> = valid_vars().into_iter().collect();
		let read = RefCell::new(Vec::new());
		Config::from_file_and_env(ConfigFile::parse(VALID).unwrap(), |key| {
			read.borrow_mut().push(key.to_owned());
			vars.get(key).map(|value| value.to_string())
		})
		.unwrap();
		let mut read = read.into_inner();
		read.sort();
		read.dedup();
		assert_eq!(
			read,
			["APP_SERVICE_TOKEN", "CREDENTIALS_DIRECTORY", "HOMESERVER_TOKEN", "HOST", "PORT"]
		);
	}

	#[test]
	fn rejects_a_malformed_port_and_missing_variables() {
		assert!(load(&with_var("PORT", "http")).is_err());
		assert!(load(&with_var("PORT", "65536")).is_err());
		assert_eq!(load_error(&without_var("PORT")), "PORT environment variable is required");
		assert_eq!(
			load_error(&without_var("HOMESERVER_TOKEN")),
			"HOMESERVER_TOKEN environment variable is required"
		);
	}

	fn credentials(files: &[(&str, &str)]) -> TempDir {
		let directory = TempDir::new().unwrap();
		for (file, contents) in files {
			std::fs::write(directory.path().join(file), contents).unwrap();
		}
		directory
	}

	fn credential_files() -> Vec<(&'static str, &'static str)> {
		vec![("app-service-token", "file-as\n"), ("homeserver-token", "file-hs\n")]
	}

	fn with_credentials(directory: &TempDir) -> Vec<(&'static str, &str)> {
		with_var("CREDENTIALS_DIRECTORY", directory.path().to_str().unwrap())
	}

	#[test]
	fn tokens_come_from_credential_files_when_the_directory_is_set() {
		let directory = credentials(&credential_files());
		let config = load(&with_credentials(&directory)).unwrap();
		assert_eq!(config.app_service_token, "file-as");
		assert_eq!(config.homeserver_token, "file-hs");
	}

	#[test]
	fn credentials_do_not_fall_back_to_the_environment() {
		let directory = credentials(&credential_files()[..1]);
		let expected = format!(
			"reading credential {}: is LoadCredential=homeserver-token missing from the unit?",
			directory.path().join("homeserver-token").display()
		);
		assert_eq!(load_error(&with_credentials(&directory)), expected);
	}

	#[test]
	fn rejects_an_empty_credential() {
		let mut files = credential_files();
		files[0].1 = "\n";
		let directory = credentials(&files);
		let expected = format!("{} is empty", directory.path().join("app-service-token").display());
		assert_eq!(load_error(&with_credentials(&directory)), expected);
	}

	#[test]
	fn tokens_come_from_the_environment_without_a_credentials_directory() {
		let config = load(&with_var("CREDENTIALS_DIRECTORY", "")).unwrap();
		assert_eq!(config.app_service_token, "as");
		assert_eq!(config.homeserver_token, "hs");
	}
}
