//! `rimcrow-mcp` 主体：stdio ↔ Streamable HTTP 极薄桥（`ServerHandler` 实现）。
//!
//! 拓扑：
//! ```text
//! MCP 客户端 --stdio--> rimcrow-mcp --Streamable HTTP--> RimCrow GUI 内嵌端点
//! ```
//!
//! 行为约定：
//!
//! initialize 永不依赖上游，本地应答，instructions 与版本自报取自嵌入快照。
//! tools/list 透传优先：endpoint 探活通过时每次实时转发上游全量清单，不可达时
//! 以嵌入快照全量应答。tools/call 先探活再转发，bearer 由 rmcp transport 注入，
//! mcp-session-id 由 transport 维护并逐请求携带，远端 isError 结果逐字回抛。
//! endpoint 缺失或 pid 过期时返回 isError:true，附因果文案与 nextActions 列表。
//! 探活通过绝不二次拉起，--spawn 只在未检出 endpoint 时拉起。桥自报版本三元组，
//! attach 成功后与上游 serverInfo.version 比对，不一致时向 stderr 输出一行警告。
//! 桥无状态：进程内只持有 endpoint 路径与上游会话，不缓存清单。
//!
//! 参数预校验防「假成功」：tools/call 转发前把调用参数对照当前生效清单（live 优先，
//! 拿不到才用嵌入快照）拦一道，schema 未声明的参数、明显类型不符、enum 越界都以
//! isError:true 加因果文案与已知参数列表拒绝；未知工具、空参数、合法参数一律零影响
//! 原样转发，拦截规则宁可放过不可误杀。

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value};

use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, ClientCapabilities,
    ClientConfig, ContentBlock, Implementation, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::{Peer, RequestContext, RunningService};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{ErrorData as McpError, RoleClient, RoleServer, ServerHandler, ServiceError, ServiceExt};

use crate::envelope::build_error_text;
use crate::endpoint::{probe_default, EndpointInfo, ProbeOutcome, ProbeStatus};
use crate::snapshot::{self, Snapshot};
use crate::spawn;
use crate::validate::{self, ArgIssue};

/// bridge 自身版本（crate 版本，版本三元组第一项）。
pub const CRATE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 建立上游会话的超时。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// `tools/list` 透传超时；超时按上游不可达处理，回落嵌入快照，不阻塞客户端。
const LIST_TIMEOUT: Duration = Duration::from_secs(10);

/// 上游会话缓存键分隔符（url + token，二者任一变化即重建会话）。
const KEY_SEP: char = '\u{0}';

/// 已 attach 的上游会话（桥只持有 endpoint 路径与会话两类状态，这是其一）。
struct Upstream {
    key: String,
    svc: RunningService<RoleClient, ClientConfig>,
}

/// `--spawn` 闸门：保证每个 bridge 进程至多发起一次拉起尝试，重复调用复用首次结果。
pub struct SpawnGate {
    endpoint_path: PathBuf,
    headless: bool,
    enabled: bool,
    attempts: std::sync::atomic::AtomicU32,
    cell: tokio::sync::OnceCell<Result<(), String>>,
}

impl SpawnGate {
    fn new(endpoint_path: PathBuf, enabled: bool, headless: bool) -> Arc<Self> {
        Arc::new(Self {
            endpoint_path,
            headless,
            enabled,
            attempts: std::sync::atomic::AtomicU32::new(0),
            cell: tokio::sync::OnceCell::new(),
        })
    }

    /// 确保 endpoint 可用；未启用 spawn 时不做任何尝试，直接返回禁用错误。
    async fn ensure(&self) -> Result<(), String> {
        if !self.enabled {
            return Err(OK_DISABLED.to_string());
        }
        self.cell
            .get_or_init(|| async {
                self.attempts
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                spawn::spawn_and_wait(&self.endpoint_path, self.headless).await
            })
            .await
            .clone()
    }

    fn attempts(&self) -> u32 {
        self.attempts.load(std::sync::atomic::Ordering::SeqCst)
    }
}

const OK_DISABLED: &str = "spawn disabled";

/// stdio 入口桥。
pub struct Bridge {
    endpoint_path: PathBuf,
    spawn_gate: Arc<SpawnGate>,
    spawn_enabled: bool,
    upstream: tokio::sync::Mutex<Option<Upstream>>,
}

impl Bridge {
    pub fn new(opts: crate::Options) -> Self {
        let endpoint_path = crate::endpoint::default_endpoint_path();
        let spawn_gate = SpawnGate::new(endpoint_path.clone(), opts.spawn, opts.headless);
        Self {
            endpoint_path,
            spawn_gate,
            spawn_enabled: opts.spawn,
            upstream: tokio::sync::Mutex::new(None),
        }
    }

    /// endpoint 路径（对外暴露用于日志）。
    pub fn endpoint_path(&self) -> &std::path::Path {
        &self.endpoint_path
    }

    fn probe(&self) -> ProbeOutcome {
        probe_default(&self.endpoint_path)
    }

    /// 版本上下文：crate 版本 + 快照 rimcrow_version + captured_at。
    fn version_ctx(&self) -> String {
        let snap = snapshot::load();
        format!(
            "rimcrow-mcp {CRATE_VERSION}（快照 RimCrow {} @ {}）",
            snap.rimcrow_version, snap.captured_at
        )
    }

    fn error_result(message: &str, actions: Vec<String>) -> CallToolResult {
        CallToolResult::error(vec![ContentBlock::text(build_error_text(
            message, &actions,
        ))])
    }

    /// 由探活结果构造降级错误：message 写明原因与当前版本，nextActions 给出可执行
    /// 动作，不使用空洞的超时措辞。
    fn degraded_for_probe(&self, out: &ProbeOutcome, spawn_note: Option<&str>) -> CallToolResult {
        let mut actions: Vec<String> = Vec::new();

        let message = match &out.status {
            ProbeStatus::UnsupportedSchema(n) => {
                actions.push(format!(
                    "升级 rimcrow-mcp.exe 到支持 endpoint schema v{n} 的版本"
                ));
                actions.push(
                    "或同步升级/降级 RimCrow，使 endpoint 回到本桥支持的 schema v1".to_string(),
                );
                actions.push("不要手工编辑 mcp-endpoint.json".to_string());
                format!(
                    "endpoint schema v{n} 不受支持（本桥支持 v1）；请升级 rimcrow-mcp.exe（{}；endpoint：{}）",
                    self.version_ctx(),
                    out.path.display()
                )
            }
            status => {
                let mut reason = out.reason.clone();
                if let Some(note) = spawn_note {
                    let attempts = self.spawn_gate.attempts().max(1);
                    reason.push_str(&format!("；已尝试拉起 RimCrow {attempts} 次失败：{note}"));
                }
                match status {
                    ProbeStatus::Missing => {
                        actions.push(
                            "启动 RimCrow GUI 后重试；或用 rimcrow-mcp --spawn 让本桥自动拉起"
                                .to_string(),
                        );
                        actions.push(
                            "确认 endpoint 文件路径（可用环境变量 RIMCROW_MCP_ENDPOINT 覆盖）"
                                .to_string(),
                        );
                    }
                    ProbeStatus::Stale if reason.contains("已不存在") => {
                        actions.push(
                            "RimCrow 已退出或 endpoint 已过期：重新启动 RimCrow 后重试"
                                .to_string(),
                        );
                        actions.push(
                            "若主 exe 刚换版，等新版实例重写 endpoint 再重试（pid/启动时间会变）"
                                .to_string(),
                        );
                    }
                    ProbeStatus::Stale => {
                        actions.push(
                            "检查 endpoint 文件内容与当前 RimCrow 实例是否对应".to_string(),
                        );
                        actions.push("重启 RimCrow GUI 让它重新原子写入 endpoint".to_string());
                    }
                    _ => {}
                }
                if spawn_note.is_some() {
                    actions.push(spawn::manual_start_hint().to_string());
                    actions.push(
                        "若拉起命令有误，检查 RIMCROW_SPAWN_CMD 或 bridge 同目录的 RimCrow.exe"
                            .to_string(),
                    );
                }
                actions.push("本桥每次调用都会重新探活，无需重启会话".to_string());
                format!("{reason}；当前无法转发 tools/call（{}）", self.version_ctx())
            }
        };

        Self::error_result(&message, actions)
    }

    /// 连接失败（探活通过但 HTTP 不通）的降级错误。
    fn degraded_connect(&self, cause: &str) -> CallToolResult {
        let message = format!(
            "连接上游端点失败：{cause}；当前无法转发 tools/call（{}）",
            self.version_ctx()
        );
        let actions = vec![
            "确认 RimCrow GUI 仍在运行（endpoint 进程可能刚退出）".to_string(),
            "稍后重试；本桥每次调用都会重新探活".to_string(),
            "若反复失败，重启 RimCrow GUI 重新写入 endpoint".to_string(),
        ];
        Self::error_result(&message, actions)
    }

    /// 探活 → （必要时）spawn → 返回可用 endpoint，否则返回降级错误。
    async fn ensure_endpoint(&self) -> Result<EndpointInfo, CallToolResult> {
        let out = self.probe();
        if let Some(info) = out.ready_info() {
            return Ok(info.clone());
        }
        if !self.spawn_enabled {
            return Err(self.degraded_for_probe(&out, None));
        }
        let spawn_result = self.spawn_gate.ensure().await;
        let out = self.probe();
        if let Some(info) = out.ready_info() {
            return Ok(info.clone());
        }
        let note = spawn_result.err();
        Err(self.degraded_for_probe(&out, note.as_deref()))
    }

    /// 后台发起一次 spawn 尝试（`tools/list` 用：不阻塞快照应答，但要让拉起流程跑起来）。
    fn kickoff_spawn(&self) {
        if !self.spawn_enabled {
            return;
        }
        let gate = self.spawn_gate.clone();
        tokio::spawn(async move {
            let _ = gate.ensure().await;
        });
    }

    /// 取得（必要时建立）上游会话的 `Peer`，附带上游 `serverInfo.version`。
    async fn upstream_peer(
        &self,
        info: &EndpointInfo,
    ) -> Result<(Peer<RoleClient>, Option<String>), CallToolResult> {
        let key = format!("{}{KEY_SEP}{}", info.url, info.token);
        let mut guard = self.upstream.lock().await;
        let need_new = match guard.as_ref() {
            Some(u) => u.key != key || u.svc.is_closed(),
            None => true,
        };
        if need_new {
            let svc = match Self::connect(info).await {
                Ok(svc) => svc,
                Err(e) => return Err(self.degraded_connect(&e)),
            };
            // 成功 attach 后比对快照版本与上游 serverInfo.version，不一致时向 stderr 输出一行警告
            if let Some(peer_info) = svc.peer_info() {
                if let Some(impl_info) = &peer_info.server_info {
                    let snap = snapshot::load();
                    if impl_info.version != snap.rimcrow_version {
                        tracing::warn!(
                            "版本错配：上游 RimCrow {} ≠ 快照 RimCrow {}（captured_at {}），rimcrow-mcp {CRATE_VERSION}——建议同批更新两者",
                            impl_info.version,
                            snap.rimcrow_version,
                            snap.captured_at
                        );
                    } else {
                        tracing::info!(
                            "已 attach 上游 RimCrow {}（与快照一致），rimcrow-mcp {CRATE_VERSION}",
                            impl_info.version
                        );
                    }
                }
            }
            *guard = Some(Upstream { key, svc });
        }
        let up = guard.as_ref().expect("upstream just inserted");
        let peer = up.svc.peer().clone();
        let upstream_version = up
            .svc
            .peer_info()
            .and_then(|pi| pi.server_info.as_ref().map(|i| i.version.clone()));
        Ok((peer, upstream_version))
    }

    /// 建立上游 Streamable HTTP 会话（bearer 注入；`mcp-session-id` 由 transport 维护）。
    async fn connect(
        info: &EndpointInfo,
    ) -> Result<RunningService<RoleClient, ClientConfig>, String> {
        let config =
            StreamableHttpClientTransportConfig::with_uri(info.url.clone()).auth_header(info.token.clone());
        let transport = StreamableHttpClientTransport::from_config(config);
        let client = ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("rimcrow-mcp-bridge", CRATE_VERSION),
        );
        match tokio::time::timeout(CONNECT_TIMEOUT, client.serve(transport)).await {
            Err(_) => Err(format!(
                "握手超时（{}s）：{}",
                CONNECT_TIMEOUT.as_secs(),
                info.url
            )),
            Ok(Err(e)) => Err(format!("initialize 握手失败（{e}）：{}", info.url)),
            Ok(Ok(svc)) => Ok(svc),
        }
    }

    /// 嵌入快照 → `tools/list` 结果（上游不可达时的保底清单）。
    fn snapshot_list(snap: &Snapshot) -> ListToolsResult {
        Self::list_result(snap.tools.clone())
    }

    /// 清单结果的统一出口，按 MCP 2026-07-28 方言自报缓存提示（该方言把这两个字段
    /// 列为清单类结果的必填项，rmcp 里它们是 `Option`，缺省时整个字段从序列化产物
    /// 消失，新版客户端会按缺必填拒收）。
    ///
    /// `ttlMs: 0` 表示清单立即过期，客户端每次现取，与「清单真相在上游、桥不缓存」
    /// 的定位一致。`cacheScope: "private"` 表示工具面随本机档案与 GUI 版本变化，属
    /// 用户特定数据，不进跨用户共享缓存。
    fn list_result(tools: Vec<Tool>) -> ListToolsResult {
        ListToolsResult {
            tools,
            ..Default::default()
        }
        .with_ttl_ms(0)
        .with_cache_scope(CacheScope::Private)
    }

    /// 上游 `tools/list` 全量拉取（分页由 rmcp `list_all_tools` 处理）。
    async fn upstream_list(&self, peer: &Peer<RoleClient>) -> Result<Vec<Tool>, String> {
        match tokio::time::timeout(LIST_TIMEOUT, peer.list_all_tools()).await {
            Err(_) => Err(format!(
                "上游 tools/list 超时（{}s）",
                LIST_TIMEOUT.as_secs()
            )),
            Ok(Err(e)) => Err(format!("上游 tools/list 失败（{e}）")),
            Ok(Ok(tools)) => Ok(tools),
        }
    }

    /// 工具名错配探测：上游回「unknown tool」类文案时的触发条件。
    fn looks_like_unknown_tool(text: &str) -> bool {
        let lower = text.to_lowercase();
        const KEYS: [&str; 10] = [
            "unknown tool",
            "unknown_tool",
            "tool not found",
            "no such tool",
            "not found: tool",
            "未知工具",
            "未找到工具",
            "找不到工具",
            "工具不存在",
            "不存在的工具",
        ];
        KEYS.iter().any(|k| lower.contains(k))
            || (lower.contains("tool")
                && (lower.contains("not found") || lower.contains("不存在")))
    }

    /// 上游报未知工具时的增强文案：点出两侧版本，说明新桥可能超前于运行中的 GUI。
    fn enriched_unknown_tool(
        &self,
        name: &str,
        upstream_text: &str,
        upstream_version: Option<&str>,
    ) -> CallToolResult {
        let snap = snapshot::load();
        let upstream = upstream_version.unwrap_or("未知（上游未上报 serverInfo.version）");
        let message = format!(
            "工具 {name} 在 RimCrow {upstream} 中不存在；rimcrow-mcp {CRATE_VERSION}（快照 RimCrow {} @ {}）可能超前于 GUI——请同批更新两者。上游原文：{upstream_text}",
            snap.rimcrow_version, snap.captured_at
        );
        let actions = vec![
            format!("调用 tools/list 核对 RimCrow {upstream} 的实际工具面"),
            "同批更新 rimcrow-mcp.exe 与 RimCrow（双产物同一条打包流水线发布）".to_string(),
            format!(
                "若只想用快照内的旧工具，先重启 RimCrow 到与快照 {} 匹配的版本",
                snap.rimcrow_version
            ),
        ];
        Self::error_result(&message, actions)
    }

    /// 上游协议错误转成可读的 isError：agent 对一次调用的结果只有一次阅读机会，
    /// 文案必须自足。
    fn upstream_protocol_error(&self, err: &McpError, name: &str) -> CallToolResult {
        let message = format!("{}（上游 JSON-RPC code={}）", err.message, err.code.0);
        let actions = vec![
            format!("调用 tools/list 核对 {name} 是否存在于当前 RimCrow"),
            "确认参数符合该工具 inputSchema".to_string(),
            format!("版本上下文：{}", self.version_ctx()),
        ];
        Self::error_result(&message, actions)
    }

    /// 传输层失败的因果降级：说明上游失去响应与当前版本，nextActions 给出自纠动作。
    fn upstream_transport_error(&self, err: &ServiceError) -> CallToolResult {
        let message = format!(
            "上游端点在转发期间失去响应：{err}；当前无法完成 tools/call（{}）",
            self.version_ctx()
        );
        let actions = vec![
            "RimCrow 可能正在退出/重启：稍后重试，本桥每次调用都会重新探活".to_string(),
            "若长时间如此，检查 endpoint 文件中的 pid 是否仍存活".to_string(),
            "重启 RimCrow GUI 后重试".to_string(),
        ];
        Self::error_result(&message, actions)
    }

    /// 当前生效清单：优先取上游实时 tools/list，取不到时回退嵌入快照；校验基准永远
    /// 是当前生效的那份清单，不是写死的快照。返回清单与来源描述，来源描述进错误文案，
    /// 便于分辨判据来自实时清单还是快照。清单不缓存，每次现拉。
    fn effective_tools(
        live: Result<Vec<Tool>, String>,
        upstream_version: Option<&str>,
    ) -> (Cow<'static, [Tool]>, String) {
        match live {
            Ok(tools) => {
                let ver = upstream_version.unwrap_or("未知版本（上游未上报 serverInfo.version）");
                (
                    Cow::Owned(tools),
                    format!("运行中 RimCrow {ver} 的实时 tools/list"),
                )
            }
            Err(e) => {
                tracing::warn!("参数预校验：上游 tools/list 不可用，回退嵌入快照：{e}");
                let snap = snapshot::load();
                (
                    Cow::Borrowed(snap.tools.as_slice()),
                    format!(
                        "桥内嵌快照 RimCrow {} @ {}（上游 tools/list 不可用）",
                        snap.rimcrow_version, snap.captured_at
                    ),
                )
            }
        }
    }

    /// 参数预校验：现拉当前生效清单（live 优先、快照兜底），逐参数对照该工具 inputSchema。
    ///
    /// 返回 `None` 表示放行，调用参数一字不改原样转发；返回 `Some` 是 isError 信封
    /// 形式的拒绝结果。
    async fn precheck(
        &self,
        name: &str,
        args: &Map<String, Value>,
        peer: &Peer<RoleClient>,
        upstream_version: Option<&str>,
    ) -> Option<CallToolResult> {
        if args.is_empty() {
            return None; // 空参数调用零影响，连 tools/list 都不必现拉
        }
        let live = self.upstream_list(peer).await;
        let (tools, source) = Self::effective_tools(live, upstream_version);
        self.precheck_with(name, args, &tools, &source)
    }

    /// 按给定清单校验并构造拒绝结果（单测入口：可直接喂快照清单，模拟「无 live」回退态）。
    fn precheck_with(
        &self,
        name: &str,
        args: &Map<String, Value>,
        tools: &[Tool],
        source: &str,
    ) -> Option<CallToolResult> {
        if args.is_empty() {
            return None;
        }
        // 清单里没有这个工具时不拦，交给上游报未知工具，随后的增强文案仍然生效
        let tool = tools.iter().find(|t| t.name.as_ref() == name)?;
        let schema = Value::Object(tool.input_schema.as_ref().clone());
        let issues = validate::validate(&schema, &Value::Object(args.clone()));
        if issues.is_empty() {
            return None;
        }
        let detail = issues
            .iter()
            .map(ArgIssue::describe)
            .collect::<Vec<_>>()
            .join("；");
        tracing::warn!(
            "参数预校验拒绝 {name}：{detail}（清单来源：{source}；rimcrow-mcp {CRATE_VERSION}）"
        );
        Some(self.precheck_error(name, &detail, &schema, source))
    }

    /// 拒绝结果与 RimCrow 错误信封同形（isError 加 message 与 nextActions）。message
    /// 写明哪些参数未被识别、服务端版本可能较旧，并列出该工具当前生效的已知参数，
    /// 便于改参重试。
    fn precheck_error(
        &self,
        name: &str,
        detail: &str,
        schema: &Value,
        source: &str,
    ) -> CallToolResult {
        let known = validate::known_params(schema);
        let known_txt = if known.is_empty() {
            "（该工具未声明任何参数）".to_string()
        } else {
            known.join("、")
        };
        let message = format!(
            "参数预校验失败：{detail}；服务端版本较旧或该参数/取值未被识别；工具 {name} 当前生效的已知参数：{known_txt}；清单来源：{source}（{}）",
            self.version_ctx()
        );
        let actions = vec![
            "按上面列出的已知参数改参后重试；未列出的参数不会被服务端识别".to_string(),
            "调用 tools/list 核对当前生效清单与该工具 inputSchema".to_string(),
            "若确实需要新参数，同批更新 RimCrow GUI 与 rimcrow-mcp.exe（双产物同一条流水线发布）"
                .to_string(),
        ];
        Self::error_result(&message, actions)
    }
}

/// 从上游 `CallToolResult` 提取全部文本内容（用于错配探测，不做其它解释）。
fn tool_result_text(result: &CallToolResult) -> String {
    let mut out = String::new();
    for block in &result.content {
        if let Some(text) = block.as_text() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&text.text);
        }
    }
    out
}

/// 补齐调用结果的 `resultType` 字段。MCP 2026-07-28 方言把该字段列为所有调用结果
/// 的必填项；上游是旧方言时结果线形里没有它，反序列化后为 `None`，新版客户端会因
/// 缺必填整单拒收。能走到透传的 Complete 在语义上就是完整结果（task 与
/// input_required 形态都已被上游拦截改造），补 `"complete"` 恒正确；旧方言对端由
/// rmcp 在响应层自动剥掉该字段，两种方言都成立。已带值的结果保持原样。
fn ensure_result_type(result: &mut CallToolResult) {
    if result.result_type.is_none() {
        result.result_type = Some(rmcp::model::ResultType::COMPLETE);
    }
}

/// 判断上游错误是否是 rmcp 的「传输已断」标记；rmcp 自带的同类判断方法未公开，
/// 这里按错误数据里的键名识别。
fn is_transport_closed_marker(err: &McpError) -> bool {
    err.data
        .as_ref()
        .and_then(|d| d.get("io.modelcontextprotocol/transportClosed"))
        .is_some()
}

impl ServerHandler for Bridge {
    /// 本地应答，永不依赖上游；instructions/version 取自嵌入快照。
    fn get_info(&self) -> ServerConfig {
        let snap = snapshot::load();
        let footer = format!(
            "[rimcrow-mcp {CRATE_VERSION} | 快照 RimCrow {} @ {} | 清单真相在上游，本桥只做透传]",
            snap.rimcrow_version, snap.captured_at
        );
        let instructions = if snap.instructions.is_empty() {
            footer
        } else {
            format!("{}\n\n{footer}", snap.instructions)
        };
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(instructions)
            .with_server_info(Implementation::new(snap.server_name.clone(), CRATE_VERSION))
    }

    /// 清单双模：探活通过时实时透传上游全量清单，否则以嵌入快照保底。
    ///
    /// 上游分页在此合并为单页返回，不下发 cursor，客户端一次性拿全量。
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let out = self.probe();
        if let Some(info) = out.ready_info() {
            match self.upstream_peer(info).await {
                Ok((peer, _)) => match self.upstream_list(&peer).await {
                    Ok(tools) => {
                        tracing::debug!("tools/list 透传上游：{} 个工具", tools.len());
                        return Ok(Self::list_result(tools));
                    }
                    Err(e) => {
                        tracing::warn!("tools/list 透传失败，回落嵌入快照：{e}");
                    }
                },
                Err(_) => {
                    // 连接失败已经打过日志/构造过错误；list 层回落嵌入快照
                    tracing::warn!(
                        "tools/list 无法透传上游（attach 失败），回落嵌入快照（{}）：{}",
                        self.version_ctx(),
                        out.reason
                    );
                }
            }
        } else {
            // 未检出 endpoint：先在后台把 spawn 跑起来（不阻塞本次快照应答）
            self.kickoff_spawn();
            tracing::warn!(
                "tools/list 无法透传上游（endpoint 不可达），回落嵌入快照（{}）：{}",
                self.version_ctx(),
                out.reason
            );
        }

        Ok(Self::snapshot_list(snapshot::load()))
    }

    /// 探活后转发并原样回抛；任何不可达都以因果 isError 加 nextActions 降级。
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let name = request.name.to_string();

        let info = match self.ensure_endpoint().await {
            Ok(info) => info,
            Err(degraded) => return Ok(degraded.into()),
        };

        let (peer, upstream_version) = match self.upstream_peer(&info).await {
            Ok(v) => v,
            Err(degraded) => return Ok(degraded.into()),
        };

        // 参数预校验防「假成功」：调用带参数时，先按当前生效清单拦下会被旧 GUI
        // 静默忽略的参数；合法与空参数零影响，照原样转发。
        if let Some(args) = request.arguments.as_ref() {
            if let Some(rejected) = self
                .precheck(&name, args, &peer, upstream_version.as_deref())
                .await
            {
                return Ok(rejected.into());
            }
        }

        match peer.call_tool_once(request).await {
            Ok(CallToolResponse::Complete(mut result)) => {
                // 远端 isError 文案逐字回抛（unknown tool 的增强文案是唯一例外）
                if result.is_error == Some(true) {
                    let text = tool_result_text(&result);
                    if Self::looks_like_unknown_tool(&text) {
                        tracing::warn!(
                            "上游拒绝工具 {name}（RimCrow {}）：{text}",
                            upstream_version.as_deref().unwrap_or("<unknown>")
                        );
                        return Ok(self
                            .enriched_unknown_tool(&name, &text, upstream_version.as_deref())
                            .into());
                    }
                }
                ensure_result_type(&mut result);
                Ok(result.into())
            }
            Ok(CallToolResponse::InputRequired(_)) => {
                let message = format!(
                    "上游对 {name} 返回 input_required（SEP-2322 多轮请求），本桥不做跨请求状态保留，无法代为续跑（{}）",
                    self.version_ctx()
                );
                let actions = vec![
                    "改用不触发多轮请求的调用方式，或直接以 RimCrow 原生客户端操作".to_string(),
                    "同批更新 rimcrow-mcp.exe 与 RimCrow".to_string(),
                ];
                Ok(Self::error_result(&message, actions).into())
            }
            Ok(CallToolResponse::Task(task)) => {
                let message = format!(
                    "上游把 {name} 物化为 task（SEP-2663），本桥不透传任务轮询（{}；task handle 见上游日志）",
                    self.version_ctx()
                );
                let actions = vec![
                    "稍后用原生 MCP 客户端直连 RimCrow 端点轮询该 task".to_string(),
                    "或同批更新 rimcrow-mcp.exe 与 RimCrow".to_string(),
                ];
                let _ = task;
                Ok(Self::error_result(&message, actions).into())
            }
            Ok(other) => {
                let message = format!(
                    "上游返回本桥不支持的 tools/call 响应形态（{}）：{other:?}",
                    self.version_ctx()
                );
                let actions = vec![
                    "直接用原生 MCP 客户端连接 RimCrow 端点完成该调用".to_string(),
                    "或同批更新 rimcrow-mcp.exe 与 RimCrow".to_string(),
                ];
                Ok(Self::error_result(&message, actions).into())
            }
            Err(ServiceError::McpError(err)) => {
                if is_transport_closed_marker(&err) {
                    return Ok(self.upstream_transport_error(&ServiceError::TransportClosed).into());
                }
                if Self::looks_like_unknown_tool(&err.message) {
                    return Ok(self
                        .enriched_unknown_tool(&name, &err.message, upstream_version.as_deref())
                        .into());
                }
                Ok(self.upstream_protocol_error(&err, &name).into())
            }
            Err(err) => Ok(self.upstream_transport_error(&err).into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_tool_detection() {
        assert!(Bridge::looks_like_unknown_tool("Unknown tool: list_mods"));
        assert!(Bridge::looks_like_unknown_tool("Tool foo not found"));
        assert!(Bridge::looks_like_unknown_tool("找不到工具 foo"));
        assert!(!Bridge::looks_like_unknown_tool("mod list not found"));
        assert!(!Bridge::looks_like_unknown_tool("ok"));
    }

    /// 上游旧方言的结果线形没有 resultType，反序列化后为 None；
    /// 补齐后线形必含 `resultType: "complete"`。
    #[test]
    fn complete_passthrough_gets_result_type_for_modern_peers() {
        let raw = json!({"content": [{"type": "text", "text": "ok"}], "isError": false});
        let mut result: CallToolResult = serde_json::from_value(raw).unwrap();
        assert_eq!(result.result_type, None);
        ensure_result_type(&mut result);
        assert_eq!(result.result_type, Some(rmcp::model::ResultType::COMPLETE));
        let line = serde_json::to_value(&result).unwrap();
        assert_eq!(line["resultType"], "complete");
        // 已带值的结果不被改写
        let mut errored = CallToolResult::error(vec![]);
        ensure_result_type(&mut errored);
        assert_eq!(errored.result_type, Some(rmcp::model::ResultType::COMPLETE));
    }

    #[test]
    fn snapshot_list_carries_every_tool_without_cursor() {
        let snap = snapshot::load();
        let list = Bridge::snapshot_list(snap);
        assert_eq!(list.tools.len(), snap.tools.len());
        assert!(list.next_cursor.is_none());
        assert_eq!(list.result_type, Some(rmcp::model::ResultType::COMPLETE));
        // 新版方言必填的缓存提示字段：结构里在，线形里也在（缺省时序列化整个省略）
        assert_eq!(list.ttl_ms, Some(0));
        assert_eq!(list.cache_scope, Some(rmcp::model::CacheScope::Private));
        let json = serde_json::to_value(&list).unwrap();
        assert_eq!(json["ttlMs"], 0);
        assert_eq!(json["cacheScope"], "private");
    }

    #[test]
    fn get_info_never_touches_upstream() {
        let bridge = Bridge::new(crate::Options {
            spawn: false,
            headless: false,
        });
        let info = bridge.get_info();
        let snap = snapshot::load();
        assert_eq!(info.server_info.name, snap.server_name);
        assert_eq!(info.server_info.version, CRATE_VERSION);
        assert!(info.capabilities.tools.is_some());
        let instructions = info.instructions.expect("instructions");
        assert!(instructions.contains(&snap.rimcrow_version), "must self-report snapshot version");
        assert!(instructions.contains(CRATE_VERSION), "must self-report crate version");
        assert!(instructions.contains(&snap.captured_at), "must self-report captured_at");
        assert!(instructions.contains(&snap.instructions), "must carry snapshot instructions");
        // get_info 不触发 spawn 与网络：未启用 spawn 时尝试计数恒为零
        assert_eq!(bridge.spawn_gate.attempts(), 0);
        assert!(!bridge.spawn_enabled);
    }

    #[test]
    fn error_results_carry_causal_envelope() {
        let bridge = Bridge::new(crate::Options {
            spawn: false,
            headless: false,
        });
        // 构造一个必然缺失的 endpoint 探活结果
        let out = ProbeOutcome {
            status: ProbeStatus::Missing,
            info: None,
            reason: "endpoint 文件不存在：/nowhere".to_string(),
            path: PathBuf::from("/nowhere"),
        };
        let result = bridge.degraded_for_probe(&out, None);
        assert_eq!(result.is_error, Some(true));
        let text = tool_result_text(&result);
        let v: serde_json::Value = serde_json::from_str(&text).expect("envelope json");
        assert_eq!(v["status"], "error");
        assert!(v["message"].as_str().unwrap().contains("endpoint 文件不存在"));
        assert!(v["message"]
            .as_str()
            .unwrap()
            .contains(CRATE_VERSION));
        assert!(v["nextActions"].as_array().unwrap().len() >= 2);
    }

    #[test]
    fn schema_mismatch_message_names_both_sides_and_upgrade_hint() {
        let bridge = Bridge::new(crate::Options {
            spawn: false,
            headless: false,
        });
        let out = ProbeOutcome {
            status: ProbeStatus::UnsupportedSchema(2),
            info: None,
            reason: "endpoint schema_version=2 不受支持（期望 1）：/x".to_string(),
            path: PathBuf::from("/x"),
        };
        let result = bridge.degraded_for_probe(&out, None);
        let text = tool_result_text(&result);
        assert!(text.contains("endpoint schema v2 不受支持（本桥支持 v1）"));
        assert!(text.contains("请升级 rimcrow-mcp.exe"));
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        let msg = v["message"].as_str().unwrap();
        let snap = snapshot::load();
        assert!(msg.contains(CRATE_VERSION), "bridge version must be named: {msg}");
        assert!(msg.contains(&snap.rimcrow_version), "snapshot version must be named: {msg}");
    }

    #[test]
    fn degraded_text_has_no_hollow_timeout_phrasing() {
        let bridge = Bridge::new(crate::Options {
            spawn: false,
            headless: false,
        });
        let out = ProbeOutcome {
            status: ProbeStatus::Stale,
            info: None,
            reason: "RimCrow 未运行（endpoint 中的 pid 123 已不存在）：/x".to_string(),
            path: PathBuf::from("/x"),
        };
        let text = tool_result_text(&bridge.degraded_for_probe(&out, None));
        assert!(!text.contains("timeout"), "no hollow timeout wording: {text}");
        assert!(!text.to_lowercase().contains("timed out"));
        assert!(text.contains("pid 123"));
    }

    #[test]
    fn spawn_gate_is_single_shot() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let gate = SpawnGate::new(PathBuf::from("/definitely/missing.json"), false, false);
        rt.block_on(async {
            // 未启用 spawn：恒返回禁用错误，且不尝试
            let r = gate.ensure().await;
            assert!(r.is_err());
            assert_eq!(gate.attempts(), 0);
        });
    }

    /// 造一个最小工具（`rmcp::model::Tool` 是 `#[non_exhaustive]`，只能反序列化构造）。
    fn tool_named(name: &str) -> Tool {
        serde_json::from_value(json!({"name": name, "inputSchema": {"type": "object"}}))
            .expect("tool json")
    }

    fn bridge_for_test() -> Bridge {
        Bridge::new(crate::Options {
            spawn: false,
            headless: false,
        })
    }

    #[test]
    fn effective_tools_prefers_live_over_snapshot() {
        let live = vec![tool_named("echo_tool")];
        let (tools, source) = Bridge::effective_tools(Ok(live), Some("9.9.9"));
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name.as_ref(), "echo_tool");
        assert!(source.contains("实时"), "{source}");
        assert!(source.contains("9.9.9"), "{source}");
        assert!(
            !source.contains("快照"),
            "live 优先，不得回落快照：{source}"
        );
    }

    #[test]
    fn effective_tools_falls_back_to_snapshot_without_live() {
        let (tools, source) = Bridge::effective_tools(Err("上游 tools/list 超时".into()), None);
        let snap = snapshot::load();
        assert_eq!(tools.len(), snap.tools.len());
        assert!(source.contains("快照"), "{source}");
        assert!(source.contains("不可用"), "{source}");
        assert!(source.contains(&snap.rimcrow_version), "{source}");
    }

    /// 未知参数在 bridge 层被拒，文案带出当前生效清单的已知参数列表。
    #[test]
    fn precheck_rejects_unknown_arg_and_lists_known_params() {
        let bridge = bridge_for_test();
        let snap = snapshot::load();
        let args: Map<String, Value> =
            serde_json::from_value(json!({"definitely_bogus": 1, "limit": 3})).unwrap();
        // 无 live 清单时直接喂快照清单，即回退态仍然能校验
        let source = "桥内嵌快照 RimCrow 0.24.7 @ 2026-09-27（上游 tools/list 不可用）";
        let result = bridge
            .precheck_with("list_mods", &args, &snap.tools, source)
            .expect("unknown arg must be rejected before forwarding");
        assert_eq!(
            result.is_error,
            Some(true),
            "MCP 标准错误结果 = isError:true"
        );

        let text = tool_result_text(&result);
        // 缓存提示字段只出现在 tools/list 等清单类结果上，
        // 工具调用的错误信封不得携带 ttlMs 与 cacheScope
        assert!(
            !text.contains("ttlMs") && !text.contains("cacheScope"),
            "{text}"
        );

        let v: Value = serde_json::from_str(&text).expect("envelope must be valid JSON");
        assert_eq!(v["status"], "error");
        let msg = v["message"].as_str().expect("message");
        assert!(msg.contains("list_mods"), "{msg}");
        assert!(msg.contains("definitely_bogus"), "{msg}");
        assert!(msg.contains("未被识别"), "{msg}");
        assert!(msg.contains("版本较旧"), "{msg}");
        assert!(msg.contains(source), "文案要说明清单来源：{msg}");
        // 已知参数逐个出现在文案里，便于改参重试
        let schema = Value::Object(
            snap.tools
                .iter()
                .find(|t| t.name.as_ref() == "list_mods")
                .expect("snapshot list_mods")
                .input_schema
                .as_ref()
                .clone(),
        );
        let known = validate::known_params(&schema);
        assert!(!known.is_empty());
        for k in &known {
            assert!(msg.contains(k), "已知参数 {k} 未出现在文案：{msg}");
        }
        // 文案同时点出桥与快照两侧版本
        assert!(msg.contains(CRATE_VERSION), "{msg}");
        assert!(msg.contains(&snap.rimcrow_version), "{msg}");
        let actions = v["nextActions"].as_array().expect("nextActions");
        assert!(actions.len() >= 2, "{actions:?}");
    }

    /// 合法参数零影响，放行后原样透传；空参数与未知工具同样不拦。
    #[test]
    fn precheck_passes_known_args_for_passthrough() {
        let bridge = bridge_for_test();
        let snap = snapshot::load();
        let args: Map<String, Value> =
            serde_json::from_value(json!({"scope": "active", "limit": 3, "offset": 0})).unwrap();
        assert!(
            bridge
                .precheck_with("list_mods", &args, &snap.tools, "测试")
                .is_none(),
            "已知参数必须原样放行"
        );
        // 空参数不拦，precheck 在拉清单之前就会早退
        let empty = Map::new();
        assert!(bridge
            .precheck_with("list_mods", &empty, &snap.tools, "测试")
            .is_none());
        // 清单里没有的工具不拦，交给上游报未知工具后走增强文案
        assert!(bridge
            .precheck_with("__nope__", &args, &snap.tools, "测试")
            .is_none());
    }
}
