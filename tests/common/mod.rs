#![allow(dead_code)]

#[path = "../../src/json.rs"]
pub mod json;

use std::{
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub const META: &str = r#""_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}"#;
pub const TASK_META: &str = r#""_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{"extensions":{"io.modelcontextprotocol/tasks":{}}}}"#;

pub fn binary() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_BIN_EXE_shell-is-all-you-need"));
    if path.is_absolute() {
        path
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path)
    }
}

pub fn temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after Unix epoch")
        .as_nanos();
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "shell-is-all-you-need-integration-{label}-{}-{nanos}-{counter}",
        std::process::id()
    ))
}

pub struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Server {
    pub fn spawn(args: &[String], cwd: Option<&Path>) -> Self {
        let mut command = Command::new(binary());
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let mut child = command.spawn().expect("spawn MCP server");
        let stdin = child.stdin.take().expect("server stdin");
        let stdout = BufReader::new(child.stdout.take().expect("server stdout"));
        Self {
            child,
            stdin: Some(stdin),
            stdout,
        }
    }

    pub fn send(&mut self, message: &str) -> String {
        self.send_only(message);
        self.read()
    }

    pub fn send_only(&mut self, message: &str) {
        let stdin = self.stdin.as_mut().expect("server stdin is open");
        writeln!(stdin, "{message}").expect("write MCP request");
        stdin.flush().expect("flush MCP request");
    }

    pub fn read(&mut self) -> String {
        let mut line = String::new();
        let count = self.stdout.read_line(&mut line).expect("read MCP response");
        assert_ne!(count, 0, "MCP server closed stdout unexpectedly");
        line.trim_end().to_owned()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.wait();
    }
}

pub fn task_id(response: &str) -> String {
    let result = json::object_get(response, "result").expect("response result");
    json::object_get(result, "taskId")
        .and_then(json::string)
        .expect("taskId")
}

pub fn task_status(response: &str) -> String {
    let result = json::object_get(response, "result").expect("response result");
    json::object_get(result, "status")
        .and_then(json::string)
        .expect("task status")
}
