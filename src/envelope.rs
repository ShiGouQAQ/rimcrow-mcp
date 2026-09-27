//! 错误信封（D6/D14）—— 与 RimCrow `backend/mcp_guard.py::build_error_text` 同形。
//!
//! 产物是**单个合法 JSON 字符串**，作为 MCP `tools/call` 的单个 text 内容返回，
//! 由调用方包成 `CallToolResult::error(...)`（MCP 侧 `isError:true`）。
//!
//! 形状：`{"status":"error","message":"...","nextActions":["...", ...]}`
//! 序列化约束对齐 Python `json.dumps(..., ensure_ascii=False, separators=(",", ":"))`：
//! 紧凑、无多余空白、非 ASCII 不转义。

use serde_json::{json, Map, Value};

/// 构造错误信封文本。
///
/// `next_actions` 为空时不写 `nextActions` 键（与 Python 侧 `if next_actions:` 一致）。
pub fn build_error_text(message: &str, next_actions: &[String]) -> String {
    let mut payload = Map::new();
    payload.insert("status".into(), Value::String("error".into()));
    payload.insert("message".into(), Value::String(message.to_string()));
    if !next_actions.is_empty() {
        payload.insert("nextActions".into(), json!(next_actions));
    }
    Value::Object(payload).to_string()
}

/// 便捷构造：`&[&str]` → `Vec<String>`（单测与调用方简化用）。
#[allow(dead_code)]
pub fn build_error_text_simple(message: &str, next_actions: &[&str]) -> String {
    build_error_text(
        message,
        &next_actions.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_shape_matches_python_guard() {
        let text = build_error_text_simple(
            "endpoint 文件不存在：/tmp/mcp-endpoint.json",
            &["启动 RimCrow GUI 后重试", "或使用 --spawn 自动拉起"],
        );
        let v: Value = serde_json::from_str(&text).expect("envelope must be valid JSON");
        assert_eq!(v["status"], "error");
        assert_eq!(v["message"], "endpoint 文件不存在：/tmp/mcp-endpoint.json");
        let actions = v["nextActions"].as_array().expect("nextActions array");
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0], "启动 RimCrow GUI 后重试");
        // 紧凑序列化：无 ": " / ", " 空格
        assert!(!text.contains(": "), "unexpected space after colon: {text}");
        assert!(!text.contains(", "), "unexpected space after comma: {text}");
        // ensure_ascii=False：中文不转义
        assert!(text.contains("文件不存在"), "non-ascii must stay raw: {text}");
        assert!(!text.contains("\\u"));
        // 字段顺序 status -> message -> nextActions（与 Python dict 插入序一致）
        assert!(text.starts_with(r#"{"status":"error","message":""#));
    }

    #[test]
    fn empty_next_actions_omits_key() {
        let text = build_error_text_simple("boom", &[]);
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["status"], "error");
        assert!(v.get("nextActions").is_none());
        assert_eq!(text, r#"{"status":"error","message":"boom"}"#);
    }

    #[test]
    fn matches_python_reference_output() {
        // 与 mcp_guard.build_error_text("x", ["a"]) 的 Python 产物逐字一致
        let mine = build_error_text("x", &["a".to_string()]);
        assert_eq!(mine, r#"{"status":"error","message":"x","nextActions":["a"]}"#);
    }
}
