use super::config::Config;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

const REGEX_METACHARACTERS: &[char] =
	&['.', '+', '*', '?', '(', ')', '|', '[', ']', '{', '}', '^', '$', '\\'];

pub fn registration_yaml(config: &Config) -> String {
	let localpart = config.app_service_localpart();
	let regex =
		quoted(&format!("^@{}:{}$", escape_regex(localpart), escape_regex(config.server_name())));
	let id = quoted(localpart);
	let url = quoted(&app_service_url(config));
	let as_token = quoted(&config.app_service_token);
	let hs_token = quoted(&config.homeserver_token);
	format!(
		"id: {id}\nurl: {url}\nas_token: {as_token}\nhs_token: {hs_token}\nsender_localpart: {id}\nrate_limited: false\nnamespaces:\n  users:\n    - exclusive: true\n      regex: {regex}\n  aliases: []\n  rooms: []\n"
	)
}

fn app_service_url(config: &Config) -> String {
	let host = match config.host.parse::<IpAddr>() {
		Ok(IpAddr::V4(ip)) if ip.is_unspecified() => Ipv4Addr::LOCALHOST.to_string(),
		Ok(IpAddr::V6(ip)) if ip.is_unspecified() => format!("[{}]", Ipv6Addr::LOCALHOST),
		Ok(IpAddr::V6(ip)) => format!("[{ip}]"),
		_ => config.host.clone(),
	};
	format!("http://{host}:{}", config.port)
}

fn escape_regex(text: &str) -> String {
	text.chars().fold(String::new(), |mut escaped, character| {
		if REGEX_METACHARACTERS.contains(&character) {
			escaped.push('\\');
		}
		escaped.push(character);
		escaped
	})
}

fn quoted(value: &str) -> String {
	format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn config(app_service_user: &str, host: &str) -> Config {
		Config {
			homeserver_url: "https://matrix.example.org".into(),
			app_service_user: app_service_user.into(),
			room_ids: vec!["!general:example.org".into()],
			host: host.into(),
			port: 8090,
			app_service_token: "as-token".into(),
			homeserver_token: "hs-token".into(),
		}
	}

	fn url_line(host: &str) -> String {
		registration_yaml(&config("@faq:example.org", host))
			.lines()
			.find(|line| line.starts_with("url: "))
			.unwrap()
			.to_string()
	}

	#[test]
	fn renders_the_registration_for_a_typical_config() {
		assert_eq!(
			registration_yaml(&config("@faq:example.org", "127.0.0.1")),
			"id: 'faq'
url: 'http://127.0.0.1:8090'
as_token: 'as-token'
hs_token: 'hs-token'
sender_localpart: 'faq'
rate_limited: false
namespaces:
  users:
    - exclusive: true
      regex: '^@faq:example\\.org$'
  aliases: []
  rooms: []
"
		);
	}

	#[test]
	fn escapes_regex_metacharacters_and_keeps_the_port_colon() {
		let yaml = registration_yaml(&config("@a.b+c:example.org:8448", "127.0.0.1"));
		assert!(yaml.contains(r"regex: '^@a\.b\+c:example\.org:8448$'"), "{yaml}");
		assert!(yaml.contains("id: 'a.b+c'\n"));
	}

	#[test]
	fn points_url_at_a_reachable_address() {
		assert_eq!(url_line("0.0.0.0"), "url: 'http://127.0.0.1:8090'");
		assert_eq!(url_line("::"), "url: 'http://[::1]:8090'");
		assert_eq!(url_line("::1"), "url: 'http://[::1]:8090'");
		assert_eq!(url_line("10.0.0.2"), "url: 'http://10.0.0.2:8090'");
		assert_eq!(url_line("faq.internal"), "url: 'http://faq.internal:8090'");
	}

	#[test]
	fn quotes_values_for_yaml() {
		let mut config = config("@faq:example.org", "127.0.0.1");
		config.app_service_token = "it's: #1".into();
		assert!(registration_yaml(&config).contains("as_token: 'it''s: #1'\n"));
	}
}
