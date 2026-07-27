# AGENTS.md

## Project overview

`rustversebot` is a Rust Telegram bot for tracking Zenless Zone Zero endgame
results. It stores data in async libSQL/Turso, renders Telegram messages from
MiniJinja templates, generates PNG cards through `rustverse_svg`, and exposes
an optional lightweight Axum dashboard.

The project is a single Cargo workspace:

- `crates/rustverse` — HoYoLAB client and player result models.
- `crates/rustverse_svg` — SVG templates and PNG rendering.
- `crates/nanoka` — public Nanoka season catalog, boss stats, and image URLs.
- `crates/rustversebot` — the Telegram bot.

Changes to a shared contract may therefore require coordinated edits in another
workspace crate. Do not copy their models or implementations between crates.

## Fast orientation

Start here when taking over work:

- `crates/rustversebot/src/main.rs` assembles `BotState`, starts the Telegram
  dispatcher, scheduler, and optional web server. The `Command` enum is the
  definitive list of bot commands.
- `handlers.rs` owns command and callback behavior, plus media-group construction.
  `scheduler.rs` owns periodic fetching, checkpoint delivery, and season
  announcements. `db.rs` owns schema migrations and all persistence. `web.rs`
  owns the optional Axum dashboard.
- `crates/rustversebot/templates/` contains Telegram-facing text. Its files
  are loaded explicitly by `src/templates.rs`. Use `bot_templates.rs` to send
  them.
- `crates/nanoka/src/lib.rs` is the async public season-data client and
  `types.rs` is the shared data contract. Use the `*_resolved` methods when
  rendering: they resolve Nanoka image paths and apply Deadly Assault stats.
- `crates/rustverse_svg/src/lib.rs` converts game/API models into template
  views and rasterizes the SVG. Templates and static art sit beside that crate.
  `examples/render_season.rs` is the quickest visual-development entry point.
- `crates/rustverse` is the HoYoLAB player-data client. It is for tracked
  player results. Nanoka is the source of seasonal rotations and future
  announcement data.

Useful focused commands from the workspace root:

```sh
# Inspect season data. --scaled exposes the Deadly Assault scaling result.
cargo run --package nanoka -- show 690431 --scaled

# Render the current Deadly Assault beta fixture for visual review.
CARGO_TARGET_DIR=/tmp/rustversebot-svg-release \
  cargo run --release --package rustverse_svg --example render_season -- \
  690431 /tmp/rustversebot-da-690431.png

# Run handler-only tests while iterating on Telegram commands.
CARGO_TARGET_DIR=/tmp/rustversebot-svg-release \
  cargo test --release --package rustversebot handlers::tests
```

Current data-model details worth preserving:

- A six-digit season ID is a test-server preview: its nominal sequence ID is
  `id / 10` (for example, `690421` follows production `69041`).
- Deadly Assault beta details may contain several modes. Render every mode.
  The normal mode is first. The complex-boss mode follows it.
- The complex mode reports `zone_type = 1002`, but if the `1301` adjustment
  table is present, its 24-level HP/ATK/points scaling is `1301..=1324`, not
  `1002..`. Keep the fallback for older payloads without that table.

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
- The project may start with a new empty database. Compatibility with the old
  pre-Turso database is not required.
- User registration, nickname lookup, leaderboards, checkpoints, and
  announcements must remain isolated by Telegram `chat_id`.
- Persist a delivery marker only after Telegram confirms successful delivery.
- A failure for one chat must not prevent processing other chats.

## Telegram templates

All ordinary user-facing Telegram text belongs in `templates/` and must be
registered in `src/templates.rs`.

The filename determines the Telegram mode:

- `*.txt.j2` — plain text, no parse mode.
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
Defense pair together. Determine `/current` and `/previous` from five-digit
production seasons. Sort them by the Europe game-server (UTC+1) start time.
`/next` normally uses the direct production neighbor. During a test window, use
the earliest six-digit preview only when its nominal sequence is exactly the
next one (for example `69041 → 690421`). Never skip a missing immediate
preview to a later one. Test-season `begin`/`end` dates must not make it the
current season.

Both modes must be available before rendering. Otherwise, return the existing
“full pair is unavailable” message. Send both PNGs as one media group.
Telegram displays only one visible caption for an album. Put both mode lines
in the first image caption. Do not add a caption to the second image.

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

- Use `nanoka::NanokaClient`.
- Accept only five-digit production IDs matching `69xxx`.
- Use `SeasonMeta.live_begin` as the season start.
- Ignore six-digit beta IDs and their broad `begin`/`end` dates.
- Fetch details with `get_boss_detail_resolved`.
- Render with `rustverse_svg::deadly_info_with_begin_time`. Pass the
  resolved live start so the `yyyy-mm-dd` heading is accurate.
- Deduplicate by `(chat_id, event_kind, season_id)`.

Do not use HoYoLAB player results to describe a future rotation.

Shiyu announcements use `get_seasons`, `get_detail_resolved`, and
`rustverse_svg::shiyu_info`. Reject a non-Shiyu resolved detail before
rendering.

## Season SVG infographics

The season renderers live in the workspace `rustverse_svg` crate:

- `deadly_info.j2` renders Deadly Assault. Its title is `Deadly Assault ·
  yyyy-mm-dd` when a start date is present.
- `shiyu_info.j2` renders only Critical Node stage 5. Its rooms are derived
  from the stage's child zones, in deterministic order. The highest-HP monster
  is the featured boss.
- `prepare_*_info` builds the template view and owns every dynamic vertical
  measurement. When adding a line, update both the template position and the
  Rust height calculation so cards never overlap or clip.
- `wrap_game_text_lines` is the source of truth for Deadly text wrapping and
  card height. It must preserve `<color=...>` spans across line boundaries by
  closing and reopening them. Otherwise SVG sibling `<tspan>` elements lose
  their color.
- `SHIYU_MECHANICS_WRAP_WIDTH` is the sole width for Shiyu mechanics: use it
  for both template wrapping and height calculations.
- Deadly and Shiyu cards show weaknesses and resistance on a dedicated row.
  Shiyu resistance belongs to the featured boss.
- Deadly rooms with no elements use the compact mechanics offset. Do not leave
  a blank weaknesses/resistance row. Boss art is clipped to the inner card
  contour so it never covers any room border.
- The complex Deadly boss is identified in the view as `is_complex`. Retain
  its subtle burgundy gradient border while keeping the standard border style
  and image clipping for every boss.
- Boss art occupies its configured fixed fraction of the card width (25% for
  Deadly, 30% for Shiyu), full height, centered crop, rounded clipping, and a
  horizontal fade at both edges.
- Every SVG template uses the shared `.watermark` class from `defs.j2`. Keep
  its gradient fill and gray outline consistent across detail, top, and season
  images.

## Web dashboard and security

- Keep the dashboard lightweight: no Node.js or frontend build toolchain.
- Public API DTOs must never expose cookies, Telegram user IDs, admin IDs, or
  raw private result payloads.
- Treat the dashboard as unauthenticated unless a trusted reverse proxy adds
  TLS and access control.
- Validate path and query parameters using allowlists and strict parsing.

## Repository hygiene

- Preserve unrelated user changes. The worktree may be dirty or uncommitted.
- Use `rg` and `rg --files` for searches.
- Use `apply_patch` for manual file edits.
- Do not run destructive Git or filesystem commands without explicit approval.
- Do not commit generated PNGs, SVG output, local databases, `.env`, or
  dependency build directories.
