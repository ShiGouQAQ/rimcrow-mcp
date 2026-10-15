# rimcrow-mcp

RimCrow 的 MCP 生产入口：一个极薄的 **stdio ↔ Streamable HTTP** 桥，把 RimCrow GUI 内嵌的 MCP 工具面接到任意只讲 stdio 的 MCP 客户端（OpenCode、ZCode、Claude Code、Codex 等）。

```text
MCP 客户端 ──stdio──> rimcrow-mcp.exe ──Streamable HTTP──> RimCrow GUI 内嵌端点
```

基于 [rmcp](https://github.com/modelcontextprotocol/rust-sdk)（MCP 官方 Rust SDK）实现，冷启动不到一秒，发布形态是单个约 4 MB 的可执行文件，运行期不落任何外部清单文件。

[RimCrow](https://github.com/Inky-Feather/RimCrow) 是 RimWorld 的模组管理器；本仓只含桥，MCP 数据面（工具实现与 endpoint 文件）由 RimCrow 侧提供。

## 为什么需要它

RimCrow 的数据面是 GUI 进程内的 loopback HTTP 端点（bearer 鉴权），而多数 agent 客户端只会拉起一个 stdio 子进程，桥负责两种形态之间的转换。GUI 没开时客户端也要能完成握手、看到工具清单，桥在编译期嵌入一份工具快照作为保底。桥与 GUI 是两个独立产物、版本可能有代差，桥把代差变成可读的错误：版本错配时点名两侧版本，新参数会被旧 GUI 静默忽略前就地拦下——「假成功」比报错危险。

## 工作方式

initialize 永不依赖上游：instructions 与版本自报（crate 版本、快照里的 RimCrow 版本、快照时间）来自嵌入快照，GUI 未开也能握手。

tools/list 双模：探活通过时实时透传上游全量清单（上游分页在桥侧合并为单页），不可达时回落嵌入快照。两条路径都按 MCP 2026-07-28 方言自报 `ttlMs: 0`（清单立即过期，真相始终在上游）与 `cacheScope: "private"`（工具面随本机档案变化，不进跨用户共享缓存）。

tools/call 先探活再转发：bearer 由 rmcp transport 注入，会话由 transport 维护，上游的 isError 结果逐字回抛。转发前做参数预校验——按当前生效清单（实时优先、快照兜底）拦下 schema 未声明的参数、明显类型不符与 enum 越界，错误文案列出该工具当前生效的已知参数；未知工具、空参数、合法参数一律原样透传。类型判定按服务端 pydantic 的宽松语义放行可转换取值，宁可放过、不可误杀。

任何不可达都以 `isError: true` 加因果文案（路径、pid、两侧版本）与 `nextActions` 动作列表降级，绝不返回空洞的 timeout 文案。桥无状态：进程内只持有 endpoint 路径与上游会话，多开是设计行为。

## 构建与接入

需要 Rust 工具链：

```powershell
cargo build --release   # 产物 target\release\rimcrow-mcp.exe
cargo test
```

获取途径（二选一）：

- **随 RimCrow fork 安装（推荐）**：fork 发布包内自带 `rimcrow-mcp.exe`（位于 RimCrow 安装目录，如 `apps\RimCrow\`），与同批 GUI 一起构建、版本对齐，装好 RimCrow 即无需单独安装。
- **从本仓构建**：`cargo build --release` 产出 `target\release\rimcrow-mcp.exe`；编译期把仓内 `tools-cache.json`（由 RimCrow 侧 `scripts/gen_tool_cache.py` 生成的工具快照）嵌入二进制，工具面变化时先重新生成快照再重编译。

接入 MCP 客户端：把本桥注册为 stdio server。以通用 `mcpServers` 配置为例：

```json
{
  "mcpServers": {
    "rimcrow": {
      "command": "C:\\path\\to\\rimcrow-mcp.exe"
    }
  }
}
```

前置条件：RimCrow GUI 在运行，或允许桥自动拉起。GUI 就绪后会把连接信息写进 `%LOCALAPPDATA%\RimCrow\mcp-endpoint.json`；桥每次调用都重新探活，GUI 重启不需要重启桥。

## CLI 与环境变量

| 参数 | 行为 |
| --- | --- |
| `--spawn` | 未检出 endpoint 时拉起 RimCrow GUI，有界轮询 endpoint 写出（15 秒）；探活通过绝不二次拉起 |
| `--spawn-headless` | 同上，拉起时附带 `--browser --no-open` |
| `-V` / `--version` | 打印版本三元组：crate 版本 + 快照 RimCrow 版本 + 快照时间 |
| `-h` / `--help` | 用法 |

| 环境变量 | 含义 |
| --- | --- |
| `RIMCROW_MCP_ENDPOINT` | 覆盖 endpoint 文件路径（默认 `%LOCALAPPDATA%\RimCrow\mcp-endpoint.json`） |
| `RIMCROW_SPAWN_CMD` | 覆盖拉起命令（按 shell 规则切分后直接执行，不经 cmd 解释器），如 `python main.py --browser --no-open` |
| `RIMCROW_MCP_LOG` | 日志级别，默认 `info`；日志只写 stderr，stdout 只有 JSON-RPC |
| `RIMCROW_MCP_SPAWN_NO_WINDOW` | 设 `0` 关闭拉起子进程时的无窗口标志（诊断用） |

## endpoint 文件契约

`mcp-endpoint.json` 由 RimCrow GUI 写出，请勿手工编辑。字段：`schema_version`（当前为 1）、`url`、`pid`、`token`、`started_at`、`mode`。

桥探活时做三道校验：schema 版本受支持；进程存活且进程名在白名单内（rimcrow.exe、python.exe、pythonw.exe）；endpoint 写出时间与进程启动时间吻合。Windows 的 pid 会复用，时间比对是防冒名连接的主防线；URL 只接受 127.0.0.1 loopback，防止被诱导连到外部地址。

## 已知边界

- 上游的多轮请求（input_required）与任务化（task）结果不代为续跑，返回因果错误并指引直连端点处理。
- 进度通知不跨桥转发。
- `--spawn` 拉起时若已有 GUI 实例在跑，新实例按 RimCrow 的单实例互斥静默退出；桥以 endpoint 写出为就绪判据，不受影响。

## 开发

源码分两层：`main` 负责 CLI、stderr 日志、panic 收敛与退出码；`bridge` 实现 MCP handler 与编排。其余五个叶子模块互不依赖、各自可单测——`endpoint`（文件探活）、`spawn`（进程拉起）、`snapshot`（嵌入快照解析）、`validate`（参数预校验）、`envelope`（错误信封）。

`fake_upstream.py` 是标准库零依赖的上游替身，用于脱离 GUI 验证整条透传链路（bearer、会话、清单与调用转发、isError 回抛、未知工具增强文案、版本错配告警）。手动使用：`python fake_upstream.py --port <port> --endpoint <路径>`，endpoint 文件写出后桥即可探活连接。

`tools-cache.json` 是构建期生成物：由 RimCrow 侧工具从真实工具注册表导出、经 `include_str!` 嵌入二进制。快照不手写、文件不手编；工具面变化时重新生成快照后重编译。

方言兼容由桥吸收：对旧方言上游自动补齐必填的 `resultType` 与缓存提示字段，对旧方言客户端由 rmcp 按协商版本剥除这些字段，两种对端都成立。

## License

本桥以 **GPL-3.0-or-later** 发布（见 [LICENSE](./LICENSE)）。RimCrow GUI 本体遵循其上游的 MIT 许可证，与本桥是相互独立的两个作品。
