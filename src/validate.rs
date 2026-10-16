//! `tools/call` 参数预校验——只拦「当前生效清单里不存在的参数」，防「假成功」。
//!
//! 动机是已实测的缺陷：桥的工具清单比运行中的 GUI 新时，新参数会被旧 GUI 静默忽略，
//! 不报错却返回语义完全不同的数据（list_mods 传 scope:"active" 加 limit:3 时，旧 GUI
//! 返回全库结果，is_active:false 的条目混在其中）。这类假成功比报错危险，故在转发前
//! 先由桥拦一道。
//!
//! 只做这一件事，且判定完全按 JSON Schema 的标准关键字语义（properties /
//! additionalProperties / patternProperties / propertyNames），不镜像任何服务端框架的
//! 类型强转或枚举语义：类型与取值错误由服务端自己校验并以 isError 报回（MCP 标准行为），
//! 桥侧再判一遍只会耦合服务端实现细节（框架升级即漂移），还可能在「清单落后于运行中
//! GUI」时误杀合法取值。缺省参数零影响，不做 required 缺失检查，缺了服务端自己会报。
//!
//! 误杀防护遵循宁可放过、不可误杀。未知键只在 additionalProperties 为 false 且没有
//! patternProperties 与 propertyNames（两者意味着动态键名）时拦，true、子 schema 与
//! 缺省形状都按 JSON Schema 默认语义放行额外属性，按 schema 判定、不一刀切。
//!
//! 判定基准与清单双模同源：上游实时 tools/list 优先，拿不到才用嵌入快照，基准永远是
//! 当前生效的那份清单而不是写死的快照。基准的选取在 bridge 层完成，本模块只接受
//! 已选好的清单与其 inputSchema。
//!
//! 产物是 ArgIssue 列表；错误信封（isError:true 加 message 与 nextActions，与 RimCrow
//! 错误信封同形）由 bridge 层组装，本模块不产出工具结果。ttlMs 与 cacheScope 是 MCP
//! 2026-07-28 规范对 tools/list 等清单类结果的缓存提示字段，不属于 tools/call 结果，
//! 错误信封不携带。

use serde_json::{Map, Value};

/// 单个参数问题：取值未被当前生效清单的 inputSchema 声明（= 旧 GUI 会静默忽略的那类
/// 参数）。`path` 为点分嵌套路径，数组元素形如 `ops[0].foo`，顶层即参数名；`known`
/// 为该层 schema 已声明的参数（顶层的同名列表由 bridge 统一附在 message 尾部）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgIssue {
    /// 未被识别的参数路径。
    pub path: String,
    /// 该层已声明的参数名（properties 键序保持 schema 声明顺序）。
    pub known: Vec<String>,
}

impl ArgIssue {
    /// 中文文案片段（进错误 message）。
    pub fn describe(&self) -> String {
        // 顶层已知参数列表由 bridge 统一附在 message 尾部，这里只补嵌套层
        if self.path.contains('.') || self.path.contains('[') {
            let k = if self.known.is_empty() {
                "该层未声明任何参数".to_string()
            } else {
                self.known.join("、")
            };
            format!("参数 {} 未被识别（同层已知参数：{k}）", self.path)
        } else {
            format!("参数 {} 未被识别", self.path)
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

/// 校验一份对象型调用参数中的未知键；返回全部问题（空 = 放行）。
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
    // 只有显式 additionalProperties:false 才拦未知键；true、子 schema 与缺省都按
    // JSON Schema 默认语义放行额外属性。patternProperties 与 propertyNames 意味着
    // 动态键名，同样放行。
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
            None if forbid_extra => out.push(ArgIssue {
                path: child,
                known: props
                    .map(|p| p.keys().cloned().collect())
                    .unwrap_or_default(),
            }),
            None => {} // schema 允许额外属性或没有 properties 可依时放过
        }
    }
}

/// 校验单个取值：只向下递归嵌套对象与数组元素找未知键；类型与取值合法性由服务端校验。
fn check_value(schema: &Value, value: &Value, path: &str, out: &mut Vec<ArgIssue>) {
    let Some(sub) = schema.as_object() else {
        return; // 非对象 schema 无从判定，放过
    };
    match value {
        Value::Object(obj) => {
            if let Some(branch) = object_branch(sub) {
                check_object(branch, obj, path, out);
            }
        }
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

/// 对象取值时用于递归的子 schema：本层有 `properties` 就用本层；否则在 anyOf/oneOf
/// 里取**唯一**的对象型分支（联合类型的可空分支常态：{对象 schema, null}）。
///
/// 多于一个对象分支时键归属不明（键可能合法地属于其中任何一支，按第一支判会误杀），
/// 按宁可放过返回 None，整层跳过未知键检查；没有对象型分支同样返回 None（对象取值
/// 与该 schema 无从对应，交给服务端判）。
fn object_branch(sub: &Map<String, Value>) -> Option<&Map<String, Value>> {
    if sub.get("properties").is_some() {
        return Some(sub);
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(branches) = sub.get(key).and_then(Value::as_array) {
            let object_branches: Vec<&Map<String, Value>> = branches
                .iter()
                .filter_map(Value::as_object)
                .filter(|b| {
                    b.get("type").and_then(Value::as_str) == Some("object")
                        || b.get("properties").is_some()
                })
                .collect();
            match object_branches.len() {
                1 => return Some(object_branches[0]),
                0 => continue,
                _ => return None,
            }
        }
    }
    None
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

    #[test]
    fn unknown_param_rejected_with_known_list() {
        let args = json!({"scope": "active", "limit": 3});
        let issues = validate(&old_list_mods_schema(), &args);
        assert_eq!(issues.len(), 1, "only scope is unknown: {issues:?}");
        assert_eq!(issues[0].path, "scope");
        assert_eq!(issues[0].known, vec!["filter", "limit", "offset"]);
        assert!(issues[0].describe().contains("scope"));
        assert!(issues[0].describe().contains("未被识别"));
        assert_eq!(
            known_params(&old_list_mods_schema()),
            vec!["filter", "limit", "offset"]
        );
    }

    #[test]
    fn known_args_pass_with_no_issues() {
        // 新快照 schema 下的完整合法调用
        let args = json!({"scope": "active", "limit": 3, "offset": 0, "filter": "raider"});
        assert!(validate(&new_list_mods_schema(), &args).is_empty());
        // 旧 GUI schema 下的调用同样合法（未知参数以「生效清单」为准，不以快照为准）
        let args = json!({"limit": 3, "offset": 0});
        assert!(validate(&old_list_mods_schema(), &args).is_empty());
    }

    /// 类型与取值合法性由服务端校验：宽松强转语义（如字符串 "5" 传给 integer）、
    /// enum 越界、类型不符等桥侧一律不判、原样放行。
    #[test]
    fn type_and_enum_issues_are_server_side_only() {
        let schema = new_list_mods_schema();
        // 字符串数字 / 可疑枚举取值都放行，交服务端判定
        assert!(validate(&schema, &json!({"limit": "not-a-number"})).is_empty());
        assert!(validate(&schema, &json!({"scope": "Maybe"})).is_empty());
        assert!(validate(&schema, &json!({"filter": 123})).is_empty());
        // 服务端同款 pydantic 宽松语义会接受的取值更不能误杀
        assert!(validate(&schema, &json!({"limit": "5"})).is_empty());
        assert!(validate(&schema, &json!({"offset": true})).is_empty());
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
        // JSON Schema 默认允许额外属性，缺省不拦
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

    #[test]
    fn nested_object_unknown_key_checked() {
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
        let args = json!({"token": {"path": "x", "bogus": 1}});
        let issues = validate(&schema, &args);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].path, "token.bogus");
        assert_eq!(issues[0].known, vec!["path", "size"]);
        // 嵌套文案带出同层已知参数
        assert!(issues[0].describe().contains("path"));
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
            ArgIssue {
                path: "ops[1].bogus".to_string(),
                known: vec!["op".to_string()],
            }
        );
    }

    /// 可空联合（{对象 schema, null}）取对象分支递归未知键检查。
    #[test]
    fn nullable_union_takes_object_branch() {
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "token": {
                    "anyOf": [
                        {"type": "object", "additionalProperties": false,
                         "properties": {"path": {"type": "string"}}},
                        {"type": "null"}
                    ]
                }
            }
        });
        let issues = validate(&schema, &json!({"token": {"bogus": 1}}));
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].path, "token.bogus");
        // null 取值无可递归对象，放过
        assert!(validate(&schema, &json!({"token": null})).is_empty());
    }

    /// 多个对象分支时键归属不明：按第一支判会误杀属于其它分支的合法键，整层放过。
    #[test]
    fn ambiguous_union_objects_are_skipped() {
        let schema = json!({
            "type": "object",
            "properties": {
                "payload": {
                    "anyOf": [
                        {"type": "object", "additionalProperties": false,
                         "properties": {"a": {"type": "string"}}},
                        {"type": "object", "additionalProperties": false,
                         "properties": {"b": {"type": "integer"}}}
                    ]
                }
            }
        });
        // b 只属于第二支：不属于第一支也不是「未被识别」——交给服务端判
        assert!(validate(&schema, &json!({"payload": {"b": 1}})).is_empty());
    }

    #[test]
    fn no_schema_nothing_to_judge() {
        // 没有 properties 的 schema（或非对象 schema）→ 无从判定 → 放过
        assert!(validate(&json!({"type": "object"}), &json!({"x": 1})).is_empty());
        assert!(validate(&json!("not-a-schema"), &json!({"x": 1})).is_empty());
        // 参数为空对象时同样无问题
        assert!(validate(&old_list_mods_schema(), &json!({})).is_empty());
    }

    #[test]
    fn describe_covers_nested_and_top_level() {
        let nested = ArgIssue {
            path: "a.b".to_string(),
            known: vec!["x".to_string()],
        };
        assert!(nested.describe().contains("a.b"));
        assert!(nested.describe().contains("同层已知参数：x"));
        let top = ArgIssue {
            path: "n".to_string(),
            known: vec![],
        };
        assert!(top.describe().contains("n 未被识别"));
        let nested_empty = ArgIssue {
            path: "a[0].b".to_string(),
            known: vec![],
        };
        assert!(nested_empty.describe().contains("该层未声明任何参数"));
    }
}
