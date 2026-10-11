# tricky-agent: Guide

An OpenAI-compatible HTTP API in front of the CLI coding agents installed on your machine
(**Cursor Agent** = `agent`, **Command Code** = `cmd`). Every request starts the CLI headless,
streams its answer back in OpenAI format, and keeps the conversation as a session.
No database, no log files, one binary, default port **8010**.

- [Quick start](#quick-start)
- [Secret key](#secret-key)
- [Choosing a model](#choosing-a-model)
- [Sessions](#sessions)
- [Extra request fields](#extra-request-fields)
- [Request log](#request-log)
- [Config file](#config-file)
- [Run it as a service on a Linux server](#run-it-as-a-service-on-a-linux-server)
- [What we learned about each CLI](#what-we-learned-about-each-cli)
- [Troubleshooting](#troubleshooting)
- [Supported models](#supported-models)

## Quick start

```
./tricky-agent                          # prints the secret key and listens on 127.0.0.1:8010
./tricky-agent --check cmd              # smoke-test an agent (also: --check agent)
./tricky-agent --init                   # write a commented config.toml template
```

```python
from openai import OpenAI

client = OpenAI(base_url="http://127.0.0.1:8010/v1", api_key="<the secret key>")

r = client.chat.completions.create(
    model="cmd/deepseek-v4-flash",                       # "<agent>/<model>"
    messages=[{"role": "user", "content": "Hello"}],
)
print(r.choices[0].message.content)
print(r.model_extra["session_id"])                       # e.g. cmd:67a32a21-...
```

Streaming works the usual way (`stream=True`).

## Secret key

Auth is always on, so a public URL (Cloudflare tunnel, ngrok, ...) is never open.
Clients send the key as `Authorization: Bearer <key>`, which is exactly what the OpenAI
SDK does with its `api_key`.

| How | Result |
|---|---|
| `TRICKY_API_KEY=... ./tricky-agent` or `--api-key ...` | fixed key |
| `api_key = "..."` under `[server]` in `config.toml` | fixed key |
| nothing set | a random `sk-ta-...` key is generated and **printed at start**, new on every run |

`/health` is the only open endpoint. A tunnel forwards traffic from `127.0.0.1`, so the
service never treats "from localhost" as trusted. This is a single shared secret, not a
user system: anyone with the key can run commands as you, so keep it private and use a
long random value.

## Choosing a model

The `model` field is **`<agent>/<model>`**:

| You send | What runs |
|---|---|
| `agent/auto` or `agent` | Cursor Agent with its own default model |
| `agent/claude-opus-5-5-high` | Cursor Agent with `--model claude-opus-5-5-high` |
| `cmd/auto` or `cmd` | Command Code with its own default (currently `deepseek/deepseek-v4-flash`) |
| `cmd/deepseek-v4-flash` | Command Code with `--model deepseek-v4-flash` |

The part after the first `/` is handed to the CLI unchanged, so **any model the CLI accepts
works**, even if `GET /v1/models` does not list it. `auto` means "do not pass `--model`".
A wrong name comes back as an HTTP 502 whose message is the CLI's own error (it lists the
valid names or suggests the closest one).

Verified on this machine (2026-10-11):

- `cmd/deepseek-v4-flash` works. The vendor prefix is optional: `cmd` resolves
  `deepseek-v4-flash` to `deepseek/deepseek-v4-flash`. The full form `cmd/deepseek/deepseek-v4-flash` also works.
- `cmd` does **not** know `auto` (`unknown model "auto"`). That is why `cmd/auto` omits the flag instead of passing it.
- `agent/claude-opus-5-5-high` works. **`agent/cursor-claude-opus-5-5` does not exist**: Cursor
  lists Opus 5.5 as `claude-opus-5-5-{low,medium,high,xhigh,max}` plus `-fast` variants. Use one of those.
- Cursor also accepts bracket overrides, e.g. `agent/claude-opus-4-8[context=1m,effort=high,fast=false]`.
- `GET /v1/models` shows `<agent>/auto` plus the short list in each agent's `models = [...]`
  (edit it in `config.toml`). The complete lists are [at the bottom](#supported-models).

## Sessions

The agent CLIs keep conversations on disk themselves; the server only remembers which
session belongs to which client id.

| Request | What happens |
|---|---|
| no session id | a new conversation; the response carries its id |
| `session_id` from a previous response, e.g. `cmd:67a3...` | the same conversation continues. Works even after a server restart |
| your own id, e.g. `"session_id": "task-42"` | first use starts a conversation, later uses continue it (the map is in memory and is lost on restart) |
| `new_session: true` | forget the previous conversation for that id and start over |

Where the id comes from: body `session_id`, else header `X-Session-Id`, else `user`.
Where you read it: the `X-Session-Id` response header, the `session_id` field of the JSON
body, and every streamed chunk. When continuing, send only the new message; the agent remembers the rest.
A `cmd:...` id used with `model="agent/..."` is rejected with a clear error, since sessions belong to one agent.
Requests on the same session run one at a time.

```python
first = client.chat.completions.create(model="agent/auto", messages=[{"role": "user", "content": "My number is 42"}])
sid = first.model_extra["session_id"]
client.chat.completions.create(model="agent/auto", messages=[{"role": "user", "content": "What is my number?"}],
                               extra_body={"session_id": sid})
```

## Extra request fields

Besides the standard OpenAI fields (`model`, `messages`, `stream`, `stream_options.include_usage`).
Send them with `extra_body={...}` in the Python SDK.

| Field | Meaning |
|---|---|
| `session_id` | see [Sessions](#sessions) |
| `new_session` | `true` = start the session over |
| `yolo` | `true`/`false` overrides `default_yolo` (`--yolo`: the agent may edit files and run commands without asking) |
| `cwd` | folder the agent works in. Default: `~/.tricky-agent/agent` (created on start). A relative path is taken inside that folder; an absolute one must be inside it or in `workspace_roots` |
| `timeout_secs` | stop the run after this many seconds (never above `server.timeout_secs`, default 600) |

`tools`, `temperature` and similar OpenAI parameters are ignored: the CLIs have no equivalent.

## Request log

Every chat request is logged in memory only (last `log_capacity`, default 500). Prompts and
answers are never stored, only metadata.

**Terminal.** One line when a request starts (`RUN`) and one when it ends:

```
2026-10-11 16:00:31.062 #1    RUN    cmd/deepseek-v4-flash once 0.00s from=127.0.0.1
2026-10-11 16:00:33.702 #1    OK     200 cmd/deepseek-v4-flash once 2.64s first=2.63s out=2ch tok=15115/2 session=cmd:b86b4c29-... from=127.0.0.1
2026-10-11 16:01:20.164 #5    FAIL   502 cmd/nope once 0.33s from=127.0.0.1 | agent exited with exit status: 1: Error: unknown model "nope"...
2026-10-11 16:01:20.502 #7    CANCEL cmd/auto once 1.00s session=cmd:486f... from=127.0.0.1 | client disconnected
```

`OK` is green, `FAIL` is **red**, `CANCEL` (client hung up, the agent is killed) is yellow, `RUN` is cyan.
Colours only appear in a real terminal (and not when `NO_COLOR` is set).
`once`/`stream` is the request mode, `first=` the time to the first piece of output, `tok=` the
tokens in/out when the agent reports them. Behind a tunnel, `from=` shows the forwarded address
(`CF-Connecting-IP` / `X-Forwarded-For`) and the direct peer: `203.0.113.5 (via 127.0.0.1)`.

**API.** `GET /requests` (needs the key) returns a snapshot, not a stream: call it when you want
to look. Requests that are still running are included, with their elapsed time so far.

| Query | Meaning |
|---|---|
| `limit=N` | newest N entries (default 50) |
| `status=running\|ok\|failed\|cancelled` | filter |
| `format=text` | the same lines as the terminal, oldest first, for `curl` or `watch` |

```
curl -s -H "Authorization: Bearer $KEY" "localhost:8010/requests?status=failed"
watch -n1 'curl -s -H "Authorization: Bearer $KEY" "localhost:8010/requests?format=text&limit=20"'
```

JSON fields per entry: `id, time, status, http_status, agent, model, stream, client, cwd, session,
resumed, prompt_chars, elapsed_ms, first_output_ms, output_chars, prompt_tokens, completion_tokens, error`.
The `summary` object has `running, ok, failed, cancelled` (counted since start), `buffered, capacity, uptime_secs`.
For a streamed request that fails after the first bytes, `http_status` is 200 (the headers were already sent) and `status` is `failed`.

## Config file

`config.toml` is optional. The defaults live inside the binary; a `config.toml` in the folder you
start from (or the path in `--config` / `TRICKY_CONFIG`) is merged on top. Set only what you change.
It is re-read when it changes (host and port need a restart); a broken edit is reported and the
previous config stays active.

```toml
[server]
port = 8010
default_yolo = false          # make --yolo opt-in per request
default_cwd = "~/.tricky-agent/agent"   # where agents work by default (this is already the default)

[agents.cmd]
models = ["deepseek-v4-flash", "kimi-k3"]     # what /v1/models advertises

[agents.agent]
enabled = false               # remove an agent

[agents.myagent]              # add one: no rebuild needed
command = "my-cli"
args = ["--prompt", "{prompt}"]
format = "plain"              # stdout is the answer, streamed line by line
```

`--init` writes the full template with every line commented out. Built-in agents use `format = "ndjson"`
with `rules` (JSON pointers) that say which fields hold the text, the session id and the token usage. `[agents.<name>]` keys:
`command`, `windows_command`, `args`, `prompt_via`, `resume_args`, `yolo_args`, `model_args`, `models`, `format`, `rules`, `enabled`.
Environment: `PORT`, `HOST`, `TRICKY_API_KEY`, `TRICKY_CONFIG`.

## Run it as a service on a Linux server

`service/` has a systemd unit and an installer for a server (x86_64 Linux; the binary is static, nothing to install).
It runs as an unprivileged user `tricky`, **not root**: the agents run with `--yolo`, so a root service would give
the whole server to anyone holding the key.

1. Copy the project folder to the server (needs `dist/tricky-agent-linux-x86_64`, `config.toml`, `tricky-agent.env`, `service/`).
2. On the server, as root: `sudo ./service/install-server.sh`. It creates the user, installs
   `/opt/tricky-agent/tricky-agent`, `/etc/tricky-agent/config.toml` and `/etc/tricky-agent/env` (the key, mode 640),
   enables the unit at boot and starts it. Re-running updates the binary and never overwrites the config or the key.
3. Install and log in the agents **as that user** (the script prints the exact commands):
   `agent` (Cursor installer, or `CURSOR_API_KEY` in `/etc/tricky-agent/env`) and `cmd` (`npm install -g command-code`, needs Node).
   Check with `tricky-agent --check agent` / `--check cmd` as the `tricky` user.

Day to day: `systemctl status tricky-agent`, `systemctl restart tricky-agent`, `journalctl -u tricky-agent -f` (the request lines).
Config edits in `/etc/tricky-agent/config.toml` apply without a restart. Remove: `sudo ./service/install-server.sh uninstall`.

Over a network the key travels in every request, so put the service behind HTTPS (a reverse proxy such as Caddy/nginx,
or a Cloudflare tunnel) and keep `host = "127.0.0.1"`; do not expose plain HTTP on the internet.
Agent folders: agents work in `~/.tricky-agent/agent` of the service user, i.e. `/var/lib/tricky-agent/.tricky-agent/agent`; add others to `workspace_roots`.

## What we learned about each CLI

Found by running both CLIs here; the built-in rules in `default-config.toml` encode all of it.

**Cursor Agent (`agent`)**

- Headless: `agent -p "<prompt>" --trust --output-format stream-json --stream-partial-output`.
  Needs `agent login` once; without it every call fails with `Authentication required`.
- `--trust` is required in a new folder, otherwise it stops at a "Workspace Trust Required" prompt.
- Output is JSON lines. The first line (`type: system, subtype: init`) carries `session_id`.
  Text arrives as `type: assistant` events, token by token.
- **Duplicates to skip:** the same text is sent again (a) as a full-text recap before each tool call (it has
  `model_call_id`) and (b) as a final full message with no `timestamp_ms`. Only events with `timestamp_ms`
  and without `model_call_id` are real deltas. Without this filter the client sees the answer twice.
- When the agent uses tools, text before and after a tool call is joined without a separator.
- `type: result` has `usage.inputTokens` / `outputTokens`.
- Resume: `--resume <session_id>`. Default model is `auto`. `--yolo` lets it edit files and run commands.
- An unknown `--model` exits with status 1 and prints the full list of valid ids.

**Command Code (`cmd`)**

- Headless: `cmd -p "<prompt>" --trust --output-format json` (this is JSON lines, one event per line, not one JSON document).
- Events are wrapped: `{"type":"event","event":{...}}`. The session id is `event.sessionId` on `run_start`.
  Text is `event.type == "text_delta"` (`event.delta`, a few words at a time rather than single tokens).
  `message_update` repeats the whole text so far and must be ignored. The last line is `type: result` with `finalText` and `usage`.
- It prints a plain-text line such as `Updated 1.74.3 -> 1.77.0` to stdout when it self-updates; non-JSON lines are skipped.
- Resume: `--resume <sessionId>`; sessions are stored on disk unless `--no-session` is passed (we never pass it).
- Model ids are `vendor/model`; the vendor prefix may be omitted. `auto` is not accepted. Default: `deepseek/deepseek-v4-flash`.
- `--permission-mode` / `--yolo` controls prompts; `--max-turns` caps a run (exit code 8 on the cap).
- On Windows a bare `cmd` means `cmd.exe`: set `windows_command` to the real launcher.

Both: with `-p` a continued session answered a question about an earlier turn correctly, and without
the session id the same question was answered "unknown", so session resume is what carries the context.

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| `401 invalid or missing API key` | wrong or missing `Authorization: Bearer <key>`; the generated key changes on every start |
| `404 unknown model` | model is not `<agent>/<model>` or the agent is disabled / misspelled |
| `502 agent exited with exit status: 1: ...` | the CLI rejected the call; the rest of the message is its own error (bad model, not logged in) |
| `502 could not start ...` | the command is not on `PATH`; the startup banner marks it `NOT FOUND` |
| `502 agent timed out` | run exceeded `timeout_secs` |
| `Address already in use` | something else owns the port; use `--port` |
| answer text appears twice | a `rules` entry for that agent is too loose, see the Cursor notes |
| `agent` says `Authentication required` | run `agent login` as the same user that runs the server |

## Supported models

Generated from `agent --list-models` and `cmd --list-models` on 2026-10-11. Both CLIs change their
lists often; run those commands for the current set. Anything the CLI accepts also works here.

### `agent/...` (Cursor Agent): 256 models

| `model` value | Description |
|---|---|
| `agent/auto` | Auto (default) |
| `agent/gpt-5.3-codex-low` | Codex 5.3 Low |
| `agent/gpt-5.3-codex-low-fast` | Codex 5.3 Low Fast |
| `agent/gpt-5.3-codex` | Codex 5.3 |
| `agent/gpt-5.3-codex-fast` | Codex 5.3 Fast |
| `agent/gpt-5.3-codex-high` | Codex 5.3 High |
| `agent/gpt-5.3-codex-high-fast` | Codex 5.3 High Fast |
| `agent/gpt-5.3-codex-xhigh` | Codex 5.3 Extra High |
| `agent/gpt-5.3-codex-xhigh-fast` | Codex 5.3 Extra High Fast |
| `agent/gpt-5.2` | GPT-5.2 |
| `agent/composer-2.5` | Composer 2.5 |
| `agent/claude-opus-5-thinking-high` | Claude Opus 5 1M Thinking |
| `agent/claude-opus-5-thinking-high-fast` | Claude Opus 5 1M Thinking Fast |
| `agent/gpt-5.6-sol-high` | GPT-5.6 Sol 1M High |
| `agent/gpt-5.6-sol-high-fast` | GPT-5.6 Sol 1M High Fast |
| `agent/gpt-5.6-sol-xhigh` | GPT-5.6 Sol 1M Extra High |
| `agent/gpt-5.6-sol-xhigh-fast` | GPT-5.6 Sol 1M Extra High Fast |
| `agent/claude-fable-5-thinking-high` | Claude Fable 5 1M Thinking (NO ZDR) |
| `agent/claude-fable-5-thinking-xhigh` | Claude Fable 5 1M Extra High Thinking (NO ZDR) |
| `agent/cursor-grok-4.5-high` | Grok 4.5 |
| `agent/cursor-grok-4.5-high-fast` | Grok 4.5 Fast |
| `agent/gemini-3.7-flash-high` | Gemini 3.7 Flash |
| `agent/claude-sonnet-5-thinking-high` | Claude Sonnet 5 1M Thinking |
| `agent/claude-sonnet-5-thinking-xhigh` | Claude Sonnet 5 1M Extra High Thinking |
| `agent/gpt-5.6-luna-high` | GPT-5.6 Luna 1M High |
| `agent/grok-4.7-low` | Grok 4.7 Low |
| `agent/grok-4.7-low-fast` | Grok 4.7 Low Fast |
| `agent/grok-4.7-medium` | Grok 4.7 Medium |
| `agent/grok-4.7-medium-fast` | Grok 4.7 Medium Fast |
| `agent/grok-4.7-high` | Grok 4.7 High |
| `agent/grok-4.7-high-fast` | Grok 4.7 High Fast |
| `agent/grok-4.7-xhigh` | Grok 4.7 Extra High |
| `agent/grok-4.7-xhigh-fast` | Grok 4.7 Extra High Fast |
| `agent/cursor-grok-4.6-low` | Grok 4.6 Low |
| `agent/cursor-grok-4.6-low-fast` | Grok 4.6 Low Fast |
| `agent/cursor-grok-4.6-medium` | Grok 4.6 Medium |
| `agent/cursor-grok-4.6-medium-fast` | Grok 4.6 Medium Fast |
| `agent/cursor-grok-4.6-high` | Grok 4.6 |
| `agent/cursor-grok-4.6-high-fast` | Grok 4.6 Fast |
| `agent/cursor-grok-4.6-xhigh` | Grok 4.6 Extra High |
| `agent/cursor-grok-4.6-xhigh-fast` | Grok 4.6 Extra High Fast |
| `agent/composer-2.5-fast` | Composer 2.5 Fast |
| `agent/claude-opus-5-5-low` | Claude Opus 5.5 1M Low |
| `agent/claude-opus-5-5-low-fast` | Claude Opus 5.5 1M Low Fast |
| `agent/claude-opus-5-5-medium` | Claude Opus 5.5 1M |
| `agent/claude-opus-5-5-medium-fast` | Claude Opus 5.5 1M Fast |
| `agent/claude-opus-5-5-high` | Claude Opus 5.5 1M High |
| `agent/claude-opus-5-5-high-fast` | Claude Opus 5.5 1M High Fast |
| `agent/claude-opus-5-5-xhigh` | Claude Opus 5.5 1M Extra High |
| `agent/claude-opus-5-5-xhigh-fast` | Claude Opus 5.5 1M Extra High Fast |
| `agent/claude-opus-5-5-max` | Claude Opus 5.5 1M Max |
| `agent/claude-opus-5-5-max-fast` | Claude Opus 5.5 1M Max Fast |
| `agent/claude-opus-5-low` | Claude Opus 5 1M Low |
| `agent/claude-opus-5-low-fast` | Claude Opus 5 1M Low Fast |
| `agent/claude-opus-5-medium` | Claude Opus 5 1M Medium |
| `agent/claude-opus-5-medium-fast` | Claude Opus 5 1M Medium Fast |
| `agent/claude-opus-5-high` | Claude Opus 5 1M |
| `agent/claude-opus-5-high-fast` | Claude Opus 5 1M Fast |
| `agent/claude-opus-5-thinking-low` | Claude Opus 5 1M Low Thinking |
| `agent/claude-opus-5-thinking-low-fast` | Claude Opus 5 1M Low Thinking Fast |
| `agent/claude-opus-5-thinking-medium` | Claude Opus 5 1M Medium Thinking |
| `agent/claude-opus-5-thinking-medium-fast` | Claude Opus 5 1M Medium Thinking Fast |
| `agent/claude-opus-5-thinking-xhigh` | Claude Opus 5 1M Extra High Thinking |
| `agent/claude-opus-5-thinking-xhigh-fast` | Claude Opus 5 1M Extra High Thinking Fast |
| `agent/claude-opus-5-thinking-max` | Claude Opus 5 1M Max Thinking |
| `agent/claude-opus-5-thinking-max-fast` | Claude Opus 5 1M Max Thinking Fast |
| `agent/claude-opus-4-8-low` | Claude Opus 4.8 1M Low |
| `agent/claude-opus-4-8-low-fast` | Claude Opus 4.8 1M Low Fast |
| `agent/claude-opus-4-8-medium` | Claude Opus 4.8 1M Medium |
| `agent/claude-opus-4-8-medium-fast` | Claude Opus 4.8 1M Medium Fast |
| `agent/claude-opus-4-8-high` | Claude Opus 4.8 1M |
| `agent/claude-opus-4-8-high-fast` | Claude Opus 4.8 1M Fast |
| `agent/claude-opus-4-8-xhigh` | Claude Opus 4.8 1M Extra High |
| `agent/claude-opus-4-8-xhigh-fast` | Claude Opus 4.8 1M Extra High Fast |
| `agent/claude-opus-4-8-max` | Claude Opus 4.8 1M Max |
| `agent/claude-opus-4-8-max-fast` | Claude Opus 4.8 1M Max Fast |
| `agent/claude-opus-4-8-thinking-low` | Claude Opus 4.8 1M Low Thinking |
| `agent/claude-opus-4-8-thinking-low-fast` | Claude Opus 4.8 1M Low Thinking Fast |
| `agent/claude-opus-4-8-thinking-medium` | Claude Opus 4.8 1M Medium Thinking |
| `agent/claude-opus-4-8-thinking-medium-fast` | Claude Opus 4.8 1M Medium Thinking Fast |
| `agent/claude-opus-4-8-thinking-high` | Claude Opus 4.8 1M Thinking |
| `agent/claude-opus-4-8-thinking-high-fast` | Claude Opus 4.8 1M Thinking Fast |
| `agent/claude-opus-4-8-thinking-xhigh` | Claude Opus 4.8 1M Extra High Thinking |
| `agent/claude-opus-4-8-thinking-xhigh-fast` | Claude Opus 4.8 1M Extra High Thinking Fast |
| `agent/claude-opus-4-8-thinking-max` | Claude Opus 4.8 1M Max Thinking |
| `agent/claude-opus-4-8-thinking-max-fast` | Claude Opus 4.8 1M Max Thinking Fast |
| `agent/gpt-5.6-sol-none` | GPT-5.6 Sol 1M None |
| `agent/gpt-5.6-sol-none-fast` | GPT-5.6 Sol 1M None Fast |
| `agent/gpt-5.6-sol-low` | GPT-5.6 Sol 1M Low |
| `agent/gpt-5.6-sol-low-fast` | GPT-5.6 Sol 1M Low Fast |
| `agent/gpt-5.6-sol-medium` | GPT-5.6 Sol 1M |
| `agent/gpt-5.6-sol-medium-fast` | GPT-5.6 Sol 1M Fast |
| `agent/gpt-5.6-sol-max` | GPT-5.6 Sol 1M Max |
| `agent/gpt-5.6-sol-max-fast` | GPT-5.6 Sol 1M Max Fast |
| `agent/gpt-5.5-none` | GPT-5.5 1M None |
| `agent/gpt-5.5-none-fast` | GPT-5.5 None Fast |
| `agent/gpt-5.5-low` | GPT-5.5 1M Low |
| `agent/gpt-5.5-low-fast` | GPT-5.5 Low Fast |
| `agent/gpt-5.5-medium` | GPT-5.5 1M |
| `agent/gpt-5.5-medium-fast` | GPT-5.5 Fast |
| `agent/gpt-5.5-high` | GPT-5.5 1M High |
| `agent/gpt-5.5-high-fast` | GPT-5.5 High Fast |
| `agent/gpt-5.5-extra-high` | GPT-5.5 1M Extra High |
| `agent/gpt-5.5-extra-high-fast` | GPT-5.5 Extra High Fast |
| `agent/claude-fable-5-1-low` | Claude Fable 5.1 1M Low (NO ZDR) |
| `agent/claude-fable-5-1-medium` | Claude Fable 5.1 1M Medium (NO ZDR) |
| `agent/claude-fable-5-1-high` | Claude Fable 5.1 1M (NO ZDR) |
| `agent/claude-fable-5-1-xhigh` | Claude Fable 5.1 1M Extra High (NO ZDR) |
| `agent/claude-fable-5-1-max` | Claude Fable 5.1 1M Max (NO ZDR) |
| `agent/claude-fable-5-1-thinking-low` | Claude Fable 5.1 1M Low Thinking (NO ZDR) |
| `agent/claude-fable-5-1-thinking-medium` | Claude Fable 5.1 1M Medium Thinking (NO ZDR) |
| `agent/claude-fable-5-1-thinking-high` | Claude Fable 5.1 1M Thinking (NO ZDR) |
| `agent/claude-fable-5-1-thinking-xhigh` | Claude Fable 5.1 1M Extra High Thinking (NO ZDR) |
| `agent/claude-fable-5-1-thinking-max` | Claude Fable 5.1 1M Max Thinking (NO ZDR) |
| `agent/claude-fable-5-low` | Claude Fable 5 1M Low (NO ZDR) |
| `agent/claude-fable-5-medium` | Claude Fable 5 1M Medium (NO ZDR) |
| `agent/claude-fable-5-high` | Claude Fable 5 1M (NO ZDR) |
| `agent/claude-fable-5-xhigh` | Claude Fable 5 1M Extra High (NO ZDR) |
| `agent/claude-fable-5-max` | Claude Fable 5 1M Max (NO ZDR) |
| `agent/claude-fable-5-thinking-low` | Claude Fable 5 1M Low Thinking (NO ZDR) |
| `agent/claude-fable-5-thinking-medium` | Claude Fable 5 1M Medium Thinking (NO ZDR) |
| `agent/claude-fable-5-thinking-max` | Claude Fable 5 1M Max Thinking (NO ZDR) |
| `agent/cursor-grok-4.5-low` | Grok 4.5 Low |
| `agent/cursor-grok-4.5-low-fast` | Grok 4.5 Low Fast |
| `agent/cursor-grok-4.5-medium` | Grok 4.5 Medium |
| `agent/cursor-grok-4.5-medium-fast` | Grok 4.5 Medium Fast |
| `agent/gemini-3.8-flash-low` | Gemini 3.8 Flash Low |
| `agent/gemini-3.8-flash-medium` | Gemini 3.8 Flash Medium |
| `agent/gemini-3.8-flash-high` | Gemini 3.8 Flash High |
| `agent/gemini-3.7-flash-low` | Gemini 3.7 Flash Low |
| `agent/gemini-3.7-flash-medium` | Gemini 3.7 Flash Medium |
| `agent/muse-spark-1.3-minimal` | Muse Spark 1.3 1M Minimal |
| `agent/muse-spark-1.3-low` | Muse Spark 1.3 1M Low |
| `agent/muse-spark-1.3-medium` | Muse Spark 1.3 1M Medium |
| `agent/muse-spark-1.3-high` | Muse Spark 1.3 1M |
| `agent/muse-spark-1.3-xhigh` | Muse Spark 1.3 1M Extra High |
| `agent/muse-spark-1.3-max` | Muse Spark 1.3 1M Max |
| `agent/gpt-5.6-terra-none` | GPT-5.6 Terra 1M None |
| `agent/gpt-5.6-terra-none-fast` | GPT-5.6 Terra 1M None Fast |
| `agent/gpt-5.6-terra-low` | GPT-5.6 Terra 1M Low |
| `agent/gpt-5.6-terra-low-fast` | GPT-5.6 Terra 1M Low Fast |
| `agent/gpt-5.6-terra-medium` | GPT-5.6 Terra 1M |
| `agent/gpt-5.6-terra-medium-fast` | GPT-5.6 Terra 1M Fast |
| `agent/gpt-5.6-terra-high` | GPT-5.6 Terra 1M High |
| `agent/gpt-5.6-terra-high-fast` | GPT-5.6 Terra 1M High Fast |
| `agent/gpt-5.6-terra-xhigh` | GPT-5.6 Terra 1M Extra High |
| `agent/gpt-5.6-terra-xhigh-fast` | GPT-5.6 Terra 1M Extra High Fast |
| `agent/gpt-5.6-terra-max` | GPT-5.6 Terra 1M Max |
| `agent/gpt-5.6-terra-max-fast` | GPT-5.6 Terra 1M Max Fast |
| `agent/claude-sonnet-5-5-low` | Claude Sonnet 5.5 Low |
| `agent/claude-sonnet-5-5-medium` | Claude Sonnet 5.5 Medium |
| `agent/claude-sonnet-5-5-high` | Claude Sonnet 5.5 High |
| `agent/claude-sonnet-5-5-xhigh` | Claude Sonnet 5.5 Extra High |
| `agent/claude-sonnet-5-5-max` | Claude Sonnet 5.5 Max |
| `agent/claude-sonnet-5-low` | Claude Sonnet 5 1M Low |
| `agent/claude-sonnet-5-medium` | Claude Sonnet 5 1M Medium |
| `agent/claude-sonnet-5-high` | Claude Sonnet 5 1M |
| `agent/claude-sonnet-5-xhigh` | Claude Sonnet 5 1M Extra High |
| `agent/claude-sonnet-5-max` | Claude Sonnet 5 1M Max |
| `agent/claude-sonnet-5-thinking-low` | Claude Sonnet 5 1M Low Thinking |
| `agent/claude-sonnet-5-thinking-medium` | Claude Sonnet 5 1M Medium Thinking |
| `agent/claude-sonnet-5-thinking-max` | Claude Sonnet 5 1M Max Thinking |
| `agent/claude-4.6-sonnet-medium` | Claude Sonnet 4.6 1M |
| `agent/claude-4.6-sonnet-medium-thinking` | Claude Sonnet 4.6 1M Thinking |
| `agent/claude-opus-4-7-low` | Claude Opus 4.7 1M Low |
| `agent/claude-opus-4-7-low-fast` | Claude Opus 4.7 1M Low Fast |
| `agent/claude-opus-4-7-medium` | Claude Opus 4.7 1M Medium |
| `agent/claude-opus-4-7-medium-fast` | Claude Opus 4.7 1M Medium Fast |
| `agent/claude-opus-4-7-high` | Claude Opus 4.7 1M High |
| `agent/claude-opus-4-7-high-fast` | Claude Opus 4.7 1M High Fast |
| `agent/claude-opus-4-7-xhigh` | Claude Opus 4.7 1M |
| `agent/claude-opus-4-7-xhigh-fast` | Claude Opus 4.7 1M Fast |
| `agent/claude-opus-4-7-max` | Claude Opus 4.7 1M Max |
| `agent/claude-opus-4-7-max-fast` | Claude Opus 4.7 1M Max Fast |
| `agent/claude-opus-4-7-thinking-low` | Claude Opus 4.7 1M Low Thinking |
| `agent/claude-opus-4-7-thinking-low-fast` | Claude Opus 4.7 1M Low Thinking Fast |
| `agent/claude-opus-4-7-thinking-medium` | Claude Opus 4.7 1M Medium Thinking |
| `agent/claude-opus-4-7-thinking-medium-fast` | Claude Opus 4.7 1M Medium Thinking Fast |
| `agent/claude-opus-4-7-thinking-high` | Claude Opus 4.7 1M High Thinking |
| `agent/claude-opus-4-7-thinking-high-fast` | Claude Opus 4.7 1M High Thinking Fast |
| `agent/claude-opus-4-7-thinking-xhigh` | Claude Opus 4.7 1M Thinking |
| `agent/claude-opus-4-7-thinking-xhigh-fast` | Claude Opus 4.7 1M Thinking Fast |
| `agent/claude-opus-4-7-thinking-max` | Claude Opus 4.7 1M Max Thinking |
| `agent/claude-opus-4-7-thinking-max-fast` | Claude Opus 4.7 1M Max Thinking Fast |
| `agent/gpt-5.4-low` | GPT-5.4 1M Low |
| `agent/gpt-5.4-medium` | GPT-5.4 1M |
| `agent/gpt-5.4-medium-fast` | GPT-5.4 Fast |
| `agent/gpt-5.4-high` | GPT-5.4 1M High |
| `agent/gpt-5.4-high-fast` | GPT-5.4 High Fast |
| `agent/gpt-5.4-xhigh` | GPT-5.4 1M Extra High |
| `agent/gpt-5.4-xhigh-fast` | GPT-5.4 Extra High Fast |
| `agent/claude-4.6-opus-high` | Claude Opus 4.6 1M |
| `agent/claude-4.6-opus-max` | Claude Opus 4.6 1M Max |
| `agent/claude-4.6-opus-high-thinking` | Claude Opus 4.6 1M Thinking |
| `agent/claude-4.6-opus-max-thinking` | Claude Opus 4.6 1M Max Thinking |
| `agent/claude-4.5-opus-high` | Claude Opus 4.5 |
| `agent/claude-4.5-opus-high-thinking` | Claude Opus 4.5 Thinking |
| `agent/gpt-5.2-low` | GPT-5.2 Low |
| `agent/gpt-5.2-low-fast` | GPT-5.2 Low Fast |
| `agent/gpt-5.2-fast` | GPT-5.2 Fast |
| `agent/gpt-5.2-high` | GPT-5.2 High |
| `agent/gpt-5.2-high-fast` | GPT-5.2 High Fast |
| `agent/gpt-5.2-xhigh` | GPT-5.2 Extra High |
| `agent/gpt-5.2-xhigh-fast` | GPT-5.2 Extra High Fast |
| `agent/gpt-5.6-luna-none` | GPT-5.6 Luna 1M None |
| `agent/gpt-5.6-luna-none-fast` | GPT-5.6 Luna 1M None Fast |
| `agent/gpt-5.6-luna-low` | GPT-5.6 Luna 1M Low |
| `agent/gpt-5.6-luna-low-fast` | GPT-5.6 Luna 1M Low Fast |
| `agent/gpt-5.6-luna-medium` | GPT-5.6 Luna 1M |
| `agent/gpt-5.6-luna-medium-fast` | GPT-5.6 Luna 1M Fast |
| `agent/gpt-5.6-luna-high-fast` | GPT-5.6 Luna 1M High Fast |
| `agent/gpt-5.6-luna-xhigh` | GPT-5.6 Luna 1M Extra High |
| `agent/gpt-5.6-luna-xhigh-fast` | GPT-5.6 Luna 1M Extra High Fast |
| `agent/gpt-5.6-luna-max` | GPT-5.6 Luna 1M Max |
| `agent/gpt-5.6-luna-max-fast` | GPT-5.6 Luna 1M Max Fast |
| `agent/gemini-3.6-flash-minimal` | Gemini 3.6 Flash Minimal |
| `agent/gemini-3.6-flash-low` | Gemini 3.6 Flash Low |
| `agent/gemini-3.6-flash-medium` | Gemini 3.6 Flash Medium |
| `agent/gemini-3.6-flash-high` | Gemini 3.6 Flash |
| `agent/gemini-3.1-pro` | Gemini 3.1 Pro |
| `agent/gpt-5.4-mini-none` | GPT-5.4 Mini None |
| `agent/gpt-5.4-mini-low` | GPT-5.4 Mini Low |
| `agent/gpt-5.4-mini-medium` | GPT-5.4 Mini |
| `agent/gpt-5.4-mini-high` | GPT-5.4 Mini High |
| `agent/gpt-5.4-mini-xhigh` | GPT-5.4 Mini Extra High |
| `agent/gpt-5.4-nano-none` | GPT-5.4 Nano None |
| `agent/gpt-5.4-nano-low` | GPT-5.4 Nano Low |
| `agent/gpt-5.4-nano-medium` | GPT-5.4 Nano |
| `agent/gpt-5.4-nano-high` | GPT-5.4 Nano High |
| `agent/gpt-5.4-nano-xhigh` | GPT-5.4 Nano Extra High |
| `agent/claude-haiku-5-5-low` | Claude Haiku 5.5 Low No Thinking |
| `agent/claude-haiku-5-5-medium` | Claude Haiku 5.5 Medium No Thinking |
| `agent/claude-haiku-5-5-high` | Claude Haiku 5.5 High No Thinking |
| `agent/claude-haiku-5-5-xhigh` | Claude Haiku 5.5 Extra High No Thinking |
| `agent/claude-haiku-5-5-max` | Claude Haiku 5.5 Max No Thinking |
| `agent/claude-haiku-5-5-thinking-low` | Claude Haiku 5.5 Low |
| `agent/claude-haiku-5-5-thinking-medium` | Claude Haiku 5.5 Medium |
| `agent/claude-haiku-5-5-thinking-high` | Claude Haiku 5.5 High |
| `agent/claude-haiku-5-5-thinking-xhigh` | Claude Haiku 5.5 Extra High |
| `agent/claude-haiku-5-5-thinking-max` | Claude Haiku 5.5 Max |
| `agent/claude-4.5-sonnet` | Claude Sonnet 4.5 |
| `agent/claude-4.5-sonnet-thinking` | Claude Sonnet 4.5 Thinking |
| `agent/gpt-5.1-low` | GPT-5.1 Low |
| `agent/gpt-5.1` | GPT-5.1 |
| `agent/gpt-5.1-high` | GPT-5.1 High |
| `agent/gemini-3-flash` | Gemini 3 Flash |
| `agent/gemini-3.5-flash` | Gemini 3.5 Flash |
| `agent/claude-4-sonnet` | Claude Sonnet 4 |
| `agent/claude-4-sonnet-thinking` | Claude Sonnet 4 Thinking |
| `agent/gpt-5-mini` | GPT-5 Mini |
| `agent/kimi-k3-low` | Kimi K3 Low |
| `agent/kimi-k3-high` | Kimi K3 High |
| `agent/kimi-k3-max` | Kimi K3 |
| `agent/kimi-k2.7-code` | Kimi K2.7 Code |
| `agent/glm-5.2-high` | GLM 5.2 |
| `agent/glm-5.2-max` | GLM 5.2 Max |

### `cmd/...` (Command Code): 88 models

The vendor prefix (e.g. `deepseek/`) is optional in requests.

#### Open Source

| `model` value | Description |
|---|---|
| `cmd/deepseek/deepseek-v4-pro` | hybrid-attention long-context reasoning |
| `cmd/deepseek/deepseek-v4-flash` | fast hybrid-attention reasoning (default) |
| `cmd/deepseek/deepseek-v4-flash-vision-exp` | fast hybrid-attention reasoning with vision |
| `cmd/deepseek/deepseek-v4-flash-fast` | low-latency V4 Flash deployment |
| `cmd/deepseek/deepseek-v4.1-flash` | V4.1 hybrid-attention reasoning with vision |
| `cmd/deepseek/deepseek-v4.1-flash-fast` | High throughput V4.1 Flash |
| `cmd/moonshotai/kimi-k3` | long-horizon coding & knowledge work with 1M context |
| `cmd/moonshotai/kimi-k2.7-code` | improved long-horizon coding with vision |
| `cmd/moonshotai/kimi-k2.7-code-highspeed` | high-speed long-horizon coding with vision |
| `cmd/moonshotai/kimi-k2.6` | long-horizon coding with vision |
| `cmd/moonshotai/kimi-k2.5` | multimodal frontend coding |
| `cmd/z-ai/glm-5.3-flash` | fast, affordable GLM coding with 1M context |
| `cmd/z-ai/glm-5.3-flashx` | high-speed GLM-5.3 Flash with 1M context |
| `cmd/zai-org/glm-5.3` | frontier coding with emergent cyber capabilities |
| `cmd/zai-org/glm-5.2` | powerful coding with 1M context and long-horizon tasks |
| `cmd/zai-org/glm-5.2-fast` | high-throughput GLM-5.2 with 1M context |
| `cmd/zai-org/glm-5.1` | long-horizon autonomous coding agent |
| `cmd/zai-org/glm-5` | multi-mode thinking & long-range planning |
| `cmd/minimaxai/minimax-m3` | frontier coding, agents & native multimodality |
| `cmd/minimaxai/minimax-m2.7` | end-to-end software engineering agent |
| `cmd/minimaxai/minimax-m2.5` | cross-platform full-stack agentic dev |
| `cmd/xiaomi/mimo-v2.6-pro` | flagship multimodal agentic coding with 1M context |
| `cmd/xiaomi/mimo-v2.6-pro-ultraspeed` | low-latency serving tier of MiMo V2.6 Pro |
| `cmd/xiaomi/mimo-v2.6-flash` | efficient multimodal agentic coding with 1M context |
| `cmd/xiaomi/mimo-v2.5-pro` | high-capability long-context agentic coding |
| `cmd/xiaomi/mimo-v2.5` | efficient long-context agentic coding |
| `cmd/qwen/qwen3.8-omni-flash` | omni-modal understanding & multimedia agentic work |
| `cmd/qwen/qwen3.8-max-0902` | upgraded Qwen 3.8 Max: stronger coding & agentic tool use |
| `cmd/qwen/qwen3.8-max` | autonomous long-horizon coding & professional work |
| `cmd/qwen/qwen3.8-27b` | compact vision-language coding & agentic work |
| `cmd/qwen/qwen3.8-flash` | fast low-cost agentic coding & reasoning |
| `cmd/qwen/qwen3.7-max` | frontier coding & long-horizon agent execution |
| `cmd/qwen/qwen3.7-plus` | agentic coding & reasoning at lower cost |
| `cmd/qwen/qwen3.7-flash` | fast low-cost agentic coding & reasoning |
| `cmd/qwen/qwen3.6-max-preview` | vibe coding & efficient agent execution |
| `cmd/qwen/qwen3.6-plus` | agentic coding & reasoning |
| `cmd/meituan/longcat-2.0` | trillion-parameter agentic coding with 1M context |
| `cmd/stepfun/step-5-preview` | 600B sparse-MoE agentic coding with 1M context |
| `cmd/stepfun/step-3.7-flash` | multimodal sparse-MoE reasoning |
| `cmd/stepfun/step-3.5-flash` | fast sparse-MoE agentic reasoning |
| `cmd/tencent/hy3-paid` | sparse-MoE reasoning & agentic tool use |
| `cmd/tencent/hy4-preview` | agentic coding & sustained multi-step tool use |
| `cmd/nvidia/nemotron-3-ultra-550b-a55b` | open reasoning model for long-horizon autonomous agents |
| `cmd/thinkingmachines/inkling` | multimodal MoE reasoning |
| `cmd/thinkingmachines/inkling-small` | lightweight MoE reasoning at lower cost and latency |
| `cmd/poolside/laguna-s-2.1-free` | FREE open-weight agentic coding and long-horizon work |
| `cmd/inclusionai/ling-3.0-flash-sante:free` | FREE health & medicine tuned lightweight-MoE, still strong on code |
| `cmd/inclusionai/ling-3.1-flash:free` | FREE hybrid-reasoning MoE for coding & tool-using agents |
| `cmd/mistral/mistral-large-4` | frontier open-weight multimodal model for coding & analysis |

#### Stealth

| `model` value | Description |
|---|---|
| `cmd/stealth/glyph-cluster:free` | FREE stealth reasoning model in early access |

#### Anthropic

| `model` value | Description |
|---|---|
| `cmd/claude-sonnet-5-5` | best combo of speed & intelligence (recommended) |
| `cmd/claude-sonnet-5` | prev Sonnet, still fast & capable |
| `cmd/claude-sonnet-4-6` | older Sonnet, still fast & capable |
| `cmd/claude-fable-5-1` | most capable for demanding reasoning & long-horizon agents |
| `cmd/claude-fable-5` | prev Fable, still strong for deep reasoning & agents |
| `cmd/claude-opus-5-5` | most intelligent Opus for agents and coding |
| `cmd/claude-opus-5` | prev flagship, still strong for agents and coding |
| `cmd/claude-opus-4-8` | older Opus, still strong for agents and coding |
| `cmd/claude-opus-4-7` | older Opus, still strong for agents and coding |
| `cmd/claude-haiku-5-5` | fastest & most compact, great for quick tasks |
| `cmd/claude-haiku-4-5` | prev Haiku, still fast for quick tasks |

#### OpenAI

| `model` value | Description |
|---|---|
| `cmd/gpt-6-astra` | most capable OpenAI model for demanding reasoning & agents |
| `cmd/gpt-6.1-sol` | near-Astra performance for complex work at a lower cost |
| `cmd/gpt-6-sol` | built for complex coding & agentic workflows |
| `cmd/gpt-6-luna` | most efficient OpenAI model for focused, high-volume tasks |
| `cmd/gpt-5.6-sol` | frontier model for complex professional work |
| `cmd/gpt-5.6-terra` | balances intelligence and cost |
| `cmd/gpt-5.6-luna` | optimized for cost-sensitive workloads |
| `cmd/gpt-5.5` | latest frontier model for general complex work |
| `cmd/gpt-5.4` | frontier model for general complex work |
| `cmd/gpt-5.3-codex` | frontier coding model |
| `cmd/gpt-5.4-mini` | fast, cost-effective model for everyday tasks |

#### Google

| `model` value | Description |
|---|---|
| `cmd/google/gemini-3.8-flash` | newest Gemini Flash, improved core reasoning |
| `cmd/google/gemini-3.7-flash` | higher-quality coding & agentic workflows, fewer tokens |
| `cmd/google/gemini-3.6-flash` | previous Gemini Flash, still fast & capable |
| `cmd/google/gemini-3.5-flash` | Pro-level coding proficiency, parallel agentic execution |
| `cmd/google/gemini-3.5-flash-lite` | upgraded agentic capabilities, ideal for subagents |
| `cmd/google/gemini-3.1-flash-lite` | high-volume workhorse model with implicit caching |

#### Sakana

| `model` value | Description |
|---|---|
| `cmd/sakana/fugu-ultra` | multi-agent orchestration across frontier models |

#### Meta

| `model` value | Description |
|---|---|
| `cmd/meta/muse-spark-1.1` | agentic performance, tool use, and computer use |
| `cmd/meta/muse-spark-1.2` | coding-optimized for agentic workflows and large codebases |
| `cmd/meta/muse-spark-1.2-contributor` | Muse Spark 1.2 at ~95% off |
| `cmd/meta/muse-spark-1.3` | multimodal reasoning for long-horizon agentic and coding workflows |
| `cmd/meta/muse-spark-1.3-contributor` | Muse Spark 1.3 at up to 95% off |

#### xAI

| `model` value | Description |
|---|---|
| `cmd/xai/grok-4.5` | smartest model for coding, agentic tasks, knowledge work |
| `cmd/xai/grok-4.6` | frontier performance on coding, knowledge work, and STEM |
| `cmd/xai/grok-4.7` | coding and knowledge work, built for multi-hour tasks |

#### Decision models (headless only)

| `model` value | Description |
|---|---|
| `cmd/typesafe/jev` | typed questions in, probabilities out, $0.042 per 1M input tokens, everything else free — Run it headlessly: cmd -p '{"state": "...", "questions": {"<name>": {"type": "noul", "instructions": "..."}}}' -m typesafe/jev |
