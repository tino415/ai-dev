# ai-dev

A debug notebook. An agent proposes blocks of code for live sessions (bash, remote iex, psql, mysql),
you approve the run in the web UI, and you decide whether the agent may read the output.

Sessions are tmux sessions on a private tmux server (`tmux -L ai-dev`), so you can attach from a terminal.
A block ends at a unique marker the tool types after your code; the typed marker never contains
itself, so the terminal echo cannot end a block early. A block that shows no marker within its session's
block seconds (120 by default, set on the Sessions page) fails; with block seconds empty it runs until its
marker, an Interrupt, or the end of its session.

## Run

```
nix develop
echo some-token > agent_token
AI_DEV_AGENT_TOKEN_FILE=$PWD/agent_token AI_DEV_ORIGIN=http://localhost:7471 ai-dev serve
ai-dev enroll    # one-time code to enroll your passkey at /enroll
```

Settings, all environment variables: `AI_DEV_HOME` (default `~/.local/share/ai-dev`),
`AI_DEV_LISTEN` (default `127.0.0.1:7471`), `AI_DEV_ORIGIN` (the address the browser uses; the passkey is bound
to its host, so use one origin only: the tailnet address, not a second `.localhost` name),
`AI_DEV_AGENT_TOKEN_FILE` (default `$AI_DEV_HOME/agent_token`), `AI_DEV_TMUX_SOCKET` (default `ai-dev`).

New sessions run your login shell with `-l`, so tools and their auth plugins resolve as in your terminal.

The NixOS module (`services.ai-dev`) writes a random agent token once under `/var/lib/ai-dev` and installs a
0400 copy for `user` and for `agentUser` (default `llm`); the user unit reads its own copy.

## Agents

The MCP endpoint is `/mcp`, with `Authorization: Bearer <agent token>`, the contents of the token file.
Until the file exists the endpoint refuses every call. Tools: `join_notebook`,
`list_notebooks`, `list_sessions`, `list_variables`, `propose_session`, `propose_block`, `read_block`.

An agent calls `join_notebook` and gets a link; after you admit it in the notebook page, calling
`join_notebook` again returns a notebook token that every other tool takes. Tokens last until revoked;
an agent that joins again under the same name replaces its earlier token.
`propose_block` and `propose_session` take a `why`, which the notebook page shows above the proposal.
In a session whose read policy is ask, "Run and let read" on a proposed block runs it and lets the
agent read the output when the block ends; "Ask me before the agent reads" takes that back while it runs.

The endpoint is stateless: a call needs no `mcp-session-id`, so a restart of ai-dev does not break a
connected agent.

## Test

```
nix develop -c cargo test
```

`tests/e2e.rs` drives a headless chromium from the dev shell against the binary, started with the
unit's narrow environment.

The merge gate is in [CONTRIBUTING.md](CONTRIBUTING.md).

## Icons

`src/icons/*.svg` are the sources of the PNGs in `src/static/`; after changing one, render it again with
`nix run nixpkgs#imagemagick -- -background none src/icons/icon.svg -resize 512x512 src/static/icon-512.png` (likewise for 192 and
for `icon-maskable.svg` to `icon-maskable-512.png`).

## Data

`$AI_DEV_HOME/notebooks/<name>.md` holds blocks only, never output. The file may change on disk, for example
from git: ai-dev compares it with the text it last read or wrote when the notebook page opens and before every
write. A press (Run, Run again, Start and run, a new block, Reject, Delete) that finds it changed loads it and
does nothing else: `<name>.md changed on disk. It was loaded again and nothing ran.` Blocks are matched by id
and follow the file's order; a block whose code, session and why did not change keeps its state and output,
one whose text changed becomes saved, one the file no longer has leaves the page, and a queued or running
block keeps running. A file with a block that has no id, an id used twice, an id another notebook holds, or
an id with more than letters, digits, `-` and `_` is not loaded, nor is a file that went missing: the notebook
keeps its blocks and ai-dev does not write that file until it is fixed. Creating a notebook whose file exists
loads that file.

Variable values, including secrets,
are in `variables.json` (mode 0600). Secret values are filled in when a block runs and masked in output;
agents see only their names. Masking matches only the literal value, so a block that transforms a secret
(`echo {{pw}} | base64`) prints something it cannot mask: keep the read policy at `ask` for sessions
that use secrets. The raw terminal log in `runtime/` (mode 0600) holds typed secrets until the session stops.

## Hidden input

A running block shows its output so far, to you only, with a masked field and `Send`. `Send` types the field's
text and Enter into the block's session, for a `sudo` password, an ssh passphrase or a `psql` prompt. The value is
never stored or logged and is masked in the output; the output shows `‹hidden input sent›` where it went, and
`read_block` says hidden input was sent, never the value. A program that echoes it puts it in the raw terminal
log in `runtime/` like any typed text. `Interrupt` sends Ctrl-C. Only a passkey session can do either; the MCP
endpoint has no such tool. `sudo`'s own credential cache is left alone, so later blocks in that session within
its timeout run `sudo` without asking. A prompt not answered within the session's block seconds fails the block, and
`Run again` retries it.

Block text and hidden input reach tmux through stdin (`load-buffer`, `paste-buffer -d`), never as arguments, so
`ps` shows neither. A bash session gets a block as one command, so the shell has read every line before the first
one runs and a prompt gets the hidden input alone. The output starts after the echo of that command. Lines after
one that opens another program's prompt, such as `ssh host`, run when that program ends, and the block runs
until then. Setup commands and iex,
psql and mysql sessions are typed line by line; there the end marker is typed again after hidden input. After an
interrupt it is typed again in every session.

A notebook reopened after a restart shows its blocks as `saved`: they hold code only and need a click to run again.

## Terminal

A running session's `Terminal ↗` link, in the notebook and on the Sessions page, opens `/sessions/<name>/terminal`
in a new tab: a live xterm.js view of the session. Typing works as in `tmux attach`, and what you type while a block
runs lands in that block's output, as the banner says. While a page terminal is open the session fits the page;
after the last one closes it goes back to 220×50, also when ai-dev starts again. A program that redraws on resize
(`top`, `less`, a readline prompt) puts that redraw into a running block's output when a page terminal opens or
closes, and on Android when the keyboard opens or closes: the terminal page asks the browser to shrink it above the
keyboard, which keeps the key row in reach there; iOS ignores that and covers it. An open page terminal is an
attached client, so the session does not idle out meanwhile. Secrets show unmasked there, as in `tmux attach`; no
agent reads this channel. The page talks to a tmux client the server runs under a PTY, over a WebSocket that needs
the passkey login and refuses a page from any origin but `AI_DEV_ORIGIN`.
