# llm-hub

**一个本地运行的 LLM 代理网关**:把"多家 OpenAI 兼容服务商 × 每家多把 API key"聚合成**一个统一的本地入口**。Agent/客户端只需指向 `http://127.0.0.1:3000/v1`,代理根据请求里的**模型名**自动把请求路由到正确的后端。

- **一个入口** —— Agent 始终只填 `http://127.0.0.1:3000/v1`,不用关心后端是哪家。
- **多 key 池化** —— 一个后端可配多把 key,粘性使用直到限流/耗光,榨干每把 key 的额度。
- **无感故障转移** —— key 耗光自动换 key,整家后端挂了自动换下一家,Agent 无感知。
- **本地私有** —— 密钥只存在本机配置文件;rustls 单二进制,部署即拷。

---

## 它解决什么问题

如果你同时用多家 OpenAI 兼容服务商(硅流、DeepSeek 官方、……),而且每家手里还有好几把 key(多个账号的免费/廉价额度),你大概率遇到过这些痛点:

1. **来回改配置** —— 用不同模型要切不同服务商,Agent 的 `base_url` / `api_key` 改来改去。
2. **单 key 一限流就停摆** —— 一把 key 跑满额度或被 429,得手动换下一把,中断体验。
3. **一家挂了全盘瘫痪** —— 某个服务商临时 5xx 或网络抽风,请求直接失败。
4. **免费额度浪费** —— 每把 key 都有独立配额,但没人记得轮流用,最后只用了一把,其余闲置。

`llm-hub` 的做法:

- **聚合入口**:所有服务商和所有 key 写进一个配置文件,Agent 只认一个地址。
- **粘性 key 池化**:代理粘住同一把 key 用,直到它被限流/耗光(401/402/403/429)才换下一把;耗光的 key 冷却 **60 秒**后会**自动归队**复用 —— 这样把每把 key 的可用吞吐压满,而不是简单轮询。
- **分类故障转移**:key 报凭证错误 → 只换**这家**的下一把 key,不放弃一个活着的后端;后端报 5xx 或连不上 → 立刻**跳到下一家**,不浪费请求。
- **多后端同模型 = 高可用**:同一个模型可以挂在多个后端上,一家挂了另一家顶上。
- **接入新后端近乎零配置**:TUI 里按 `f` 自动从后端拉取模型列表。

---

## 快速开始

三步走:

```bash
# 1. 生成示例配置(若配置文件不存在)
llm-hub --init

# 2. 编辑配置,填入你的真实后端与多把 key
#    可以手动编辑 JSON,也可以用 TUI:
llm-hub --admin

# 3. 启动代理
llm-hub --serve
```

然后把任意 OpenAI 兼容客户端的入口指过来:

```
base_url = http://127.0.0.1:3000/v1
api_key  = <随便填>   # 代理忽略它,改用配置里每个后端的 key
```

发一个请求测试:

```bash
curl http://127.0.0.1:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"deepseek-chat","messages":[{"role":"user","content":"你好"}]}'
```

---

## 配置文件

### 路径(跨平台)

| 系统 | 路径 |
|------|------|
| Linux | `~/.config/llm-hub/settings.json` |
| macOS | `~/Library/Application Support/llm-hub/settings.json` |
| Windows | `%APPDATA%\llm-hub\settings.json` |

### 格式示例(多 key + 多后端同模型)

下面这个例子同时演示了**池化**(硅流放了 3 把 key)和**高可用**(`deepseek-chat` 同时挂在 `deepseek-official` 和 `硅流` 两家):

```json
{
  "backends": [
    {
      "name": "硅流",
      "base_url": "https://api.siliconflow.cn",
      "keys": ["sk-key-1", "sk-key-2", "sk-key-3"],
      "models": ["Qwen/Qwen3-32B", "deepseek-ai/DeepSeek-V3", "deepseek-chat"]
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

> 请求里的 `model: "deepseek-chat"` 时,代理会按配置顺序先试 `硅流`,硅流挂了再试 `deepseek-official`,Agent 完全无感。

### 字段说明

| 字段 | 类型 | 说明 |
|------|------|------|
| `name` | string | 显示用名称(日志、TUI 里展示)。 |
| `base_url` | string | OpenAI 兼容 API 的根地址,结尾斜杠可有可无。 |
| `keys` | `array<string>` | 一把或多把 key。**粘性使用**:优先用当前粘住的那把,直到它被限流/耗光才换下一把;耗光后冷却 60 秒自动归队。 |
| `models` | `array<string>` | 该后端提供的模型名列表,与请求体里的 `model` 字段匹配。 |

**两个要点**:
- **同一个 model 可以出现在多个 backend 里** → 高可用,一家挂了另一家顶。
- **同一个 backend 可以放多把 key** → 池化,榨干每把 key 的额度。

---

## 工作原理

请求进来后,代理按"模型名"找后端,再在每个后端内部用"粘性 key",并对每一次上游响应**分类处理**:

```
请求 POST /v1/<路径> ── 读取 model 字段 ──► 候选后端(配置顺序)
        │
        ▼
  按顺序逐个后端尝试,每个后端内部优先用"当前粘住的 key":
        │
        ├─ 2xx 成功        ──► 流式原样回传 Agent(字节级透传 SSE),粘住这把 key
        ├─ 401/402/403/429 ──► 这把 key 冷却 60s,换【同后端】下一把 key 继续
        ├─ 5xx / 网络错误   ──► 整个后端冷却 10s,跳过其剩余 key,换【下一家】后端
        └─ 其它 4xx        ──► 请求本身有问题,状态码 + 响应体原样回传,不重试
        │
        ▼
  所有后端都试过仍失败 → Agent 收到 502 + 各后端失败原因
```

### 为什么这么设计

- **粘性而非轮询**:轮询会让每把 key 都被均匀限流,反而总吞吐更低;粘性是把一把 key 用到耗尽再换,最大化单把 key 在窗口内的可用额度,冷却 60 秒后又能自动复用 —— 对"多账号免费 key 池"这种场景最划算。
- **key 耗光 vs 后端挂了 分开处理**:一把 key 限流不代表整家服务商死了(换 key 就行);反之整家 5xx 了,在它身上继续换 key 是浪费时间。代理据此决定是"换 key"还是"换后端",不浪费请求。
- **冷却自动恢复**:限流窗口通常很短,key 冷却 60 秒、后端冷却 10 秒到期后自动重新加入,无需重启服务。
- **其它 4xx 原样回传**:400/404/422 这类是请求本身的问题(比如参数错了、模型名拼错),重试多少次结果都一样,直接把上游的真实报错透传给 Agent,便于排查。

---

## 在客户端里接入

任何 OpenAI 兼容的客户端/SDK,把 `base_url` 指向 `http://127.0.0.1:3000/v1`,`api_key` 随便填一个非空值即可。

**OpenAI Python SDK 风格:**

```python
from openai import OpenAI

client = OpenAI(
    base_url="http://127.0.0.1:3000/v1",
    api_key="any-value",   # 代理忽略它,实际用配置里的 key
)

resp = client.chat.completions.create(
    model="deepseek-chat",   # 代理据此路由到对应后端
    messages=[{"role": "user", "content": "你好"}],
)
```

**curl / 通用 HTTP:**

```bash
curl http://127.0.0.1:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer any-value" \
  -d '{"model":"deepseek-chat","messages":[{"role":"user","content":"你好"}]}'
```

> 查看代理聚合后的全部可用模型:`curl http://127.0.0.1:3000/v1/models`

---

## TUI 配置管理

用 `llm-hub --admin` 打开终端 TUI 编辑器。常用键位:

### 浏览模式

| 键位 | 作用 |
|------|------|
| `↑` / `↓` | 选择后端 |
| `Enter` | 进入编辑该后端 |
| `a` | 新增后端 |
| `d` | 删除后端 |
| `f` | **自动获取模型列表**(见下) |
| `s` | 保存 |
| `q` / `Ctrl+C` | 退出(有改动时自动保存) |

### 编辑模式

| 键位 | 作用 |
|------|------|
| `Tab` / `Shift+Tab`(或 `↑` / `↓`) | 在字段间切换(name / base_url / keys / models) |
| `←` / `→` | 移动光标 |
| `Backspace` / `Delete` | 删除 |
| `Esc` / `Enter` | 提交 |

> `keys` 和 `models` 用**逗号分隔**输入,例如 `sk-1, sk-2, sk-3`。

### `f` 自动获取模型列表

在浏览模式下选中某个后端,按 `f`:代理会用该后端的 `base_url` + 第一把 key,请求 `{base_url}/v1/models`(失败则回退 `{base_url}/models`),解析 OpenAI 风格的模型列表并**自动填入 `models` 字段**。过程中状态栏会显示"正在获取模型列表…"。接入一个新后端,基本只需要填 `name` / `base_url` / `keys`,然后按 `f` 即可。

---

## 命令参考 / 构建安装

### 命令

| 命令 | 说明 |
|------|------|
| `llm-hub --serve` | 启动代理服务,默认监听 `127.0.0.1:3000`。 |
| `llm-hub --serve --bind <addr>` | 自定义监听地址,如 `--bind 127.0.0.1:4000`。 |
| `llm-hub --admin` | 启动终端 TUI 配置编辑器。 |
| `llm-hub --init` | 配置文件不存在时,写入一份示例配置并退出。 |

### 端点

| 方法 / 路径 | 说明 |
|------|------|
| `POST /v1/{*path}` | 代理到匹配的后端(如 `/v1/chat/completions`、`/v1/completions`、`/v1/embeddings`)。原始请求体原样转发;选中后端的 key 作为 `Authorization: Bearer <key>`。 |
| `GET /v1/models` | 返回所有配置模型的并集(OpenAI 风格)。 |
| `GET /health` | 返回 `llm-hub ok`。 |

### 构建

```bash
cargo build            # debug 版
cargo build --release  # 优化版,推荐日常使用
```

二进制名即 `llm-hub`(release 版在 `target/release/llm-hub`)。若设了 `CARGO_TARGET_DIR`,去对应目录找二进制。

---

## 小贴士 / FAQ

- **同模型配多后端做高可用**:把同一个模型名写进多个 backend 的 `models` 里,一家挂了另一家自动顶上,Agent 无感。
- **多 key 池化榨额度**:给一个 backend 填多把 key,代理粘性使用 + 耗光冷却复用,把每把 key 的可用吞吐压满 —— 适合多个账号的免费/廉价 key 池。
- **限流后自动恢复**:被 429 的 key 冷却 60 秒、被 5xx 的后端冷却 10 秒,到期自动重新加入,无需重启。
- **密钥只存本机**:所有 key 只写在本地 `settings.json` 里,不外发;代理本身只监听本地回环地址。
- **不是轮询负载均衡**:当前实现是"粘性耗尽"策略 —— 优先用同一把 key 直到它被限流,而非把请求均匀分散到所有 key,目的是最大化总吞吐。
- **看路由细节**:设 `RUST_LOG=debug` 可输出详细的路由与故障转移日志,排查问题时很有用。
