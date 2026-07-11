# @ipconfiger/llm-hub

A local LLM proxy that routes requests to multiple backends by model name.

## Install

```bash
npm install -g @ipconfiger/llm-hub
llm-hub --serve
```

At install time a `postinstall` script automatically downloads the correct
prebuilt binary for your platform from [GitHub Releases](https://github.com/ipconfiger/LLM-HUB/releases).

### Supported platforms

| OS      | Architecture |
| ------- | ------------ |
| macOS   | arm64 (Apple Silicon), x64 (Intel) |
| Linux   | x64, arm64 (musl static default; set `LLM_HUB_VARIANT=gnu` for glibc) |
| Windows | x64 |

### Environment variables

| Variable | Default | Description |
| -------- | ------- | ----------- |
| `LLM_HUB_VARIANT` | _(unset)_ | Set to `gnu` on Linux to download the glibc-linked binary instead of musl. |
| `LLM_HUB_FORCE_DOWNLOAD` | _(unset)_ | Set to `1` to force re-download even if the binary already exists. |

### Requirements

- Node.js 18+ (only for install/download; the binary itself is standalone)
- If your platform has no prebuilt binary, build from source:
  [LLM-HUB](https://github.com/ipconfiger/LLM-HUB)

## Usage

```bash
llm-hub --serve              # start the HTTP proxy server
llm-hub --admin              # TUI admin interface
llm-hub --init               # create a sample config
llm-hub --bind 0.0.0.0:8080  # custom bind address (with --serve)
```

See the [project README](https://github.com/ipconfiger/LLM-HUB) for full docs.

## License

MIT
