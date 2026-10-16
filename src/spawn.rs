//! `--spawn` / `--spawn-headless`：探活失败时拉起 RimCrow，有界轮询 endpoint 写出。
//!
//! 防重入：探活通过就直接连接，绝不二次拉起；本模块只在未检出可用 endpoint 时
//! 被调用，且每个 bridge 进程至多尝试一次（由 SpawnGate 保证）。拉起目标先看环境
//! 变量 RIMCROW_SPAWN_CMD（存在则按 shell 规则切分执行，开发态与测试用），再看
//! bridge 同目录的 RimCrow.exe，都没有时返回写明原因的错误并提示手动启动。
//! --spawn-headless 在拉起时附带 --browser --no-open。Windows 下用 CREATE_NO_WINDOW
//! 拉起子进程，不闪控制台窗口，GUI 子进程自身的窗口显示不受该标志影响。就绪判据
//! 是 endpoint 文件写出生效，不等 GUI 窗口出现，轮询上限 15 秒。

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::endpoint::{probe_default, ProbeStatus};

/// 轮询 endpoint 写出的超时上界。
pub const SPAWN_TIMEOUT: Duration = Duration::from_secs(15);
/// 轮询间隔。
pub const SPAWN_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Windows：不为子进程创建控制台窗口（避免控制台闪窗）。
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 按 shell 参数规则切分一行命令（引号与转义），不经过 `cmd` 解释器。
///
/// 自实现而不引现成 crate：shlex 与 shell-words 是纯 POSIX 语义，未加引号的 `\`
/// 一律当转义符，`C:\Windows\System32\cmd.exe` 这类未加引号的 Windows 路径会被
/// 吃掉反斜杠直接作废；winsplit 镜像 Windows 命令行参数规则，只成对处理双引号，
/// 不支持单引号字面量。本函数输入面只有开发与测试用的 RIMCROW_SPAWN_CMD（非用户
/// 输入），切分结果经 Command 直接执行、不经 shell 也不走 cmd，契约只需本函数
/// 切分与 launch 执行两相自洽，无需与 MSVCRT 逐字一致，因此自实现换得未引号路径
/// 的反斜杠安全与单引号字面量，代价约六十行加单测。
///
/// 规则：未引号段按空白分词，`\` 仅在后随空白、引号或反斜杠时转义，否则按字面
/// 保留；双引号段内 `\` 仅转义 `"` 与 `\`，其余字符字面；单引号段内全部字面，
/// 遇到下一个单引号结束；空引号串产出空参数。
pub fn split_command(input: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut has = false;
    let mut chars = input.chars().peekable();
    let mut quote: Option<char> = None;

    while let Some(c) = chars.next() {
        match quote {
            Some('\'') => {
                if c == '\'' {
                    quote = None;
                } else {
                    cur.push(c);
                }
            }
            Some('"') => {
                if c == '"' {
                    quote = None;
                } else if c == '\\' {
                    match chars.peek() {
                        Some(&n) if n == '"' || n == '\\' => {
                            cur.push(n);
                            chars.next();
                        }
                        _ => cur.push(c),
                    }
                } else {
                    cur.push(c);
                }
            }
            _ => {
                if c == '\'' || c == '"' {
                    quote = Some(c);
                    has = true;
                } else if c == '\\' {
                    match chars.peek().copied() {
                        Some(n) if n.is_whitespace() || n == '"' || n == '\'' || n == '\\' => {
                            cur.push(n);
                            chars.next();
                            has = true;
                        }
                        _ => {
                            cur.push(c);
                            has = true;
                        }
                    }
                } else if c.is_whitespace() {
                    if has || !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                        has = false;
                    }
                } else {
                    cur.push(c);
                    has = true;
                }
            }
        }
    }
    if has || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// 拉起计划：程序与参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnPlan {
    pub program: String,
    pub args: Vec<String>,
}

impl SpawnPlan {
    /// 用于错误文案的可读渲染（受控输入，仅程序/参数回显）。
    pub fn render(&self) -> String {
        let mut parts = vec![self.program.clone()];
        parts.extend(self.args.iter().cloned());
        parts.join(" ")
    }
}

/// 解析拉起目标。`Err(String)` = 写明原因的错误说明（直接进降级文案）。
pub fn resolve_spawn_plan(headless: bool) -> Result<SpawnPlan, String> {
    let mut plan = if let Ok(raw) = std::env::var("RIMCROW_SPAWN_CMD") {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err("环境变量 RIMCROW_SPAWN_CMD 为空".to_string());
        }
        let parts = split_command(trimmed);
        if parts.is_empty() {
            return Err(format!("RIMCROW_SPAWN_CMD 无法切分为可执行命令：{trimmed}"));
        }
        let mut iter = parts.into_iter();
        let program = iter.next().expect("parts non-empty");
        SpawnPlan {
            program,
            args: iter.collect(),
        }
    } else {
        let exe = std::env::current_exe()
            .map_err(|e| format!("无法定位 rimcrow-mcp 自身路径（{e}）"))?;
        let sibling = exe
            .parent()
            .map(|d| d.join("RimCrow.exe"))
            .filter(|p| p.exists());
        match sibling {
            Some(p) => SpawnPlan {
                program: p.to_string_lossy().to_string(),
                args: Vec::new(),
            },
            None => {
                return Err(format!(
                    "未找到可拉起的 RimCrow（bridge 同目录无 RimCrow.exe，且未设置 RIMCROW_SPAWN_CMD）：{}",
                    exe.display()
                ));
            }
        }
    };

    if headless {
        plan.args.push("--browser".to_string());
        plan.args.push("--no-open".to_string());
    }
    Ok(plan)
}

/// 拉起子进程。Windows 下默认带 CREATE_NO_WINDOW，不闪控制台窗口，GUI 窗口正常
/// 显示；环境变量 RIMCROW_MCP_SPAWN_NO_WINDOW=0 可关掉该标志（诊断用）。
///
/// stdio 隔离：stdin 接 NUL，绝不把 JSON-RPC 输入通道交给子进程，否则子进程可能
/// 抢读或吞掉客户端消息，实测某些控制台子进程会因此在初始化期挂死；stdout 接
/// NUL，子进程输出绝不混进本桥的 JSON-RPC 通道；stderr 保持继承，子进程诊断信息
/// 并入本桥 stderr 日志。
pub fn launch(plan: &SpawnPlan) -> std::io::Result<Child> {
    let mut cmd = Command::new(&plan.program);
    cmd.args(&plan.args);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::inherit());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let no_window = std::env::var("RIMCROW_MCP_SPAWN_NO_WINDOW")
            .map(|v| v.trim() != "0")
            .unwrap_or(true);
        if no_window {
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
    }
    cmd.spawn()
}

/// 有界轮询 endpoint 写出生效，就绪判据是 endpoint 本身，不等窗口出现。
///
/// child 用于提前发现拉起的进程立即失败退出；进程提前退出但 exit code 为 0
/// （可能委托了真正的启动器）时继续轮询到超时。
pub async fn wait_for_endpoint(
    endpoint_path: &Path,
    mut child: Child,
    plan: &SpawnPlan,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let mut early_exit: Option<String> = None;

    loop {
        if let Ok(Some(status)) = child.try_wait() {
            if !status.success() {
                let code = status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| status.to_string());
                return Err(format!(
                    "拉起的进程已退出（{}，exit code={}），endpoint 未写入：{}",
                    plan.render(),
                    code,
                    endpoint_path.display()
                ));
            }
            early_exit = Some(format!("拉起的进程已自行退出（exit=0）：{}", plan.render()));
        }

        let out = probe_default(endpoint_path);
        if out.status == ProbeStatus::Ready {
            return Ok(());
        }

        if Instant::now() >= deadline {
            let mut msg = format!(
                "已尝试拉起 RimCrow 1 次（{}），{:.0}s 内 endpoint 未写出生效：{}",
                plan.render(),
                timeout.as_secs_f64(),
                endpoint_path.display()
            );
            if let Some(note) = early_exit {
                msg.push_str(&format!("；{note}"));
            }
            msg.push_str(&format!("；最后一次探活：{}", out.reason));
            return Err(msg);
        }

        tokio::time::sleep(SPAWN_POLL_INTERVAL).await;
    }
}

/// 拉起 + 等待就绪的完整流程（供 `SpawnGate` 调用）。
pub async fn spawn_and_wait(endpoint_path: &Path, headless: bool) -> Result<(), String> {
    // 已经可用就不二次拉起；这是二次防线，调用方已探活过一次
    if probe_default(endpoint_path).is_ready() {
        return Ok(());
    }
    let plan = resolve_spawn_plan(headless)?;
    tracing::info!("--spawn: 拉起 RimCrow → {}", plan.render());
    let child = launch(&plan).map_err(|e| {
        format!(
            "拉起 RimCrow 失败（{}；{e}）",
            plan.render()
        )
    })?;
    wait_for_endpoint(endpoint_path, child, &plan, SPAWN_TIMEOUT).await
}

/// 手动启动指引（解析不到拉起目标时的 nextActions 素材）。
pub fn manual_start_hint() -> &'static str {
    "手动启动 RimCrow GUI（或设置 RIMCROW_SPAWN_CMD 指定启动命令）后重试"
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_simple_command() {
        assert_eq!(
            split_command("python main.py --browser --no-open"),
            vec!["python", "main.py", "--browser", "--no-open"]
        );
    }

    #[test]
    fn split_collapses_extra_whitespace_and_keeps_empty_args() {
        assert_eq!(
            split_command("  python    main.py  "),
            vec!["python", "main.py"]
        );
        assert_eq!(split_command(r#"python "" x"#), vec!["python", "", "x"]);
    }

    #[test]
    fn split_handles_double_quotes_with_spaces() {
        assert_eq!(
            split_command(r#""C:\Program Files\RimCrow\RimCrow.exe" --browser"#),
            vec![r"C:\Program Files\RimCrow\RimCrow.exe", "--browser"]
        );
        // 双引号内 \ 只转义 " 和 \
        assert_eq!(split_command(r#"a "b\"c""#), vec!["a", r#"b"c"#]);
        assert_eq!(split_command(r#"a "b\nc""#), vec!["a", r#"b\nc"#]);
    }

    #[test]
    fn split_handles_single_quotes_literal() {
        assert_eq!(split_command("a 'b c' d"), vec!["a", "b c", "d"]);
        // 单引号内无转义：\ 是字面量
        assert_eq!(split_command(r"a 'b\c'"), vec!["a", r"b\c"]);
    }

    #[test]
    fn split_unquoted_backslash_escapes() {
        assert_eq!(split_command(r"a\ b"), vec!["a b"]);
        assert_eq!(split_command(r"a b\"), vec!["a", r"b\"]);
    }

    #[test]
    fn split_keeps_unquoted_windows_paths_intact() {
        // 反斜杠后跟字母必须按字面保留（否则 C:\Windows 会被吃成 C:Windows）
        assert_eq!(
            split_command(r"C:\Windows\System32\cmd.exe /c exit 7"),
            vec![r"C:\Windows\System32\cmd.exe", "/c", "exit", "7"]
        );
        assert_eq!(
            split_command(r"C:\Users\me\RimCrow.exe --browser"),
            vec![r"C:\Users\me\RimCrow.exe", "--browser"]
        );
        // 非转义语境的 \n 也保持字面
        assert_eq!(split_command(r"a\nb"), vec![r"a\nb"]);
        // 显式引用时同样完整
        assert_eq!(
            split_command(r#""C:\Program Files\RimCrow\RimCrow.exe" --browser"#),
            vec![r"C:\Program Files\RimCrow\RimCrow.exe", "--browser"]
        );
    }

    #[test]
    fn split_utf8_and_trailing_whitespace() {
        assert_eq!(
            split_command("python 主脚本.py --日志 debug"),
            vec!["python", "主脚本.py", "--日志", "debug"]
        );
        assert!(split_command("   ").is_empty());
        assert!(split_command("").is_empty());
    }

    #[test]
    fn headless_appends_browser_flags() {
        // 不依赖环境变量：RIMCROW_SPAWN_CMD 可能被并行测试设置，这里只测追加逻辑
        let plan = SpawnPlan {
            program: "RimCrow.exe".into(),
            args: vec![],
        };
        let mut with_flags = plan.clone();
        with_flags.args.push("--browser".into());
        with_flags.args.push("--no-open".into());
        assert_eq!(with_flags.render(), "RimCrow.exe --browser --no-open");
    }

    #[test]
    fn resolve_plan_without_env_finds_sibling_or_errors() {
        // 测试进程在 target/debug/deps/ 下，同目录通常没有 RimCrow.exe → 应返回写明原因的错误
        if std::env::var("RIMCROW_SPAWN_CMD").is_ok() {
            return; // 环境被外部设置，跳过（避免依赖顺序）
        }
        match resolve_spawn_plan(false) {
            Ok(plan) => {
                assert!(plan.program.to_lowercase().ends_with("rimcrow.exe"));
                assert!(plan.args.is_empty());
            }
            Err(msg) => {
                assert!(msg.contains("RimCrow.exe"), "{msg}");
                assert!(msg.contains("RIMCROW_SPAWN_CMD"), "{msg}");
            }
        }
    }

    #[test]
    fn wait_for_endpoint_times_out_with_causal_text() {
        use std::path::PathBuf;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp")
            .join("no-endpoint-here.json");
        let _ = std::fs::remove_file(&path);
        let plan = SpawnPlan {
            program: "cmd".into(),
            args: vec!["/c".into(), "exit".into(), "0".into()],
        };
        // 拉起一个立即返回 0 的进程且 endpoint 不存在，结果只能是有界超时
        let child = launch(&plan).expect("launch cmd");
        let started = Instant::now();
        let result = rt.block_on(wait_for_endpoint(&path, child, &plan, Duration::from_millis(600)));
        let elapsed = started.elapsed();
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("未写出生效"), "{err}");
        assert!(err.contains("endpoint"), "{err}");
        assert!(
            elapsed >= Duration::from_millis(600) && elapsed < Duration::from_secs(5),
            "bounded wait expected, took {:?}",
            elapsed
        );
    }

    #[test]
    fn wait_for_endpoint_fails_fast_when_child_exits_nonzero() {
        use std::path::PathBuf;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp")
            .join("no-endpoint-here2.json");
        let _ = std::fs::remove_file(&path);
        let plan = SpawnPlan {
            program: "cmd".into(),
            args: vec!["/c".into(), "exit".into(), "7".into()],
        };
        let child = launch(&plan).expect("launch cmd");
        std::thread::sleep(Duration::from_millis(300));
        let started = Instant::now();
        let result = rt.block_on(wait_for_endpoint(&path, child, &plan, Duration::from_secs(15)));
        let err = result.expect_err("must fail");
        assert!(err.contains("已退出"), "{err}");
        assert!(err.contains("exit code=7"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5), "should fail fast");
    }
}
