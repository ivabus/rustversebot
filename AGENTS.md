# AGENTS.md

## Project overview

`rustversebot` is a Rust Telegram bot for tracking Zenless Zone Zero endgame
results. It stores data in async libSQL/Turso, renders Telegram messages from
MiniJinja templates, generates PNG cards through `rustverse_svg`, and exposes
an optional lightweight Axum dashboard.

The project is a single Cargo workspace:

- `crates/rustverse` — HoYoLAB client and player result models;
- `crates/rustverse_svg` — SVG templates and PNG rendering;
- `crates/nanoka` — public Nanoka season catalogue, boss stats, and image URLs;
- `crates/rustversebot` — the Telegram bot.

Changes to a shared contract may therefore require coordinated edits in another
workspace crate. Do not copy their models or implementations between crates.

## Build and verification

Run from the workspace root:

```sh
cargo fmt --all
CARGO_TARGET_DIR=/tmp/rustversebot-svg-release cargo test --release --workspace
```

When `rustverse_svg` is changed, also run:

```sh
rustfmt --edition 2024 crates/rustverse_svg/src/lib.rs
CARGO_TARGET_DIR=/tmp/rustversebot-svg-release \
  cargo test --release --package rustverse_svg --lib
```

Use release-mode tests by default: the season PNG fixtures are rasterized and
debug builds waste substantial time. Run `cargo clippy` when a change merits a
lint pass, but do not substitute it for the release test suites above.

For visual changes to season infographics, render real data after the tests:

```sh
CARGO_TARGET_DIR=/tmp/rustversebot-svg-release \
  cargo run --release --package rustverse_svg --example render_season -- \
  69041 /tmp/rustversebot-deadly.png
CARGO_TARGET_DIR=/tmp/rustversebot-svg-release \
  cargo run --release --package rustverse_svg --example render_season -- \
  62053 /tmp/rustversebot-shiyu.png
```

Inspect the generated PNGs rather than relying only on successful rasterization.

## Runtime configuration

Copy `.env.example` to `.env` and export it before starting the bot:

```sh
set -a
source .env
set +a
cargo run
```

Important variables:

- `TELOXIDE_TOKEN` and `BOT_ADMIN_ID` are required.
- `TURSO_DATABASE_URL=file:local.db` uses an ordinary local database file.
- Remote Turso additionally requires `TURSO_AUTH_TOKEN`.
- `BOT_CONFIG_PATH` defaults to `config.toml`.
- The web dashboard is disabled unless `BOT_WEB_BIND` is set.
- `IMAGE_CACHE_DIR` must be writable when SVG cards contain remote images.

Never commit `.env`, database files, cookies, tokens, or generated caches.

## Database rules

- All database access must remain asynchronous through `libsql`.
- Do not introduce `rusqlite`, blocking mutexes, or synchronous DB access.
- Update `SCHEMA_VERSION` and add a forward migration for every schema change.
- The project may start with a new empty database; compatibility with the old
  pre-Turso database is not required.
- User registration, nickname lookup, leaderboards, checkpoints, and
  announcements must remain isolated by Telegram `chat_id`.
- Persist a delivery marker only after Telegram confirms successful delivery.
- A failure for one chat must not prevent processing other chats.

## Telegram templates

All ordinary user-facing Telegram text belongs in `templates/` and must be
registered in `src/templates.rs`.

The filename determines the Telegram mode:

- `*.txt.j2` — plain text, no parse mode;
- `*.html.j2` — Telegram HTML.

Do not add MarkdownV2 templates. Send ordinary rendered templates through
`BotTemplateSender`. The deliberate exception is the static two-line caption
for a `/previous`, `/current`, or `/next` media group: it is built alongside
the `InputMedia` and explicitly uses Telegram HTML to bold the mode name and
date. Escape every dynamic value before using this exception.

HTML templates are autoescaped. Do not pre-escape template arguments.
Respect Telegram limits: 4096 characters for messages and 1024 for captions.
The template registry tests must continue to validate every template file.

## Season-pair commands

`/previous`, `/current`, and `/next` render the Deadly Assault and Shiyu
Defense pair together. Select five-digit production seasons of each mode,
sort them by their API UTC+8 start time, identify the current season, then
select its direct neighbour for previous/next. Do not make `/next` mean “any
future season”, and do not fall back when the immediate next season is absent.

Both modes must be available before rendering; otherwise return the existing
“full pair is unavailable” message. Send both PNGs as one media group. Telegram
displays only one visible caption for an album, so put the two mode lines into
the first image's caption and leave the second image uncaptioned.

## Scheduler invariants

- Scheduler work must be async and shutdown-aware.
- Slow synchronous SVG rasterization must run in `tokio::task::spawn_blocking`.
- Rate-limit Telegram and HoYoLAB requests and preserve retry backoff.
- Checkpoint messages are valid only inside their short delivery window.
  Never send a checkpoint after its window has expired.
- Deadly Assault announcements are valid only before the season starts.
  Never send an overdue announcement after `live_begin`.

### Nanoka season selection

For automatic Deadly Assault announcements:

- use `nanoka::NanokaClient`;
- accept only five-digit production IDs matching `69xxx`;
- use `SeasonMeta.live_begin` as the season start;
- ignore six-digit beta IDs and their broad `begin`/`end` dates;
- fetch details with `get_boss_detail_resolved`;
- render with `rustverse_svg::deadly_info_with_begin_time`, passing the
  resolved live start so the `yyyy-mm-dd` heading is accurate;
- deduplicate by `(chat_id, event_kind, season_id)`.

Do not use HoYoLAB player results to describe a future rotation.

Shiyu announcements use `get_seasons`, `get_detail_resolved`, and
`rustverse_svg::shiyu_info`; reject a non-Shiyu resolved detail before
rendering.

## Season SVG infographics

The season renderers live in the workspace `rustverse_svg` crate:

- `deadly_info.j2` renders Deadly Assault; its title is `Deadly Assault ·
  yyyy-mm-dd` when a start date is present.
- `shiyu_info.j2` renders only Critical Node stage 5. Its rooms are derived
  from the stage's child zones, in deterministic order; the highest-HP monster
  is the featured boss.
- `prepare_*_info` builds the template view and owns every dynamic vertical
  measurement. When adding a line, update both the template position and the
  Rust height calculation so cards never overlap or clip.
- `SHIYU_MECHANICS_WRAP_WIDTH` is the sole width for Shiyu mechanics: use it
  for both template wrapping and height calculations.
- Deadly and Shiyu cards show weaknesses and resistance on a dedicated row;
  Shiyu resistance belongs to the featured boss.
- Boss art occupies its configured fixed fraction of the card width (25% for
  Deadly, 30% for Shiyu), full height, centred crop, rounded clipping, and a
  horizontal fade at both edges.
- Every SVG template uses the shared `.watermark` class from `defs.j2`; keep
  its gradient fill and grey outline consistent across detail, top, and season
  images.

## Web dashboard and security

- Keep the dashboard lightweight: no Node.js or frontend build toolchain.
- Public API DTOs must never expose cookies, Telegram user IDs, admin IDs, or
  raw private result payloads.
- Treat the dashboard as unauthenticated unless a trusted reverse proxy adds
  TLS and access control.
- Validate path and query parameters using allowlists and strict parsing.

## Repository hygiene

- Preserve unrelated user changes; the worktree may be dirty or uncommitted.
- Use `rg` and `rg --files` for searches.
- Use `apply_patch` for manual file edits.
- Do not run destructive Git or filesystem commands without explicit approval.
- Do not commit generated PNGs, SVG output, local databases, `.env`, or
  dependency build directories.
