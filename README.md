# rimcrow-mcp

RimCrow MCP **生产入口** `rimcrow-mcp.exe` —— 一个极薄的 stdio ↔ Streamable HTTP 桥。

```text
MCP 客户端(ZCode/OpenCode) --stdio--> rimcrow-mcp.exe --Streamable HTTP--> RimCrow.exe 内嵌端点
                                    （冷启动 <1s）                        （mcp-embed，FastMCP 工具层）
                                     tools/list 透传优先                    endpoint 文件 url/pid/token
                                     嵌入快照 = GUI 未开时保底               探活通过 → 直接 attach，绝不二次拉起
```

权威规格：`E:\Games\RimWorld\dev\RimCrow\.worktrees\feat-mcp-server\plan\SPEC-rust-bridge.md`（R1-R9、§5 错配矩阵、§7 交付件、§8 验收）。
endpoint 文件契约：`plan\SPEC-mcp-embed.md` §4.3（`schema_version=1`）。

## 构建 / 测试

```powershell
cargo build --release        # 产物 target\release\rimcrow-mcp.exe（约 4.0 MB）
cargo test                   # 47 个单测

# 冒烟五态唯一入口已收敛到 RimCrow 仓库（本目录不留重复件）：
cd E:\Games\RimWorld\dev\RimCrow\.worktrees\feat-mcp-server
uv run python scripts\smoke_bridge.py      # cold / mismatch / fake / spawn / live 全跑
uv run python scripts\smoke_bridge.py --live   # 单场景
```

| 冒烟场景（scripts/smoke_bridge.py） | 覆盖 |
| --- | --- |
| `--cold` | R1 握手 <1s、R2 快照清单、R4 因果 isError+nextActions、stdout 纪律、退出码 0、10 次 spawn/kill + temp 零新增 |
| `--mismatch` | §5 场景 3（pid 过期）/ 场景 6（endpoint schema v2）文案与两侧版本号 |
| `--fake` | R3 透传（list/call）、远端 isError 逐字回抛、§5 场景 4（unknown tool 版本增强）、R8 版本错配告警、bearer 拒绝——**上游为本目录 `fake_upstream.py` 测试替身，非 RimCrow** |
| `--spawn` | R5/D11 拉起→endpoint 生效→转发、二次调用不重复拉起、防重入进程计数 |
| `--live` | 真实 RimCrow 端点透传；端点不在时打印 `[SKIP]` |

`tools-cache.json` **是构建期输入，不是手写文件**，由 RimCrow 仓库的生成器产出：

```powershell
cd E:\Games\RimWorld\dev\RimCrow\.worktrees\feat-mcp-server
uv run python scripts\gen_tool_cache.py     # 输出 → 本目录 tools-cache.json
cd E:\Games\RimWorld\dev\rimcrow-mcp
cargo build --release                        # include_str! 重新嵌入
```

## CLI

```
rimcrow-mcp.exe [--spawn] [--spawn-headless] [--version] [--help]
```

| 参数 | 行为 |
| --- | --- |
| `--spawn` | 未检出 endpoint → 拉起 RimCrow → 有界 15s 轮询 endpoint 写出生效；**探活通过绝不二次拉起**（D11） |
| `--spawn-headless` | 同上，拉起时附带 `--browser --no-open` |
| `-V/--version` | 打印版本三元组：crate 版本 + 快照 rimcrow_version + captured_at |
| `-h/--help` | 用法 |

环境变量：

| 变量 | 含义 |
| --- | --- |
| `RIMCROW_MCP_ENDPOINT` | 覆盖 endpoint 文件路径（默认 `%LOCALAPPDATA%\RimCrow\mcp-endpoint.json`） |
| `RIMCROW_SPAWN_CMD` | 覆盖拉起命令（按 shell 规则切分后直接执行，不经 `cmd`），如 `python main.py --browser --no-open`。**为何自实现而不引 `shlex` / `shell-words` / `winsplit`**：见 `src/spawn.rs` 中 `split_command` 的文档注释（POSIX 库会吃掉未引号路径的反斜杠、`winsplit` 不支持单引号字面量） |
| `RIMCROW_MCP_LOG` | 日志级别，默认 `info`；**日志只写 stderr** |
| `RIMCROW_MCP_SPAWN_NO_WINDOW` | 设 `0` 可关掉拉起子进程时的 `CREATE_NO_WINDOW`（诊断用） |

拉起目标解析顺序：`RIMCROW_SPAWN_CMD` → bridge 同目录 `RimCrow.exe` → 因果错误 + `nextActions`。

**子进程 stdio 隔离（R7，实测教训）**：拉起的子进程 `stdin=NUL`、`stdout=NUL`、
`stderr=继承`。若继承桥的 stdin/stdout，实测会出现两个故障——子进程把输出混进
JSON-RPC 通道（`cmd /c echo hi` 直接打脏帧）、以及控制台子进程在初始化期挂死约 15s
（Python 侧表现为解释器停在 `site` 阶段、CPU 恒 0）。`stderr` 保留继承以便诊断信息进日志。

## 行为要点

- **R1 保底握手**：`initialize` 纯本地（instructions / serverInfo.version 来自嵌入快照），实测 ~15-25 ms。
- **R2 清单双模**：`tools/list` 每次实时透传上游（上游分页在此合并成单页、不下发 cursor）；
  探活失败 / 上游不可达 → 嵌入快照全量应答 + stderr 因果日志。
- **R3 转发**：`tools/call` 探活 → HTTP（`Authorization: Bearer <token>` 由 rmcp transport 注入，
  `mcp-session-id` 由 transport 建会话时持有并逐请求携带）→ 远端 `isError` 结果逐字回抛。
- **R4 降级**：endpoint 缺失 / pid 过期 / schema 超版 / 非 loopback → `isError:true` +
  因果文案（含路径、pid、两侧版本号）+ `nextActions[]`，**无空洞 timeout 文案**。
- **R7 stdout 纪律**：stdout 只有 JSON-RPC；tracing 全 stderr；EOF → 退出码 0；
  panic → 退出码 1 + stderr 一行（自定义 hook，不弹窗）。
- **R8 版本自报**：`rimcrow-mcp <crate>（快照 RimCrow <X> @ <captured_at>）`；
  成功 attach 后比对上游 `serverInfo.version`，不一致 → stderr 警告一行（不阻断）。
- **R9 桥无状态**：进程内只持有 endpoint 路径 + 上游会话（会话属允许状态），不缓存清单；多开是设计行为。

## 源码结构

| 文件 | 职责 |
| --- | --- |
| `src/main.rs` | CLI、日志→stderr、panic hook、退出码、tokio runtime 接线 |
| `src/bridge.rs` | `ServerHandler`（initialize / tools/list / tools/call）、上游会话缓存、`SpawnGate` |
| `src/endpoint.rs` | endpoint 文件读取 + 探活（`sysinfo`：pid 存活 / 进程名白名单 / 启动时间 CLOCK_SKEW=5、MAX_BOOT=300） |
| `src/envelope.rs` | 错误信封 `{"status":"error","message":...,"nextActions":[...]}`（与 `mcp_guard.build_error_text` 同形） |
| `src/snapshot.rs` | `include_str!("../tools-cache.json")` 解析 → `rmcp::model::Tool` |
| `src/spawn.rs` | `--spawn` 拉起（`CREATE_NO_WINDOW` + stdio 隔离）、shell 式参数切分（自实现，选型理由见 `split_command` 注释）、15s 有界轮询 |
| `fake_upstream.py` | 最小 Streamable HTTP MCP 上游（**测试替身，非 RimCrow**），供 RimCrow 仓 `scripts/smoke_bridge.py` 的 fake 场景使用；**为何不用 rmcp 的 `StreamableHttpService`（feature `transport-streamable-http-server`，本 crate `Cargo.toml` 未启用）** 见该文件模块 docstring |

## 已知边界

- 数据面单例互斥已落地（`backend/lifecycle.py`，MAP D21）：`--spawn` 拉起时若已有实例在跑，
  新实例静默退出（exit 0），桥按「endpoint 写出生效」为就绪判据，不受影响。
- 进度通知（`progress`）不跨桥转发；上游 MRTR `input_required` / Tasks `task` 不代为续跑，均给因果 `isError`。
- 旧 `RimCrow.exe --mcp` Python 薄层已按 D3 五修从代码与客户端配置中移除；残留进程与本桥无关。
