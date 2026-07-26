# rustversebot

Telegram-бот для отслеживания результатов Zenless Zone Zero: Deadly Assault и
Shiyu Defense.

## Требования

- Telegram bot token от [@BotFather](https://t.me/BotFather);
- Telegram user ID администратора.

Все компоненты находятся в одном Cargo workspace:

```text
crates/
├── nanoka/
├── rustverse/
├── rustverse_svg/
└── rustversebot/
```

## Настройка и запуск

Переменные из `.env.example` являются примером: приложение не читает `.env`
самостоятельно, поэтому экспортируйте их в shell или настройте в менеджере
процессов:

```sh
export TELOXIDE_TOKEN="..."
export BOT_ADMIN_ID="123456789"
export TURSO_DATABASE_URL="file:local.db"
export RUST_LOG="rustversebot=info"
cargo run --package rustversebot
```

При `TURSO_DATABASE_URL=file:local.db` база хранится в обычном локальном файле,
а отдельный сервер и токен не нужны. Таблицы и миграции создаются автоматически.

Для Turso Cloud задайте удалённый URL и токен:

```sh
export TURSO_DATABASE_URL="libsql://your-database.turso.io"
export TURSO_AUTH_TOKEN="..."
```

Настройки фонового обновления:

- `BOT_SCHEDULER_INTERVAL_SECS` — период опроса в секундах (по умолчанию `300`);
- `BOT_CHECKPOINT_WINDOW_SECS` — окно отправки checkpoint после его наступления
  (по умолчанию `300`; просроченные checkpoint не отправляются);
- `BOT_REQUEST_SPACING_MS` — пауза между запросами HoYoLAB (по умолчанию
  `500`);
- `BOT_RETRY_ATTEMPTS` — число попыток отправки checkpoint и запросов HoYoLAB
  (по умолчанию `3`);
- `BOT_RETENTION_DAYS` — срок хранения исторических снимков результатов
  (по умолчанию `90`; последний снимок каждой серии сохраняется).
- `BOT_ANNOUNCEMENT_LEAD_HOURS` — за сколько часов до даты начала из Nanoka
  отправлять карточки следующих сезонов Deadly Assault и Shiyu Defense
  (по умолчанию `24`; после начала сезона опоздавшая карточка не отправляется).

Данные будущих сезонов, характеристики противников и ссылки на изображения бот
получает через crate `nanoka`. Production-индексы Deadly Assault и Shiyu
Defense сохраняются в БД в нормализованной таблице по `(тип режима, ID сезона)`;
бот ожидает окончания текущего сезона, затем обновляет индекс и анонсирует
следующий в настроенном временном окне.
Детали сезона и карточки не кэшируются. Каждая карточка рендерится один раз на
сезон и отправляется каждому чату с зарегистрированными UID не более одного раза.

## Веб-панель

Панель и read-only JSON API выключены по умолчанию. Чтобы запустить их, задайте
адрес:

```sh
export BOT_WEB_BIND="127.0.0.1:8080"
```

Если панель доступна пользователям через HTTPS, задайте её внешний адрес:

```sh
export BOT_PUBLIC_WEB_URL="https://zzz.example.com"
```

Тогда под результатами `/da` и `/shiyu` появится кнопка «История и график».
Локальный адрес не подставляется автоматически: без `BOT_PUBLIC_WEB_URL` кнопки
нет.

После запуска доступны `/`, `/healthz`, `/api/chats`,
`/api/chats/{chat_id}/leaderboard/{kind}` и
`/api/users/{uid}/history`. Значение `kind` — `deadly_assault` или
`shiyu_defense`.

Панель не содержит аутентификации. Не публикуйте её напрямую в интернет:
оставьте loopback-адрес либо используйте доверенный reverse proxy с
аутентификацией и TLS.

Если файл конфигурации недоступен или некорректен, бот запустится со встроенной
конфигурацией и запишет предупреждение в лог.

## HoYoLAB cookie

Cookie настраивается администратором командой `/cookie <cookie_string>` и
сохраняется в Turso в открытом виде. Используйте отдельный приватный чат с
ботом, удалите сообщение с командой после настройки, ограничьте доступ к базе
в панели Turso и регулярно меняйте токен. Не добавляйте `.env`, токены, логи
или экспорт базы в Git.

## Проверки

```sh
cargo fmt --all --check
cargo test --release --workspace
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
```

## Nix и Attic

Flake намеренно публикует пакет только для `aarch64-darwin` (Apple Silicon).
Локальная сборка и запуск:

```sh
nix build .#rustversebot
nix run .#rustversebot
```

GitHub Actions подключает Attic как substituter перед сборкой. Сначала workflow
собирает и отправляет отдельный `cargoArtifacts` со скомпилированными Cargo
dependencies, затем собирает бота и отправляет его runtime closure. Пока
`Cargo.lock`, Cargo manifests, toolchain и системные build inputs не меняются,
следующие сборки получают dependency artifacts из Attic и компилируют только
изменившийся код workspace. Сборка выполняется на нативном ARM64 macOS runner.
Workflow использует GitHub Environment с именем `Actions`.
Создайте в нём environment secrets:

- `ATTIC_SERVER` — URL сервера, например `https://attic.example.org`;
- `ATTIC_TOKEN` — токен с правом push в этот кэш.

Необязательный `ATTIC_CACHE` переопределяет имя кэша; без него workflow
использует `rustversebot`.

Для использования публичного кэша узнайте его endpoint и public key у
администратора Attic:

```sh
attic cache info rustversebot
```

Добавьте их в `~/.config/nix/nix.conf`:

```ini
extra-substituters = https://attic.example.org/rustversebot
extra-trusted-public-keys = rustversebot:BASE64_PUBLIC_KEY
```

После перезапуска Nix daemon установите или запустите пакет из GitHub flake;
готовые store paths будут загружены из Attic вместо локальной компиляции:

```sh
nix profile install github:OWNER/rustversebot#rustversebot
rustversebot

# либо без установки
nix run github:OWNER/rustversebot#rustversebot
```

Перед запуском экспортируйте как минимум `TELOXIDE_TOKEN`, `BOT_ADMIN_ID` и
`TURSO_DATABASE_URL`. Расписание production-сезонов бот получает из Nanoka.

## Шаблоны Telegram

Шаблоны находятся в `templates/`, а режим отправки определяется их суффиксом:

- `*.txt.j2` отправляется как обычный текст без `parse_mode`;
- `*.html.j2` отправляется с `ParseMode::Html`, а значения MiniJinja автоматически
  экранируются для HTML.

Добавление шаблона с другим суффиксом или файла, отсутствующего во встроенном
реестре, приводит к падению теста валидации.

## Детали и сравнение сезонов

Команды `/previous`, `/current` и `/next` отправляют одним альбомом инфографику
Deadly Assault и пятой волны Shiyu Defense для соответствующей пары сезонов.
Будущим считается сезон, непосредственно следующий за текущим; если Nanoka ещё
не опубликовала хотя бы один из режимов пары, бот сообщает, что полной пары нет.

Команды `/da` и `/shiyu` принимают UID или nickname. Если аргумент не указан,
бот показывает inline-кнопки только для UID, зарегистрированных в текущем чате.
Callback повторно проверяет членство UID в чате, поэтому старая или подменённая
кнопка не раскрывает данные другого чата.

Caption результата автоматически сравнивает два последних сезона: показывает
дельту очков, а также звёзд для Deadly Assault или пройденных этажей для Shiyu
Defense. Пока предыдущего сезона нет, строка сравнения не выводится.
