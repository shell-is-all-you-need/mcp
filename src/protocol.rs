use crate::{
    json,
    process::{Invocation, Output},
    task::EXTENSION_ID,
    tool::{ServerConfig, Tool},
};

pub(super) const MCP_PROTOCOL_VERSION: &str = "2026-07-28";

const OUTPUT_SCHEMA: &str = concat!(
    "{\"type\":\"object\",\"properties\":{",
    "\"exitCode\":{\"type\":[\"integer\",\"null\"]},",
    "\"stdout\":{\"type\":\"string\"},",
    "\"stderr\":{\"type\":\"string\"},",
    "\"timedOut\":{\"type\":\"boolean\"},",
    "\"stdoutTruncated\":{\"type\":\"boolean\"},",
    "\"stderrTruncated\":{\"type\":\"boolean\"}",
    "},\"required\":[\"exitCode\",\"stdout\",\"stderr\",\"timedOut\",\"stdoutTruncated\",\"stderrTruncated\"],",
    "\"additionalProperties\":false}"
);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) enum RequestId {
    String(String),
    Number(String),
}

impl RequestId {
    pub(super) fn parse(raw: &str) -> Option<Self> {
        if let Some(value) = json::string(raw) {
            return Some(Self::String(value));
        }
        json::number(raw).then(|| Self::Number(raw.trim().to_owned()))
    }

    fn json(&self) -> String {
        match self {
            Self::String(value) => json::quote(value),
            Self::Number(value) => value.clone(),
        }
    }
}

pub(super) enum Action {
    Ignore,
    Cancel(RequestId),
    Respond(String),
    Call {
        id: RequestId,
        tool_index: usize,
        invocation: Invocation,
        task: bool,
    },
    TaskGet {
        id: RequestId,
        task_id: String,
    },
    TaskUpdate {
        id: RequestId,
        task_id: String,
    },
    TaskCancel {
        id: RequestId,
        task_id: String,
    },
}

struct RequestMeta {
    tasks: bool,
}

fn server_meta() -> String {
    format!(
        "{{\"io.modelcontextprotocol/serverInfo\":{{\"name\":{},\"version\":{}}}}}",
        json::quote(env!("CARGO_PKG_NAME")),
        json::quote(env!("CARGO_PKG_VERSION"))
    )
}

fn result_object(result_type: &str, fields: &str) -> String {
    format!(
        "{{\"resultType\":{},\"_meta\":{}{fields}}}",
        json::quote(result_type),
        server_meta()
    )
}

fn response(id: &RequestId, result: &str) -> String {
    format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{result}}}",
        id.json()
    )
}

fn complete_response(id: &RequestId, fields: &str) -> String {
    response(id, &result_object("complete", fields))
}

pub(super) fn task_response(id: &RequestId, fields: &str) -> String {
    response(id, &result_object("task", fields))
}

pub(super) fn task_state_response(id: &RequestId, fields: &str) -> String {
    complete_response(id, fields)
}

pub(super) fn task_ack(id: &RequestId) -> String {
    complete_response(id, "")
}

pub(super) fn rpc_error(
    id: Option<&RequestId>,
    code: i32,
    message: &str,
    data: Option<&str>,
) -> String {
    let id = id.map(RequestId::json).unwrap_or_else(|| "null".into());
    let data = data
        .map(|value| format!(",\"data\":{value}"))
        .unwrap_or_default();
    format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"error\":{{\"code\":{code},\"message\":{}{data}}}}}",
        json::quote(message)
    )
}

fn missing_tasks_capability(id: &RequestId) -> String {
    let data = format!(
        "{{\"requiredCapabilities\":{{\"extensions\":{{{}:{{}}}}}}}}",
        json::quote(EXTENSION_ID)
    );
    rpc_error(
        Some(id),
        -32021,
        "Missing required client capability",
        Some(&data),
    )
}

fn validate_meta(message: &str, id: &RequestId) -> Result<RequestMeta, String> {
    let params = json::object_get(message, "params")
        .and_then(json::object_entries)
        .ok_or_else(|| rpc_error(Some(id), -32602, "request params are required", None))?;
    let meta_raw = params
        .iter()
        .find_map(|(key, value)| (key == "_meta").then_some(*value))
        .ok_or_else(|| rpc_error(Some(id), -32602, "request _meta is required", None))?;
    let meta = json::object_entries(meta_raw)
        .ok_or_else(|| rpc_error(Some(id), -32602, "request _meta must be an object", None))?;

    let version = meta
        .iter()
        .find_map(|(key, value)| {
            (key == "io.modelcontextprotocol/protocolVersion").then_some(*value)
        })
        .and_then(json::string)
        .ok_or_else(|| {
            rpc_error(
                Some(id),
                -32602,
                "protocol version metadata is required",
                None,
            )
        })?;
    if version != MCP_PROTOCOL_VERSION {
        let data = format!(
            "{{\"supported\":[{}],\"requested\":{}}}",
            json::quote(MCP_PROTOCOL_VERSION),
            json::quote(&version)
        );
        return Err(rpc_error(
            Some(id),
            -32022,
            "Unsupported protocol version",
            Some(&data),
        ));
    }

    let capabilities_raw = meta
        .iter()
        .find_map(|(key, value)| {
            (key == "io.modelcontextprotocol/clientCapabilities").then_some(*value)
        })
        .ok_or_else(|| {
            rpc_error(
                Some(id),
                -32602,
                "clientCapabilities metadata is required",
                None,
            )
        })?;
    if json::object_entries(capabilities_raw).is_none() {
        return Err(rpc_error(
            Some(id),
            -32602,
            "clientCapabilities metadata must be an object",
            None,
        ));
    }

    let tasks = if let Some(extensions) = json::object_get(capabilities_raw, "extensions") {
        let extension_entries = json::object_entries(extensions).ok_or_else(|| {
            rpc_error(
                Some(id),
                -32602,
                "clientCapabilities.extensions must be an object",
                None,
            )
        })?;
        if extension_entries
            .iter()
            .any(|(_, settings)| json::object_entries(settings).is_none())
        {
            return Err(rpc_error(
                Some(id),
                -32602,
                "clientCapabilities extension settings must be objects",
                None,
            ));
        }
        extension_entries
            .iter()
            .any(|(name, _)| name == EXTENSION_ID)
    } else {
        false
    };

    if let Some(client_info) = meta
        .iter()
        .find_map(|(key, value)| (key == "io.modelcontextprotocol/clientInfo").then_some(*value))
    {
        let valid = json::object_get(client_info, "name")
            .and_then(json::string)
            .is_some()
            && json::object_get(client_info, "version")
                .and_then(json::string)
                .is_some();
        if !valid {
            return Err(rpc_error(
                Some(id),
                -32602,
                "clientInfo metadata must contain string name and version",
                None,
            ));
        }
    }

    Ok(RequestMeta { tasks })
}

fn tool_json(tool: &Tool) -> String {
    format!(
        "{{\"name\":{},\"description\":{},\"inputSchema\":{},\"outputSchema\":{OUTPUT_SCHEMA}}}",
        json::quote(&tool.name),
        json::quote(&tool.description),
        tool.input_schema
    )
}

fn discover(id: &RequestId, tasks_enabled: bool) -> String {
    let extensions = if tasks_enabled {
        format!(",\"extensions\":{{{}:{{}}}}", json::quote(EXTENSION_ID))
    } else {
        String::new()
    };
    complete_response(
        id,
        &format!(
            ",\"supportedVersions\":[{}],\"capabilities\":{{\"tools\":{{}}{extensions}}},\"ttlMs\":0,\"cacheScope\":\"private\"",
            json::quote(MCP_PROTOCOL_VERSION)
        ),
    )
}

fn list_tools(id: &RequestId, config: &ServerConfig, params: &str) -> String {
    if json::object_get(params, "cursor").is_some() {
        return rpc_error(Some(id), -32602, "invalid tools/list cursor", None);
    }
    let tools = config
        .tools
        .iter()
        .map(|definition| tool_json(&definition.tool))
        .collect::<Vec<_>>()
        .join(",");
    complete_response(
        id,
        &format!(",\"tools\":[{tools}],\"ttlMs\":0,\"cacheScope\":\"private\""),
    )
}

fn structured(
    exit_code: Option<i32>,
    stdout: &str,
    stderr: &str,
    timed_out: bool,
    stdout_truncated: bool,
    stderr_truncated: bool,
) -> String {
    let exit_code = exit_code
        .map(|value| value.to_string())
        .unwrap_or_else(|| "null".into());
    format!(
        "{{\"exitCode\":{exit_code},\"stdout\":{},\"stderr\":{},\"timedOut\":{timed_out},\"stdoutTruncated\":{stdout_truncated},\"stderrTruncated\":{stderr_truncated}}}",
        json::quote(stdout),
        json::quote(stderr)
    )
}

pub(super) fn tool_error_result(message: &str) -> String {
    let structured = structured(None, "", message, false, false, false);
    result_object(
        "complete",
        &format!(
            ",\"content\":[{{\"type\":\"text\",\"text\":{}}},{{\"type\":\"text\",\"text\":{}}}],\"structuredContent\":{structured},\"isError\":true",
            json::quote(message),
            json::quote(&structured)
        ),
    )
}

pub(super) fn tool_error(id: &RequestId, message: &str) -> String {
    response(id, &tool_error_result(message))
}

pub(super) fn tool_output_result(output: Output) -> String {
    let success = output.exit_code == Some(0) && !output.timed_out;
    let summary = format!(
        "exit_code={}\ntimed_out={}\nstdout{}:\n{}\nstderr{}:\n{}",
        output
            .exit_code
            .map(|value| value.to_string())
            .unwrap_or_else(|| "null".into()),
        output.timed_out,
        if output.stdout_truncated {
            " (truncated)"
        } else {
            ""
        },
        output.stdout,
        if output.stderr_truncated {
            " (truncated)"
        } else {
            ""
        },
        output.stderr
    );
    let text = if success { &output.stdout } else { &summary };
    let structured = structured(
        output.exit_code,
        &output.stdout,
        &output.stderr,
        output.timed_out,
        output.stdout_truncated,
        output.stderr_truncated,
    );
    result_object(
        "complete",
        &format!(
            ",\"content\":[{{\"type\":\"text\",\"text\":{}}},{{\"type\":\"text\",\"text\":{}}}],\"structuredContent\":{structured},\"isError\":{}",
            json::quote(text),
            json::quote(&structured),
            !success
        ),
    )
}

pub(super) fn tool_output(id: &RequestId, output: Output) -> String {
    response(id, &tool_output_result(output))
}

fn notification_has_current_envelope(message: &str) -> bool {
    let Some(params) = json::object_get(message, "params") else {
        return false;
    };
    let Some(meta) = json::object_get(params, "_meta") else {
        return false;
    };
    if json::object_get(meta, "io.modelcontextprotocol/protocolVersion")
        .and_then(json::string)
        .as_deref()
        != Some(MCP_PROTOCOL_VERSION)
    {
        return false;
    }
    json::object_get(meta, "io.modelcontextprotocol/clientCapabilities")
        .and_then(json::object_entries)
        .is_some()
}

fn cancellation(message: &str) -> Action {
    let Some(params) = json::object_get(message, "params") else {
        return Action::Ignore;
    };
    let Some(raw) = json::object_get(params, "requestId") else {
        return Action::Ignore;
    };
    RequestId::parse(raw).map_or(Action::Ignore, Action::Cancel)
}

fn task_id(params: &str, id: &RequestId) -> Result<String, String> {
    json::object_get(params, "taskId")
        .and_then(json::string)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| rpc_error(Some(id), -32602, "taskId is required", None))
}

pub(super) fn handle(message: &str, config: &ServerConfig) -> Action {
    if !json::validate(message) {
        return Action::Respond(rpc_error(None, -32700, "Parse error", None));
    }
    if json::object_entries(message).is_none() {
        return Action::Respond(rpc_error(None, -32600, "Invalid Request", None));
    }
    if json::object_get(message, "jsonrpc")
        .and_then(json::string)
        .as_deref()
        != Some("2.0")
    {
        return Action::Respond(rpc_error(None, -32600, "Invalid Request", None));
    }
    let Some(method) = json::object_get(message, "method").and_then(json::string) else {
        return Action::Respond(rpc_error(None, -32600, "Invalid Request", None));
    };

    let raw_id = json::object_get(message, "id");
    if raw_id.is_none() {
        return if method == "notifications/cancelled" && notification_has_current_envelope(message)
        {
            cancellation(message)
        } else {
            Action::Ignore
        };
    }
    let Some(id) = raw_id.and_then(RequestId::parse) else {
        return Action::Respond(rpc_error(None, -32600, "Invalid Request", None));
    };

    let meta = match validate_meta(message, &id) {
        Ok(meta) => meta,
        Err(response) => return Action::Respond(response),
    };
    let params = json::object_get(message, "params").expect("metadata validation requires params");

    match method.as_str() {
        "server/discover" => Action::Respond(discover(&id, config.tasks_enabled())),
        "tools/list" => Action::Respond(list_tools(&id, config, params)),
        "tools/call" => {
            let Some(name) = json::object_get(params, "name").and_then(json::string) else {
                return Action::Respond(rpc_error(
                    Some(&id),
                    -32602,
                    "tools/call name is required",
                    None,
                ));
            };
            let Some((tool_index, definition)) = config.find(&name) else {
                return Action::Respond(rpc_error(
                    Some(&id),
                    -32602,
                    &format!("Unknown tool: {name}"),
                    None,
                ));
            };
            let arguments = json::object_get(params, "arguments").unwrap_or("{}");
            if json::object_entries(arguments).is_none() {
                return Action::Respond(rpc_error(
                    Some(&id),
                    -32602,
                    "tools/call arguments must be an object",
                    None,
                ));
            }
            match definition.tool.bind(arguments) {
                Ok(invocation) => Action::Call {
                    id,
                    tool_index,
                    invocation,
                    task: definition.tasks.enabled() && meta.tasks,
                },
                Err(error) => Action::Respond(tool_error(&id, &error)),
            }
        }
        "tasks/get" | "tasks/update" | "tasks/cancel" if !config.tasks_enabled() => {
            Action::Respond(rpc_error(Some(&id), -32601, "Method not found", None))
        }
        "tasks/get" | "tasks/update" | "tasks/cancel" if !meta.tasks => {
            Action::Respond(missing_tasks_capability(&id))
        }
        "tasks/get" => match task_id(params, &id) {
            Ok(task_id) => Action::TaskGet { id, task_id },
            Err(response) => Action::Respond(response),
        },
        "tasks/update" => {
            let task_id = match task_id(params, &id) {
                Ok(task_id) => task_id,
                Err(response) => return Action::Respond(response),
            };
            let Some(input_responses) =
                json::object_get(params, "inputResponses").and_then(json::object_entries)
            else {
                return Action::Respond(rpc_error(
                    Some(&id),
                    -32602,
                    "tasks/update inputResponses must be an object",
                    None,
                ));
            };
            if input_responses
                .iter()
                .any(|(_, response)| json::object_entries(response).is_none())
            {
                return Action::Respond(rpc_error(
                    Some(&id),
                    -32602,
                    "tasks/update inputResponses values must be objects",
                    None,
                ));
            }
            Action::TaskUpdate { id, task_id }
        }
        "tasks/cancel" => match task_id(params, &id) {
            Ok(task_id) => Action::TaskCancel { id, task_id },
            Err(response) => Action::Respond(response),
        },
        _ => Action::Respond(rpc_error(Some(&id), -32601, "Method not found", None)),
    }
}
