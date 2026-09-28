# VPS Local

Local experience on remote machines: repos on your VPS or Raspberry Pi
mirrored to your laptop automatically. A [Herdr](https://herdr.dev) plugin
written in Rust, powered by [Mutagen](https://mutagen.io) sync.

## The problem

Herdr's saved-machine feature runs your agents on a VPS while your laptop is
just a screen — but Herdr does not copy files to your machine. Without a
bridge, repos that agents change on the VPS are invisible to your local IDE
and tools. You end up feeling like a guest in the VPS terminal instead of
working in your own.

## How it works

One Mutagen daemon runs on your laptop; Mutagen auto-deploys a small agent
to each remote over SSH (no manual install there). Each configured session
mirrors a local directory (alpha) with a remote directory (beta):

- **Bidirectional by default** (`two-way-safe`): changes propagate both ways
  within about a second; only changed files transfer, not the whole tree.
- **Watched, not polled**: both sides report filesystem events, so idle
  sessions cost nothing and changes propagate immediately.
- **Conflicts never overwrite**: if the same file changes on both sides
  before a sync, both versions are kept, the session reports
  `Conflicts: N` and stays alive. Resolve by editing one side to the
  final content (verified: the counter clears and syncing resumes).
- **Git mirrors too**: `.git` directories propagate by default, so a commit
  made on the VPS shows up in your local `git log` and vice versa
  (verified both directions). Write git objects on one side at a time;
  treat the other side as a reader to avoid object races.

This plugin is the glue: a Herdr startup hook runs `ensure` after every
server start (e.g. reboot), which starts the daemon, creates missing local
dirs, creates missing sessions, and resumes paused ones. A `status` action
prints each configured session's state. Session definitions live in the
plugin config, not in code.

## Measured latency

Setup: laptop ↔ Raspberry Pi Zero 2 W (aarch64, WiFi LAN), Mutagen 0.18.1,
existing session, small text files (n=5) plus one 1 MB file per direction.

| Case | Result |
| --- | --- |
| Small file, laptop → Pi | <0.2 s (raw 577–738 ms including ~588 ms SSH setup; net 0–150 ms) |
| Small file, Pi → laptop | ~58 ms (57–59 ms, no SSH in the loop) |
| 1 MB file, laptop → Pi | ~0.14 s (single sample, ±SSH noise) |
| 1 MB file, Pi → laptop | ~0.63 s (n=2: 628, 629 ms) |

Notes:

- Steady-state Mutagen uses a persistent agent connection, so per-change
  propagation does not pay SSH setup; the raw laptop → remote numbers do
  (one blocking SSH per sample), hence the adjusted column.
- Asymmetry on big files is the Pi: weak CPU for hashing/staging plus slow
  WiFi uplink. A wired VPS will beat these numbers.
- Initial scan is a one-time cost per session start (proportional to file
  count); steady-state cost is per changed file only. On big codebases the
  biggest lever is `ignore` patterns (`node_modules`, `.venv`, `target`,
  `__pycache__`, build outputs).

Reproduce against any synced pair:

```bash
./tests/latency.sh <local-dir> <ssh-target> '<remote-dir>' [samples]
```

## Why teams care: execution stays on company machines

The pattern this plugin enables is bigger than convenience. Agents, builds,
test suites, and credentials run on company-controlled machines (VPS,
on-prem servers); engineers' laptops hold a working mirror for viewing and
editing. In practice that means:

- **Secrets never live on laptops**: API keys, cloud credentials, and agent
  tickets stay on the server where the work runs.
- **Uniform, auditable environments**: everyone builds and tests in the same
  place instead of N snowflake laptops.
- **Thin-client mobility**: any laptop becomes a full workstation in minutes;
  a lost or replaced laptop loses no work state — the server holds the
  source of truth and mirrors re-sync.
- **Clean offboarding of access**: revoking SSH and terminating sessions
  cuts a machine off; no work state stranded on personal hardware.

Note the honest boundary: mirrors do contain file bytes, so this hardens
*execution and secrets*, not file visibility. Teams with stricter needs can
combine it with one-way modes, narrower sync scopes, or disk encryption on
top — the session config supports per-repo modes and ignores.

## Install

Requires Herdr 0.9+, Mutagen, and Rust (for the one-time build):

```bash
herdr plugin install yavuzhansarmaz/herdr-vps-local
```

Copy the example config and edit it (find the plugin dir with
`herdr plugin list`):

```bash
cp <plugin-dir>/config.toml.example "$(herdr plugin config-dir vps.local)/config.toml"
```

Config lives at `$(herdr plugin config-dir vps.local)/config.toml`:

```toml
[[sessions]]
name = "bud"
alpha = "~/Desktop/PP/bud"     # local path
beta = "vps:~/repos/bud"       # SSH target:path
ignore = ["node_modules", ".venv", "__pycache__", "target"]
```

Remote endpoints need passwordless SSH and accept Mutagen's auto-deployed
agent (Linux/macOS, x86_64/aarch64/armv7).

Check after a restart:

```bash
herdr plugin log list --plugin vps.local
mutagen sync list
```

## Compatibility

Tested on Linux with Herdr 0.9.1 and Mutagen 0.18.1, syncing to a Raspberry
Pi Zero 2 W (aarch64) over LAN. macOS is declared but not yet verified.

## License

MIT — see [LICENSE](LICENSE).
