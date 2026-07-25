# Эксплуатация rustversebot

## Контейнер

Из корня workspace выполните:

```sh
docker build -t rustversebot:local .
docker run -d --name rustversebot \
  --restart unless-stopped \
  --env-file /secure/path/rustversebot.env \
  -p 127.0.0.1:8080:8080 \
  rustversebot:local
```

Образ не содержит Node или отдельную frontend-сборку. В контейнере
`BOT_WEB_BIND=0.0.0.0:8080`, но пример публикует порт только на loopback хоста.
Проверка состояния доступна на `GET /healthz`.

Не используйте каталог выше workspace как контекст: это без необходимости
передаст Docker daemon больше локальных файлов. `.dockerignore` исключает
Git-метаданные, build-артефакты и локальные секреты.

## systemd

Создайте системного пользователя и установите файлы:

```sh
sudo useradd --system --home-dir /var/lib/rustversebot \
  --shell /usr/sbin/nologin rustversebot
sudo install -D -o root -g root -m 0755 target/release/rustversebot \
  /usr/local/bin/rustversebot
sudo install -D -o root -g root -m 0644 config.toml \
  /etc/rustversebot/config.toml
sudo install -D -o root -g root -m 0600 deploy/rustversebot.env.example \
  /etc/rustversebot/rustversebot.env
sudo install -D -o root -g root -m 0644 deploy/rustversebot.service \
  /etc/systemd/system/rustversebot.service
```

Заполните секреты в `/etc/rustversebot/rustversebot.env`, затем:

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now rustversebot
sudo systemctl status rustversebot
curl --fail http://127.0.0.1:8080/healthz
```

Панель по умолчанию слушает только `127.0.0.1`. Для внешнего доступа
предпочтителен reverse proxy с TLS и аутентификацией, а не публичный bind.

## Данные и восстановление

Рабочая база находится в Turso. Настройте резервное копирование и point-in-time
recovery средствами выбранного плана Turso и периодически проверяйте процедуру
восстановления на отдельной базе. Экспорты храните зашифрованными: база содержит
Telegram-данные и HoYoLAB cookie.

## Остановка

Unit посылает стандартный `SIGTERM` и даёт процессу 30 секунд. Перед production
развёртыванием проверьте это поведение в целевом окружении: приложение
обрабатывает `SIGTERM` и согласованно останавливает HTTP-сервер, scheduler и
Telegram dispatcher.
