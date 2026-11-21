# ai-dev

A debug notebook. An agent proposes blocks of code for live sessions (bash, remote iex, psql, mysql),
you approve the run in the web UI, and you decide whether the agent may read the output.

Sessions are tmux sessions on a private tmux server (`tmux -L ai-dev`), so you can attach from a terminal.
A block ends at a unique marker the tool types after your code; the typed marker never contains
itself, so the terminal echo cannot end a block early.

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

The endpoint is stateless: a call needs no `mcp-session-id`, so a restart of ai-dev does not break a
connected agent.

## Test

```
nix develop -c cargo test
```

`tests/e2e.rs` drives a headless chromium from the dev shell against the binary, started with the
unit's narrow environment.

## Icons

`src/icons/*.svg` are the sources of the PNGs in `src/static/`; after changing one, render it again with
`nix run nixpkgs#imagemagick -- -background none src/icons/icon.svg -resize 512x512 src/static/icon-512.png` (likewise for 192 and
for `icon-maskable.svg` to `icon-maskable-512.png`).

## Data

`$AI_DEV_HOME/notebooks/<name>.md` holds blocks only, never output. Variable values, including secrets,
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
its timeout run `sudo` without asking. A prompt not answered within the block timeout fails the block, and
`Run again` retries it.

Block text and hidden input reach tmux through stdin (`load-buffer`, `paste-buffer -d`), never as arguments, so
`ps` shows neither. After hidden input or an interrupt the block's end marker is typed again, because a prompt
that reads the terminal discards what was typed ahead.

A notebook reopened after a restart shows its blocks as `saved`: they hold code only and need a click to run again.
