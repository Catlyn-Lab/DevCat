//! A local MCP server ends with what it started: when its chat lets go of it, and when the app
//! quits (`stop_all`). Its own test binary: `stop_all` ends every server in the process.
#![cfg(unix)]
use devcat_harness::mcp::{McpTool, Server, ServerConfig};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn python() -> Option<&'static str> {
    ["python3", "python"].into_iter().find(|p| std::process::Command::new(p).arg("--version").output().is_ok_and(|o| o.status.success()))
}

/// A server that starts a helper first (like `npx` starting node), writing the helper's pid.
fn config(py: &str, pid_file: &Path) -> ServerConfig {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp_server.py");
    let cmd = format!("sleep 300 & echo $! > '{}'; exec {py} '{}'", pid_file.display(), script.display());
    ServerConfig { name: "calc".into(), command: "/bin/sh".into(), args: vec!["-c".into(), cmd], source: "test".into(), ..Default::default() }
}

fn alive(pid: &str) -> bool {
    std::process::Command::new("kill").args(["-0", pid]).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success())
}

fn gone_soon(pid: &str) -> bool {
    let until = Instant::now() + Duration::from_secs(3);
    while alive(pid) && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(50));
    }
    !alive(pid)
}

fn temp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("devcat-mcp-stop-{}-{name}", std::process::id()))
}

#[tokio::test]
async fn a_server_ends_with_its_helpers() {
    let Some(py) = python() else {
        eprintln!("no python: skipped");
        return;
    };
    // dropped (its chat closed)
    let file = temp("drop");
    let server = Server::start(&config(py, &file)).await.expect("server starts");
    let tools = McpTool::all(&server);
    let helper = std::fs::read_to_string(&file).unwrap().trim().to_string();
    assert!(alive(&helper));
    drop(tools);
    drop(server);
    assert!(gone_soon(&helper), "the helper outlived its server");
    let _ = std::fs::remove_file(&file);

    // still held when the app quits
    let file = temp("quit");
    let server = Server::start(&config(py, &file)).await.expect("server starts");
    let helper = std::fs::read_to_string(&file).unwrap().trim().to_string();
    assert!(alive(&helper));
    devcat_harness::mcp::stop_all();
    assert!(gone_soon(&helper), "the helper outlived the app");
    drop(server);
    let _ = std::fs::remove_file(&file);
}
