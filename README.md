# Tokenlytics

<img width="524" height="399" alt="Screenshot 2026-04-28 at 00 21 06" src="https://github.com/user-attachments/assets/0a2b66f8-05cf-4653-bd0e-74d00fc5676b" />


Tokenlytics is a small Rust dashboard for Claude Code token usage. It reads Claude data from `~/.claude/stats-cache.json` and `~/.claude/projects/**/*.jsonl`, then serves a local dashboard and JSON endpoints.

The server keeps an in-memory snapshot and runs a Rust watcher backed by OS file events (`FSEvents` on macOS, `inotify` on Linux through `notify`). When Claude writes a new JSONL line or updates the stats cache, Tokenlytics reloads the snapshot and pushes it to the dashboard over Server-Sent Events. If OS watching is unavailable, it falls back to a one-second fingerprint poll.

Headline token totals are `input + output`, ignore cache tokens, and deduplicate repeated Claude response records by `message.id` or `requestId`.

The headline windows are rolling time ranges:

- `last 24h` = now minus 24 hours through now
- `last 7d` = now minus 168 hours through now
- `last 30d` = now minus 720 hours through now

Claude Code `/stats` and `ccusage` can use different aggregation rules. Tokenlytics optimizes for internal usage tracking, not exact parity with those tools.

## Requirements

- macOS or Linux
- [Rust/Cargo](https://rustup.rs)
- Claude Code usage data in `~/.claude`

## Quick Start

```bash
./run
```

Open `http://localhost:3456`.

To use another port:

```bash
PORT=4000 ./run
```

You can also run it through Cargo:

```bash
cargo run
```

## Development

```bash
cargo run
cargo test
```

To compare Tokenlytics with local Claude Code calendar buckets:

```bash
cargo test -- --ignored --nocapture
```

## API

- `GET /api/usage` - full usage data
- `GET /api/tokens` - token totals and trends
- `GET /api/models` - per-model usage
- `GET /api/stream` - realtime Server-Sent Events stream
- `GET /api/realtime` - alias for `/api/stream`

The realtime stream sends an initial `usage` event immediately, then sends another `usage` event whenever the backend snapshot changes. The event `data` payload is the same JSON shape returned by `/api/usage`.

If Claude data is missing, the dashboard still starts and shows zeroed usage until data exists.
