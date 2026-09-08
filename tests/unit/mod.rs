use crate::{cli, json, path_policy, process, protocol, stdio, task, tool};

use std::{
    fs::{self, OpenOptions},
    io::Write as _,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_path(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after Unix epoch")
        .as_nanos();
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "shell-is-all-you-need-{label}-{}-{nanos}-{counter}",
        std::process::id()
    ))
}

fn meta() -> &'static str {
    r#""_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}"#
}

fn task_meta() -> &'static str {
    r#""_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{"extensions":{"io.modelcontextprotocol/tasks":{}}}}"#
}

fn echo_tool(schema: Option<String>) -> tool::Tool {
    tool::Tool::new(tool::ToolConfig {
        name: "echo".into(),
        description: None,
        input_schema: schema,
        invocation: vec!["echo".into(), "{value}".into()],
        path_fields: vec![],
        roots: vec![],
        denied_paths: vec![],
    })
    .unwrap()
}

fn handle(request: &str, selected: &tool::Tool, tasks_enabled: bool) -> protocol::Action {
    let config = tool::ServerConfig::new(vec![tool::ToolDefinition {
        tool: selected.clone(),
        limits: process::Limits {
            timeout_ms: None,
            output_limit: process::Limits::DEFAULT_OUTPUT_LIMIT,
            max_concurrency: process::Limits::DEFAULT_MAX_CONCURRENCY,
        },
        rate_limit: tool::InvocationRateLimit {
            count: tool::InvocationRateLimit::DEFAULT_COUNT,
            window_ms: tool::InvocationRateLimit::DEFAULT_WINDOW_MS,
        },
        tasks: task::Config {
            store_dir: tasks_enabled.then(|| "unused-test-store".into()),
            ttl_ms: None,
            poll_interval_ms: None,
        },
    }]);
    protocol::handle(request, &config)
}

#[test]
fn json_parser_is_strict() {
    assert!(json::validate(r#"{"a":[1,true,null,"x"]}"#));
    assert!(!json::validate(r#"{"a":1,"a":2}"#));
    assert!(!json::validate("01"));
    assert!(json::number("-1.5e2"));
    assert!(json::integer("1.0"));
    assert!(json::integer("1.2e1"));
    assert!(!json::integer("1.2"));
    assert!(!json::integer("1e-3"));
    assert!(json::integer("0e-999999999999999999999999"));
    assert_eq!(json::boolean("true"), Some(true));
}

#[test]
fn cli_task_options_parse() {
    let action = cli::parse_args(vec![
        "--tool".into(),
        "--name".into(),
        "subagent".into(),
        "--task-store-dir".into(),
        "./tasks".into(),
        "--task-ttl-ms".into(),
        "60000".into(),
        "--task-poll-interval-ms".into(),
        "1000".into(),
        "--exec".into(),
        "mock-task".into(),
        "{task}".into(),
    ])
    .unwrap();
    let cli::Action::Run(config) = action else {
        panic!("expected run action")
    };
    let tasks = &config.tools[0].tasks;
    assert_eq!(tasks.store_dir.as_deref(), Some("./tasks"));
    assert_eq!(tasks.ttl_ms, Some(60_000));
    assert_eq!(tasks.poll_interval_ms, Some(1_000));
}

#[test]
fn cli_task_policy_requires_store_dir() {
    let error = cli::parse_args(vec![
        "--tool".into(),
        "--name".into(),
        "subagent".into(),
        "--task-ttl-ms".into(),
        "60000".into(),
        "--exec".into(),
        "mock-task".into(),
        "{task}".into(),
    ])
    .err()
    .unwrap();
    assert!(error.contains("require --task-store-dir"));
}

#[test]
fn cli_rate_limit_options_are_per_tool_and_require_a_complete_pair() {
    let cli::Action::Run(config) = cli::parse_args(
        [
            "--tool",
            "--name",
            "limited",
            "--process-rate-limit-count",
            "3",
            "--process-rate-limit-window-ms",
            "25",
            "--exec",
            "echo",
            "--tool",
            "--name",
            "defaulted",
            "--exec",
            "echo",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
    )
    .unwrap() else {
        panic!("expected run action")
    };
    assert_eq!(config.tools[0].rate_limit.count, 3);
    assert_eq!(config.tools[0].rate_limit.window_ms, 25);
    assert_eq!(
        config.tools[1].rate_limit.count,
        tool::InvocationRateLimit::DEFAULT_COUNT
    );
    assert_eq!(
        config.tools[1].rate_limit.window_ms,
        tool::InvocationRateLimit::DEFAULT_WINDOW_MS
    );

    for options in [
        vec!["--process-rate-limit-count", "1"],
        vec!["--process-rate-limit-window-ms", "1"],
        vec![
            "--process-rate-limit-count",
            "0",
            "--process-rate-limit-window-ms",
            "1",
        ],
        vec![
            "--process-rate-limit-count",
            "1",
            "--process-rate-limit-window-ms",
            "0",
        ],
        vec![
            "--process-rate-limit-count",
            "1",
            "--process-rate-limit-count",
            "2",
            "--process-rate-limit-window-ms",
            "1",
        ],
        vec![
            "--process-rate-limit-count",
            "1",
            "--process-rate-limit-window-ms",
            "1",
            "--process-rate-limit-window-ms",
            "2",
        ],
    ] {
        let mut args = vec!["--tool", "--name", "x"];
        args.extend(options);
        args.extend(["--exec", "echo"]);
        assert!(cli::parse_args(args.into_iter().map(str::to_owned).collect()).is_err());
    }
}

#[test]
fn fixed_window_rate_limiter_recovers_without_unbounded_history() {
    let mut limiter = stdio::FixedWindowRateLimiter::new(2, 10);
    assert!(limiter.admit().is_ok());
    assert!(limiter.admit().is_ok());
    assert!(limiter.admit().is_err());
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert!(limiter.admit().is_ok());
}

#[test]
fn cli_names_are_consistent() {
    let action = cli::parse_args(vec![
        "--tool".into(),
        "--name".into(),
        "read".into(),
        "--fs-path-field".into(),
        "path".into(),
        "--fs-root".into(),
        ".".into(),
        "--exec".into(),
        "cat".into(),
        "{path}".into(),
    ])
    .unwrap();
    assert!(matches!(action, cli::Action::Run { .. }));
}

#[test]
fn cli_parses_multiple_independent_tools_in_order() {
    let cli::Action::Run(config) = cli::parse_args(
        [
            "--tool",
            "--name",
            "one",
            "--exec",
            "printf",
            "{first}",
            "--tool",
            "--name",
            "two",
            "--input-schema",
            r#"{"type":"object","properties":{},"additionalProperties":false}"#,
            "--process-timeout-ms",
            "7",
            "--exec",
            "printf",
            "--help",
            "--",
            "-x",
            "--tool",
            "--name",
            "three",
            "--exec",
            "printf",
            "{third}",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
    )
    .unwrap() else {
        panic!("expected run action")
    };
    assert_eq!(config.tools.len(), 3);
    assert_eq!(config.tools[0].tool.name, "one");
    assert_eq!(config.tools[1].tool.name, "two");
    assert_eq!(config.tools[2].tool.name, "three");
    assert_eq!(config.tools[1].limits.timeout_ms, Some(7));
    assert_eq!(
        config.tools[1].tool.bind("{}").unwrap().args,
        ["--help", "--", "-x"]
    );
    assert!(config.tools[0].tool.bind(r#"{"first":"a"}"#).is_ok());
    assert!(config.tools[0].tool.bind(r#"{"third":"a"}"#).is_err());
    assert!(config.tools[2].tool.bind(r#"{"third":"a"}"#).is_ok());
}

#[test]
fn cli_rejects_missing_blocks_fields_duplicates_and_legacy_flags() {
    for args in [
        vec![],
        vec!["--tool", "--exec", "echo"],
        vec!["--tool", "--name", "x"],
        vec!["--tool", "--name", "x", "--exec"],
        vec![
            "--tool", "--name", "--tool", "--name", "next", "--exec", "echo",
        ],
        vec![
            "--tool", "--name", "x", "--exec", "echo", "--tool", "--name", "x", "--exec", "echo",
        ],
        vec!["--tool-name", "x"],
        vec!["--tool-description", "x"],
        vec!["--exec", "echo"],
    ] {
        assert!(cli::parse_args(args.into_iter().map(str::to_owned).collect()).is_err());
    }
}

#[test]
fn explicit_empty_schema_is_valid_for_no_argument_tool() {
    let tool = new_tool_with(
        Some(r#"{"type":"object","properties":{},"additionalProperties":false}"#),
        &["printf"],
        &[],
    )
    .unwrap();
    assert!(tool.bind("{}").is_ok());
    assert!(tool.bind(r#"{"other":"x"}"#).is_err());
}

#[test]
fn inferred_schema_is_closed() {
    let tool = echo_tool(None);
    assert!(tool.input_schema.contains("\"additionalProperties\":false"));
    assert!(tool.bind(r#"{"value":"ok"}"#).is_ok());
    assert!(tool.bind(r#"{"value":"ok","extra":"no"}"#).is_err());
}

#[test]
fn literal_braces_render_by_doubling() {
    let tool = tool::Tool::new(tool::ToolConfig {
        name: "test".into(),
        description: None,
        input_schema: None,
        invocation: vec!["echo".into(), "{{literal}} {value}".into()],
        path_fields: vec![],
        roots: vec![],
        denied_paths: vec![],
    })
    .unwrap();
    let invocation = tool.bind(r#"{"value":"ok"}"#).unwrap();
    assert_eq!(invocation.args, vec!["{literal} ok"]);
}

#[test]
fn schema_type_is_enforced() {
    let schema = r#"{"type":"object","properties":{"value":{"type":"boolean"}},"required":["value"],"additionalProperties":false}"#.to_string();
    let tool = echo_tool(Some(schema));
    assert!(tool.bind(r#"{"value":true}"#).is_ok());
    assert!(tool.bind(r#"{"value":"true"}"#).is_err());
}

#[test]
fn unsupported_schema_assertions_fail_closed() {
    let schema = r#"{"type":"object","properties":{"value":{"type":"string","pattern":"x"}},"required":["value"],"additionalProperties":false}"#.to_string();
    assert!(
        tool::Tool::new(tool::ToolConfig {
            name: "echo".into(),
            description: None,
            input_schema: Some(schema),
            invocation: vec!["echo".into(), "{value}".into()],
            path_fields: vec![],
            roots: vec![],
            denied_paths: vec![],
        })
        .is_err()
    );
}

#[test]
fn filesystem_policy_allows_root_and_denies_subtree() {
    let root = temp_path("path-policy");
    let denied = root.join("secret");
    fs::create_dir_all(&denied).unwrap();
    fs::write(root.join("ok.txt"), "ok").unwrap();
    fs::write(denied.join("no.txt"), "no").unwrap();

    let tool = tool::Tool::new(tool::ToolConfig {
        name: "read".into(),
        description: None,
        input_schema: None,
        invocation: vec!["cat".into(), "{path}".into()],
        path_fields: vec!["path".into()],
        roots: vec![root.to_string_lossy().into_owned()],
        denied_paths: vec![denied.to_string_lossy().into_owned()],
    })
    .unwrap();

    let ok_path = root.join("ok.txt").to_string_lossy().into_owned();
    let denied_path = denied.join("no.txt").to_string_lossy().into_owned();
    let ok = format!(r#"{{"path":{}}}"#, json::quote(&ok_path));
    let no = format!(r#"{{"path":{}}}"#, json::quote(&denied_path));
    assert!(tool.bind(&ok).is_ok());
    assert!(tool.bind(&no).is_err());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn filesystem_policy_handles_nonexistent_targets() {
    let root = temp_path("path-policy-nonexistent");
    fs::create_dir_all(&root).unwrap();
    let tool = tool::Tool::new(tool::ToolConfig {
        name: "write".into(),
        description: None,
        input_schema: None,
        invocation: vec!["touch".into(), "{path}".into()],
        path_fields: vec!["path".into()],
        roots: vec![root.to_string_lossy().into_owned()],
        denied_paths: vec![],
    })
    .unwrap();

    let inside = root.join("new/child.txt").to_string_lossy().into_owned();
    let outside = root
        .parent()
        .unwrap()
        .join("outside.txt")
        .to_string_lossy()
        .into_owned();
    assert!(
        tool.bind(&format!(r#"{{"path":{}}}"#, json::quote(&inside)))
            .is_ok()
    );
    assert!(
        tool.bind(&format!(r#"{{"path":{}}}"#, json::quote(&outside)))
            .is_err()
    );
    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn filesystem_policy_rejects_symlink_escape() {
    use std::os::unix::fs::symlink;

    let root = temp_path("path-policy-symlink-root");
    let outside = temp_path("path-policy-symlink-outside");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret.txt"), "secret").unwrap();
    symlink(&outside, root.join("link")).unwrap();

    let tool = tool::Tool::new(tool::ToolConfig {
        name: "read".into(),
        description: None,
        input_schema: None,
        invocation: vec!["cat".into(), "{path}".into()],
        path_fields: vec!["path".into()],
        roots: vec![root.to_string_lossy().into_owned()],
        denied_paths: vec![],
    })
    .unwrap();
    let escaped = root.join("link/secret.txt").to_string_lossy().into_owned();
    assert!(
        tool.bind(&format!(r#"{{"path":{}}}"#, json::quote(&escaped)))
            .is_err()
    );

    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(outside).unwrap();
}

#[test]
fn protocol_rejects_non_string_non_number_request_ids() {
    let tool = echo_tool(None);
    for raw_id in ["null", "true", "{}", "[]"] {
        let request = format!(
            r#"{{"jsonrpc":"2.0","id":{raw_id},"method":"server/discover","params":{{{}}}}}"#,
            meta()
        );
        assert!(matches!(
            handle(&request, &tool, false),
            protocol::Action::Respond(_)
        ));
    }
}

#[test]
fn protocol_accepts_string_and_numeric_request_ids() {
    let tool = echo_tool(None);
    for raw_id in ["1", "1.5", "-2e3", r#""request-1""#] {
        let request = format!(
            r#"{{"jsonrpc":"2.0","id":{raw_id},"method":"server/discover","params":{{{}}}}}"#,
            meta()
        );
        let protocol::Action::Respond(response) = handle(&request, &tool, false) else {
            panic!("expected response")
        };
        assert!(response.contains(&format!("\"id\":{raw_id}")));
    }
}

#[test]
fn cancellation_notification_requires_current_envelope() {
    let tool = echo_tool(None);
    let current = r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":"x","_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}"#;
    assert!(matches!(
        handle(current, &tool, false),
        protocol::Action::Cancel(protocol::RequestId::String(id)) if id == "x"
    ));
    let legacy =
        r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":"x"}}"#;
    assert!(matches!(
        handle(legacy, &tool, false),
        protocol::Action::Ignore
    ));
}

#[test]
fn unsupported_version_has_current_error_shape() {
    let tool = echo_tool(None);
    let request = r#"{"jsonrpc":"2.0","id":1,"method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"old","io.modelcontextprotocol/clientCapabilities":{}}}}"#;
    let protocol::Action::Respond(response) = handle(request, &tool, false) else {
        panic!("expected response")
    };
    assert!(response.contains("\"code\":-32022"));
    assert!(response.contains("\"supported\":[\"2026-07-28\"]"));
    assert!(response.contains("\"requested\":\"old\""));
}

#[test]
fn output_schema_is_advertised() {
    let tool = echo_tool(None);
    let request = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{{{}}}}}"#,
        meta()
    );
    let protocol::Action::Respond(response) = handle(&request, &tool, false) else {
        panic!("expected response")
    };
    assert!(response.contains("\"outputSchema\""));
    for field in [
        "exitCode",
        "stdout",
        "stderr",
        "timedOut",
        "stdoutTruncated",
        "stderrTruncated",
    ] {
        assert!(response.contains(field));
    }
}

#[test]
fn structured_result_keeps_raw_text_and_adds_serialized_json_fallback() {
    let result = protocol::tool_output_result(process::Output {
        exit_code: Some(0),
        stdout: "useful output".into(),
        stderr: String::new(),
        timed_out: false,
        stdout_truncated: false,
        stderr_truncated: false,
    });
    let content = json::object_get(&result, "content")
        .and_then(json::array_values)
        .unwrap();
    assert_eq!(content.len(), 2);
    assert_eq!(
        json::object_get(content[0], "text").and_then(json::string),
        Some("useful output".into())
    );
    let fallback = json::object_get(content[1], "text")
        .and_then(json::string)
        .unwrap();
    assert!(json::validate(&fallback));
    assert_eq!(
        json::object_get(&fallback, "stdout").and_then(json::string),
        Some("useful output".into())
    );
    assert_eq!(
        json::object_get(&result, "structuredContent").unwrap(),
        fallback
    );
}

#[test]
fn task_capability_selects_task_execution() {
    let tool = echo_tool(None);
    let request = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"echo","arguments":{{"value":"ok"}},{}}}}}"#,
        task_meta()
    );
    assert!(matches!(
        handle(&request, &tool, true),
        protocol::Action::Call { task: true, .. }
    ));
}

#[test]
fn task_methods_require_negotiated_capability() {
    let tool = echo_tool(None);
    let request = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tasks/get","params":{{"taskId":"x",{}}}}}"#,
        meta()
    );
    let protocol::Action::Respond(response) = handle(&request, &tool, true) else {
        panic!("expected response")
    };
    assert!(response.contains("\"code\":-32021"));
}

#[test]
fn extension_settings_must_be_objects() {
    let tool = echo_tool(None);
    let request = r#"{"jsonrpc":"2.0","id":1,"method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{"extensions":{"example.invalid":true}}}}}"#;
    let protocol::Action::Respond(response) = handle(request, &tool, true) else {
        panic!("expected response")
    };
    assert!(response.contains("\"code\":-32602"));
}

#[test]
fn tasks_update_requires_object_responses() {
    let tool = echo_tool(None);
    let request = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tasks/update","params":{{"taskId":"x","inputResponses":{{"answer":1}},{}}}}}"#,
        task_meta()
    );
    let protocol::Action::Respond(response) = handle(&request, &tool, true) else {
        panic!("expected response")
    };
    assert!(response.contains("\"code\":-32602"));
}

#[test]
fn discover_advertises_tasks_only_when_enabled() {
    let tool = echo_tool(None);
    let request = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"server/discover","params":{{{}}}}}"#,
        meta()
    );
    let protocol::Action::Respond(enabled) = handle(&request, &tool, true) else {
        panic!("expected response")
    };
    let protocol::Action::Respond(disabled) = handle(&request, &tool, false) else {
        panic!("expected response")
    };
    assert!(enabled.contains("io.modelcontextprotocol/tasks"));
    assert!(!disabled.contains("io.modelcontextprotocol/tasks"));
}

fn task_config(directory: &std::path::Path) -> task::Config {
    task::Config {
        store_dir: Some(directory.to_string_lossy().into_owned()),
        ttl_ms: None,
        poll_interval_ms: None,
    }
}

#[test]
fn task_ids_are_opaque_and_unique() {
    let directory = temp_path("task-id");
    let mut store = task::Store::new(task_config(&directory)).unwrap();
    let first = store.create().unwrap().0;
    let second = store.create().unwrap().0;
    assert_eq!(first.len(), 37);
    assert!(first.starts_with("task-"));
    assert!(first[5..].bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_ne!(first, second);
    drop(store);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn completed_task_survives_reopen() {
    let directory = temp_path("task-complete");
    let task_id;
    {
        let mut store = task::Store::new(task::Config {
            store_dir: Some(directory.to_string_lossy().into_owned()),
            ttl_ms: None,
            poll_interval_ms: Some(1000),
        })
        .unwrap();
        task_id = store.create().unwrap().0;
        store
            .complete(&task_id, r#"{"resultType":"complete"}"#.into())
            .unwrap();
    }
    {
        let mut store = task::Store::new(task_config(&directory)).unwrap();
        let state = store.get(&task_id).unwrap().unwrap();
        assert!(state.contains("\"status\":\"completed\""));
        assert!(state.contains("\"pollIntervalMs\":1000"));
        assert!(state.contains("\"result\":"));
    }
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn truncated_task_snapshot_is_repaired_before_recovery_append() {
    let directory = temp_path("task-truncated");
    let task_id;
    {
        let mut store = task::Store::new(task_config(&directory)).unwrap();
        task_id = store.create().unwrap().0;
        let path = directory.join(format!("{task_id}.jsonl"));
        OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(b"{\"partial\"")
            .unwrap();
    }
    for _ in 0..2 {
        let mut store = task::Store::new(task_config(&directory)).unwrap();
        let state = store.get(&task_id).unwrap().unwrap();
        assert!(state.contains("\"status\":\"failed\""));
        drop(store);
    }
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn pending_task_cancellation_survives_reopen() {
    let directory = temp_path("task-cancel");
    let task_id;
    {
        let mut store = task::Store::new(task_config(&directory)).unwrap();
        task_id = store.create().unwrap().0;
        assert!(store.cancel(&task_id).unwrap());
    }
    {
        let mut store = task::Store::new(task_config(&directory)).unwrap();
        let state = store.get(&task_id).unwrap().unwrap();
        assert!(state.contains("\"status\":\"cancelled\""));
    }
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn interrupted_task_becomes_failed_after_reopen() {
    let directory = temp_path("task-interrupted");
    let task_id;
    {
        let mut store = task::Store::new(task_config(&directory)).unwrap();
        task_id = store.create().unwrap().0;
    }
    {
        let mut store = task::Store::new(task_config(&directory)).unwrap();
        let state = store.get(&task_id).unwrap().unwrap();
        assert!(state.contains("\"status\":\"failed\""));
        assert!(state.contains("\"error\":"));
    }
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn task_ttl_removes_expired_record() {
    let directory = temp_path("task-ttl");
    let mut store = task::Store::new(task::Config {
        store_dir: Some(directory.to_string_lossy().into_owned()),
        ttl_ms: Some(1),
        poll_interval_ms: None,
    })
    .unwrap();
    let task_id = store.create().unwrap().0;
    std::thread::sleep(std::time::Duration::from_millis(5));
    let expired = store.prune_expired().unwrap();
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0], task_id);
    assert!(store.get(&task_id).unwrap().is_none());
    assert!(!directory.join(format!("{task_id}.jsonl")).exists());
    drop(store);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn task_router_prunes_owners_independently_and_bounds_routing_state() {
    let directory = temp_path("router-ttl");
    let first = directory.join("first");
    let second = directory.join("second");
    let args = vec![
        "--tool".into(),
        "--name".into(),
        "first".into(),
        "--task-store-dir".into(),
        first.to_string_lossy().into_owned(),
        "--task-ttl-ms".into(),
        "1".into(),
        "--exec".into(),
        "echo".into(),
        "--tool".into(),
        "--name".into(),
        "second".into(),
        "--task-store-dir".into(),
        second.to_string_lossy().into_owned(),
        "--task-ttl-ms".into(),
        "1000".into(),
        "--exec".into(),
        "echo".into(),
    ];
    let cli::Action::Run(config) = cli::parse_args(args).unwrap() else {
        panic!("expected run action")
    };
    let mut router = stdio::TaskRouter::new(&config).unwrap();
    let second_id = router.create(1).unwrap().0;
    let stale_id = router.create(1).unwrap().0;
    router.make_owner_stale_for_test(&stale_id).unwrap();
    assert!(router.get(&stale_id).unwrap().is_none());
    assert_eq!(router.owner_count(), 1);
    for _ in 0..5 {
        let first_id = router.create(0).unwrap().0;
        std::thread::sleep(std::time::Duration::from_millis(3));
        assert!(router.get(&first_id).unwrap().is_none());
        assert_eq!(router.owner_count(), 1);
    }
    assert!(router.get(&second_id).unwrap().is_some());
    drop(router);

    let restarted = stdio::TaskRouter::new(&config).unwrap();
    assert_eq!(restarted.owner_count(), 1);
    drop(restarted);
    fs::remove_dir_all(directory).unwrap();
}

#[cfg(any(unix, windows))]
#[test]
fn task_store_is_exclusively_locked() {
    let directory = temp_path("task-lock");
    let first = task::Store::new(task_config(&directory)).unwrap();
    let error = task::Store::new(task_config(&directory)).err().unwrap();
    assert!(error.contains("exclusively lock"));
    drop(first);
    task::Store::new(task_config(&directory)).unwrap();
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn json_helpers_cover_escapes_and_malformed_values() {
    assert_eq!(
        json::string(r#""\"\\\/\b\f\n\r\t\u0041\ud83d\ude00""#).as_deref(),
        Some("\"\\/\u{8}\u{c}\n\r\tA😀")
    );
    for invalid in [
        r#""\x""#,
        r#""\ud800""#,
        r#""\ud800\u0041""#,
        r#""\udc00""#,
        "\"unterminated",
        "\"\u{1}\"",
        "1.",
        "1e",
        r#"{"a" 1}"#,
        r#"{"a":1,}"#,
        "[1,]",
        "[1 2]",
        "truth",
    ] {
        assert!(
            !json::validate(invalid),
            "unexpected valid JSON: {invalid:?}"
        );
    }

    let escaped = json::escape("\"\\\n\r\t\u{8}\u{c}\u{1}");
    assert_eq!(escaped, "\\\"\\\\\\n\\r\\t\\b\\f\\u0001");
    assert_eq!(json::object_entries("{}"), Some(vec![]));
    assert_eq!(json::array_values("[]"), Some(vec![]));
    assert!(json::object_entries("[]").is_none());
    assert!(json::object_entries(r#"{"a":1,"a":2}"#).is_none());
    assert!(json::object_entries(r#"{"a"}"#).is_none());
    assert!(json::array_values("{}").is_none());
    assert!(json::array_values("[1,]").is_none());
    assert_eq!(json::boolean("false"), Some(false));
    assert_eq!(json::boolean("null"), None);
    assert!(!json::integer("not-a-number"));
    assert!(json::integer("100e-2"));
    assert!(!json::integer("101e-2"));
    assert!(json::integer("1e999999999999999999999999"));
    assert!(!json::integer("1e-999999999999999999999999"));
    assert!(json::integer("0e-999999999999999999999999"));
    assert!(json::integer("1e+2"));
    assert!(json::object_entries(r#"{"a":1 "b":2}"#).is_none());
    assert!(json::array_values("[1 2]").is_none());

    let deep = format!("{}0{}", "[".repeat(98), "]".repeat(98));
    assert!(!json::validate(&deep));
}

#[test]
fn path_policy_defaults_and_rejects_invalid_roots() {
    let policy = path_policy::PathPolicy::new(vec![], vec![]).unwrap();
    assert!(policy.check("path", ".").is_ok());
    assert!(policy.check("path", "../../../../../../escape").is_err());

    let file = temp_path("not-a-root");
    fs::write(&file, "file").unwrap();
    assert!(
        path_policy::PathPolicy::new(vec![file.to_string_lossy().into_owned()], vec![]).is_err()
    );
    fs::remove_file(file).unwrap();
}

#[test]
fn cli_help_version_full_options_and_errors() {
    assert!(matches!(
        cli::parse_args(vec!["-h".into()]),
        Ok(cli::Action::Help)
    ));
    assert!(matches!(
        cli::parse_args(vec!["--version".into()]),
        Ok(cli::Action::Version)
    ));

    let schema_path = temp_path("schema.json");
    fs::write(
        &schema_path,
        r#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}"#,
    )
    .unwrap();
    let action = cli::parse_args(vec![
        "--tool".into(),
        "--name".into(),
        "all".into(),
        "--description".into(),
        "description".into(),
        "--input-schema".into(),
        format!("@{}", schema_path.display()),
        "--fs-path-field".into(),
        "path".into(),
        "--fs-root".into(),
        ".".into(),
        "--fs-deny-path".into(),
        schema_path.to_string_lossy().into_owned(),
        "--task-store-dir".into(),
        temp_path("tasks").to_string_lossy().into_owned(),
        "--task-ttl-ms".into(),
        "2".into(),
        "--task-poll-interval-ms".into(),
        "3".into(),
        "--process-timeout-ms".into(),
        "4".into(),
        "--process-output-limit-bytes".into(),
        "5".into(),
        "--process-max-concurrency".into(),
        "6".into(),
        "--exec".into(),
        "echo".into(),
        "{path}".into(),
    ])
    .unwrap();
    let cli::Action::Run(config) = action else {
        panic!("expected run action")
    };
    let limits = config.tools[0].limits;
    assert_eq!(limits.timeout_ms, Some(4));
    assert_eq!(limits.output_limit, 5);
    assert_eq!(limits.max_concurrency, 6);
    fs::remove_file(schema_path).unwrap();

    for args in [
        vec!["--unknown"],
        vec!["--tool"],
        vec!["--tool", "--name", "x", "--name", "y"],
        vec![
            "--tool",
            "--name",
            "x",
            "--input-schema",
            "{}",
            "--input-schema",
            "{}",
        ],
        vec!["--tool", "--name", "x", "--task-ttl-ms", "0"],
        vec!["--tool", "--name", "x", "--task-ttl-ms", "x"],
        vec![
            "--tool",
            "--name",
            "x",
            "--task-ttl-ms",
            "1",
            "--task-ttl-ms",
            "2",
        ],
        vec![
            "--tool",
            "--name",
            "x",
            "--task-poll-interval-ms",
            "1",
            "--task-poll-interval-ms",
            "2",
        ],
        vec![
            "--tool",
            "--name",
            "x",
            "--process-timeout-ms",
            "1",
            "--process-timeout-ms",
            "2",
        ],
        vec![
            "--tool",
            "--name",
            "x",
            "--process-output-limit-bytes",
            "1",
            "--process-output-limit-bytes",
            "2",
        ],
        vec![
            "--tool",
            "--name",
            "x",
            "--process-max-concurrency",
            "1",
            "--process-max-concurrency",
            "2",
        ],
        vec!["--exec", "echo"],
        vec!["--tool", "--name", "x"],
    ] {
        assert!(cli::parse_args(args.into_iter().map(str::to_owned).collect()).is_err());
    }
    assert!(
        cli::parse_args(vec![
            "--tool".into(),
            "--name".into(),
            "x".into(),
            "--input-schema".into(),
            "@does-not-exist".into(),
        ])
        .is_err()
    );
}

fn new_tool_with(
    schema: Option<&str>,
    invocation: &[&str],
    paths: &[&str],
) -> Result<tool::Tool, String> {
    tool::Tool::new(tool::ToolConfig {
        name: "test".into(),
        description: Some("test tool".into()),
        input_schema: schema.map(str::to_owned),
        invocation: invocation.iter().map(|value| (*value).to_owned()).collect(),
        path_fields: paths.iter().map(|value| (*value).to_owned()).collect(),
        roots: vec![],
        denied_paths: vec![],
    })
}

#[test]
fn tool_configuration_rejects_invalid_contracts() {
    assert!(
        tool::Tool::new(tool::ToolConfig {
            name: "bad name".into(),
            description: None,
            input_schema: None,
            invocation: vec!["echo".into()],
            path_fields: vec![],
            roots: vec![],
            denied_paths: vec![],
        })
        .is_err()
    );
    for invocation in [
        vec![],
        vec![""],
        vec!["echo", "{bad"],
        vec!["echo", "}"],
        vec!["echo", "{}"],
    ] {
        assert!(new_tool_with(None, &invocation, &[]).is_err());
    }

    for schema in [
        "not json",
        r#"{"type":"object","properties":{"x":{"type":"string"}},"required":["x"],"additionalProperties":false,"minimum":1}"#,
        r#"{"$schema":1,"type":"object","properties":{"x":{"type":"string"}},"required":["x"],"additionalProperties":false}"#,
        r#"{"$schema":"old","type":"object","properties":{"x":{"type":"string"}},"required":["x"],"additionalProperties":false}"#,
        r#"{"type":"array","properties":{"x":{"type":"string"}},"required":["x"],"additionalProperties":false}"#,
        r#"{"type":"object","properties":{"x":{"type":"string"}},"required":["x"],"additionalProperties":true}"#,
        r#"{"type":"object","required":["x"],"additionalProperties":false}"#,
        r#"{"type":"object","properties":[],"required":["x"],"additionalProperties":false}"#,
        r#"{"type":"object","properties":{},"required":["x"],"additionalProperties":false}"#,
        r#"{"type":"object","properties":{"x":{"type":"string"}},"required":"x","additionalProperties":false}"#,
        r#"{"type":"object","properties":{"x":{"type":"string"}},"required":[1],"additionalProperties":false}"#,
        r#"{"type":"object","properties":{"x":{"type":"string"}},"required":[],"additionalProperties":false}"#,
        r#"{"type":"object","properties":{"x":{"type":"string"}},"required":["x","x"],"additionalProperties":false}"#,
        r#"{"type":"object","properties":{"x":[]},"required":["x"],"additionalProperties":false}"#,
        r#"{"type":"object","properties":{"x":{"type":"array"}},"required":["x"],"additionalProperties":false}"#,
        r#"{"title":1,"type":"object","properties":{"x":{"type":"string"}},"required":["x"],"additionalProperties":false}"#,
    ] {
        assert!(
            new_tool_with(Some(schema), &["echo", "{x}"], &[]).is_err(),
            "accepted {schema}"
        );
    }

    assert!(new_tool_with(None, &["echo", "{x}"], &["x", "x"]).is_err());
    assert!(
        tool::Tool::new(tool::ToolConfig {
            name: "test".into(),
            description: None,
            input_schema: None,
            invocation: vec!["echo".into()],
            path_fields: vec![],
            roots: vec![".".into()],
            denied_paths: vec![],
        })
        .is_err()
    );
    assert!(new_tool_with(None, &["echo", "{x}"], &["missing"]).is_err());

    let number_schema = r#"{"type":"object","properties":{"x":{"type":"number"}},"required":["x"],"additionalProperties":false}"#;
    assert!(new_tool_with(Some(number_schema), &["echo", "{x}"], &["x"]).is_err());
}

#[test]
fn tool_binding_covers_all_scalar_types_and_errors() {
    let schema = r#"{"type":"object","properties":{"s":{"type":"string"},"n":{"type":"number"},"i":{"type":"integer"},"b":{"type":"boolean"}},"required":["s","n","i","b"],"additionalProperties":false}"#;
    let tool = new_tool_with(Some(schema), &["echo", "{s}", "{n}", "{i}", "{b}"], &[]).unwrap();
    let invocation = tool
        .bind(r#"{"s":"ok","n":1.5,"i":2.0,"b":false}"#)
        .unwrap();
    assert_eq!(invocation.args, ["ok", "1.5", "2.0", "false"]);
    assert!(tool.bind("[]").is_err());
    assert!(
        tool.bind(r#"{"s":"ok","n":1.5,"i":2.2,"b":false}"#)
            .is_err()
    );
    assert!(
        tool.bind(r#"{"s":"ok","n":1.5,"i":2,"extra":false}"#)
            .is_err()
    );

    let nul = echo_tool(None);
    assert!(nul.bind(r#"{"value":"\u0000"}"#).is_err());
}

fn protocol_response(request: &str, tasks_enabled: bool) -> String {
    match handle(request, &echo_tool(None), tasks_enabled) {
        protocol::Action::Respond(response) => response,
        _ => panic!("expected protocol response for {request}"),
    }
}

#[test]
fn protocol_validation_and_method_errors_are_covered() {
    for request in [
        "not json",
        "[]",
        r#"{"jsonrpc":"1.0","id":1,"method":"x","params":{}}"#,
        r#"{"jsonrpc":"2.0","id":1,"params":{}}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"x"}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"x","params":[]}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"x","params":{}}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"x","params":{"_meta":1}}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"x","params":{"_meta":{}}}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"x","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"x","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":1}}}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"x","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{"extensions":1}}}}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"x","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{},"io.modelcontextprotocol/clientInfo":{"name":"x"}}}}"#,
    ] {
        assert!(protocol_response(request, false).contains("\"error\""));
    }

    let requests = [
        format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{{"cursor":"x",{}}}}}"#,
            meta()
        ),
        format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{{}}}}}"#,
            meta()
        ),
        format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"other",{}}}}}"#,
            meta()
        ),
        format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"echo","arguments":[],{}}}}}"#,
            meta()
        ),
        format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"echo","arguments":{{}},{}}}}}"#,
            meta()
        ),
        format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"tasks/get","params":{{"taskId":"x",{}}}}}"#,
            task_meta()
        ),
        format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"tasks/get","params":{{"taskId":"",{}}}}}"#,
            task_meta()
        ),
        format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"tasks/update","params":{{"taskId":"x",{}}}}}"#,
            task_meta()
        ),
        format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"unknown","params":{{{}}}}}"#,
            meta()
        ),
    ];
    for request in requests {
        assert!(
            protocol_response(&request, false).contains("\"error\"")
                || request.contains("arguments\":{}")
        );
    }

    for method in ["tasks/get", "tasks/update", "tasks/cancel"] {
        let request = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"{method}","params":{{"taskId":"x","inputResponses":{{}},{}}}}}"#,
            task_meta()
        );
        let action = handle(&request, &echo_tool(None), true);
        assert!(matches!(
            action,
            protocol::Action::TaskGet { .. }
                | protocol::Action::TaskUpdate { .. }
                | protocol::Action::TaskCancel { .. }
        ));
    }

    for notification in [
        r#"{"jsonrpc":"2.0","method":"notifications/cancelled"}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"_meta":{}}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"old","io.modelcontextprotocol/clientCapabilities":{}}}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}"#,
    ] {
        assert!(matches!(
            handle(notification, &echo_tool(None), false),
            protocol::Action::Ignore
        ));
    }

    for method in ["tasks/get", "tasks/update", "tasks/cancel"] {
        let request = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"{method}","params":{{{}}}}}"#,
            task_meta()
        );
        assert!(protocol_response(&request, true).contains("taskId is required"));
    }
    let update = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tasks/update","params":{{"taskId":"x",{}}}}}"#,
        task_meta()
    );
    assert!(protocol_response(&update, true).contains("inputResponses must be an object"));

    let no_tasks = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tasks/cancel","params":{{"taskId":"x",{}}}}}"#,
        meta()
    );
    assert!(protocol_response(&no_tasks, true).contains("Missing required client capability"));
}

#[test]
fn process_and_stdio_helpers_cover_normal_and_cancelled_paths() {
    use std::sync::{Arc, atomic::AtomicBool};

    let cancelled = Arc::new(AtomicBool::new(true));
    assert!(matches!(
        process::run(
            process::Invocation {
                command: "unused".into(),
                args: vec![]
            },
            process::Limits {
                timeout_ms: None,
                output_limit: 10,
                max_concurrency: 1
            },
            cancelled,
        )
        .unwrap(),
        process::RunResult::Cancelled
    ));

    let mut empty = std::io::Cursor::new(Vec::<u8>::new());
    assert!(stdio::read_input_line(&mut empty).unwrap().is_none());
    let mut no_newline = std::io::Cursor::new(b"hello".to_vec());
    assert!(
        matches!(stdio::read_input_line(&mut no_newline).unwrap(), Some(stdio::InputLine::Message(value)) if value == "hello")
    );
    let mut crlf = std::io::Cursor::new(b"hello\r\n".to_vec());
    assert!(
        matches!(stdio::read_input_line(&mut crlf).unwrap(), Some(stdio::InputLine::Message(value)) if value == "hello")
    );
    let mut invalid = std::io::Cursor::new(vec![0xff, b'\n']);
    assert_eq!(
        stdio::read_input_line(&mut invalid).unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );

    let id = protocol::RequestId::Number("1".into());
    assert!(stdio::unknown_task(&id).contains("unknown task"));
    assert!(stdio::task_store_error(&id, "bad").contains("task store failure"));
}

#[test]
fn task_store_terminal_and_invalid_record_paths() {
    let mut disabled = task::Store::new(task::Config::default()).unwrap();
    assert!(!disabled.enabled());
    assert!(disabled.get("missing").unwrap().is_none());
    assert!(!disabled.cancel("missing").unwrap());
    assert!(disabled.complete("missing", "{}".into()).unwrap().is_none());
    disabled.cancelled("missing").unwrap();
    disabled.cancel_all().unwrap();

    let directory = temp_path("task-terminal");
    let mut store = task::Store::new(task_config(&directory)).unwrap();
    let (id, _, _) = store.create().unwrap();
    assert!(store.acknowledge_update(&id).unwrap());
    assert!(!store.acknowledge_update("missing").unwrap());
    store.cancelled(&id).unwrap();
    store.cancelled(&id).unwrap();
    assert!(store.cancel(&id).unwrap());
    let fields = store.complete(&id, "{}".into()).unwrap().unwrap();
    assert!(fields.contains("cancelled"));
    drop(store);
    fs::remove_dir_all(directory).unwrap();

    let file_path = temp_path("task-store-file");
    fs::write(&file_path, "not a directory").unwrap();
    assert!(task::Store::new(task_config(&file_path)).is_err());
    fs::remove_file(file_path).unwrap();

    let directory = temp_path("task-invalid-record");
    fs::create_dir_all(&directory).unwrap();
    let invalid_id = "task-00000000000000000000000000000000";
    for (index, snapshot) in [
        "not-json",
        r#"{"taskId":"bad"}"#,
        r#"{"taskId":"task-00000000000000000000000000000000","status":"invalid","statusMessage":null,"createdAt":"x","lastUpdatedAt":"x","createdUnixMs":1,"ttlMs":null,"pollIntervalMs":null,"cancelRequested":false,"result":null,"error":null}"#,
        r#"{"taskId":"task-00000000000000000000000000000000","status":"completed","statusMessage":null,"createdAt":"x","lastUpdatedAt":"x","createdUnixMs":1,"ttlMs":null,"pollIntervalMs":null,"cancelRequested":false,"result":null,"error":null}"#,
    ].iter().enumerate() {
        let path = directory.join(format!("{invalid_id}.jsonl"));
        fs::write(&path, format!("{snapshot}\n")).unwrap();
        let error = task::Store::new(task_config(&directory)).err().unwrap();
        assert!(error.contains("invalid task record"), "case {index}: {error}");
        fs::remove_file(path).unwrap();
    }
    fs::remove_dir_all(directory).unwrap();
}
