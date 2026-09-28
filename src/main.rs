//! `rimcrow-mcp` —— RimCrow MCP 生产入口（唯一的 stdio 入口）。
//!
//! 一个极薄的 **stdio ↔ Streamable HTTP 桥**：
//! ```text
//! MCP 客户端(ZCode/OpenCode) --stdio--> rimcrow-mcp.exe --Streamable HTTP--> RimCrow.exe 内嵌端点
//! ```
//!
//! CLI：`rimcrow-mcp.exe [--spawn] [--spawn-headless] [--version] [--help]`
//!
//! env：
//! - `RIMCROW_MCP_ENDPOINT` 覆盖 endpoint 文件路径
//! - `RIMCROW_SPAWN_CMD` 覆盖 `--spawn` 的拉起命令（开发态/测试用）
//! - `RIMCROW_MCP_LOG` 日志级别（默认 `info`）
//!
//! **R7 stdout 纪律**：stdout 只有 JSON-RPC；tracing 全走 stderr；
//! EOF/断连 → 退出码 0；panic → 退出码 1 + stderr 一行，绝不弹窗。

mod bridge;
mod endpoint;
mod envelope;
mod snapshot;
mod spawn;
mod validate;

use std::io::Write;
use std::panic::AssertUnwindSafe;

use rmcp::service::QuitReason;
use rmcp::{ServiceExt, transport::stdio};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use crate::bridge::{Bridge, CRATE_VERSION};

/// bridge CLI 选项。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Options {
    /// 未检出 endpoint 时拉起 RimCrow（探活通过绝不二次拉起，D11）。
    pub spawn: bool,
    /// 拉起时附带 `--browser --no-open`。
    pub headless: bool,
}

const USAGE: &str = "\
rimcrow-mcp — RimCrow MCP bridge (stdio <-> Streamable HTTP)

USAGE:
    rimcrow-mcp.exe [OPTIONS]

OPTIONS:
    --spawn           endpoint 未检出时拉起 RimCrow GUI，有界等待 15s
                      （探活通过绝不二次拉起；就绪判据 = endpoint 写出生效）
    --spawn-headless  同 --spawn，拉起时附带 --browser --no-open
    -V, --version     打印版本三元组并退出
    -h, --help        打印本帮助并退出

ENV:
    RIMCROW_MCP_ENDPOINT  覆盖 endpoint 文件路径
                          （默认 %LOCALAPPDATA%\\RimCrow\\mcp-endpoint.json）
    RIMCROW_SPAWN_CMD     覆盖 --spawn 的拉起命令，例如
                          \"python main.py --browser --no-open\"
    RIMCROW_MCP_LOG       日志级别（默认 info；日志只写 stderr）
";

/// 解析后的命令。
#[derive(Debug)]
enum Cli {
    Run(Options),
    Help,
    Version,
}

/// 参数解析（不引第三方 CLI 库：单 exe、参数极少）。
fn parse_args(args: &[String]) -> Result<Cli, String> {
    let mut opts = Options::default();
    for arg in args {
        match arg.as_str() {
            "--spawn" => opts.spawn = true,
            "--spawn-headless" => {
                opts.spawn = true;
                opts.headless = true;
            }
            "-h" | "--help" => return Ok(Cli::Help),
            "-V" | "--version" => return Ok(Cli::Version),
            other => return Err(format!("未知参数：{other}")),
        }
    }
    Ok(Cli::Run(opts))
}

/// 版本三元组（R8）：crate 版本 + 快照 rimcrow_version + captured_at。
pub fn version_line() -> String {
    let snap = snapshot::load();
    format!(
        "rimcrow-mcp {CRATE_VERSION} (snapshot RimCrow {} @ {})",
        snap.rimcrow_version, snap.captured_at
    )
}

/// R7：panic 收敛为 stderr 一行（不弹任何窗口/对话框）。
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let payload = info.payload();
        let msg = payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".to_string());
        let loc = info
            .location()
            .map(|l| format!(" ({l})"))
            .unwrap_or_default();
        eprintln!("rimcrow-mcp: panic: {msg}{loc}");
    }));
}

/// 日志 → stderr（绝不碰 stdout）；级别由 `RIMCROW_MCP_LOG` 控制，默认 info。
fn init_logging() {
    let directive = std::env::var("RIMCROW_MCP_LOG")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "info".to_string());
    let filter = EnvFilter::try_new(&directive).unwrap_or_else(|_| EnvFilter::new("info"));
    let writer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_ansi(false);
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(writer)
        .try_init();
}

/// 主循环：stdio serve → 等待 EOF/断连 → 退出码。
fn run(opts: Options) -> i32 {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("rimcrow-mcp: 无法创建 tokio runtime：{e}");
            return 1;
        }
    };

    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
        rt.block_on(async move {
            let bridge = Bridge::new(opts);
            tracing::info!(
                "{} | endpoint={} | spawn={}",
                version_line(),
                bridge.endpoint_path().display(),
                opts.spawn
            );
            let service = match bridge.serve(stdio()).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("stdio 服务启动失败：{e}");
                    return 1;
                }
            };
            match service.waiting().await {
                Ok(QuitReason::Closed) => {
                    tracing::info!("stdin 关闭，干净退出（0）");
                    0
                }
                Ok(QuitReason::Cancelled) => {
                    tracing::info!("会话被取消，退出（0）");
                    0
                }
                Ok(other) => {
                    tracing::error!("服务任务异常终止：{other:?}");
                    1
                }
                Err(e) => {
                    tracing::error!("服务任务 join 失败：{e}");
                    1
                }
            }
        })
    }));

    match outcome {
        Ok(code) => code,
        Err(_) => 1, // panic 已由 hook 打过一行
    }
}

fn main() {
    install_panic_hook();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = match parse_args(&args) {
        Ok(cli) => cli,
        Err(msg) => {
            eprintln!("rimcrow-mcp: {msg}");
            eprint!("{USAGE}");
            let _ = std::io::stderr().flush();
            std::process::exit(2);
        }
    };

    let opts = match cli {
        Cli::Help => {
            print!("{USAGE}");
            let _ = std::io::stdout().flush();
            std::process::exit(0);
        }
        Cli::Version => {
            println!("{}", version_line());
            let _ = std::io::stdout().flush();
            std::process::exit(0);
        }
        Cli::Run(opts) => opts,
    };

    init_logging();
    let code = run(opts);

    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_no_args_as_defaults() {
        let cli = parse_args(&[]).unwrap();
        match cli {
            Cli::Run(o) => {
                assert!(!o.spawn);
                assert!(!o.headless);
            }
            _ => panic!("expected run"),
        }
    }

    #[test]
    fn parses_spawn_and_headless_implies_spawn() {
        match parse_args(&["--spawn".to_string()]).unwrap() {
            Cli::Run(o) => {
                assert!(o.spawn);
                assert!(!o.headless);
            }
            _ => panic!(),
        }
        match parse_args(&["--spawn-headless".to_string()]).unwrap() {
            Cli::Run(o) => {
                assert!(o.spawn);
                assert!(o.headless);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn parses_help_and_version() {
        assert!(matches!(
            parse_args(&["--help".to_string()]).unwrap(),
            Cli::Help
        ));
        assert!(matches!(parse_args(&["-h".to_string()]).unwrap(), Cli::Help));
        assert!(matches!(
            parse_args(&["--version".to_string()]).unwrap(),
            Cli::Version
        ));
        assert!(matches!(
            parse_args(&["-V".to_string()]).unwrap(),
            Cli::Version
        ));
    }

    #[test]
    fn rejects_unknown_arg() {
        let err = parse_args(&["--nope".to_string()]).unwrap_err();
        assert!(err.contains("--nope"));
    }

    #[test]
    fn version_line_reports_triple() {
        let line = version_line();
        assert!(line.starts_with("rimcrow-mcp "), "{line}");
        assert!(line.contains(CRATE_VERSION), "{line}");
        assert!(line.contains("snapshot RimCrow"), "{line}");
        let snap = snapshot::load();
        assert!(line.contains(&snap.rimcrow_version), "{line}");
        assert!(line.contains(&snap.captured_at), "{line}");
    }
}
