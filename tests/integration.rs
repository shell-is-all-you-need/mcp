mod common;

use common::{META, Server, TASK_META, binary, task_id, task_status, temp_dir};
use std::{fs, thread, time::Duration};

fn version_tool_args(task_store: Option<&std::path::Path>) -> Vec<String> {
    let mut args = vec!["--tool".into(), "--name".into(), "probe".into()];
    if let Some(store) = task_store {
        args.extend([
            "--task-store-dir".into(),
            store.to_string_lossy().into_owned(),
            "--task-poll-interval-ms".into(),
            "10".into(),
        ]);
    }
    args.extend([
        "--exec".into(),
        binary().to_string_lossy().into_owned(),
        "--version".into(),
    ]);
    args
}

#[test]
fn help_and_version_commands_exit_successfully() {
    for argument in ["--help", "--version"] {
        let output = std::process::Command::new(binary())
            .arg(argument)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(!output.stdout.is_empty());
    }
}

#[test]
fn tools_list_and_call_match_output_schema() {
    let mut server = Server::spawn(&version_tool_args(None), None);
    let list = server.send(&format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{{{META}}}}}"#
    ));
    assert!(list.contains("\"outputSchema\""));
    assert!(list.contains("\"additionalProperties\":false"));

    let call = server.send(&format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"probe","arguments":{{}},{META}}}}}"#
    ));
    assert!(call.contains("\"resultType\":\"complete\""));
    assert!(call.contains("\"structuredContent\":"));
    assert!(call.contains("\"exitCode\":0"));
    assert!(call.contains("shell-is-all-you-need 0.1.1"));
    assert!(call.contains("\"isError\":false"));
}

#[test]
fn one_server_lists_and_dispatches_multiple_tools_in_order() {
    let args = vec![
        "--tool".into(),
        "--name".into(),
        "echo_a".into(),
        "--exec".into(),
        "printf".into(),
        "A:{value}".into(),
        "--tool".into(),
        "--name".into(),
        "echo_b".into(),
        "--exec".into(),
        "printf".into(),
        "B:{value}".into(),
    ];
    let mut server = Server::spawn(&args, None);
    let listed = server.send(&format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{{{META}}}}}"#
    ));
    assert!(listed.find("echo_a").unwrap() < listed.find("echo_b").unwrap());
    assert_eq!(listed.matches("\"outputSchema\"").count(), 2);

    for (id, name, expected) in [(2, "echo_a", "A:ok"), (3, "echo_b", "B:ok")] {
        let response = server.send(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{name}","arguments":{{"value":"ok"}},{META}}}}}"#
        ));
        assert!(response.contains(expected));
        assert!(!response.contains(if name == "echo_a" { "B:ok" } else { "A:ok" }));
        assert!(response.contains("\"isError\":false"));
    }
    let unknown = server.send(&format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{{"name":"missing","arguments":{{}},{META}}}}}"#
    ));
    assert!(unknown.contains("Unknown tool"));
}

#[cfg(unix)]
#[test]
fn separate_tools_have_separate_concurrency_limits() {
    let args = vec![
        "--tool".into(),
        "--name".into(),
        "slow_a".into(),
        "--exec".into(),
        "sh".into(),
        "-c".into(),
        "sleep 0.1; printf A".into(),
        "--tool".into(),
        "--name".into(),
        "slow_b".into(),
        "--exec".into(),
        "sh".into(),
        "-c".into(),
        "sleep 0.1; printf B".into(),
    ];
    let mut server = Server::spawn(&args, None);
    for (id, name) in [(1, "slow_a"), (2, "slow_b")] {
        server.send_only(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{name}","arguments":{{}},{META}}}}}"#
        ));
    }
    let first = server.read();
    let second = server.read();
    assert!(!first.contains("concurrency limit reached"));
    assert!(!second.contains("concurrency limit reached"));
    assert!(first.contains("\"id\":1") || second.contains("\"id\":1"));
    assert!(first.contains("\"id\":2") || second.contains("\"id\":2"));
}

#[cfg(unix)]
#[test]
fn rate_limit_counts_only_valid_selected_tool_admissions_and_recovers() {
    let directory = temp_dir("rate-limit");
    fs::create_dir_all(&directory).unwrap();
    let marker = directory.join("spawned");
    let args = vec![
        "--tool".into(),
        "--name".into(),
        "limited".into(),
        "--process-rate-limit-count".into(),
        "1".into(),
        "--process-rate-limit-window-ms".into(),
        "250".into(),
        "--exec".into(),
        "sh".into(),
        "-c".into(),
        "printf x >> \"$1\"; printf %s \"$2\"".into(),
        "rate-test".into(),
        marker.to_string_lossy().into_owned(),
        "{value}".into(),
    ];
    let mut server = Server::spawn(&args, Some(&directory));
    let malformed = server.send(&format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"limited","arguments":{{}},{META}}}}}"#
    ));
    assert!(malformed.contains("\"isError\":true"));
    let unknown = server.send(&format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"unknown","arguments":{{}},{META}}}}}"#
    ));
    assert!(unknown.contains("Unknown tool"));

    let admitted = server.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"limited","arguments":{{"value":"first"}},{META}}}}}"#
    ));
    assert!(admitted.contains("first"));
    let rejected = server.send(&format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{{"name":"limited","arguments":{{"value":"second"}},{META}}}}}"#
    ));
    assert!(rejected.contains("\"isError\":true"));
    assert!(rejected.contains("tool invocation rate limit reached"));
    assert_eq!(fs::read_to_string(&marker).unwrap(), "x");

    thread::sleep(Duration::from_millis(350));
    let renewed = server.send(&format!(
        r#"{{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{{"name":"limited","arguments":{{"value":"third"}},{META}}}}}"#
    ));
    assert!(renewed.contains("third"));
    assert_eq!(fs::read_to_string(&marker).unwrap(), "xx");
    drop(server);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn separate_tools_have_independent_rate_budgets() {
    let mut args = Vec::new();
    for (name, output) in [("a", "A"), ("b", "B")] {
        args.extend([
            "--tool".into(),
            "--name".into(),
            name.into(),
            "--process-rate-limit-count".into(),
            "1".into(),
            "--process-rate-limit-window-ms".into(),
            "10000".into(),
            "--exec".into(),
            "printf".into(),
            output.into(),
        ]);
    }
    let mut server = Server::spawn(&args, None);
    for (id, name, output) in [(1, "a", "A"), (2, "b", "B")] {
        let response = server.send(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{name}","arguments":{{}},{META}}}}}"#
        ));
        assert!(response.contains(output));
        assert!(!response.contains("rate limit reached"));
    }
    let rejected = server.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"a","arguments":{{}},{META}}}}}"#
    ));
    assert!(rejected.contains("rate limit reached"));
}

#[test]
fn rate_limited_task_call_creates_no_second_task_record() {
    let directory = temp_dir("task-rate-limit");
    fs::create_dir_all(&directory).unwrap();
    let store = directory.join("tasks");
    let args = vec![
        "--tool".into(),
        "--name".into(),
        "task".into(),
        "--task-store-dir".into(),
        store.to_string_lossy().into_owned(),
        "--process-rate-limit-count".into(),
        "1".into(),
        "--process-rate-limit-window-ms".into(),
        "10000".into(),
        "--exec".into(),
        binary().to_string_lossy().into_owned(),
        "--version".into(),
    ];
    let mut server = Server::spawn(&args, Some(&directory));
    let created = server.send(&format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"task","arguments":{{}},{TASK_META}}}}}"#
    ));
    assert!(created.contains("\"resultType\":\"task\""));
    let rejected = server.send(&format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"task","arguments":{{}},{TASK_META}}}}}"#
    ));
    assert!(rejected.contains("tool invocation rate limit reached"));
    let records = fs::read_dir(&store)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .count();
    assert_eq!(records, 1);
    drop(server);
    fs::remove_dir_all(directory).unwrap();
}

#[cfg(unix)]
#[test]
fn task_ttl_housekeeping_removes_record_and_routing_owner() {
    let directory = temp_dir("task-ttl-housekeeping");
    fs::create_dir_all(&directory).unwrap();
    let store = directory.join("tasks");
    let args = vec![
        "--tool".into(),
        "--name".into(),
        "expiring".into(),
        "--task-store-dir".into(),
        store.to_string_lossy().into_owned(),
        "--task-ttl-ms".into(),
        "10".into(),
        "--exec".into(),
        "sh".into(),
        "-c".into(),
        "sleep 10".into(),
    ];
    let mut server = Server::spawn(&args, Some(&directory));
    let created = server.send(&format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"expiring","arguments":{{}},{TASK_META}}}}}"#
    ));
    let expired_id = task_id(&created);
    thread::sleep(Duration::from_millis(250));
    assert_eq!(
        fs::read_dir(&store)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry
                .path()
                .extension()
                .is_some_and(|value| value == "jsonl"))
            .count(),
        0
    );
    let unknown = server.send(&format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"tasks/get","params":{{"taskId":{}, {TASK_META}}}}}"#,
        common::json::quote(&expired_id)
    ));
    assert!(unknown.contains("unknown task"));
    drop(server);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn oversized_input_is_rejected_without_stopping_server() {
    let mut server = Server::spawn(&version_tool_args(None), None);
    let oversized = "x".repeat(8 * 1024 * 1024 + 1);
    let rejected = server.send(&oversized);
    assert!(rejected.contains("\"code\":-32600"));
    assert!(rejected.contains("exceeds the 8388608-byte limit"));

    let discover = server.send(&format!(
        r#"{{"jsonrpc":"2.0","id":99,"method":"server/discover","params":{{{META}}}}}"#
    ));
    assert!(discover.contains("\"id\":99"));
    assert!(discover.contains("\"supportedVersions\":[\"2026-07-28\"]"));
}

#[cfg(unix)]
#[test]
fn timeout_and_output_limits_are_reported_in_structured_content() {
    let timeout_args = vec![
        "--tool".into(),
        "--name".into(),
        "limited".into(),
        "--process-timeout-ms".into(),
        "20".into(),
        "--exec".into(),
        "sh".into(),
        "-c".into(),
        "sleep 10".into(),
    ];
    let mut timeout_server = Server::spawn(&timeout_args, None);
    let timed_out = timeout_server.send(&format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"limited","arguments":{{}},{META}}}}}"#
    ));
    assert!(timed_out.contains("\"timedOut\":true"));
    assert!(timed_out.contains("\"isError\":true"));

    let output_args = vec![
        "--tool".into(),
        "--name".into(),
        "limited".into(),
        "--process-output-limit-bytes".into(),
        "3".into(),
        "--exec".into(),
        "sh".into(),
        "-c".into(),
        "printf 123456; printf abcdef >&2".into(),
    ];
    let mut output_server = Server::spawn(&output_args, None);
    let truncated = output_server.send(&format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"limited","arguments":{{}},{META}}}}}"#
    ));
    assert!(truncated.contains("\"stdout\":\"123\""));
    assert!(truncated.contains("\"stderr\":\"abc\""));
    assert!(truncated.contains("\"stdoutTruncated\":true"));
    assert!(truncated.contains("\"stderrTruncated\":true"));
}

#[test]
fn spawn_failure_is_a_schema_conforming_tool_error() {
    let args = vec![
        "--tool".into(),
        "--name".into(),
        "missing".into(),
        "--exec".into(),
        "shell-is-all-you-need-command-that-does-not-exist".into(),
    ];
    let mut server = Server::spawn(&args, None);
    let response = server.send(&format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"missing","arguments":{{}},{META}}}}}"#
    ));
    assert!(response.contains("\"isError\":true"));
    assert!(response.contains("\"exitCode\":null"));
    assert!(response.contains("failed to spawn process"));
    for field in [
        "stdout",
        "stderr",
        "timedOut",
        "stdoutTruncated",
        "stderrTruncated",
    ] {
        assert!(response.contains(&format!("\"{field}\":")));
    }
}

#[cfg(unix)]
#[test]
fn non_utf8_process_output_is_lossily_converted() {
    let args = vec![
        "--tool".into(),
        "--name".into(),
        "bytes".into(),
        "--exec".into(),
        "sh".into(),
        "-c".into(),
        "printf '\\377'".into(),
    ];
    let mut server = Server::spawn(&args, None);
    let response = server.send(&format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"bytes","arguments":{{}},{META}}}}}"#
    ));
    let result = common::json::object_get(&response, "result").unwrap();
    let structured = common::json::object_get(result, "structuredContent").unwrap();
    let stdout = common::json::object_get(structured, "stdout")
        .and_then(common::json::string)
        .unwrap();
    assert_eq!(stdout, "\u{fffd}");
}

#[cfg(unix)]
#[test]
fn concurrency_limit_rejects_excess_call_and_recovers() {
    let args = vec![
        "--tool".into(),
        "--name".into(),
        "slow".into(),
        "--exec".into(),
        "sh".into(),
        "-c".into(),
        "sleep 0.1; printf done".into(),
    ];
    let mut server = Server::spawn(&args, None);
    server.send_only(&format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"slow","arguments":{{}},{META}}}}}"#
    ));
    server.send_only(&format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"slow","arguments":{{}},{META}}}}}"#
    ));
    let rejected = server.read();
    assert!(rejected.contains("\"id\":2"));
    assert!(rejected.contains("concurrency limit reached"));
    let completed = server.read();
    assert!(completed.contains("\"id\":1"));
    assert!(completed.contains("done"));

    let retried = server.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"slow","arguments":{{}},{META}}}}}"#
    ));
    assert!(retried.contains("\"id\":3"));
    assert!(retried.contains("\"isError\":false"));
}

#[test]
fn completed_task_is_pollable_and_survives_restart() {
    let directory = temp_dir("durable");
    fs::create_dir_all(&directory).unwrap();
    let store = directory.join("tasks");
    let args = version_tool_args(Some(&store));

    let mut server = Server::spawn(&args, Some(&directory));
    let discover = server.send(&format!(
        r#"{{"jsonrpc":"2.0","id":0,"method":"server/discover","params":{{{TASK_META}}}}}"#
    ));
    assert!(discover.contains("io.modelcontextprotocol/tasks"));

    let created = server.send(&format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"probe","arguments":{{}},{TASK_META}}}}}"#
    ));
    assert!(created.contains("\"resultType\":\"task\""));
    let task_id = task_id(&created);

    let updated = server.send(&format!(
        r#"{{"jsonrpc":"2.0","id":500,"method":"tasks/update","params":{{"taskId":{},"inputResponses":{{}}, {TASK_META}}}}}"#,
        common::json::quote(&task_id)
    ));
    assert!(updated.contains("\"resultType\":\"complete\""));

    let mut completed = None;
    for request_id in 2..=202 {
        let response = server.send(&format!(
            r#"{{"jsonrpc":"2.0","id":{request_id},"method":"tasks/get","params":{{"taskId":{}, {TASK_META}}}}}"#,
            common::json::quote(&task_id)
        ));
        if task_status(&response) == "completed" {
            completed = Some(response);
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let completed = completed.expect("task should complete");
    assert!(completed.contains("\"result\":"));
    assert!(completed.contains("\"structuredContent\":"));
    drop(server);

    let mut restarted = Server::spawn(&args, Some(&directory));
    let recovered = restarted.send(&format!(
        r#"{{"jsonrpc":"2.0","id":300,"method":"tasks/get","params":{{"taskId":{}, {TASK_META}}}}}"#,
        common::json::quote(&task_id)
    ));
    assert_eq!(task_status(&recovered), "completed");
    assert!(recovered.contains("\"result\":"));
    drop(restarted);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn multiple_task_stores_route_ids_and_survive_restart() {
    let directory = temp_dir("multi-task");
    fs::create_dir_all(&directory).unwrap();
    let first_store = directory.join("first");
    let second_store = directory.join("second");
    let executable = binary().to_string_lossy().into_owned();
    let args = vec![
        "--tool".into(),
        "--name".into(),
        "ordinary".into(),
        "--exec".into(),
        executable.clone(),
        "--version".into(),
        "--tool".into(),
        "--name".into(),
        "first".into(),
        "--task-store-dir".into(),
        first_store.to_string_lossy().into_owned(),
        "--exec".into(),
        executable.clone(),
        "--version".into(),
        "--tool".into(),
        "--name".into(),
        "second".into(),
        "--task-store-dir".into(),
        second_store.to_string_lossy().into_owned(),
        "--exec".into(),
        executable,
        "--version".into(),
    ];
    let mut server = Server::spawn(&args, Some(&directory));
    let discover = server.send(&format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"server/discover","params":{{{TASK_META}}}}}"#
    ));
    assert!(discover.contains("io.modelcontextprotocol/tasks"));
    let ordinary = server.send(&format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"ordinary","arguments":{{}},{TASK_META}}}}}"#
    ));
    assert!(ordinary.contains("\"resultType\":\"complete\""));

    let mut ids = Vec::new();
    for (request_id, name) in [(3, "first"), (4, "second")] {
        let created = server.send(&format!(
            r#"{{"jsonrpc":"2.0","id":{request_id},"method":"tools/call","params":{{"name":"{name}","arguments":{{}},{TASK_META}}}}}"#
        ));
        assert!(created.contains("\"resultType\":\"task\""));
        ids.push(task_id(&created));
    }
    for (offset, task) in ids.iter().enumerate() {
        let mut done = false;
        for attempt in 0..100 {
            let response = server.send(&format!(
                r#"{{"jsonrpc":"2.0","id":{},"method":"tasks/get","params":{{"taskId":{}, {TASK_META}}}}}"#,
                100 + offset * 100 + attempt,
                common::json::quote(task)
            ));
            if task_status(&response) == "completed" {
                done = true;
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(done, "task from store {offset} should complete");
    }
    drop(server);

    let mut restarted = Server::spawn(&args, Some(&directory));
    for (offset, task) in ids.iter().enumerate() {
        let response = restarted.send(&format!(
            r#"{{"jsonrpc":"2.0","id":{},"method":"tasks/get","params":{{"taskId":{}, {TASK_META}}}}}"#,
            500 + offset,
            common::json::quote(task)
        ));
        assert_eq!(task_status(&response), "completed");
    }
    drop(restarted);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn duplicate_canonical_task_store_directories_are_rejected() {
    let directory = temp_dir("duplicate-task-store");
    fs::create_dir_all(&directory).unwrap();
    let store = directory.join("tasks");
    let output = std::process::Command::new(binary())
        .current_dir(&directory)
        .args([
            "--tool",
            "--name",
            "one",
            "--task-store-dir",
            "tasks",
            "--exec",
            "printf",
            "one",
            "--tool",
            "--name",
            "two",
            "--task-store-dir",
        ])
        .arg(&store)
        .args(["--exec", "printf", "two"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("duplicate canonical task-store"));
    fs::remove_dir_all(directory).unwrap();
}

#[cfg(unix)]
#[test]
fn task_cancellation_uses_tasks_cancel() {
    let directory = temp_dir("task-cancel");
    fs::create_dir_all(&directory).unwrap();
    let store = directory.join("tasks");
    let args = vec![
        "--tool".into(),
        "--name".into(),
        "slow".into(),
        "--task-store-dir".into(),
        store.to_string_lossy().into_owned(),
        "--task-poll-interval-ms".into(),
        "10".into(),
        "--exec".into(),
        "sh".into(),
        "-c".into(),
        "sleep 10".into(),
    ];
    let mut server = Server::spawn(&args, Some(&directory));
    let created = server.send(&format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"slow","arguments":{{}},{TASK_META}}}}}"#
    ));
    let task_id = task_id(&created);
    let cancel = server.send(&format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"tasks/cancel","params":{{"taskId":{}, {TASK_META}}}}}"#,
        common::json::quote(&task_id)
    ));
    assert!(cancel.contains("\"resultType\":\"complete\""));

    let mut cancelled = false;
    for request_id in 3..=203 {
        let response = server.send(&format!(
            r#"{{"jsonrpc":"2.0","id":{request_id},"method":"tasks/get","params":{{"taskId":{}, {TASK_META}}}}}"#,
            common::json::quote(&task_id)
        ));
        if task_status(&response) == "cancelled" {
            cancelled = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(cancelled, "task should eventually become cancelled");
    drop(server);
    fs::remove_dir_all(directory).unwrap();
}

#[cfg(unix)]
#[test]
fn cancelled_sync_request_produces_no_late_response() {
    let args = vec![
        "--tool".into(),
        "--name".into(),
        "slow".into(),
        "--exec".into(),
        "sh".into(),
        "-c".into(),
        "sleep 10".into(),
    ];
    let mut server = Server::spawn(&args, None);
    server.send_only(&format!(
        r#"{{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{{"name":"slow","arguments":{{}},{META}}}}}"#
    ));
    server.send_only(
        r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":10,"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}"#,
    );
    server.send_only(&format!(
        r#"{{"jsonrpc":"2.0","id":11,"method":"server/discover","params":{{{META}}}}}"#
    ));
    let response = server.read();
    assert!(response.contains("\"id\":11"));
    assert!(!response.contains("\"id\":10"));
}
