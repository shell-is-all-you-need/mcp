use crate::{
    process::Limits,
    task::Config as TaskConfig,
    tool::{InvocationRateLimit, ServerConfig, Tool, ToolConfig, ToolDefinition},
};
use std::{collections::HashSet, env};

pub(super) const USAGE: &str = "shell-is-all-you-need

Usage:
  shell-is-all-you-need --tool --name NAME [TOOL_OPTIONS] --exec COMMAND [ARG_TEMPLATE ...] [--tool --name NAME [TOOL_OPTIONS] --exec COMMAND [ARG_TEMPLATE ...]]...

Tool blocks:
  --tool                             Start a tool definition; also ends preceding child argv
  --name NAME                        Name exposed to the MCP client
  --description TEXT                Description shown to the model
  --input-schema JSON|@FILE          Closed flat JSON Schema 2020-12 input schema

Filesystem policy (per tool):
  --fs-path-field FIELD              Treat rendered argv items containing FIELD as paths; repeatable
  --fs-root DIR                      Permit paths under this filesystem root; repeatable
  --fs-deny-path PATH                Deny this path and its descendants; repeatable

Task execution (per tool):
  --task-store-dir DIR               Enable MCP Tasks and persist task state here
  --task-ttl-ms MILLISECONDS         Optional task retention TTL (default: unlimited)
  --task-poll-interval-ms MILLISECONDS Suggested client polling interval

Process limits (per tool):
  --process-timeout-ms MILLISECONDS  Optional per-call timeout (no default timeout)
  --process-output-limit-bytes BYTES Limit for each output stream (default: 1048576)
  --process-max-concurrency COUNT    Maximum simultaneous calls to this tool (default: 1)
  --process-rate-limit-count COUNT  Maximum admitted calls per rate window (default: 60)
  --process-rate-limit-window-ms MS Rate-limit window (default: 60000)

Execution:
  --exec COMMAND [ARG_TEMPLATE ...]  Begin direct child argv

Other:
  -h, --help
  -V, --version

Child argv after --exec continues verbatim until the next exact --tool token or EOF.
The literal child argv value --tool is reserved and cannot currently be forwarded.
When --fs-path-field is used without --fs-root, the current directory is the root.
Denied paths take precedence over roots. Concurrency and invocation rate limits are
independent per tool; total server throughput is their aggregate.";

pub(super) enum Action {
    Run(ServerConfig),
    Help,
    Version,
}

#[derive(Default)]
struct Builder {
    name: Option<String>,
    description: Option<String>,
    input_schema: Option<String>,
    path_fields: Vec<String>,
    roots: Vec<String>,
    denied_paths: Vec<String>,
    task_store_dir: Option<String>,
    task_ttl_ms: Option<u64>,
    task_poll_interval_ms: Option<u64>,
    timeout_ms: Option<u64>,
    output_limit: Option<usize>,
    max_concurrency: Option<usize>,
    rate_limit_count: Option<usize>,
    rate_limit_window_ms: Option<u64>,
    invocation: Option<Vec<String>>,
}

fn once(slot: &mut Option<String>, option: &str, value: &str) -> Result<(), String> {
    if slot.replace(value.to_owned()).is_some() {
        return Err(format!("{option} may only be specified once per tool"));
    }
    Ok(())
}

fn positive_u64(option: &str, value: &str) -> Result<u64, String> {
    let value = value
        .parse::<u64>()
        .map_err(|_| format!("{option} requires a positive integer"))?;
    if value == 0 {
        return Err(format!("{option} requires a positive integer"));
    }
    Ok(value)
}

fn positive_usize(option: &str, value: &str) -> Result<usize, String> {
    usize::try_from(positive_u64(option, value)?)
        .map_err(|_| format!("{option} is too large for this platform"))
}

fn positive_task_ms(option: &str, value: &str) -> Result<u64, String> {
    const MAX_SAFE_JSON_INTEGER: u64 = 9_007_199_254_740_991;
    let value = positive_u64(option, value)?;
    if value > MAX_SAFE_JSON_INTEGER {
        return Err(format!(
            "{option} must not exceed {MAX_SAFE_JSON_INTEGER} for MCP JSON interoperability"
        ));
    }
    Ok(value)
}

fn finish(builder: Builder) -> Result<ToolDefinition, String> {
    if builder.task_store_dir.is_none()
        && (builder.task_ttl_ms.is_some() || builder.task_poll_interval_ms.is_some())
    {
        return Err("--task-ttl-ms and --task-poll-interval-ms require --task-store-dir".into());
    }
    if builder.rate_limit_count.is_some() != builder.rate_limit_window_ms.is_some() {
        return Err("--process-rate-limit-count and --process-rate-limit-window-ms must be specified together".into());
    }
    let name = builder.name.ok_or("each --tool block requires --name")?;
    if name.is_empty() {
        return Err("tool name must not be empty".into());
    }
    let invocation = builder
        .invocation
        .ok_or_else(|| format!("tool {name:?} requires --exec"))?;
    let tool = Tool::new(ToolConfig {
        name,
        description: builder.description,
        input_schema: builder.input_schema,
        invocation,
        path_fields: builder.path_fields,
        roots: builder.roots,
        denied_paths: builder.denied_paths,
    })?;
    Ok(ToolDefinition {
        tool,
        limits: Limits {
            timeout_ms: builder.timeout_ms,
            output_limit: builder.output_limit.unwrap_or(Limits::DEFAULT_OUTPUT_LIMIT),
            max_concurrency: builder
                .max_concurrency
                .unwrap_or(Limits::DEFAULT_MAX_CONCURRENCY),
        },
        rate_limit: InvocationRateLimit {
            count: builder
                .rate_limit_count
                .unwrap_or(InvocationRateLimit::DEFAULT_COUNT),
            window_ms: builder
                .rate_limit_window_ms
                .unwrap_or(InvocationRateLimit::DEFAULT_WINDOW_MS),
        },
        tasks: TaskConfig {
            store_dir: builder.task_store_dir,
            ttl_ms: builder.task_ttl_ms,
            poll_interval_ms: builder.task_poll_interval_ms,
        },
    })
}

pub(super) fn parse() -> Result<Action, Box<dyn std::error::Error>> {
    parse_args(env::args().skip(1).collect())
        .map_err(|error| -> Box<dyn std::error::Error> { error.into() })
}

pub(super) fn parse_args(args: Vec<String>) -> Result<Action, String> {
    if args.as_slice() == ["-h"] || args.as_slice() == ["--help"] {
        return Ok(Action::Help);
    }
    if args.as_slice() == ["-V"] || args.as_slice() == ["--version"] {
        return Ok(Action::Version);
    }
    if args.is_empty() {
        return Err("at least one --tool block is required".into());
    }

    let mut definitions = Vec::new();
    let mut current: Option<Builder> = None;
    let mut index = 0;
    while index < args.len() {
        let option = args[index].as_str();
        if option == "--tool" {
            if let Some(builder) = current.take() {
                definitions.push(finish(builder)?);
            }
            current = Some(Builder::default());
            index += 1;
            continue;
        }
        let builder = current
            .as_mut()
            .ok_or_else(|| format!("unknown argument {option:?}; expected --tool"))?;
        if option == "--exec" {
            if builder.invocation.is_some() {
                return Err("--exec may only be specified once per tool".into());
            }
            let end = args[index + 1..]
                .iter()
                .position(|value| value == "--tool")
                .map_or(args.len(), |offset| index + 1 + offset);
            builder.invocation = Some(args[index + 1..end].to_vec());
            index = end;
            continue;
        }
        let takes_value = matches!(
            option,
            "--name"
                | "--description"
                | "--input-schema"
                | "--fs-path-field"
                | "--fs-root"
                | "--fs-deny-path"
                | "--task-store-dir"
                | "--task-ttl-ms"
                | "--task-poll-interval-ms"
                | "--process-timeout-ms"
                | "--process-output-limit-bytes"
                | "--process-max-concurrency"
                | "--process-rate-limit-count"
                | "--process-rate-limit-window-ms"
        );
        if !takes_value {
            return Err(format!("unknown argument {option:?}"));
        }
        index += 1;
        let value = args
            .get(index)
            .ok_or_else(|| format!("{option} requires a value"))?;
        if value == "--tool" {
            return Err(format!("{option} requires a value"));
        }
        match option {
            "--name" => once(&mut builder.name, option, value)?,
            "--description" => once(&mut builder.description, option, value)?,
            "--input-schema" => {
                if builder.input_schema.is_some() {
                    return Err("--input-schema may only be specified once per tool".into());
                }
                builder.input_schema = Some(if let Some(path) = value.strip_prefix('@') {
                    std::fs::read_to_string(path)
                        .map_err(|error| format!("cannot read input schema {path:?}: {error}"))?
                } else {
                    value.clone()
                });
            }
            "--fs-path-field" => builder.path_fields.push(value.clone()),
            "--fs-root" => builder.roots.push(value.clone()),
            "--fs-deny-path" => builder.denied_paths.push(value.clone()),
            "--task-store-dir" => once(&mut builder.task_store_dir, option, value)?,
            "--task-ttl-ms" => {
                if builder.task_ttl_ms.is_some() {
                    return Err(format!("{option} may only be specified once per tool"));
                }
                builder.task_ttl_ms = Some(positive_task_ms(option, value)?);
            }
            "--task-poll-interval-ms" => {
                if builder.task_poll_interval_ms.is_some() {
                    return Err(format!("{option} may only be specified once per tool"));
                }
                builder.task_poll_interval_ms = Some(positive_task_ms(option, value)?);
            }
            "--process-timeout-ms" => {
                if builder.timeout_ms.is_some() {
                    return Err(format!("{option} may only be specified once per tool"));
                }
                builder.timeout_ms = Some(positive_u64(option, value)?);
            }
            "--process-output-limit-bytes" => {
                if builder.output_limit.is_some() {
                    return Err(format!("{option} may only be specified once per tool"));
                }
                builder.output_limit = Some(positive_usize(option, value)?);
            }
            "--process-max-concurrency" => {
                if builder.max_concurrency.is_some() {
                    return Err(format!("{option} may only be specified once per tool"));
                }
                builder.max_concurrency = Some(positive_usize(option, value)?);
            }
            "--process-rate-limit-count" => {
                if builder.rate_limit_count.is_some() {
                    return Err(format!("{option} may only be specified once per tool"));
                }
                builder.rate_limit_count = Some(positive_usize(option, value)?);
            }
            "--process-rate-limit-window-ms" => {
                if builder.rate_limit_window_ms.is_some() {
                    return Err(format!("{option} may only be specified once per tool"));
                }
                builder.rate_limit_window_ms = Some(positive_u64(option, value)?);
            }
            _ => unreachable!(),
        }
        index += 1;
    }
    if let Some(builder) = current {
        definitions.push(finish(builder)?);
    }
    if definitions.is_empty() {
        return Err("at least one --tool block is required".into());
    }
    let mut names = HashSet::new();
    for definition in &definitions {
        if !names.insert(definition.tool.name.clone()) {
            return Err(format!("duplicate tool name {:?}", definition.tool.name));
        }
    }
    Ok(Action::Run(ServerConfig::new(definitions)))
}
