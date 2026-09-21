# plz

A small Rust daemon that checks interactive shell commands with TypeSafe.
The first check starts the daemon automatically; subsequent shells share its
Unix socket and permanent SQLite cache.

## Fish

```fish
nix profile add github:pcarrier/plz
plz set-key
plz init fish | source
```

Put `plz init fish | source` at the end of `fish_user_key_bindings` (or in
`config.fish` if you don't customize bindings). Enter, Ctrl-J, and Ctrl-Enter
then check the entire buffer before executing it, in default and insert modes.
Denied commands stay in the buffer for editing, with the confirmation answer
and denial message preserved above the prompt even for multiline input.
Incomplete input still uses fish's normal multiline editing.

When a command needs confirmation, type **O + Enter** to allow once, **A + Enter**
to allow always, or anything else for no. Confirmation reads from `/dev/tty`,
separately from the command on stdin. No terminal, API failure, or malformed
response means the command does not run.

Use `plz off` to disable checks and `plz on` to enable them again. Checks default
to on. The setting is saved in the DB, shared across shells using that DB, and
takes effect on the next command without a daemon restart. While off, commands
run without API calls or confirmation prompts; bypassed commands aren't cached
as approvals. The exact commands `plz on` and `plz off` always work locally.

`plz status` shows on/off, the active prompt, total input/output tokens, and estimated USD cost.
It works locally without an API key. The DB's `usage` view extracts token counts
from saved API responses, including existing cached responses, and calculates
cost at the [published rate](https://docs.typesafe.ai/models): **$0.042 per million
input tokens, output free**. Cache hits add no cost. These are estimates for
cached responses, not a billing statement; responses without token counts are
reported as incomplete, and failed requests have no recorded usage. The rate
assumes the default provider/model pricing, including when overrides are used.

Status also shows **cache hits, misses, and hit ratio**, plus uncached waiting-time
**min/p50/p90/p99/max, count, and total**. One sample is saved per completed check,
summing its daemon round trips (including startup, queueing, and errors). Time
spent answering a confirmation is excluded. Only cache misses contribute to the
waiting-time stats; the hit ratio is hits / (hits + misses). Local controls and
checks while off don't add samples. Percentiles use nearest rank; durations are
shown in milliseconds and the total in seconds. These statistics start with this
version. Checks without cache metadata (for example from an older daemon) are
reported separately and excluded; restart an older daemon to enable metadata.

## Prompt

Run `plz edit-prompt` to edit the review instructions with `$VISUAL`, `$EDITOR`,
or `vi` if neither is set. Save and exit to store the prompt in the DB. The
command works locally without an API key. Empty edits and failed editor exits
leave the saved prompt unchanged.

The default is:

> Catch likely shell mistakes: typos, wrong paths or flags, and unintended
> destructive effects. Ask for confirmation when a command looks accidental;
> otherwise allow it.

Changes apply to the next check across shells. Each prompt has separate cached
verdicts and Always approvals; switching back reuses that prompt's cache. An
in-progress confirmation keeps the prompt it was checked with. After upgrading
from a version without editable prompts, restart the old daemon once; clients
reject replies from daemons that would ignore the saved prompt.

## API key

Run `plz set-key` and paste your key at the hidden prompt. It is stored in the
`settings` table of the private SQLite database, so no global environment
variable is needed. Run it again to replace the key, or `plz unset-key` to remove
it. Changes take effect on the next check without restarting the daemon.

For scripting, `plz set-key` also accepts the key on stdin, for example
`plz set-key < /path/to/key-file`. A nonempty `TYPESAFE_API_KEY` overrides the
stored key; unset that variable to use the database. The exact command
`plz set-key` is allowed locally so you can configure an already-guarded shell
before its first API call.

## Cache and daemon

State lives in `$XDG_STATE_HOME/plz` (default `~/.local/state/plz`), or
`$PLZ_STATE_DIR` when set to an absolute path:

- `cache.sqlite3`: verdicts and full successful API responses in `verdicts`;
  confirmation answers (`once`, `always`, `no`) in `answers`; the saved API key
  and on/off setting and prompt in `settings`; token counts and estimated costs in `usage`;
  per-check waiting times and cache-hit metadata in `checks`.
- `socket`, `lock`, `pid`: per-user daemon communication and startup lock.
- `daemon.log`: daemon startup errors.

Nothing expires. Cache keys include the exact command text, working directory,
shell, PATH, OS, user ID, model, endpoint, and review policy. **Always** applies
to that exact key, not a command prefix or pattern. **Once** and **no** are
recorded forever but leave the cached confirmation verdict intact, so the next
execution asks again without another API call. Failed API requests are retried
on the next invocation rather than cached as decisions. Transient network errors
(including connection resets and timeouts) are also retried automatically, up to
four attempts with 100/200/400 ms backoff. Waiting-time stats include these retries.

The daemon detaches from the shell and serializes requests, reusing an HTTP
connection. State is private to your user (directory mode 0700, database mode
0600). The saved API key is stored as plaintext in `settings`, separately from
cached verdicts and confirmation answers. The client sends the selected key
over the private socket with each request. Cached decisions
work without a key. To stop the daemon, send SIGTERM to the PID in `pid`; the
next check starts it again and recovers the stale socket.

Optional environment overrides:

| Variable | Default |
| --- | --- |
| `TYPESAFE_API_KEY` | Uses the key saved by `plz set-key` when unset or empty |
| `TYPESAFE_MODEL` | `jev-latest` |
| `TYPESAFE_URL` | `https://api.typesafe.ai/v1/systemone` |

The full command and its context are sent to TypeSafe on cache misses. This is
an interactive input check: scripts and bindings that invoke fish's `execute`
directly bypass it. Verdicts evaluate source text, not current filesystem
contents or expanded shell variables/functions; the permanent cache does not
track changes to those. The default review policy focuses on likely mistakes.

## Development

```sh
nix develop
cargo build
cargo test
python tests/fish.py target/debug/plz
cargo fmt --check
cargo clippy --all-targets -- -D warnings
nix build
nix flake check
```

`plz check [--shell fish]` reads a command from stdin and returns 0 for allow,
1 for user denial, and 2 for an error. It never executes the command itself.
