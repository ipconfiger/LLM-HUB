# @ipconfiger/llm-hub

A local LLM proxy that routes requests to multiple backends by model name.

## Install

```bash
npm install -g @ipconfiger/llm-hub
llm-hub --serve
```

This package ships a small Node.js launcher that automatically selects the
correct prebuilt binary for your platform. npm downloads **only** the binary
matching your OS/architecture.

### Supported platforms

| OS      | Architecture |
| ------- | ------------ |
| macOS   | arm64 (Apple Silicon), x64 (Intel) |
| Linux   | x64, arm64 (musl static)           |
| Windows | x64                                |

### Requirements

- Node.js 18+ (only needed for the launcher; the binary itself is standalone)
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
