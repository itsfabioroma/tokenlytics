```
o-O-o  o-o  o  o o--o o   o o    o   o o-O-o o-O-o   o-o  o-o
  |   o   o | /  |    |\  | |     \ /    |     |    /    |
  |   |   | OO   O-o  | \ | |      O     |     |   O      o-o
  |   o   o | \  |    |  \| |      |     |     |    \        |
  o    o-o  o  o o--o o   o O---o  o     o   o-O-o   o-o o--o
```

<h2 align="center"><em>Realtime token tracker to compete with your friends on tokenmaxing</em></h2>

tokenlytics is an open source token tracker. watches your `~/.claude` and `~/.codex` folders. all local. optionally compete on tokenmaxing with your friends to see who becomes the first token trillionaire.

<img width="725" height="659" alt="Screenshot 2026-04-28 at 00 30 52" src="https://github.com/user-attachments/assets/99164503-4db0-48a2-86b0-0a08717eba89" />


## install

```bash
curl -fsSL https://ultracontext.com/tokenlytics.sh | sh
```

or from source:

```bash
cargo install --git https://github.com/itsfabioroma/tokenlytics
```

## the cli

bare `tokenlytics` prints a clean stats block: figlet header, table by period × source, unified sparkline of the last 24 hours.

<p align="center">
  <!-- TODO: add CLI screenshot here -->
  <img width="640" alt="tokenlytics cli" src="" />
</p>

## the dashboard

```
http://localhost:6969
```

live token usage with sparklines, trends, and per-model breakdown. realtime via server-sent events. opens automatically as soon as the daemon is up.

<p align="center">
  <img width="524" height="399" alt="tokenlytics dashboard" src="https://github.com/user-attachments/assets/0a2b66f8-05cf-4653-bd0e-74d00fc5676b" />
</p>

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

opt-in. picked during the first-run wizard, changeable via `tokenlytics --reconfigure`.

- **off** — just track yourself locally
- **global** — compete with everyone running tokenlytics
- **friends** — host or join a private leaderboard

display name and token totals are the only things that leave your machine, and only if you enabled it.

## what's local, what's not

|  | local | over the network |
|---|:---:|:---:|
| token counts | ✓ |  |
| dashboard | ✓ |  |
| your messages, prompts, code | ✓ |  |
| display name + totals |  | leaderboard server (if enabled) |

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

## update

```bash
tokenlytics update
```

re-runs the install script and fetches the latest binary. the server enforces a minimum client version — old clients get HTTP 426 and the dashboard shows an upgrade banner.

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
