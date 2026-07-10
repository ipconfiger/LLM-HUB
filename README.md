# llm-hub

A local LLM proxy that routes requests to multiple OpenAI-compatible backends by **model name**. Point any agent/tool at a single local URL and `llm-hub` forwards each request to the right backend, with automatic failover and round-robin key load balancing.

- **One URL for everything** — agents use `http://127.0.0.1:3000/v1` as their base URL.
- **Model-based routing** — the `model` field in the request body selects the backend(s).
- **Failover** — backends (and their keys) are tried in order until one succeeds.
- **Round-robin keys** — when a backend has several API keys, the starting key rotates per request so load (and rate limits) are spread across keys.
- **Streaming** — upstream SSE is passed through byte-for-byte to the agent.
- **rustls** — upstream HTTPS uses `reqwest` with the `rustls` TLS backend (no OpenSSL dependency).
- **TUI editor** — edit the config interactively with `--admin`.

## Build

Rust toolchain (edition 2021; MSRV 1.85+ recommended — the dependencies' MSRV).

```bash
# debug build
cargo build

# optimized release build (recommended for daily use)
cargo build --release
```

The binary is named **`llm-hub`**. With a standard cargo setup it lands at:

- debug:   `target/debug/llm-hub`
- release: `target/release/llm-hub`

> If `CARGO_TARGET_DIR` is set (this machine uses a global target dir), look for the binary there, e.g. `$CARGO_TARGET_DIR/release/llm-hub`.

## Usage

```bash
# Seed a sample config (if it doesn't exist yet) and exit
llm-hub --init

# Run the proxy server (default bind 127.0.0.1:3000)
llm-hub --serve
llm-hub --serve --bind 127.0.0.1:4000

# Edit the configuration in a terminal UI
llm-hub --admin
```

| Flag      | Description                                                        |
| --------- | ------------------------------------------------------------------ |
| `--serve` | Start the HTTP proxy server.                                       |
| `--admin` | Start the TUI config editor.                                       |
| `--bind`  | Address to bind the server to (default `127.0.0.1:3000`). `--serve` only. |
| `--init`  | Write a sample config to the config path and exit.                 |

Then configure your agent (e.g. an OpenAI-compatible client) with:

```
base_url = http://127.0.0.1:3000/v1
api_key  = <any value>   # llm-hub ignores this and uses the keys from its config
```

## Configuration

The config file lives at (via the platform config dir):

- Linux:   `~/.config/llm-hub/settings.json`
- macOS:   `~/Library/Application Support/llm-hub/settings.json`
- Windows: `%APPDATA%\llm-hub\settings.json`

Format:

```json
{
  "backends": [
    {
      "name": "硅流",
      "base_url": "https://api.siliconflow.cn",
      "keys": ["sk-key-1", "sk-key-2"],
      "models": ["Qwen/Qwen3-32B", "deepseek-ai/DeepSeek-V3"]
    },
    {
      "name": "deepseek-official",
      "base_url": "https://api.deepseek.com",
      "keys": ["sk-xxx"],
      "models": ["deepseek-chat"]
    }
  ]
}
```

| Field      | Type           | Notes                                                              |
| ---------- | -------------- | ------------------------------------------------------------------ |
| `name`     | string         | Human-readable label shown in logs and the TUI.                     |
| `base_url` | string         | Base URL of the OpenAI-compatible API (no trailing slash needed).  |
| `keys`     | array<string>  | One or more API keys; rotated per request and used for failover.   |
| `models`   | array<string>  | Model names served by this backend (matched against request `model`). |

## How routing & failover work

1. A request arrives at `POST /v1/<path>` (e.g. `/v1/chat/completions`).
2. `llm-hub` reads the `model` field from the request body.
3. It resolves the ordered list of candidate endpoints: every backend whose `models` contains that model, in configuration order. For each backend, **one entry per non-empty key** is produced, with the key list **rotated by a round-robin counter** so the first key tried advances on each request (load balancing across keys).
4. Candidates are tried in order. If a candidate fails to connect or returns a **non-2xx status** *before* any bytes are streamed, the next candidate is tried (failover). Once a candidate starts streaming a `2xx` response, the proxy commits to it and streams the body through to the agent (mid-stream errors end the stream).
5. If no candidate succeeds, the agent receives `502` with an error message.

Endpoints:

- `POST /v1/{*path}` — proxied to the matched backend (e.g. `/v1/chat/completions`, `/v1/completions`, `/v1/embeddings`). The raw request body is forwarded unchanged; the selected backend's key is sent as `Authorization: Bearer <key>`.
- `GET /v1/models` — returns the union of all configured models (OpenAI-style).
- `GET /health` — `llm-hub ok`.

## Architecture

```
Agent ──HTTP──► Axum handler ──MPSC(job)──► worker task
   ▲                                           │
   │                                     resolve(model)
   │                                           ▼
   │                                  ordered backends/keys
   │                                           │
   └──stream(SSE)◄──MPSC(bytes)◄── reqwest (rustls) ──► upstream
```

- **`main.rs`** — clap CLI (`--serve` / `--admin` / `--bind` / `--init`), logging init, dispatch.
- **`config.rs`** — `Config` / `Backend` structs, load/save, and the `Resolver` (round-robin key load balancing + model→backend resolution).
- **`worker.rs`** — MPSC worker: receives `ProxyJob`s, runs each in its own task.
- **`proxy.rs`** — the upstream try-loop (ordered failover, streaming).
- **`server.rs`** — Axum `Router`, handlers, `serve()`.
- **`admin.rs`** — ratatui + crossterm TUI config editor.

## Development

```bash
cargo build          # build
cargo test           # unit tests
cargo run -- --serve # run the proxy from source
```

Set `RUST_LOG=debug` for verbose routing/failover logs.

## License

MIT
