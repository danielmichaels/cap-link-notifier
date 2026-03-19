# cap-link-notifier

Sidecar container that watches a self-hosted [Cap](https://cap.so) instance's logs for magic login links and forwards them to Telegram.

Cap doesn't support username/password auth — it uses email magic links via Resend. Without configuring Resend, those links are dumped to stdout. This tool captures them and pushes a Telegram notification so you can tap the link from your phone.

Written in Rust as a learning exercise. See inline comments in `src/main.rs` for explanations of Rust patterns.

## How it works

1. Connects to the Docker daemon via the mounted socket (`bollard` crate)
2. Discovers the Cap container by its compose service label
3. Streams logs as an async `Stream` and regex-matches URLs with auth patterns
4. POSTs matched links to the Telegram Bot API (`reqwest` crate)
5. Automatically reconnects on container restarts

## Setup

### 1. Create a Telegram bot

- Message [@BotFather](https://t.me/BotFather) → `/newbot` → grab the token
- Message your bot, then hit `https://api.telegram.org/bot<TOKEN>/getUpdates` to find your `chat_id`

### 2. Preview your log format

Before deploying, trigger a login on your Cap instance and check what the link looks like:

```bash
task logs:preview
# or manually:
docker logs -f <cap-container> 2>&1 | grep -iE "auth|login|token|callback"
```

Adjust `LINK_PATTERN` in your env if the default regex doesn't match.

### 3. Deploy

**Option A: Add to Cap's docker-compose** (recommended)

Copy the `cap-link-notifier/` directory alongside your Cap compose file and merge the service from `docker-compose.notifier.yml`:

```yaml
services:
  # ... existing cap services ...

  link-notifier:
    build:
      context: ./cap-link-notifier
    restart: unless-stopped
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock:ro
    environment:
      TELEGRAM_BOT_TOKEN: ${TELEGRAM_BOT_TOKEN}
      TELEGRAM_CHAT_ID: ${TELEGRAM_CHAT_ID}
      SERVICE_VALUE: cap
```

**Option B: Run standalone**

```bash
cp .env.example .env
# edit .env with your Telegram credentials
task docker:run
```

**Option C: Run locally** (for development)

```bash
cp .env.example .env
# edit .env
task run
# or with debug logging:
task run:debug
```

## Configuration

| Env var | Default | Description |
|---|---|---|
| `TELEGRAM_BOT_TOKEN` | *required* | Bot token from @BotFather |
| `TELEGRAM_CHAT_ID` | *required* | Your Telegram chat ID |
| `SERVICE_VALUE` | `cap` | Docker compose service name to watch |
| `SERVICE_LABEL` | `com.docker.compose.service` | Docker label key for discovery |
| `LINK_PATTERN` | `https?://...auth...` | Regex to extract links from logs |
| `DISCOVERY_INTERVAL` | `5s` | Poll interval when container not found |
| `RECONNECT_DELAY` | `3s` | Backoff before reconnecting after stream loss |
| `RUST_LOG` | `cap_link_notifier=info` | Log verbosity filter |

## Coolify notes

Coolify may assign different Docker labels to containers. Run `task inspect` on the host to see what labels are in use, then set `SERVICE_LABEL` and `SERVICE_VALUE` accordingly.
