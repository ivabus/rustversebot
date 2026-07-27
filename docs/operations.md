# rustversebot operations

## Container

Run these commands from the workspace root:

```sh
docker build -t rustversebot:local .
docker run -d --name rustversebot \
  --restart unless-stopped \
  --env-file /secure/path/rustversebot.env \
  -p 127.0.0.1:8080:8080 \
  rustversebot:local
```

The image does not contain Node or a separate frontend build.
The container sets `BOT_WEB_BIND=0.0.0.0:8080`.
The example exposes the port only through the host loopback interface.
Use `GET /healthz` to check the service.

Use the workspace directory as the Docker build context.
A parent directory can send unnecessary local files to the Docker daemon.
The `.dockerignore` file excludes Git data, build artifacts, and local secrets.

## systemd

1. Create the system user.

2. Install the executable, environment file, and service file.

```sh
sudo useradd --system --home-dir /var/lib/rustversebot \
  --shell /usr/sbin/nologin rustversebot
sudo install -D -o root -g root -m 0755 target/release/rustversebot \
  /usr/local/bin/rustversebot
sudo install -D -o root -g root -m 0600 deploy/rustversebot.env.example \
  /etc/rustversebot/rustversebot.env
sudo install -D -o root -g root -m 0644 deploy/rustversebot.service \
  /etc/systemd/system/rustversebot.service
```

3. Add the secrets to `/etc/rustversebot/rustversebot.env`.

4. Start and check the service.

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now rustversebot
sudo systemctl status rustversebot
curl --fail http://127.0.0.1:8080/healthz
```

The dashboard listens on `127.0.0.1` by default.
For external access, use a reverse proxy with TLS and authentication.

## Data and recovery

The production database is in Turso.
Configure backups and point-in-time recovery with your Turso plan.
Test the recovery procedure regularly with a separate database.
Store exports in encrypted storage.
The database contains Telegram data and a HoYoLAB cookie.

## Shutdown

The service sends `SIGTERM` and gives the process 30 seconds to stop.
Test this behavior in the target environment before a production deployment.
The application handles `SIGTERM`.
It then stops the HTTP server, scheduler, and Telegram dispatcher in order.
