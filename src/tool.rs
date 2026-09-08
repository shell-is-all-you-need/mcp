use crate::{
    json,
    path_policy::PathPolicy,
    process::{Invocation, Limits},
    task::Config as TaskConfig,
};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScalarType {
    String,
    Number,
    Integer,
    Boolean,
}

impl ScalarType {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "string" => Some(Self::String),
            "number" => Some(Self::Number),
            "integer" => Some(Self::Integer),
            "boolean" => Some(Self::Boolean),
            _ => None,
        }
    }

    fn validate(self, raw: &str) -> Option<String> {
        match self {
            Self::String => json::string(raw),
            Self::Number if json::number(raw) => Some(raw.trim().to_owned()),
            Self::Integer if json::integer(raw) => Some(raw.trim().to_owned()),
            Self::Boolean => json::boolean(raw).map(|value| value.to_string()),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Number => "number",
            Self::Integer => "integer",
            Self::Boolean => "boolean",
        }
    }
}

#[derive(Clone)]
struct Field {
    name: String,
    kind: ScalarType,
}

pub(super) struct ToolConfig {
    pub(super) name: String,
    pub(super) description: Option<String>,
    pub(super) input_schema: Option<String>,
    pub(super) invocation: Vec<String>,
    pub(super) path_fields: Vec<String>,
    pub(super) roots: Vec<String>,
    pub(super) denied_paths: Vec<String>,
}

#[derive(Clone)]
pub(super) struct Tool {
    pub(super) name: String,
    pub(super) description: String,
    pub(super) input_schema: String,
    command: String,
    templates: Vec<String>,
    fields: Vec<Field>,
    path_checks: Vec<(String, usize)>,
    path_policy: Option<PathPolicy>,
}

pub(super) struct ToolDefinition {
    pub(super) tool: Tool,
    pub(super) limits: Limits,
    pub(super) rate_limit: InvocationRateLimit,
    pub(super) tasks: TaskConfig,
}

#[derive(Clone, Copy)]
pub(super) struct InvocationRateLimit {
    pub(super) count: usize,
    pub(super) window_ms: u64,
}

impl InvocationRateLimit {
    pub(super) const DEFAULT_COUNT: usize = 60;
    pub(super) const DEFAULT_WINDOW_MS: u64 = 60_000;
}

pub(super) struct ServerConfig {
    pub(super) tools: Vec<ToolDefinition>,
    tools_by_name: HashMap<String, usize>,
}

impl ServerConfig {
    pub(super) fn new(tools: Vec<ToolDefinition>) -> Self {
        let tools_by_name = tools
            .iter()
            .enumerate()
            .map(|(index, definition)| (definition.tool.name.clone(), index))
            .collect();
        Self {
            tools,
            tools_by_name,
        }
    }

    pub(super) fn find(&self, name: &str) -> Option<(usize, &ToolDefinition)> {
        let index = *self.tools_by_name.get(name)?;
        Some((index, &self.tools[index]))
    }

    pub(super) fn tasks_enabled(&self) -> bool {
        self.tools
            .iter()
            .any(|definition| definition.tasks.enabled())
    }
}

fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn placeholders(template: &str) -> Result<Vec<String>, String> {
    let bytes = template.as_bytes();
    let mut names = Vec::new();
    let mut index = 0;

    while index < bytes.len() {
        match bytes[index] {
            b'{' if bytes.get(index + 1) == Some(&b'{') => index += 2,
            b'{' => {
                let after = &template[index + 1..];
                let end = after
                    .find('}')
                    .ok_or_else(|| format!("unclosed placeholder in {template:?}"))?;
                let name = &after[..end];
                if !valid_name(name) {
                    return Err(format!("invalid placeholder {{{name}}}"));
                }
                if !names.iter().any(|item| item == name) {
                    names.push(name.to_owned());
                }
                index += end + 2;
            }
            b'}' if bytes.get(index + 1) == Some(&b'}') => index += 2,
            b'}' => return Err(format!("unmatched `}}` in {template:?}")),
            _ => {
                let ch = template[index..]
                    .chars()
                    .next()
                    .expect("index is within UTF-8 string");
                index += ch.len_utf8();
            }
        }
    }

    Ok(names)
}

fn render(template: &str, values: &[(String, String)]) -> String {
    let bytes = template.as_bytes();
    let mut output = String::with_capacity(template.len());
    let mut index = 0;

    while index < bytes.len() {
        match bytes[index] {
            b'{' if bytes.get(index + 1) == Some(&b'{') => {
                output.push('{');
                index += 2;
            }
            b'{' => {
                let after = &template[index + 1..];
                let end = after.find('}').expect("template validated");
                let name = &after[..end];
                let value = values
                    .iter()
                    .find_map(|(key, value)| (key == name).then_some(value.as_str()))
                    .expect("placeholder contract validated");
                output.push_str(value);
                index += end + 2;
            }
            b'}' if bytes.get(index + 1) == Some(&b'}') => {
                output.push('}');
                index += 2;
            }
            b'}' => unreachable!("template validated"),
            _ => {
                let ch = template[index..]
                    .chars()
                    .next()
                    .expect("index is within UTF-8 string");
                output.push(ch);
                index += ch.len_utf8();
            }
        }
    }

    output
}

fn inferred_schema(names: &[String]) -> String {
    let properties = names
        .iter()
        .map(|name| format!("{}:{{\"type\":\"string\"}}", json::quote(name)))
        .collect::<Vec<_>>()
        .join(",");
    let required = names
        .iter()
        .map(|name| json::quote(name))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"type\":\"object\",\"properties\":{{{properties}}},\"required\":[{required}],\"additionalProperties\":false}}"
    )
}

fn only_keys(entries: &[(String, &str)], allowed: &[&str], context: &str) -> Result<(), String> {
    if let Some((key, _)) = entries
        .iter()
        .find(|(key, _)| !allowed.contains(&key.as_str()))
    {
        return Err(format!(
            "unsupported JSON Schema keyword {key:?} in {context}; this zero-dependency server accepts only its documented flat 2020-12 subset"
        ));
    }
    Ok(())
}

fn optional_string_keyword(schema: &str, keyword: &str, context: &str) -> Result<(), String> {
    if json::object_get(schema, keyword).is_some_and(|raw| json::string(raw).is_none()) {
        return Err(format!("{context}.{keyword} must be a string"));
    }
    Ok(())
}

fn schema_contract(schema: &str, placeholders: &[String]) -> Result<Vec<Field>, String> {
    const ROOT_KEYS: &[&str] = &[
        "$schema",
        "$comment",
        "title",
        "description",
        "type",
        "properties",
        "required",
        "additionalProperties",
    ];
    const PROPERTY_KEYS: &[&str] = &["$comment", "title", "description", "type"];
    const DRAFT_2020_12: &str = "https://json-schema.org/draft/2020-12/schema";

    let entries =
        json::object_entries(schema).ok_or("--input-schema must be a valid JSON object")?;
    only_keys(&entries, ROOT_KEYS, "input schema")?;
    for keyword in ["$comment", "title", "description"] {
        optional_string_keyword(schema, keyword, "inputSchema")?;
    }

    if let Some(raw) = json::object_get(schema, "$schema") {
        let dialect = json::string(raw).ok_or("inputSchema.$schema must be a string")?;
        if dialect != DRAFT_2020_12 {
            return Err(format!(
                "unsupported JSON Schema dialect {dialect:?}; only {DRAFT_2020_12:?} is supported"
            ));
        }
    }

    if json::object_get(schema, "type")
        .and_then(json::string)
        .as_deref()
        != Some("object")
    {
        return Err("inputSchema.type must be \"object\"".into());
    }
    if json::object_get(schema, "additionalProperties").map(str::trim) != Some("false") {
        return Err("inputSchema.additionalProperties must be false".into());
    }

    let properties_raw =
        json::object_get(schema, "properties").ok_or("inputSchema.properties is required")?;
    let properties =
        json::object_entries(properties_raw).ok_or("inputSchema.properties must be an object")?;
    let property_names = properties
        .iter()
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();

    if property_names.len() != placeholders.len()
        || property_names
            .iter()
            .any(|name| !placeholders.contains(name))
    {
        return Err("inputSchema.properties must contain exactly the command placeholders".into());
    }

    let required = match json::object_get(schema, "required") {
        Some(required_raw) => json::array_values(required_raw)
            .ok_or("inputSchema.required must be an array")?
            .into_iter()
            .map(|value| json::string(value).ok_or("inputSchema.required entries must be strings"))
            .collect::<Result<Vec<_>, _>>()?,
        None if placeholders.is_empty() => Vec::new(),
        None => return Err("inputSchema.required is required".into()),
    };
    if required.len() != placeholders.len()
        || placeholders.iter().any(|name| !required.contains(name))
    {
        return Err(
            "inputSchema.required must contain every command placeholder exactly once".into(),
        );
    }
    let mut required_unique = required.clone();
    required_unique.sort();
    required_unique.dedup();
    if required_unique.len() != required.len() {
        return Err("inputSchema.required must not contain duplicates".into());
    }

    placeholders
        .iter()
        .map(|name| {
            let raw = properties
                .iter()
                .find_map(|(property, raw)| (property == name).then_some(*raw))
                .expect("property set validated");
            let property = json::object_entries(raw)
                .ok_or_else(|| format!("inputSchema.properties.{name} must be an object"))?;
            only_keys(&property, PROPERTY_KEYS, &format!("property {name:?}"))?;
            for keyword in ["$comment", "title", "description"] {
                optional_string_keyword(
                    raw,
                    keyword,
                    &format!("inputSchema.properties.{name}"),
                )?;
            }
            let kind = json::object_get(raw, "type")
                .and_then(json::string)
                .and_then(|value| ScalarType::parse(&value))
                .ok_or_else(|| {
                    format!(
                        "inputSchema.properties.{name}.type must be one of string, number, integer, or boolean"
                    )
                })?;
            Ok(Field {
                name: name.clone(),
                kind,
            })
        })
        .collect()
}

impl Tool {
    pub(super) fn new(config: ToolConfig) -> Result<Self, String> {
        let ToolConfig {
            name,
            description,
            input_schema: schema,
            invocation,
            path_fields,
            roots,
            denied_paths,
        } = config;
        if !valid_name(&name) {
            return Err("tool name must be 1-128 ASCII letters, digits, `.`, `_`, or `-`".into());
        }

        let command = invocation
            .first()
            .ok_or("a command is required after --exec")?
            .clone();
        if command.is_empty() {
            return Err("command after --exec must not be empty".into());
        }
        let templates = invocation[1..].to_vec();
        let template_fields = templates
            .iter()
            .map(|template| placeholders(template))
            .collect::<Result<Vec<_>, _>>()?;

        let mut names = Vec::new();
        for fields in &template_fields {
            for name in fields {
                if !names.contains(name) {
                    names.push(name.clone());
                }
            }
        }

        let input_schema = schema.unwrap_or_else(|| inferred_schema(&names));
        let fields = schema_contract(&input_schema, &names)?;

        let mut unique_path_fields = path_fields.clone();
        unique_path_fields.sort();
        unique_path_fields.dedup();
        if unique_path_fields.len() != path_fields.len() {
            return Err("--fs-path-field must not repeat the same field".into());
        }
        if path_fields.is_empty() && (!roots.is_empty() || !denied_paths.is_empty()) {
            return Err("--fs-root and --fs-deny-path require --fs-path-field".into());
        }

        let mut path_checks = Vec::new();
        for field in &path_fields {
            let definition = fields
                .iter()
                .find(|definition| definition.name == *field)
                .ok_or_else(|| {
                    format!("filesystem path field {field:?} must be a command placeholder")
                })?;
            if definition.kind != ScalarType::String {
                return Err(format!(
                    "filesystem path field {field:?} must have JSON Schema type string"
                ));
            }
            for (index, names) in template_fields.iter().enumerate() {
                if names.contains(field) {
                    path_checks.push((field.clone(), index));
                }
            }
        }

        let path_policy = if path_fields.is_empty() {
            None
        } else {
            Some(PathPolicy::new(roots, denied_paths)?)
        };

        Ok(Self {
            description: description
                .unwrap_or_else(|| format!("Run {command} with a fixed argument template.")),
            name,
            input_schema,
            command,
            templates,
            fields,
            path_checks,
            path_policy,
        })
    }

    pub(super) fn bind(&self, arguments: &str) -> Result<Invocation, String> {
        let entries = json::object_entries(arguments).ok_or("arguments must be an object")?;
        if entries.len() != self.fields.len() {
            return Err("tool arguments must contain exactly the declared input fields".into());
        }

        let values = self
            .fields
            .iter()
            .map(|field| {
                let raw = entries
                    .iter()
                    .find_map(|(name, value)| (name == &field.name).then_some(*value))
                    .ok_or_else(|| format!("missing tool argument {:?}", field.name))?;
                let value = field.kind.validate(raw).ok_or_else(|| {
                    format!(
                        "tool argument {:?} must be a JSON {}",
                        field.name,
                        field.kind.name()
                    )
                })?;
                Ok((field.name.clone(), value))
            })
            .collect::<Result<Vec<_>, String>>()?;

        if let Some((name, _)) = entries
            .iter()
            .find(|(name, _)| !self.fields.iter().any(|field| field.name == name.as_str()))
        {
            return Err(format!("unknown tool argument {name:?}"));
        }

        let mut args = Vec::with_capacity(self.templates.len());
        for template in &self.templates {
            let rendered = render(template, &values);
            if rendered.contains('\0') {
                return Err("rendered process argument contains a NUL byte".into());
            }
            args.push(rendered);
        }

        if let Some(policy) = &self.path_policy {
            for (field, index) in &self.path_checks {
                policy.check(field, &args[*index])?;
            }
        }

        Ok(Invocation {
            command: self.command.clone(),
            args,
        })
    }
}
