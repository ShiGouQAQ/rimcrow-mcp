//! `tools/call` 参数预校验 —— 只拦「当前生效清单里不存在的参数」（防「假成功」）。
//!
//! 动机（已实测缺陷）：桥的工具清单比运行中的 GUI 新时，新参数会被旧 GUI **静默忽略**
//! ——不报错，却返回语义完全不同的数据（`list_mods {scope:"active",limit:3}` 在旧 GUI
//! 返回 `total:2223` 全库结果，`is_active:false` 的条目混在其中）。这类假成功比报错危险，
//! 故在转发前先由桥拦一道。
//!
//! **判定基准与 R2 同源**：live（上游实时 `tools/list`）优先，拿不到才用嵌入快照——
//! 基准永远是「当前生效的那份清单」，不是写死的快照（谁生效就按谁判）。基准的选取在
//! `bridge.rs::effective_tools`，本模块只接受「已选好的清单 + 其 inputSchema」。
//!
//! **误杀防护（宁可放过，不可误杀）**：
//! - 未知键只在 `additionalProperties:false`（且无 `patternProperties`/`propertyNames`，
//!   两者意味着动态键名）时拦；`true` / 子 schema / 缺省（JSON Schema 默认允许额外属性）
//!   → 一律放行，按 schema 判定、不一刀切；
//! - 类型只报**明确不符**：单一 `type`，或 anyOf/oneOf 每一支都有明确 `type` 且全不匹配；
//!   `allOf`、含自由 schema 的分支、无 `type` → 不判；
//! - 类型判定按 pydantic 2.x lax（宽松）转换放行服务端会接受的取值——已用 RimCrow 同款
//!   pydantic 2.12.5 实测：`"5"→5`、`3.0→3`、`true→1`、`"yes"→true` 都是**合法调用**，
//!   只拦服务端同样会拒绝的取值（`"not-a-number"`、`3.5`、`5` 给 string、`"Active"` 给 enum）；
//! - 缺省参数（没传的键）零影响：不做 `required` 缺失检查（缺了服务端自己会报）；
//! - `enum` 只对标量取值比对（对象/数组取值的相等性语义拿不准 → 放过）。
//!
//! 产物是 `ArgIssue` 列表；错误信封（`isError:true` + message + nextActions，D6 同形）
//! 由 `bridge.rs::precheck_error` 组装——本模块不产出工具结果，也不引入 `ttlMs`/`cacheScope`
//! 之类另一客户端(ZCode)的私有字段。

use serde_json::{Map, Value};

/// 单个参数问题；`path` 为点分嵌套路径，数组元素形如 `ops[0].foo`，顶层即参数名。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgIssue {
    /// `additionalProperties:false` 对象里出现的未知键（= 旧 GUI 会静默忽略的那类参数）。
    Unknown {
        path: String,
        /// 该层 schema 声明的已知参数（顶层的同名列表由 bridge 附在 message 尾部）。
        known: Vec<String>,
    },
    /// 类型明确不符（已按 pydantic lax 转换放行可转换取值）。
    Type {
        path: String,
        expected: String,
        actual: String,
    },
    /// 标量取值不在 `enum` 允许范围内。
    Enum {
        path: String,
        allowed: Vec<String>,
        actual: String,
    },
}

impl ArgIssue {
    /// 中文文案片段（进错误 message，多条以「；」连接）。
    pub fn describe(&self) -> String {
        match self {
            ArgIssue::Unknown { path, known } => {
                // 顶层已知参数列表由 bridge 统一附在 message 尾部，这里只补嵌套层的
                if path.contains('.') || path.contains('[') {
                    let k = if known.is_empty() {
                        "该层未声明任何参数".to_string()
                    } else {
                        known.join("、")
                    };
                    format!("参数 {path} 未被识别（同层已知参数：{k}）")
                } else {
                    format!("参数 {path} 未被识别")
                }
            }
            ArgIssue::Type {
                path,
                expected,
                actual,
            } => format!("参数 {path} 类型不符：期望 {expected}，实际 {actual}"),
            ArgIssue::Enum {
                path,
                allowed,
                actual,
            } => format!(
                "参数 {path} 取值 {actual} 不在允许范围（允许：{}）",
                allowed.join("、")
            ),
        }
    }
}

/// 顶层已知参数名（`properties` 键序保持 schema 声明顺序）。
pub fn known_params(schema: &Value) -> Vec<String> {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|p| p.keys().cloned().collect())
        .unwrap_or_default()
}

/// 校验一份对象型调用参数；返回全部问题（空 = 放行）。
///
/// `schema` = 该工具的 `inputSchema`；`args` = 调用参数（两者非对象时无从判定 → 放行）。
pub fn validate(schema: &Value, args: &Value) -> Vec<ArgIssue> {
    let mut issues = Vec::new();
    if let (Some(root), Some(obj)) = (schema.as_object(), args.as_object()) {
        check_object(root, obj, "", &mut issues);
    }
    issues
}

/// 递归校验一层对象：按 `properties` 判已知键，按 `additionalProperties` 判未知键。
fn check_object(
    schema: &Map<String, Value>,
    args: &Map<String, Value>,
    path: &str,
    out: &mut Vec<ArgIssue>,
) {
    let props = schema.get("properties").and_then(Value::as_object);
    // 只有显式 additionalProperties:false 才拦未知键；true / 子 schema / 缺省（JSON Schema
    // 默认允许额外属性）→ 放行。patternProperties / propertyNames 意味着动态键名 → 也放行。
    let forbid_extra = schema.get("additionalProperties") == Some(&Value::Bool(false))
        && schema.get("patternProperties").is_none()
        && schema.get("propertyNames").is_none();

    for (key, value) in args {
        let child = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}.{key}")
        };
        match props.and_then(|p| p.get(key)) {
            Some(sub) => check_value(sub, value, &child, out),
            None if forbid_extra => out.push(ArgIssue::Unknown {
                path: child,
                known: props
                    .map(|p| p.keys().cloned().collect())
                    .unwrap_or_default(),
            }),
            None => {} // schema 允许额外属性（或没有 properties 可依）→ 放过
        }
    }
}

/// 校验单个取值：类型 → enum → 递归嵌套对象 / 数组元素。
fn check_value(schema: &Value, value: &Value, path: &str, out: &mut Vec<ArgIssue>) {
    let Some(sub) = schema.as_object() else {
        return; // 非对象 schema → 拿不准 → 放过
    };

    if let Some(expected) = type_conflict(sub, value) {
        out.push(ArgIssue::Type {
            path: path.to_string(),
            expected,
            actual: type_name(value),
        });
        return; // 类型已不符，不再往下递归
    }

    // enum：标量才比（对象/数组取值拿不准 → 放过）
    if !value.is_object() && !value.is_array() {
        if let Some(allowed) = sub.get("enum").and_then(Value::as_array) {
            if !allowed.iter().any(|a| json_eq(a, value)) {
                out.push(ArgIssue::Enum {
                    path: path.to_string(),
                    allowed: allowed.iter().map(render).collect(),
                    actual: render(value),
                });
            }
        }
    }

    match value {
        Value::Object(obj) => check_object(object_branch(sub), obj, path, out),
        Value::Array(items) => {
            if let Some(items_schema) = sub.get("items") {
                for (i, item) in items.iter().enumerate() {
                    check_value(items_schema, item, &format!("{path}[{i}]"), out);
                }
            }
        }
        _ => {}
    }
}

/// 对象取值时用于递归的子 schema：本层有 `properties` 就用本层，否则在
/// anyOf/oneOf 里取第一个对象型分支（联合类型的可空/多形分支）。
fn object_branch(sub: &Map<String, Value>) -> &Map<String, Value> {
    if sub.get("properties").is_some() {
        return sub;
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(branches) = sub.get(key).and_then(Value::as_array) {
            for b in branches {
                if let Some(branch) = b.as_object() {
                    let is_object = branch.get("type").and_then(Value::as_str) == Some("object")
                        || branch.get("properties").is_some();
                    if is_object {
                        return branch;
                    }
                }
            }
        }
    }
    sub
}

/// 类型冲突检测：明确不符时返回期望类型描述，否则 `None`（放行）。
///
/// 只在「单一 `type`」或「anyOf/oneOf 每一支都有明确 `type` 且全不匹配」时判定；
/// 其余形状（`allOf`、自由分支、无 `type`）一律不判——拿不准就只拦未知参数。
fn type_conflict(schema: &Map<String, Value>, value: &Value) -> Option<String> {
    if let Some(types) = declared_types(schema.get("type")) {
        if types.iter().any(|t| type_matches(t, value)) {
            return None;
        }
        return Some(types.join(" | "));
    }
    for key in ["anyOf", "oneOf"] {
        let Some(branches) = schema.get(key).and_then(Value::as_array) else {
            continue;
        };
        if branches.is_empty() {
            continue;
        }
        let mut expected = Vec::with_capacity(branches.len());
        for b in branches {
            match declared_types(b.get("type")) {
                Some(ts) => expected.push(ts.join(" | ")),
                None => return None, // 含自由分支 → 拿不准 → 整体放过
            }
        }
        let hit = branches.iter().any(|b| {
            declared_types(b.get("type"))
                .map(|ts| ts.iter().any(|t| type_matches(t, value)))
                .unwrap_or(true) // 上面已过滤，安全默认 = 放过
        });
        if hit {
            return None;
        }
        return Some(expected.join(" | "));
    }
    None
}

/// 从 schema 的 `type` 关键字取明确类型列表（缺省/形状不明 → `None` = 不判定）。
fn declared_types(v: Option<&Value>) -> Option<Vec<&str>> {
    let types = match v? {
        Value::String(s) => vec![s.as_str()],
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                out.push(it.as_str()?);
            }
            out
        }
        _ => return None,
    };
    if types.is_empty() {
        return None;
    }
    Some(types)
}

/// JSON Schema 类型是否匹配（含 pydantic 2.x lax 转换放行；见模块文档「误杀防护」）。
fn type_matches(expected: &str, value: &Value) -> bool {
    match expected {
        "string" => value.is_string(),
        "boolean" => match value {
            Value::Bool(_) => true,
            Value::String(s) => pydantic_truthy(s) || pydantic_falsy(s),
            // pydantic：0/1 → bool（已实测），其余数字拒绝
            Value::Number(n) => n.as_f64().is_some_and(|f| f == 0.0 || f == 1.0),
            _ => false,
        },
        "integer" => numeric_like(value, true),
        "number" => numeric_like(value, false),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "null" => value.is_null(),
        _ => true, // 未知类型关键字（含 "any" 等）→ 不判定
    }
}

/// 数字类目标匹配（`integer_only` = 只接受整值）。
///
/// pydantic 2.12.5 实测放行的可转换取值一律通过（误杀防护）：
/// `true→1`、`"5"→5`、`3.0→3`；明确拒绝的才判冲突：`"not-a-number"`、`3.5`（给 integer）。
fn numeric_like(value: &Value, integer_only: bool) -> bool {
    match value {
        Value::Number(n) => {
            if !integer_only {
                return true;
            }
            if n.as_i64().is_some() || n.as_u64().is_some() {
                return true;
            }
            n.as_f64()
                .is_some_and(|f| f.is_finite() && f.fract() == 0.0)
        }
        Value::Bool(_) => true, // pydantic：True → 1
        Value::String(s) => match s.trim().parse::<f64>() {
            Err(_) => false,
            Ok(f) => !integer_only || (f.is_finite() && f.fract() == 0.0),
        },
        _ => false,
    }
}

/// pydantic lax 接受的真值字符串（大小写不敏感）。
fn pydantic_truthy(s: &str) -> bool {
    matches!(
        s.trim().to_ascii_lowercase().as_str(),
        "y" | "yes" | "t" | "true" | "on" | "1"
    )
}

/// pydantic lax 接受的假值字符串（大小写不敏感）。
fn pydantic_falsy(s: &str) -> bool {
    matches!(
        s.trim().to_ascii_lowercase().as_str(),
        "n" | "no" | "f" | "false" | "off" | "0"
    )
}

/// 取值的 JSON 文本（字符串带引号，便于进文案）。
fn render(v: &Value) -> String {
    v.to_string()
}

/// 取值是否相等（数字按数值比：`1` == `1.0`，对齐 JSON Schema / pydantic Literal 语义）。
fn json_eq(a: &Value, b: &Value) -> bool {
    if a.is_number() && b.is_number() {
        if let (Some(x), Some(y)) = (a.as_f64(), b.as_f64()) {
            return x == y;
        }
    }
    a == b
}

/// 取值类型的展示名（数字按整值区分 integer/number）。
fn type_name(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(_) => "boolean".to_string(),
        Value::String(_) => "string".to_string(),
        Value::Array(_) => "array".to_string(),
        Value::Object(_) => "object".to_string(),
        Value::Number(n) => {
            if n.as_i64().is_some()
                || n.as_u64().is_some()
                || n.as_f64()
                    .is_some_and(|f| f.is_finite() && f.fract() == 0.0)
            {
                "integer".to_string()
            } else {
                "number".to_string()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 旧 GUI 版 list_mods 的 inputSchema（无 scope；additionalProperties:false）。
    fn old_list_mods_schema() -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "filter": {"type": "string", "default": ""},
                "limit": {"anyOf": [{"type": "integer"}, {"type": "null"}], "default": null},
                "offset": {"type": "integer", "default": 0}
            }
        })
    }

    /// 新桥快照版 list_mods 的 inputSchema（多了 scope 枚举）。
    fn new_list_mods_schema() -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "filter": {"type": "string", "default": ""},
                "limit": {"anyOf": [{"type": "integer"}, {"type": "null"}], "default": null},
                "offset": {"type": "integer", "default": 0},
                "scope": {"type": "string", "enum": ["all", "active", "inactive"], "default": "all"}
            }
        })
    }

    // ---- 必做覆盖 ①：未知参数被拒，且带出已知参数列表 ----

    #[test]
    fn unknown_param_rejected_with_known_list() {
        let args = json!({"scope": "active", "limit": 3});
        let issues = validate(&old_list_mods_schema(), &args);
        assert_eq!(issues.len(), 1, "only scope is unknown: {issues:?}");
        match &issues[0] {
            ArgIssue::Unknown { path, known } => {
                assert_eq!(path, "scope");
                assert_eq!(known, &["filter", "limit", "offset"]);
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
        // 文案含参数名（顶层已知列表由 bridge 附在 message 尾部）
        assert!(issues[0].describe().contains("scope"));
        assert!(issues[0].describe().contains("未被识别"));
        assert_eq!(
            known_params(&old_list_mods_schema()),
            vec!["filter", "limit", "offset"]
        );
    }

    // ---- 必做覆盖 ②：合法参数零影响（原样放行 → 透传） ----

    #[test]
    fn known_args_pass_with_no_issues() {
        // 新快照 schema 下的完整合法调用
        let args = json!({"scope": "active", "limit": 3, "offset": 0, "filter": "raider"});
        assert!(validate(&new_list_mods_schema(), &args).is_empty());
        // 旧 GUI schema 下的调用同样合法（未知参数以「生效清单」为准，不以快照为准）
        let args = json!({"limit": 3, "offset": 0});
        assert!(validate(&old_list_mods_schema(), &args).is_empty());
    }

    #[test]
    fn omitted_and_default_params_are_never_reported() {
        // 缺省参数零影响：required 不做检查、默认值不补齐也不报
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["filter", "limit"],
            "properties": {"filter": {"type": "string"}, "limit": {"type": "integer"}}
        });
        assert!(validate(&schema, &json!({})).is_empty());
        assert!(validate(&schema, &json!({"filter": "x"})).is_empty());
    }

    // ---- additionalProperties：按 schema 判定，不一刀切 ----

    #[test]
    fn additional_properties_true_allows_extra_keys() {
        let schema = json!({
            "type": "object",
            "additionalProperties": true,
            "properties": {"a": {"type": "integer"}}
        });
        assert!(validate(&schema, &json!({"bogus": 1, "a": 1})).is_empty());
    }

    #[test]
    fn additional_properties_absent_allows_extra_keys() {
        // JSON Schema 默认允许额外属性 → 缺省不拦（快照 46 个工具全部显式 false，此为防御）
        let schema = json!({"type": "object", "properties": {"a": {"type": "integer"}}});
        assert!(validate(&schema, &json!({"bogus": 1})).is_empty());
    }

    #[test]
    fn pattern_properties_suppresses_unknown_key_check() {
        // 动态键名（patternProperties）→ 不能按「properties 未声明」误杀
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "patternProperties": {"^.*$": {"type": "integer"}}
        });
        assert!(validate(&schema, &json!({"any.key": 1})).is_empty());
    }

    #[test]
    fn open_map_nested_object_not_flagged() {
        // 对齐快照：base_version_token / ops.items 都是 additionalProperties:true 的开放映射
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "base_version_token": {"type": "object", "additionalProperties": true},
                "ops": {"type": "array", "items": {"type": "object", "additionalProperties": true}}
            }
        });
        let args = json!({
            "base_version_token": {"any.path": ["p", 1, 2, "h"]},
            "ops": [{"op": "swap", "i": 1}]
        });
        assert!(validate(&schema, &args).is_empty());
    }

    // ---- 递归：嵌套属性与数组元素 ----

    #[test]
    fn nested_object_unknown_key_and_type_checked() {
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "token": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {"path": {"type": "string"}, "size": {"type": "integer"}}
                }
            }
        });
        let args = json!({"token": {"path": "x", "bogus": 1, "size": "abc"}});
        let issues = validate(&schema, &args);
        assert_eq!(issues.len(), 2, "{issues:?}");
        match &issues[0] {
            ArgIssue::Unknown { path, known } => {
                assert_eq!(path, "token.bogus");
                assert_eq!(known, &["path", "size"]);
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
        // 嵌套文案带出同层已知参数
        assert!(issues[0].describe().contains("path"));
        match &issues[1] {
            ArgIssue::Type {
                path,
                expected,
                actual,
            } => {
                assert_eq!(path, "token.size");
                assert_eq!(expected, "integer");
                assert_eq!(actual, "string");
            }
            other => panic!("expected Type, got {other:?}"),
        }
    }

    #[test]
    fn array_elements_are_recursed() {
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "ops": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {"op": {"type": "string"}}
                    }
                }
            }
        });
        let args = json!({"ops": [{"op": "swap"}, {"bogus": 1}]});
        let issues = validate(&schema, &args);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(
            issues[0],
            ArgIssue::Unknown {
                path: "ops[1].bogus".to_string(),
                known: vec!["op".to_string()],
            }
        );
    }

    // ---- 类型明显不符 → 报；拿不准 → 放过 ----

    #[test]
    fn clear_type_mismatch_reported() {
        let schema = json!({"type": "object", "properties": {"filter": {"type": "string"}}});
        let issues = validate(&schema, &json!({"filter": 123}));
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(matches!(
            &issues[0],
            ArgIssue::Type { path, expected, actual }
                if path == "filter" && expected == "string" && actual == "integer"
        ));
        let text = issues[0].describe();
        assert!(text.contains("期望 string，实际 integer"), "{text}");
    }

    #[test]
    fn union_with_all_declared_branches_mismatch_reported() {
        // 限速参数（anyOf integer|null）传入服务端必拒的取值（pydantic 2.12 实测同判）
        let schema = old_list_mods_schema();
        let issues = validate(&schema, &json!({"limit": "not-a-number"}));
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(matches!(
            &issues[0],
            ArgIssue::Type { expected, .. } if expected == "integer | null"
        ));
        // 可空联合：null 合法
        assert!(validate(&schema, &json!({"limit": null})).is_empty());
    }

    #[test]
    fn union_with_free_branch_skipped() {
        // 分支没有明确 type（自由 schema）→ 拿不准 → 整体放过
        let schema = json!({
            "type": "object",
            "properties": {"x": {"anyOf": [{"minLength": 1}, {"type": "null"}]}}
        });
        assert!(validate(&schema, &json!({"x": 123})).is_empty());
        // allOf 同样不判
        let schema = json!({
            "type": "object",
            "properties": {"y": {"allOf": [{"type": "string"}]}}
        });
        assert!(validate(&schema, &json!({"y": 999})).is_empty());
        // 分支带 pattern 时仍按 type 判（服务端同拒 123），但 pattern 本身不判（放过 "b"）
        let schema = json!({
            "type": "object",
            "properties": {"z": {"anyOf": [{"type": "string", "pattern": "^a"}, {"type": "null"}]}}
        });
        assert_eq!(validate(&schema, &json!({"z": 123})).len(), 1);
        assert!(validate(&schema, &json!({"z": "b"})).is_empty());
    }

    #[test]
    fn pydantic_lax_coercible_values_pass() {
        // RimCrow 同款 pydantic 2.12.5 实测：这些是服务端会接受的合法调用，不得误杀
        let schema = json!({
            "type": "object",
            "properties": {
                "limit": {"type": "integer"},
                "offset": {"type": "integer"},
                "ratio": {"type": "number"},
                "dry_run": {"type": "boolean"}
            }
        });
        let legal = json!({
            "limit": "5",        // int("5") → 5
            "offset": true,      // True → 1
            "ratio": 3.0,        // 3.0 → 3.0
            "dry_run": "yes"     // "yes" → true
        });
        assert!(
            validate(&schema, &legal).is_empty(),
            "legal coercions must pass"
        );
        // 服务端同样拒绝的取值（pydantic 2.12 实测：3.5 / "x" / 2 均报错）
        let illegal = json!({"limit": 3.5, "dry_run": "maybe"});
        let issues = validate(&schema, &illegal);
        assert_eq!(issues.len(), 2, "{issues:?}");
        assert!(issues.iter().all(|i| matches!(i, ArgIssue::Type { .. })));
    }

    #[test]
    fn no_schema_nothing_to_judge() {
        // 没有 properties 的 schema（或非对象 schema）→ 无从判定 → 放过
        assert!(validate(&json!({"type": "object"}), &json!({"x": 1})).is_empty());
        assert!(validate(&json!("not-a-schema"), &json!({"x": 1})).is_empty());
        // 参数为空对象时同样无问题
        assert!(validate(&old_list_mods_schema(), &json!({})).is_empty());
    }

    // ---- enum：只对标量比，数字按数值等价 ----

    #[test]
    fn enum_violation_reported_and_compliant_passes() {
        let schema = new_list_mods_schema();
        assert!(validate(&schema, &json!({"scope": "active"})).is_empty());
        let issues = validate(&schema, &json!({"scope": "Active"}));
        assert_eq!(issues.len(), 1, "{issues:?}");
        match &issues[0] {
            ArgIssue::Enum {
                path,
                allowed,
                actual,
            } => {
                assert_eq!(path, "scope");
                assert_eq!(allowed, &["\"all\"", "\"active\"", "\"inactive\""]);
                assert_eq!(actual, "\"Active\"");
            }
            other => panic!("expected Enum, got {other:?}"),
        }
        assert!(issues[0].describe().contains("\"Active\""));
        // 数字按数值比：1.0 == 1
        let schema = json!({"type": "object", "properties": {"n": {"enum": [1, 2]}}});
        assert!(validate(&schema, &json!({"n": 1.0})).is_empty());
        assert_eq!(validate(&schema, &json!({"n": 3})).len(), 1);
        // 对象/数组取值的 enum → 拿不准 → 放过
        let schema = json!({"type": "object", "properties": {"o": {"enum": [{"a": 1}]}}});
        assert!(validate(&schema, &json!({"o": {"b": 2}})).is_empty());
    }

    // ---- 类型集合 / 展示名 ----

    #[test]
    fn type_array_and_unknown_keyword_do_not_flag() {
        let schema = json!({"type": "object", "properties": {"t": {"type": ["string", "null"]}}});
        assert!(validate(&schema, &json!({"t": null})).is_empty());
        assert!(validate(&schema, &json!({"t": "x"})).is_empty());
        assert_eq!(validate(&schema, &json!({"t": 5})).len(), 1);
        // 未知关键字 → 不判定
        let schema = json!({"type": "object", "properties": {"u": {"type": "whatever"}}});
        assert!(validate(&schema, &json!({"u": 5})).is_empty());
    }

    #[test]
    fn describe_covers_every_issue_kind() {
        let unknown = ArgIssue::Unknown {
            path: "a.b".to_string(),
            known: vec!["x".to_string()],
        };
        assert!(unknown.describe().contains("a.b"));
        assert!(unknown.describe().contains("同层已知参数：x"));
        let ty = ArgIssue::Type {
            path: "n".to_string(),
            expected: "integer".to_string(),
            actual: "string".to_string(),
        };
        assert!(ty.describe().contains("类型不符"));
        let en = ArgIssue::Enum {
            path: "s".to_string(),
            allowed: vec!["\"a\"".to_string()],
            actual: "\"b\"".to_string(),
        };
        assert!(en.describe().contains("不在允许范围"));
    }
}
