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
├── rustverse_ai/
├── rustverse_images/
└── rustversebot/
```

## GachaBase agent diff analysis

The standalone `rustverse_ai` CLI takes a GachaBase agent diff URL and prints a
Russian report. It compares the previous and current agent data in English and
Simplified Chinese and sends the changed fields from both languages to the
[OpenAI Responses API](https://developers.openai.com/api/reference/resources/responses/methods/create).
GachaBase serves Simplified Chinese as `chs`; the model receives it under `cn`.

The Rig agent chooses when to look up evidence, revise its interpretation and
return an answer. A run has up to 12 model turns with a final text-only turn at
the limit. Empty normalized diffs produce a local report. The analysis compares
the Core Passive at its highest available rank and other skills at level 12.

- `list_recent_revisions` lists recent design revisions from beta and release.
- `search_gachabase` searches a revision's catalog by category, localized name,
  alias, slug or ID. The categories are agents, W-Engines, drive discs,
  Bangboo and tutorials. For agents an empty query lists the entire matching
  roster in one response. Set `specialty`, `element`, or both using names from
  the selected language. For example, setting `Anomaly` and `Fire` in English selects
  agents matching both. Named searches can use the same filters.
- `get_gachabase_entry` reads a category and ID in English or Simplified Chinese.
  An optional exact version pins the result to a revision, including release
  revisions. Both lookup tools reuse a pinned revision. Questions default to the
  newest design revision across beta and release by GachaBase design timestamp.
  Reports default to their target revision. A null selector returns compact
  pages, skills or effects, with presentation colors removed. Copy `next_offset`
  to continue at the next record. A `read` pointer identifies an oversized record.
  Use a JSON pointer for fields, `@raw` for the original paginated DTO with all ranks,
  or `text:<phrase>` to locate mentions in descriptions.
  For agents, `@team` reads the Core Passive and Additional Ability summaries
  with paths to their full conditions. In an interactive team comparison using
  this view, the next turn after 24 catalog lookups switches to calculation
  or a final answer. The same switch occurs with two tool turns left.
  Agent reads accept `skill_level` and `core_level`. Reports use level 12 and maximum
  Core Passive rank by default. Large diffs list selectors per group. Each group
  pages whole changes and supplies separate read pointers for oversized values.
- `compute` evaluates short Rhai scripts on numeric fields from the current run.
  Bindings reference immutable sources and JSON pointers. For instance,
  `/skill_data/*/damage_multiplier_percent` selects an array. With two such
  bindings, `sum(x1) - sum(x0)` computes the difference between their totals.
  Execution has bounded inputs, operations and output and exposes no I/O.

The CLI reads the API key from `OPENAI_API_KEY` or, when it is unset or blank,
from a single line in `~/.openai`. To analyze Sunbringer, run:

```sh
cargo run --release -p rustverse_ai -- \
  'https://zzz.gachabase.net/diff/agents/1601/sunbringer?prev=beta_3.4.5_20694646&cur=beta_3.4.5_20699967&lang=en'
```

The default model is `gpt-6.2-sol` with `high` reasoning effort. In Telegram the
administrator can use `/model` to inspect the current setting or
`/model gpt-6.2-sol@medium` to change it. The setting is stored in libSQL
and applies to new prompts and changelog reports across all chats. Use
`@default` for the provider's default reasoning setting. Before the first
override the bot uses `OPENAI_MODEL` or its built-in default.
For the CLI, select another Responses API model with
`--model` or `OPENAI_MODEL`. Rust callers can use
`rustverse_ai::analyze(url, api_key, model).await`.

### Telegram beta pages

Для локальной правки страниц запустите из корня проекта:

```sh
./dev/beta
```

Откройте [предпросмотр](http://127.0.0.1:8765/). При первом запуске скрипт копирует
пример Кафки в `target/beta-preview/report.md`. Редактируйте этот Markdown или
`crates/rustverse_images/beta_agent.typ`: после сохранения страница обновляется.
Готовый PNG лежит в `target/beta-preview/public/page.png`. Ошибки Typst видны
в терминале; исправьте шаблон и сохраните его, чтобы повторить рендер.
Для остановки нажмите Ctrl+C.

Нужны Python 3, Cargo, Typst и resvg. На macOS Apple Silicon их также можно
получить командой `nix develop -c ./dev/beta`. Предпросмотр использует
сохранённые данные. Ключ OpenAI нужен только для запуска нового анализа.

```sh
./dev/beta render                     # Однократный рендер рабочего отчёта
./dev/beta watch --report my-report.md --port 8766
./dev/beta test                       # Тесты workspace без сети, включая рендер PNG
./dev/beta analyze 'https://zzz.gachabase.net/diff/agents/1651/kafka?prev=beta_3.4.4_20541895&cur=beta_3.4.5_20691792&lang=chs' > my-report.md
```

`/prompt <вопрос>` asks the same agent a standalone question. Each reply opens
with the user's original question as a Telegram quote. In private chats
the bot streams temporary Telegram drafts and sends the completed answer as
persistent messages. Both use Telegram HTML for emphasis, quotations, code and
HTTP(S) links. In groups a newly announced research stage immediately updates
the same progress message. Text and counter updates are throttled to once per
15 seconds, and Telegram's retry delays apply to all progress edits. The initial
status and the final footer show the model, tool calls, model turns and the
confirmed token usage across completed turns. Beside the tokens the bot displays
an estimated USD total for the current request using Rig's per-turn usage costs.
Catalog estimates use standard token rates. A dash means pricing is unavailable
or incomplete. Each follow-up starts a new total. The agent announces a new
research objective once. Further lookups and pages keep that status until the
objective changes. Missing token usage also appears as a dash. The footer
appears once when a long answer spans multiple messages.

Reply to any part of a completed answer with a text question to continue, or
use `/prompt <question>` as a reply. A standalone command starts a new conversation.
libSQL caches each delivered question and answer for 24 hours. The agent receives
the unexpired turns leading to that specific answer in the same chat and topic.
In groups, participants can continue a visible answer under their own access
and rate limits. Separate replies to an earlier answer create independent branches.

`/diff` opens a beta/release picker. Choose the starting and ending design
revisions for a direct comparison. `/diff <prev> <cur>` accepts exact names.
`/betadiff` compares the latest GachaBase beta design revision with its immediate
predecessor, including hotfixes within the same version. Use
`/betadiff beta_3.4.4_20541895 beta_3.4.5_20691792` for an explicit range.
The bot sends one album per revision in chronological order, with a separate PNG
for each changed character. A single page is sent on its own. For more than ten
pages it sends numbered albums. Captions link each character to the exact GachaBase diff.

Manual AI commands share one request per Telegram user per 60 seconds across
chats. Opening the picker is free. The administrator bypasses the cooldown and
the manual range budget, and can use `/resetlimit <telegram_id>` to reset a
user's cooldown. `BETA_MAX_NEW_REPORTS` defaults to 10 uncached reports per
manual comparison. The scheduler tracks its own delivery progress.
Manual generation runs separately from Telegram update handling with four active
jobs and eight queue slots. A full queue replies immediately and consumes no
cooldown. In-flight and queued jobs stop when the bot shuts down.

The administrator can open `/aiusers [page]` in a private chat to look up
Telegram IDs, names, last AI command, admission decision, Moscow time and
request count. The access journal stores each attempt, including denials.
Question text, model answers and credentials are excluded from its schema.
`/aiban <telegram_id>` and `/aiunban <telegram_id>` revoke and restore access
to manual AI commands across all chats. A cooldown reset preserves the ban.

With an OpenAI key configured for the bot, the scheduler monitors the changelog
for chats with registered UIDs. The first poll records independent beta and release
baselines per chat. Later polls deliver missed revisions in order within each branch. Reports
are cached in libSQL by revision pair, agent, OpenAI model and a fingerprint of
the system instructions, review prompt and tools. Commands and hotfix polls
reuse this cache across chats and restarts. Earlier unversioned reports are
recomputed on demand. A confirmed album records all its page deliveries atomically.
An album containing an oversized page is sent as a group of PNG documents.

`rustverse_images` renders each page at 1280 pixels wide with automatic height,
using the Deadly Assault palette, cards, font and watermark. To render a saved report:

```sh
cargo run --release -p rustverse_images -- beta kafka.md kafka.png
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

Chats with a registered UID automatically receive new ZZZ promo codes with
redemption links and rewards. On the first successful poll for a chat, the bot
saves the current list silently. It checks for new codes on every scheduler tick
and records confirmed deliveries per chat across restarts. Use `/promocodes`,
`/codes` or `/promo` to see the current list at any time.

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
