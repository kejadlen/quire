# quire-ci deployment

`quire-ci` is the webhook receiver / CI dispatcher. It runs as a separate
container from `quire`, sharing the same image but with a different entrypoint.

## Data layout

```
/var/quire-ci/
  config.fnl     operator-created; required before first start
  quire-ci.db    SQLite database; created automatically on first start
```

## Config

Create `/var/quire-ci/config.fnl` on the host before starting the container.
Minimal config:

```fennel
{:webhook-secret "change-me"}
```

| Key               | Required | Default | Purpose                                      |
|-------------------|----------|---------|----------------------------------------------|
| `:webhook-secret` | yes      | —       | Shared HMAC-SHA256 secret with quire-server. |
| `:port`           | no       | `3000`  | TCP port to listen on.                       |
| `:sentry :dsn`    | no       | —       | Sentry DSN for error reporting.              |

`:webhook-secret` accepts a plain string or a Docker secret reference:

```fennel
{:webhook-secret {:file "/run/secrets/webhook_secret"}}
```

## Docker Compose

```yaml
services:
  quire-ci:
    image: quire
    entrypoint: quire-ci
    command: serve
    volumes:
      - quire-ci-data:/var/quire-ci
    ports:
      - "3001:3000"
    restart: unless-stopped
    secrets:
      - webhook_secret   # optional; only if using {:file ...} in config.fnl

volumes:
  quire-ci-data:

secrets:
  webhook_secret:
    file: ./secrets/webhook_secret
```

## Wiring to quire-server

`quire-server` POSTs push events to `quire-ci` over HTTP. Set
`:quire-ci-url` in `/var/quire/config.fnl` to point at the `quire-ci`
container:

```fennel
{:quire-ci-url "http://quire-ci:3000/webhook"
 :webhook-secret "change-me"}
```

Both sides must share the same `:webhook-secret`.

## Health check

```
GET /health  →  200 "ok"
```

Suitable for a Docker healthcheck or reverse-proxy probe.
