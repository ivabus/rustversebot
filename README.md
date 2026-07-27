# AI usage disclosure

Almost entire project was written using Deepseek V4 Pro and GPT-5.6 Sol / Terra, except the initial rustverse_svg "renderer" and templates, which were done solely by me.

# rustversebot

`rustversebot` is a Telegram bot that tracks Zenless Zone Zero endgame results.
It supports Deadly Assault and Shiyu Defense.

## Requirements

- Get a Telegram bot token from [@BotFather](https://t.me/BotFather).
- Get the Telegram user ID of the administrator.

The Cargo workspace contains these crates:

```text
crates/
├── nanoka/
├── rustverse/
├── rustverse_svg/
└── rustversebot/
```

## Configuration and startup

The application does not load `.env` automatically.
Export the variables from `.env.example`.
You can also configure them in a process manager.

```sh
export TELOXIDE_TOKEN="..."
export BOT_ADMIN_ID="123456789"
export TURSO_DATABASE_URL="file:local.db"
export RUST_LOG="rustversebot=info"
cargo run --package rustversebot
```

`TURSO_DATABASE_URL=file:local.db` stores the database in a local file.
This configuration does not require a separate server or token.
The application creates tables and runs migrations automatically.

The scheduler uses these variables:

- `BOT_SCHEDULER_INTERVAL_SECS` sets the polling interval. The default is `300`.
- `BOT_CHECKPOINT_WINDOW_SECS` sets the checkpoint delivery window. The default is `300`.
- `BOT_REQUEST_SPACING_MS` sets the delay between HoYoLAB requests. The default is `500`.
- `BOT_RETRY_ATTEMPTS` sets the retry count. The default is `3`.
- `BOT_RETENTION_DAYS` sets the result retention period. The default is `90`.
- `BOT_ANNOUNCEMENT_LEAD_HOURS` sets the season announcement lead time. The default is `24`.

The bot does not send an expired checkpoint or a late season announcement.
It always keeps the last snapshot in each result series.

The `nanoka` crate supplies future seasons, enemy data, and image URLs.
The bot stores production season indexes by mode and season ID.
It updates an index after the current season ends.
It then announces the next season during the configured window.
The bot does not cache season details or cards.
It renders each card once and sends it once to each eligible chat.

## Web dashboard

The read-only dashboard and JSON API are disabled by default.
Set a bind address to start them:

```sh
export BOT_WEB_BIND="127.0.0.1:8080"
```

Set the public URL when users access the dashboard through HTTPS:

```sh
export BOT_PUBLIC_WEB_URL="https://zzz.example.com"
```

This URL adds a history button to `/da` and `/shiyu` results.
The bot does not substitute a local URL.

The server provides these routes:

- `/`
- `/healthz`
- `/api/chats`
- `/api/chats/{chat_id}/leaderboard/{kind}`
- `/api/users/{uid}/history`

The `kind` value is `deadly_assault` or `shiyu_defense`.
The dashboard has no authentication.
Do not expose it directly to the internet.
Use a loopback address or a trusted reverse proxy with TLS and authentication.

If the configuration file is invalid or unavailable, the bot uses its built-in
configuration.
It also writes a warning to the log.

## HoYoLAB cookie

The administrator sets the cookie with `/cookie <cookie_string>`.
The bot stores the cookie as plain text in the local database.

1. Use a private chat with the bot.
2. Delete the command message after configuration.
3. Restrict access to the database file.
4. Do not commit `.env`, tokens, logs, or database exports.

## Verification

```sh
cargo fmt --all --check
cargo test --release --workspace
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
```

## Nix and Attic

The flake publishes the package only for `aarch64-darwin`.

```sh
nix build .#rustversebot
nix run .#rustversebot
```

GitHub Actions configures Attic as a substituter before the build.
The workflow first builds and uploads the `cargoArtifacts` dependencies.
It then builds the bot and uploads its runtime closure.
Unchanged build inputs let later builds reuse the dependency artifacts.
The workflow runs on a native ARM64 macOS runner.

Create these secrets in the `Actions` GitHub Environment:

- `ATTIC_SERVER` contains the server URL.
- `ATTIC_TOKEN` contains a token with cache push access.

The optional `ATTIC_CACHE` variable changes the cache name.
The default cache name is `rustversebot`.

Get the public cache endpoint and key from the Attic administrator:

```sh
attic cache info rustversebot
```

Add them to `~/.config/nix/nix.conf`:

```ini
extra-substituters = https://attic.example.org/rustversebot
extra-trusted-public-keys = rustversebot:BASE64_PUBLIC_KEY
```

Restart the Nix daemon.
Then install or run the package:

```sh
nix profile install github:OWNER/rustversebot#rustversebot
rustversebot

# Run without installation.
nix run github:OWNER/rustversebot#rustversebot
```

Before startup, export `TELOXIDE_TOKEN`, `BOT_ADMIN_ID`, and
`TURSO_DATABASE_URL`.

## Telegram templates

The `templates/` directory contains Telegram message templates.
The file suffix selects the send mode:

- `*.txt.j2` sends plain text without a parse mode.
- `*.html.j2` uses `ParseMode::Html`.

MiniJinja automatically escapes template values for HTML.
The registry test rejects an unsupported suffix or an unregistered file.

## Season details and comparisons

The `/previous`, `/current`, and `/next` commands send one two-image album.
The album contains Deadly Assault and Shiyu Defense season cards.
The Shiyu Defense card shows stage five.

The next season must directly follow the current season.
If Nanoka lacks either mode, the bot reports that the full pair is unavailable.

The `/da` and `/shiyu` commands accept a UID or nickname.
Without an argument, the bot shows buttons for UIDs in the current chat.
Each callback checks chat membership again.
This check prevents an old or modified button from exposing another chat's data.

The result caption compares the two most recent seasons.
It shows the score change.
For Deadly Assault, it also shows the star change.
For Shiyu Defense, it shows the change in cleared floors.
The bot omits the comparison until a previous season exists.
