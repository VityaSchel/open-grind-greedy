# Greedy

Open Grind's Matrix chat bot.

- Knowledge base powered by [fastembed](https://github.com/Anush008/fastembed-rs) (ONNX Runtime, CPU-only) local embedding model

## Install

1. Install a C/C++ toolchain and Rust 1.89 or newer with [rustup](https://rustup.rs); On Linux the prebuilt ONNX Runtime needs glibc 2.39 and GCC 13's libstdc++ or newer (Ubuntu 24.04, Debian 13, Arch); Debian 12 and Ubuntu 22.04 fail to link
2. Run `cargo build --release --locked`, to download ONNX Runtime on the first build
3. `install -Dm755 target/release/greedy /usr/local/bin/greedy`

## Setup with systemd

1. Systemd unit: `install -Dm644 contrib/greedy.service /etc/systemd/system/open-grind-greedy.service`
2. Configuration: `install -d -m755 /etc/open-grind-greedy`
3. FAQ and config: `install -m644 faq.json config.yaml /etc/open-grind-greedy/`
4. Tokens:
   ```sh
   install -d -m700 /etc/open-grind-greedy/credentials
   cd /etc/open-grind-greedy/credentials
   umask 077
   printf %s "$(openssl rand -hex 32)" > app-service-token
   printf %s "$(openssl rand -hex 32)" > homeserver-token
   ```
5. Get registration config, `HOST` and `PORT` as in the service unit: `(HOST=127.0.0.1 PORT=8090 CREDENTIALS_DIRECTORY=/etc/open-grind-greedy/credentials exec greedy registration --config /etc/open-grind-greedy/config.yaml)`
6. Continuwuity: send `!admin appservices register` to the admin room with the YAML in a code block in the same message
7. Set the display name:
   ```sh
   curl -f -X PUT --json '{"displayname":"Greedy"}' \
   	-H "Authorization: Bearer $(cat /etc/open-grind-greedy/credentials/app-service-token)" \
   	"https://matrix.opengrind.org/_matrix/client/v3/profile/@greedy:opengrind.org/displayname"
   ```
8. Run `systemctl daemon-reload && systemctl enable --now open-grind-greedy` and wait for `greedy appservice listening` in `journalctl -u open-grind-greedy -f`. The first start downloads the model into `/var/cache/open-grind-greedy/`, and every start embeds the whole FAQ, which takes a few minutes.
9. Invite `app_service_user` to each room in `room_ids`

## Environment variables

| Variable            | Description                                                     |
| ------------------- | --------------------------------------------------------------- |
| `PORT`, `HOST`      | Listen address, set in the unit; `HOST` defaults to `127.0.0.1` |
| `APP_SERVICE_TOKEN` | `as_token`                                                      |
| `HOMESERVER_TOKEN`  | `hs_token`                                                      |

When `$CREDENTIALS_DIRECTORY` is set, as by `LoadCredential=` in [the unit](contrib/greedy.service), the tokens are read from the files `$CREDENTIALS_DIRECTORY/app-service-token` and `$CREDENTIALS_DIRECTORY/homeserver-token`.

## [faq.json](./faq.json)

A JSON array of entries; unknown fields are rejected.

```json
[
	{
		"id": "report-bug",
		"question": "How do I report a bug?",
		"paraphrases": ["where do we send logs if trying to be helpful"],
		"answer": "Search the [issue tracker](https://git.opengrind.org/open-grind/open-grind/issues) first."
	}
]
```

- `id`: unique.
- `question`: the canonical phrasing, shown in question lists.
- `paraphrases`: optional. An entry scores its best match across the question and paraphrases.
- `answer`: markdown.

Model used is `bge-small`.

## CLI

`greedy bot [--config <path, default ./config.yaml>] [--faq <path, default ./faq.json>]` serves the appservice.

`greedy registration [--config <path, default ./config.yaml>]` prints the appservice registration YAML.

`greedy match [--faq <path, default ./faq.json>] [--quiet] --query <text> [--threshold 0.75]` prints JSON and exits 0 on a match, 1 without one and 2 on errors. `id` is `null` below the threshold; `candidates` holds the top 5 (shortened below).

```json
{"candidates":[{"id":"download-links","score":0.837},{"id":"how-to-install","score":0.811}],"id":"download-links","matched":true,"score":0.837}
```

`greedy calibrate [--faq <path, default ./faq.json>] [--quiet] --samples <file> [--threshold 0.75]` scores real chat messages, one per line (`#` lines are skipped). It prints the question-like ones (a `?`, at least 4 words, not starting with `>`) scoring at or above the threshold, then a histogram and a sweep of thresholds from 0.50 to 0.90 over their scores.

## Development

```sh
cargo test               # unit tests and bot tests against a mock homeserver
cargo test -- --ignored  # model-backed tests; download the model on first run
cargo clippy --all-targets -- -D warnings
```
