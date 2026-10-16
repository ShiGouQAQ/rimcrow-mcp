//! 构建期嵌入的工具清单快照（`tools-cache.json`）。
//!
//! 快照由 RimCrow 仓库的生成器从真实工具层导出，手写清单必与实际 schema 走样，
//! 本文件只解析、不生成。bridge 侧用 `include_str!` 在编译期嵌入，发布形态是
//! 单个可执行文件，运行期不落任何外部清单文件。
//!
//! 快照只在两种场合充当权威数据：tools/list 上游不可达时的保底清单，以及 initialize
//! 的 instructions 与版本上报（这两样永远不依赖上游）。

use std::sync::OnceLock;

use rmcp::model::Tool;
use serde::Deserialize;

/// 嵌入的原始 JSON（构建期决定，运行期不可变）。
pub const RAW: &str = include_str!("../tools-cache.json");

/// 快照文件契约的 schema 版本。
pub const SNAPSHOT_SCHEMA_VERSION: i64 = 1;

/// 解析后的快照（`tools` 已转成 rmcp `Tool`）。
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// 快照格式版本（解析期已校验 == 1，保留用于版本上报与诊断）。
    #[allow(dead_code)]
    pub schema_version: i64,
    pub rimcrow_version: String,
    pub captured_at: String,
    pub server_name: String,
    pub instructions: String,
    /// 工具清单（生成器输出顺序，不要求排序）。
    pub tools: Vec<Tool>,
    /// 解析时被丢弃的非法条目数（>0 说明生成器产物有问题，会打 stderr 警告）。
    pub dropped: usize,
}

#[derive(Debug, Deserialize)]
struct RawSnapshot {
    schema_version: i64,
    rimcrow_version: String,
    captured_at: String,
    server_name: String,
    instructions: String,
    #[serde(default)]
    tools: Vec<serde_json::Value>,
}

/// 解析快照文本（可单测）。
pub fn parse_snapshot(raw: &str) -> Result<Snapshot, String> {
    let parsed: RawSnapshot =
        serde_json::from_str(raw).map_err(|e| format!("快照 JSON 解析失败：{e}"))?;
    if parsed.schema_version != SNAPSHOT_SCHEMA_VERSION {
        return Err(format!(
            "快照 schema_version={} 不受支持（本桥支持 {}）",
            parsed.schema_version, SNAPSHOT_SCHEMA_VERSION
        ));
    }
    let mut tools = Vec::with_capacity(parsed.tools.len());
    let mut dropped = 0usize;
    for (idx, value) in parsed.tools.into_iter().enumerate() {
        match serde_json::from_value::<Tool>(value.clone()) {
            Ok(tool) => tools.push(tool),
            Err(e) => {
                dropped += 1;
                tracing::warn!(
                    "tools-cache.json 第 {} 条工具无法解析为 MCP Tool，已丢弃：{e}",
                    idx + 1
                );
            }
        }
    }
    Ok(Snapshot {
        schema_version: parsed.schema_version,
        rimcrow_version: parsed.rimcrow_version,
        captured_at: parsed.captured_at,
        server_name: parsed.server_name,
        instructions: parsed.instructions,
        tools,
        dropped,
    })
}

/// 进程内单例读取嵌入快照；快照是构建期常量，单例不构成跨请求状态。
pub fn load() -> &'static Snapshot {
    static SNAPSHOT: OnceLock<Snapshot> = OnceLock::new();
    SNAPSHOT.get_or_init(|| match parse_snapshot(RAW) {
        Ok(s) => {
            if s.dropped > 0 {
                tracing::error!(
                    "tools-cache.json 有 {} 条工具被丢弃（构建期产物异常）",
                    s.dropped
                );
            }
            s
        }
        Err(e) => {
            tracing::error!("嵌入快照不可用（{e}）；tools/list 将返回空清单");
            Snapshot {
                schema_version: 0,
                rimcrow_version: "unknown".to_string(),
                captured_at: "unknown".to_string(),
                server_name: "rimcrow-mcp".to_string(),
                instructions: String::new(),
                tools: Vec::new(),
                dropped: 0,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_snapshot_parses_and_is_v1() {
        let snap = parse_snapshot(RAW).expect("embedded tools-cache.json must parse");
        assert_eq!(snap.schema_version, SNAPSHOT_SCHEMA_VERSION);
        assert!(!snap.rimcrow_version.is_empty(), "rimcrow_version required");
        assert!(!snap.captured_at.is_empty(), "captured_at required");
        assert!(!snap.server_name.is_empty(), "server_name required");
        assert!(!snap.tools.is_empty(), "snapshot must contain tools");
        assert_eq!(snap.dropped, 0, "every snapshot tool must be a valid MCP Tool");
    }

    #[test]
    fn snapshot_tools_are_round_trippable() {
        let snap = parse_snapshot(RAW).unwrap();
        for tool in &snap.tools {
            assert!(!tool.name.is_empty(), "tool name must not be empty");
            assert!(
                tool.input_schema.contains_key("type") || !tool.input_schema.is_empty(),
                "tool {} must carry an inputSchema",
                tool.name
            );
            // 序列化后重新解析应无损，保证上游与快照两侧的 schema 形状同构
            let json = serde_json::to_value(tool).unwrap();
            let back: rmcp::model::Tool = serde_json::from_value(json).unwrap();
            assert_eq!(back.name, tool.name);
        }
    }

    #[test]
    fn duplicate_tool_names_not_expected() {
        let snap = parse_snapshot(RAW).unwrap();
        let mut names: Vec<String> = snap.tools.iter().map(|t| t.name.to_string()).collect();
        let before = names.len();
        names.sort();
        names.dedup();
        assert_eq!(before, names.len(), "snapshot tool names must be unique");
    }

    #[test]
    fn unsupported_schema_is_rejected() {
        let bad = r#"{"schema_version":2,"rimcrow_version":"1","captured_at":"t","server_name":"s","instructions":"i","tools":[]}"#;
        let err = parse_snapshot(bad).unwrap_err();
        assert!(err.contains("schema_version=2"), "{err}");
    }

    #[test]
    fn broken_json_is_rejected() {
        assert!(parse_snapshot("{oops").is_err());
    }

    #[test]
    fn invalid_tool_entry_is_dropped_not_fatal() {
        let raw = r#"{"schema_version":1,"rimcrow_version":"1","captured_at":"t","server_name":"s","instructions":"i","tools":[{"name":"ok","inputSchema":{"type":"object"}},{"bogus":1}]}"#;
        let snap = parse_snapshot(raw).unwrap();
        assert_eq!(snap.tools.len(), 1);
        assert_eq!(snap.dropped, 1);
        assert_eq!(snap.tools[0].name.as_ref(), "ok");
    }

    #[test]
    fn load_returns_singleton() {
        let a = load() as *const _;
        let b = load() as *const _;
        assert_eq!(a, b);
    }
}
