# AI usage disclosure

Almost entire project was written using Deepseek V4 Pro and GPT-5.6 Sol / Terra, except the initial rustverse_svg "renderer" and templates, which were done solely by me.

# rustversebot

`rustversebot` is a Telegram bot that tracks Zenless Zone Zero endgame results.
It supports Deadly Assault and Shiyu Defense.

## Requirements

- Get a Telegram bot token from [@BotFather](https://t.me/BotFather).
- Get the Telegram user ID of the administrator.
- Install the `typst` and `resvg` CLI executables. The Nix package includes both.

The Cargo workspace contains these crates:

```text
crates/
├── nanoka/
├── rustverse/
├── rustverse_images/
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

The shared leaderboard follows the Europe game server (fixed UTC+1). A 04:00
server release is 06:00 Moscow time. The 24-hour checkpoint opens at 06:00
Moscow time the following day. The 14-day report uses the ending season's
results even when the next season has already opened. Telegram and the
dashboard show dates in Moscow time.

The `nanoka` crate supplies future seasons, enemy data, and image URLs.
The bot stores production season indexes by mode and season ID.
It refreshes the indexes hourly and checks the announcement window on each
scheduler poll. Deadly Assault uses both five- and six-digit IDs with a
published `live_begin`. A season can be announced while the current one is
still running. Delivery stops when the new season starts.
The bot does not cache season details or cards.
It renders each card once and sends it once to each eligible chat.

## Image rendering

`rustverse_images` runs `typst compile --format svg` and pipes its stdout directly
into `resvg -z 4 - output.png`. Each render uses a private temporary directory.
Templates, static images, and the font are bundled into the Rust binary.

Before compilation, remote PNG and WebP URLs are collected recursively from the
input JSON and downloaded into `IMAGE_CACHE_DIR` (default: `images`). Filenames
are preserved and URLs in JSON are unchanged. Only complete downloads enter the
cache. HTTP 404 is reported and leaves the image absent; Typst fails if the
template needs that image. Other download errors and CLI failures stop the render.

`TYPST_BIN` and `RESVG_BIN` can override the executable paths. Compilation and
rasterization have a combined 120-second timeout and are cancelled with the task.

```sh
cargo run --release --package rustverse_images -- \
  individual_shiyu.typ player.json /tmp/player.png season.json
cargo run --release --package rustverse_images --features render-season \
  --example render_season -- 69041 /tmp/deadly.png
```

The generic CLI takes a template name, input JSON, output PNG, and optionally a
second JSON file. Individual Shiyu needs the matching Nanoka season as that
second file. Deadly season JSON must include `begin_time` and `end_time`; the
season example and bot supply them from the season index.

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
The chat leaderboards are `/top_da` and `/top_shiyu`.
Without an argument, the bot shows buttons for UIDs in the current chat.
Each callback checks chat membership again.
This check prevents an old or modified button from exposing another chat's data.

The result caption compares the two most recent seasons.
It shows the score change.
For Deadly Assault, it also shows the star change.
For Shiyu Defense, it shows the change in cleared floors.
The bot omits the comparison until a previous season exists.
