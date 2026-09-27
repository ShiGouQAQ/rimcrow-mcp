//! endpoint 文件契约（SPEC-mcp-embed §4.3, schema_version=1）与探活。
//!
//! 与 RimCrow `backend/mcp_endpoint.py::probe_endpoint` **语义等价**：
//! - 路径：env `RIMCROW_MCP_ENDPOINT` 覆盖 → `%LOCALAPPDATA%\RimCrow\mcp-endpoint.json`
//! - 状态：`missing` / `stale`（含 schema、url、pid、进程名、启动时间细分）/ `ready`
//! - pid 二次校验：进程存活 **且** 进程名在白名单 **且**
//!   `started_at ∈ [create_time - CLOCK_SKEW, create_time + MAX_BOOT]`
//!   （Windows pid 会复用，启动时间比对才是主防线）
//! - 进程信息用成熟库 `sysinfo` 获取（进程名 / 启动时间），不自己造轮子。
//!
//! `reason` 是给 agent 看的因果陈述（含路径与 pid 事实），直接进降级错误文案。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// 本桥支持的 endpoint schema 版本。
pub const SCHEMA_VERSION: i64 = 1;
/// endpoint 路径覆盖用的环境变量名。
pub const ENDPOINT_ENV: &str = "RIMCROW_MCP_ENDPOINT";
/// `started_at` 与进程 `create_time` 允许的下界偏差（秒）。
pub const CLOCK_SKEW: f64 = 5.0;
/// `started_at` 与进程 `create_time` 允许的上界偏差（秒，endpoint 写出距进程启动的上限）。
pub const MAX_BOOT: f64 = 300.0;

/// loopback 前缀（Python 侧 `str.startswith("http://127.0.0.1:")`）。
pub const LOOPBACK_PREFIX: &str = "http://127.0.0.1:";

/// endpoint 文件的结构化形态（字段与 SPEC-mcp-embed §4.3 逐条对应）。
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct EndpointInfo {
    pub url: String,
    pub pid: u32,
    pub token: String,
    #[serde(default)]
    pub started_at: String,
    #[serde(default)]
    pub mode: String,
    /// 缺失 / null → `None`，读出时按 0 处理（对齐 Python `raw.get(...) or 0`）。
    #[serde(default)]
    pub schema_version: Option<i64>,
}

impl EndpointInfo {
    /// schema 版本读数（缺失按 0，与 Python 一致）。
    pub fn schema(&self) -> i64 {
        self.schema_version.unwrap_or(0)
    }
}

/// 探活状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeStatus {
    /// endpoint 可用，可直接 attach。
    Ready,
    /// endpoint 文件不存在。
    Missing,
    /// 文件损坏 / schema 不匹配 / url 非 loopback / pid 陈旧。
    Stale,
    /// `schema_version` 高于本桥支持（专门分支，文案固定）。
    UnsupportedSchema(i64),
}

/// 探活产物：状态 + 结构化 endpoint + 给 agent 的因果陈述。
#[derive(Debug, Clone)]
pub struct ProbeOutcome {
    pub status: ProbeStatus,
    pub info: Option<EndpointInfo>,
    pub reason: String,
    pub path: PathBuf,
}

impl ProbeOutcome {
    pub fn is_ready(&self) -> bool {
        self.status == ProbeStatus::Ready
    }

    /// `Ready` 时取出 info。
    pub fn ready_info(&self) -> Option<&EndpointInfo> {
        match self.status {
            ProbeStatus::Ready => self.info.as_ref(),
            _ => None,
        }
    }
}

/// 探活允许的进程名集合（小写，BTreeSet 保证 `join("/")` 输出顺序稳定）。
///
/// - `rimcrow.exe`：打包主程序名（`pack_pyinstaller.py` 的 APP_NAME）
/// - `python.exe` / `pythonw.exe`：开发态 GUI 与 dev attach（对齐 Python 侧
///   `Path(sys.executable).name`；Windows 下同一份代码可能是 `python.exe` 或 `pythonw.exe`）
pub fn default_allowed_names() -> BTreeSet<String> {
    ["rimcrow.exe", "python.exe", "pythonw.exe"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// endpoint 文件路径：env 覆盖 → `%LOCALAPPDATA%\RimCrow\mcp-endpoint.json`。
pub fn default_endpoint_path() -> PathBuf {
    if let Ok(raw) = std::env::var(ENDPOINT_ENV) {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    let base = std::env::var("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok()
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| {
            // 对齐 Python `Path.home()`（Windows = USERPROFILE，兜底 HOME）
            std::env::var("USERPROFILE")
                .ok()
                .map(PathBuf::from)
                .or_else(|| std::env::var("HOME").ok().map(PathBuf::from))
        })
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("RimCrow").join("mcp-endpoint.json")
}

/// 只读解析 endpoint 文件（不做进程探活）。
///
/// `Ok(None)` = 文件不存在；`Err(String)` = 损坏 / 字段缺失。
/// 单测直接消费；`probe` 内联了同逻辑以便区分 NotFound / 读取失败。
#[allow(dead_code)]
pub fn read_endpoint(path: &Path) -> Result<Option<EndpointInfo>, String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("读取失败（{e}）")),
    };
    match serde_json::from_str::<EndpointInfo>(&raw) {
        Ok(info) => Ok(Some(info)),
        Err(e) => Err(format!("JSON 解析失败（{e}）")),
    }
}

/// 探活：返回 `ProbeOutcome`（状态 + 因果 reason + path）。
///
/// `allowed` 为进程名白名单（小写）。语义与 `mcp_endpoint.probe_endpoint` 逐条对应。
pub fn probe(path: &Path, allowed: &BTreeSet<String>) -> ProbeOutcome {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return ProbeOutcome {
                status: ProbeStatus::Missing,
                info: None,
                reason: format!("endpoint 文件不存在：{}", path.display()),
                path: path.to_path_buf(),
            };
        }
        Err(_) => {
            return ProbeOutcome {
                status: ProbeStatus::Stale,
                info: None,
                reason: format!("endpoint 文件损坏或字段缺失：{}", path.display()),
                path: path.to_path_buf(),
            };
        }
    };

    let info = match serde_json::from_str::<EndpointInfo>(&raw) {
        Ok(v) => v,
        Err(_) => {
            return ProbeOutcome {
                status: ProbeStatus::Stale,
                info: None,
                reason: format!("endpoint 文件损坏或字段缺失：{}", path.display()),
                path: path.to_path_buf(),
            };
        }
    };

    let outcome = |status: ProbeStatus, reason: String, info: Option<EndpointInfo>| ProbeOutcome {
        status,
        info,
        reason,
        path: path.to_path_buf(),
    };

    // 1) schema 版本
    if info.schema() != SCHEMA_VERSION {
        return outcome(
            ProbeStatus::UnsupportedSchema(info.schema()),
            format!(
                "endpoint schema_version={} 不受支持（期望 {}）：{}",
                info.schema(),
                SCHEMA_VERSION,
                path.display()
            ),
            Some(info),
        );
    }

    // 2) url 必须是 loopback（防止被诱导连到外部地址）
    if !info.url.starts_with(LOOPBACK_PREFIX) {
        return outcome(
            ProbeStatus::Stale,
            format!("endpoint url 非 loopback，拒绝连接：{}", info.url),
            Some(info),
        );
    }

    // 3) pid 存活 + 进程名白名单 + 启动时间比对
    let pid = Pid::from_u32(info.pid);
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        false,
        ProcessRefreshKind::nothing(),
    );
    let process = match system.process(pid) {
        Some(p) => p,
        None => {
            return outcome(
                ProbeStatus::Stale,
                format!(
                    "RimCrow 未运行（endpoint 中的 pid {} 已不存在）：{}",
                    info.pid,
                    path.display()
                ),
                Some(info),
            );
        }
    };

    let actual_name_os = process.name().to_os_string();
    let actual_name = actual_name_os.to_string_lossy().to_string();
    let actual_lower = actual_name.to_lowercase();
    if !actual_lower.is_empty() && !allowed.contains(&actual_lower) {
        let allowed_joined = allowed.iter().cloned().collect::<Vec<_>>().join("/");
        return outcome(
            ProbeStatus::Stale,
            format!(
                "pid {} 已被其它进程占用（{} ∉ {}）：{}",
                info.pid,
                actual_name,
                allowed_joined,
                path.display()
            ),
            Some(info),
        );
    }

    let create_epoch = process.start_time() as f64;
    if let Some(started_epoch) = parse_epoch(&info.started_at) {
        if started_epoch < create_epoch - CLOCK_SKEW {
            return outcome(
                ProbeStatus::Stale,
                format!(
                    "pid {} 启动时间晚于 endpoint 写出时间（pid 复用）：{}",
                    info.pid,
                    path.display()
                ),
                Some(info),
            );
        }
        if started_epoch > create_epoch + MAX_BOOT {
            return outcome(
                ProbeStatus::Stale,
                format!(
                    "pid {} 与 endpoint 的 started_at 不吻合：{}",
                    info.pid,
                    path.display()
                ),
                Some(info),
            );
        }
    }

    outcome(ProbeStatus::Ready, "ok".to_string(), Some(info))
}

/// 探活便捷入口：使用默认白名单。
pub fn probe_default(path: &Path) -> ProbeOutcome {
    probe(path, &default_allowed_names())
}

/// ISO8601（RFC3339，含偏移）→ epoch 秒；解析失败返回 `None`（跳过时间校验，
/// 与 Python `_parse_epoch` 失败 → 不校验一致）。
pub fn parse_epoch(value: &str) -> Option<f64> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(value) {
        return Some(dt.timestamp() as f64 + dt.timestamp_subsec_nanos() as f64 / 1e9);
    }
    // Python `datetime.fromisoformat` 也接受无偏移的 naive 字符串（按本地时区解释）
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f") {
        use chrono::TimeZone;
        if let Some(dt) = chrono::Local.from_local_datetime(&naive).single() {
            return Some(dt.timestamp() as f64 + dt.timestamp_subsec_nanos() as f64 / 1e9);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// 测试产物一律落在 crate 内的 `target/test-tmp/`（不污染系统临时目录）。
    fn tmp_file(tag: &str) -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp");
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!(
            "endpoint-{}-{}-{}.json",
            tag,
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ))
    }

    fn write(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
    }

    fn self_start_rfc3339() -> (String, f64) {
        let mut sys = System::new();
        let pid = Pid::from_u32(std::process::id());
        sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            false,
            ProcessRefreshKind::nothing(),
        );
        let p = sys.process(pid).expect("test process must be visible");
        let epoch = p.start_time() as f64;
        let dt = chrono::DateTime::from_timestamp(epoch as i64, 0).expect("valid timestamp");
        (dt.to_rfc3339(), epoch)
    }

    fn self_allowed() -> BTreeSet<String> {
        // 测试进程名 = rimcrow-mcp-<hash>.exe，注入到白名单里即可测 ready 分支
        let name = std::env::current_exe()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_lowercase();
        let mut set = BTreeSet::new();
        set.insert(name);
        set
    }

    #[test]
    fn missing_file_is_missing() {
        let p = tmp_file("missing");
        let _ = std::fs::remove_file(&p);
        let out = probe(&p, &default_allowed_names());
        assert_eq!(out.status, ProbeStatus::Missing);
        assert!(out.reason.contains("endpoint 文件不存在"), "{}", out.reason);
        assert!(out.reason.contains(&p.display().to_string()));
    }

    #[test]
    fn corrupt_file_is_stale() {
        let p = tmp_file("corrupt");
        write(&p, "{not json");
        let out = probe(&p, &default_allowed_names());
        assert_eq!(out.status, ProbeStatus::Stale);
        assert!(out.reason.contains("损坏"), "{}", out.reason);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn missing_fields_is_stale() {
        let p = tmp_file("partial");
        write(&p, r#"{"url":"http://127.0.0.1:1/mcp"}"#);
        let out = probe(&p, &default_allowed_names());
        assert_eq!(out.status, ProbeStatus::Stale);
        assert!(out.reason.contains("损坏"), "{}", out.reason);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn schema_v2_is_unsupported_with_causal_text() {
        let p = tmp_file("schema");
        write(
            &p,
            r#"{"schema_version":2,"url":"http://127.0.0.1:1/mcp","pid":1,"token":"t","started_at":"2026-01-01T00:00:00+00:00","mode":"desktop"}"#,
        );
        let out = probe(&p, &default_allowed_names());
        assert_eq!(out.status, ProbeStatus::UnsupportedSchema(2));
        assert!(out.reason.contains("schema_version=2"), "{}", out.reason);
        assert!(out.reason.contains("期望 1"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn schema_absent_reads_as_zero() {
        let p = tmp_file("schema0");
        write(
            &p,
            r#"{"url":"http://127.0.0.1:1/mcp","pid":1,"token":"t"}"#,
        );
        let info = read_endpoint(&p).unwrap().unwrap();
        assert_eq!(info.schema(), 0);
        assert_eq!(info.started_at, "");
        let out = probe(&p, &default_allowed_names());
        assert_eq!(out.status, ProbeStatus::UnsupportedSchema(0));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn non_loopback_url_is_rejected() {
        let p = tmp_file("url");
        write(
            &p,
            r#"{"schema_version":1,"url":"http://192.168.1.5:3944/mcp","pid":1,"token":"t","started_at":"2026-01-01T00:00:00+00:00","mode":"desktop"}"#,
        );
        let out = probe(&p, &default_allowed_names());
        assert_eq!(out.status, ProbeStatus::Stale);
        assert!(out.reason.contains("非 loopback"), "{}", out.reason);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn dead_pid_is_stale() {
        let p = tmp_file("deadpid");
        write(
            &p,
            r#"{"schema_version":1,"url":"http://127.0.0.1:3944/mcp","pid":4294967295,"token":"t","started_at":"2026-01-01T00:00:00+00:00","mode":"desktop"}"#,
        );
        let out = probe(&p, &default_allowed_names());
        assert_eq!(out.status, ProbeStatus::Stale);
        assert!(
            out.reason.contains("pid 4294967295 已不存在"),
            "{}",
            out.reason
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn foreign_process_name_is_stale() {
        let (started, _) = self_start_rfc3339();
        let p = tmp_file("name");
        write(
            &p,
            &format!(
                r#"{{"schema_version":1,"url":"http://127.0.0.1:3944/mcp","pid":{},"token":"t","started_at":"{}","mode":"desktop"}}"#,
                std::process::id(),
                started
            ),
        );
        let mut allowed = BTreeSet::new();
        allowed.insert("definitely-not-us.exe".to_string());
        let out = probe(&p, &allowed);
        assert_eq!(out.status, ProbeStatus::Stale);
        assert!(out.reason.contains("已被其它进程占用"), "{}", out.reason);
        assert!(out.reason.contains("definitely-not-us.exe"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn own_process_with_matching_start_time_is_ready() {
        let (started, _) = self_start_rfc3339();
        let p = tmp_file("ready");
        write(
            &p,
            &format!(
                r#"{{"schema_version":1,"url":"http://127.0.0.1:3944/mcp","pid":{},"token":"tok","started_at":"{}","mode":"desktop"}}"#,
                std::process::id(),
                started
            ),
        );
        let out = probe(&p, &self_allowed());
        assert_eq!(out.status, ProbeStatus::Ready, "reason={}", out.reason);
        let info = out.ready_info().unwrap();
        assert_eq!(info.token, "tok");
        assert_eq!(info.pid, std::process::id());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn start_time_after_create_is_pid_reuse() {
        let (started, epoch) = self_start_rfc3339();
        let shifted = chrono::DateTime::from_timestamp((epoch - 100.0) as i64, 0)
            .unwrap()
            .to_rfc3339();
        let _ = started;
        let p = tmp_file("reuse");
        write(
            &p,
            &format!(
                r#"{{"schema_version":1,"url":"http://127.0.0.1:3944/mcp","pid":{},"token":"t","started_at":"{}","mode":"desktop"}}"#,
                std::process::id(),
                shifted
            ),
        );
        let out = probe(&p, &self_allowed());
        assert_eq!(out.status, ProbeStatus::Stale);
        assert!(out.reason.contains("pid 复用"), "{}", out.reason);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn start_time_far_after_create_is_mismatch() {
        let (started, epoch) = self_start_rfc3339();
        let shifted = chrono::DateTime::from_timestamp((epoch + 1000.0) as i64, 0)
            .unwrap()
            .to_rfc3339();
        let _ = started;
        let p = tmp_file("mismatch");
        write(
            &p,
            &format!(
                r#"{{"schema_version":1,"url":"http://127.0.0.1:3944/mcp","pid":{},"token":"t","started_at":"{}","mode":"desktop"}}"#,
                std::process::id(),
                shifted
            ),
        );
        let out = probe(&p, &self_allowed());
        assert_eq!(out.status, ProbeStatus::Stale);
        assert!(out.reason.contains("started_at 不吻合"), "{}", out.reason);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn default_path_uses_env_override() {
        // env 测试串行执行会互相干扰，这里只验证非覆盖分支的形状
        let p = default_endpoint_path();
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        assert_eq!(name, "mcp-endpoint.json");
        assert!(p.to_string_lossy().ends_with("RimCrow\\mcp-endpoint.json")
            || p.to_string_lossy().ends_with("RimCrow/mcp-endpoint.json"));
    }

    #[test]
    fn rfc3339_and_naive_parse_epoch() {
        let a = parse_epoch("2026-09-27T10:45:19.527884+00:00").unwrap();
        let b = parse_epoch("2026-09-27T10:45:19+00:00").unwrap();
        assert!((a - b).abs() < 1.0);
        assert!(parse_epoch("not a date").is_none());
        // naive 字符串按本地时区解释（不 panic、可解析）
        let naive = parse_epoch("2026-09-27T10:45:19.5");
        assert!(naive.is_some(), "naive local parse should work");
    }

    #[test]
    fn real_live_endpoint_if_present() {
        // 若本机恰好有 RimCrow 在跑（开发机常态），探活应为 ready；否则跳过。
        let p = default_endpoint_path();
        if !p.exists() {
            return;
        }
        let out = probe_default(&p);
        // 只断言「不 panic 且 reason 是因果陈述」，不强制 ready（CI/无 GUI 环境）
        assert!(!out.reason.is_empty());
        if out.is_ready() {
            let info = out.ready_info().unwrap();
            assert!(info.url.starts_with(LOOPBACK_PREFIX));
            assert!(!info.token.is_empty());
        } else {
            eprintln!("[test] live endpoint not ready: {}", out.reason);
        }
    }
}
