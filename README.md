```
                    o-O-o  o-o  o  o o--o o   o o    o   o o-O-o o-O-o   o-o  o-o
                      |   o   o | /  |    |\  | |     \ /    |     |    /    |
                      |   |   | OO   O-o  | \ | |      O     |     |   O      o-o
                      |   o   o | \  |    |  \| |      |     |     |    \        |
                      o    o-o  o  o o--o o   o O---o  o     o   o-O-o   o-o o--o
```

<h2 align="center"><em>Realtime token tracker to compete with your friends on tokenmaxxing</em></h2>

tokenlytics is an open source token tracker. watches your `~/.claude` and `~/.codex` folders. all local. optionally compete with your friends to see who becomes the first token trillionaire of your feud.

<table align="center">
  <tr>
    <td align="center">
      <img alt="tokenlytics cli" width="400" src="https://github.com/user-attachments/assets/0a2b66f8-05cf-4653-bd0e-74d00fc5676b" />
      <br />
      <sub><strong>CLI</strong></sub>
    </td>
    <td align="center">
      <img alt="tokenlytics web" width="400" src="https://github.com/user-attachments/assets/5d55c617-b5be-4c8c-b0b6-bdebff46713b" />
      <br />
      <sub><strong>Web</strong></sub>
    </td>
  </tr>
</table>

## install

```bash
curl -fsSL https://ultracontext.com/tokenlytics.sh | sh
```

or from source:

```bash
cargo install --git https://github.com/ultracontext/tokenlytics
```

## the dashboard

```
http://localhost:6969
```

live token usage with sparklines, trends, and per-model breakdown. realtime via server-sent events. open in your browser any time the daemon is up.

when the first-run wizard asks for a port, it is for the local dashboard/API.

## use it

```bash
tokenlytics              # show your stats (auto-starts the daemon)
tokenlytics on           # start the background daemon
tokenlytics off          # stop the daemon
tokenlytics status       # is it running?
tokenlytics update       # fetch the latest
tokenlytics --version
```

bare `tokenlytics` auto-starts the daemon if it's not running. ctrl+c on `tokenlytics on` doesn't kill it (detached via `setsid`) — only `tokenlytics off` does.

## leaderboard

<img width="332" height="417" alt="7edfcdd5-8ea6-4fd1-8b29-fc51394c9142" src="https://github.com/user-attachments/assets/5b5a7215-3b97-41e0-b69d-3421238d8dbd" />


opt-in. picked during the first-run wizard, changeable via `tokenlytics --reconfigure`.

- **off** — just track yourself locally
- **global** — compete with everyone running tokenlytics. live at [ultracontext.com/tokenlytics](https://ultracontext.com/tokenlytics)
- **friends** — host or join a private leaderboard

display name, token totals, and aggregate per-model token totals are the only things that leave your machine, and only if you enabled it.

## what's local, what's not

|  | local | over the network |
|---|:---:|:---:|
| token counts | ✓ |  |
| dashboard | ✓ |  |
| your messages, prompts, code | ✓ |  |
| display name + totals + per-model totals |  | leaderboard server (if enabled) |

if leaderboard is off, **nothing** leaves your machine.

## persistence

every token event is mirrored into `~/.tokenlytics/usage.db` (SQLite, bundled). claude and codex delete sessions after ~30 days; tokenlytics keeps them forever.

`tokenlytics update` and any rebuild never touch your data — the binary lives in `~/.cargo/bin/` or `/usr/local/bin/`, your data lives in `~/.tokenlytics/`.

## where stuff lives

```
~/.tokenlytics/
  config.toml          your name, port, leaderboard mode
  usage.db             every token event, ever
  leaderboard.json     friend rankings (if you host)
  tokenlytics.log      daemon stdout/stderr
  tokenlytics.pid      daemon pid (cleaned by `off`)
```

## host your own leaderboard

run tokenlytics in **server mode** to back a public or private leaderboard.

```bash
LEADERBOARD=1 LEADERBOARD_SERVER=1 \
  TOKENLYTICS_ADMIN_TOKEN=$(openssl rand -hex 32) \
  tokenlytics serve --no-setup
```

server mode locks the API surface to a tight whitelist:

| public | locked down |
|---|---|
| `GET /api/leaderboard` | `GET /` (no dashboard) |
| `GET /api/version` | `/api/usage` `/api/tokens` `/api/models` `/api/stream` |
| `POST /api/leaderboard/submit` | `POST /api/self-update` |

with `TOKENLYTICS_ADMIN_TOKEN` set you also get admin endpoints (`Authorization: Bearer <token>`):

```bash
# wipe entire leaderboard
curl -X POST https://your-host/api/admin/wipe \
  -H "Authorization: Bearer $TOKEN"

# delete one entry
curl -X DELETE https://your-host/api/admin/entry/<name> \
  -H "Authorization: Bearer $TOKEN"
```

without the env var, admin paths return 404 (they don't exist). the global host at `tokenlytics.ultracontext.com` runs this exact setup behind Cloudflare + nginx + systemd. ready-made deploy script in `scripts/vps-deploy.sh`.

## update

```bash
tokenlytics update
```

re-runs the install script and atomically replaces the binary in place — works on macOS and Linux, even with the daemon running.

**auto-update**: each release bumps the global server's `MIN_CLIENT_VERSION` to its own version. Older clients hitting the leaderboard get HTTP 426; the dashboard then triggers `/api/self-update` on the local daemon, which fetches the new binary and re-execs itself. You see a brief "updating…" banner and the page reloads on the new version. No manual step.

## requirements

- macOS or Linux
- claude code usage in `~/.claude`, codex usage in `~/.codex`, or both

## development

```bash
cargo run -- on        # run locally without installing
cargo test
```

---

made by Fabio Roma · `[ ultracontext ]`
