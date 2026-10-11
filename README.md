# tricky-agent

OpenAI-compatible API (`/v1/chat/completions`, `/v1/models`) in front of local CLI coding agents
(`agent` = Cursor Agent, `cmd` = Command Code). One binary, no database, default port **8010**.

```
./tricky-agent                     # prints a secret key, listens on 127.0.0.1:8010
./tricky-agent --check cmd         # smoke-test an agent
./tricky-agent --init              # write a commented config.toml template (optional file)
```

```python
from openai import OpenAI
c = OpenAI(base_url="http://127.0.0.1:8010/v1", api_key="<secret key>")
r = c.chat.completions.create(model="cmd/deepseek-v4-flash", messages=[{"role": "user", "content": "hi"}])
sid = r.model_extra["session_id"]    # pass it back as extra_body={"session_id": sid} to continue
```

- model is `<agent>/<model>` (`agent/auto`, `agent/claude-opus-5-5-high`, `cmd/auto`, `cmd/deepseek-v4-flash`)
- auth is always on (static Bearer key); every request is logged in the terminal (failures in red) and at `GET /requests`

**Full guide, request log, config, server (systemd) setup and the complete supported-model lists: [GUIDE.md](GUIDE.md).**

## Build
```
cargo build --release                                      # native
cargo build --release --target x86_64-unknown-linux-musl   # static Linux binary
cargo build --release                                      # on Windows -> target\release\tricky-agent.exe
```
