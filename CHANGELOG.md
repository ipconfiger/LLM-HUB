# 更新日志

本项目所有值得注意的变更都会记录在此文件中。

本文件遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/) 规范,版本号遵循 [Semantic Versioning](https://semver.org/lang/zh-CN/)。

## [0.1.0] - 未发布

首个版本。一个本地运行的 OpenAI 兼容 LLM 代理网关,把多家服务商 × 多把 key 聚合成一个统一入口,按模型名路由,粘性 key + 分类故障转移。

### Added

- **核心代理**:本地运行的 OpenAI 兼容 LLM 代理。Agent 指向 `http://127.0.0.1:3000/v1`,代理根据请求体里的**模型名**把请求路由到匹配的后端;原始请求体透传,选中后端的 key 作为 `Bearer` 鉴权,SSE 流式**字节级透传**回传。基于 Axum + tokio MPSC worker 架构,reqwest 使用 **rustls**。
- **多服务商聚合 + 按模型路由**:配置里把同一个模型挂到多个后端(高可用),请求按模型名自动找到服务它的后端列表。
- **粘性 key + 分类故障转移**(核心策略):一个后端可配多把 key;代理**粘性使用同一把 key 直到它被耗尽**再换下一把,并按错误类型分别处理:
  - `2xx`:流式回传,粘住该 key;
  - key 耗光(`401` / `402` / `403` / `429`):该 key 冷却 **60 秒**,换**同一后端的下一把 key**;
  - 后端不可达(网络错误 / `5xx`):整个后端冷却 **10 秒**,**跳过其剩余 key**,直接换下一个后端;
  - 其它 `4xx`(如 `400` / `404` / `422`):原样回传 Agent,不重试;
  - 所有后端失败 → `502`;冷却到期的 key / 后端**自动重新加入**池中。
- **Admin TUI 配置编辑器**(`--admin`):ratatui + crossterm 终端界面,可浏览 / 新增 / 删除后端、编辑各字段(`name` / `base_url` / `keys` / `models`),保存到 `~/.config/llm-hub/settings.json`;支持 CJK 安全光标、脏标记、退出自动保存。
- **自动获取模型列表**:TUI 中选中后端按 `f`,自动请求后端 `/v1/models`(回退 `/models`)并用第一把 key 鉴权,解析 OpenAI 风格列表后自动填入 `models` 字段;异步执行,不阻塞界面。
- **CLI**:`--serve`(启动代理,默认 `127.0.0.1:3000`,可 `--bind` 自定义)、`--admin`(TUI)、`--init`(写入示例配置)。
- **npm 分发**:支持 `npm install -g @ipconfiger/llm-hub` 安装预构建二进制(macOS arm64 / x64、Linux x64 / arm64 musl 静态、Windows x64),npm 自动只下载当前平台对应的二进制;底层为 optionalDependencies + 按平台子包 + Node 启动器模式。
- **Linux glibc(gnu)二进制变体**(可选):除默认 musl 静态版外,额外提供 `@ipconfiger/llm-hub-linux-{x64,arm64}-gnu` 子包;通过环境变量 `LLM_HUB_VARIANT=gnu` 启用,适合需要在 glibc 环境动态链接的场景。
- **持续集成**:push / PR 时运行 `cargo fmt` / `cargo clippy -D warnings` / `cargo test` 检查;打 tag `v*` 时由 GitHub Actions 交叉编译(mac 原生、Linux musl / gnu 走 cargo-zigbuild、Windows msvc)并自动发布各平台子包与主包到 npm。

### Changed

- 故障转移策略从"轮询负载均衡"改为"**粘性 key 耗尽**"(榨干每把 key 再换),更适合多 key 池化、最大化每把 key 可用额度的场景。

### Fixed

- 修复 npm 发布工作流中两处版本同步问题:子包 tarball 之前用提交版本打包(现改为打包前写入 tag 版本);主包 `optionalDependencies` 不会随 `version` 一起更新(现已同步)。

[0.1.0]: https://github.com/ipconfiger/LLM-HUB/releases
