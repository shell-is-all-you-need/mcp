use crate::{
    process::{self, RunResult},
    protocol::{self, Action, RequestId},
    task::{self, Store as TaskStore},
    tool::ServerConfig,
};
use std::{
    collections::{HashMap, HashSet},
    io::{self, BufRead, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

const MAX_INPUT_LINE_BYTES: usize = 8 * 1024 * 1024;
const EVENT_QUEUE_CAPACITY: usize = 64;

enum Event {
    Input(String),
    InputTooLarge,
    InputClosed,
    InputError(String),
    CallFinished {
        id: RequestId,
        outcome: Result<RunResult, String>,
    },
    TaskFinished {
        tool_index: usize,
        task_id: String,
        outcome: Result<RunResult, String>,
    },
}

#[derive(Debug)]
pub(super) enum InputLine {
    Message(String),
    TooLarge,
}

pub(super) fn read_input_line(reader: &mut impl BufRead) -> io::Result<Option<InputLine>> {
    let mut bytes = Vec::new();
    let mut too_large = false;
    let mut read_any = false;

    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            if !read_any {
                return Ok(None);
            }
            break;
        }
        read_any = true;

        let newline = available.iter().position(|byte| *byte == b'\n');
        let count = newline.unwrap_or(available.len());
        if !too_large {
            let remaining = MAX_INPUT_LINE_BYTES.saturating_sub(bytes.len());
            if count <= remaining {
                bytes.extend_from_slice(&available[..count]);
            } else {
                too_large = true;
                bytes.clear();
            }
        }
        reader.consume(count + usize::from(newline.is_some()));
        if newline.is_some() {
            break;
        }
    }

    if too_large {
        return Ok(Some(InputLine::TooLarge));
    }
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    String::from_utf8(bytes)
        .map(InputLine::Message)
        .map(Some)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "MCP input is not UTF-8"))
}

fn write_message(stdout: &mut impl Write, message: &str) -> io::Result<()> {
    writeln!(stdout, "{message}")?;
    stdout.flush()
}

struct ActiveCall {
    tool_index: usize,
    cancelled: Arc<AtomicBool>,
}

pub(super) struct FixedWindowRateLimiter {
    limit: usize,
    window: Duration,
    window_started: Instant,
    admitted: usize,
}

impl FixedWindowRateLimiter {
    pub(super) fn new(limit: usize, window_ms: u64) -> Self {
        Self {
            limit,
            window: Duration::from_millis(window_ms),
            window_started: Instant::now(),
            admitted: 0,
        }
    }

    pub(super) fn admit(&mut self) -> Result<(), u128> {
        let elapsed = self.window_started.elapsed();
        if elapsed >= self.window {
            self.window_started = Instant::now();
            self.admitted = 0;
        }
        if self.admitted >= self.limit {
            return Err(self.window.saturating_sub(elapsed).as_millis().max(1));
        }
        self.admitted += 1;
        Ok(())
    }
}

pub(super) struct TaskRouter {
    stores: Vec<Option<TaskStore>>,
    owners: HashMap<String, usize>,
}

impl TaskRouter {
    pub(super) fn new(config: &ServerConfig) -> Result<Self, String> {
        let mut directories = HashSet::new();
        let mut prepared = Vec::with_capacity(config.tools.len());
        for definition in &config.tools {
            let directory = task::canonical_directory(&definition.tasks)?;
            if let Some(directory) = &directory
                && !directories.insert(directory.clone())
            {
                return Err(format!(
                    "duplicate canonical task-store directory configured: {}",
                    directory.display()
                ));
            }
            prepared.push(directory);
        }

        let mut stores = Vec::with_capacity(config.tools.len());
        let mut owners = HashMap::new();
        for (tool_index, (definition, directory)) in config.tools.iter().zip(prepared).enumerate() {
            if definition.tasks.enabled() {
                let store = TaskStore::new_prepared(definition.tasks.clone(), directory)?;
                for task_id in store.task_ids() {
                    if owners.insert(task_id.to_owned(), tool_index).is_some() {
                        return Err(format!("duplicate task ID {task_id:?} across task stores"));
                    }
                }
                stores.push(Some(store));
            } else {
                stores.push(None);
            }
        }
        Ok(Self { stores, owners })
    }

    fn prune_store(&mut self, tool_index: usize) -> Result<(), String> {
        let Some(store) = self.stores[tool_index].as_mut() else {
            return Ok(());
        };
        for task_id in store.prune_expired()? {
            self.owners.remove(&task_id);
        }
        Ok(())
    }

    fn prune_all(&mut self) -> Result<(), String> {
        for index in 0..self.stores.len() {
            self.prune_store(index)?;
        }
        Ok(())
    }

    fn enabled(&self) -> bool {
        self.stores.iter().any(Option::is_some)
    }

    pub(super) fn create(
        &mut self,
        tool_index: usize,
    ) -> Result<(String, Arc<AtomicBool>, String), String> {
        self.prune_store(tool_index)?;
        let created = self.stores[tool_index]
            .as_mut()
            .ok_or("selected tool does not support MCP Tasks")?
            .create()?;
        if self.owners.insert(created.0.clone(), tool_index).is_some() {
            return Err("generated task ID collides with an existing task".into());
        }
        Ok(created)
    }

    fn owner(&self, task_id: &str) -> Option<usize> {
        self.owners.get(task_id).copied()
    }

    pub(super) fn get(&mut self, task_id: &str) -> Result<Option<String>, String> {
        let Some(owner) = self.owner(task_id) else {
            return Ok(None);
        };
        self.prune_store(owner)?;
        if self.owner(task_id).is_none() {
            return Ok(None);
        }
        let result = self.stores[owner]
            .as_mut()
            .expect("task owner has store")
            .get(task_id)?;
        if result.is_none() {
            self.owners.remove(task_id);
        }
        Ok(result)
    }

    fn acknowledge_update(&mut self, task_id: &str) -> Result<bool, String> {
        let Some(owner) = self.owner(task_id) else {
            return Ok(false);
        };
        self.prune_store(owner)?;
        if self.owner(task_id).is_none() {
            return Ok(false);
        }
        let exists = self.stores[owner]
            .as_mut()
            .expect("task owner has store")
            .acknowledge_update(task_id)?;
        if !exists {
            self.owners.remove(task_id);
        }
        Ok(exists)
    }

    fn cancel(&mut self, task_id: &str) -> Result<bool, String> {
        let Some(owner) = self.owner(task_id) else {
            return Ok(false);
        };
        self.prune_store(owner)?;
        if self.owner(task_id).is_none() {
            return Ok(false);
        }
        let exists = self.stores[owner]
            .as_mut()
            .expect("task owner has store")
            .cancel(task_id)?;
        if !exists {
            self.owners.remove(task_id);
        }
        Ok(exists)
    }

    fn complete(
        &mut self,
        tool_index: usize,
        task_id: &str,
        result: String,
    ) -> Result<Option<String>, String> {
        self.stores[tool_index]
            .as_mut()
            .expect("task worker has store")
            .complete(task_id, result)
    }

    fn cancelled(&mut self, tool_index: usize, task_id: &str) -> Result<(), String> {
        self.stores[tool_index]
            .as_mut()
            .expect("task worker has store")
            .cancelled(task_id)
    }

    fn cancel_all(&mut self) -> Result<(), String> {
        self.prune_all()?;
        for store in self.stores.iter_mut().flatten() {
            store.cancel_all()?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn owner_count(&self) -> usize {
        self.owners.len()
    }

    #[cfg(test)]
    pub(super) fn make_owner_stale_for_test(&mut self, task_id: &str) -> Result<(), String> {
        if let Some(owner) = self.owner(task_id) {
            self.stores[owner]
                .as_mut()
                .expect("task owner has store")
                .forget_for_test(task_id)?;
        }
        Ok(())
    }
}

fn cancel_all(active: &HashMap<RequestId, ActiveCall>, tasks: &mut TaskRouter) {
    for call in active.values() {
        call.cancelled.store(true, Ordering::Release);
    }
    let _ = tasks.cancel_all();
}

pub(super) fn unknown_task(id: &RequestId) -> String {
    protocol::rpc_error(Some(id), -32602, "unknown task", None)
}

pub(super) fn task_store_error(id: &RequestId, error: &str) -> String {
    let data = format!("{{\"message\":{}}}", crate::json::quote(error));
    protocol::rpc_error(Some(id), -32603, "task store failure", Some(&data))
}

pub(super) fn serve(config: ServerConfig) -> Result<(), Box<dyn std::error::Error>> {
    let mut tasks = TaskRouter::new(&config)?;
    let config = Arc::new(config);
    // Bound queued input so a producer cannot make memory usage grow without limit.
    // Worker completion shares the queue, ensuring backpressure without polling.
    let (events_tx, events_rx) = mpsc::sync_channel::<Event>(EVENT_QUEUE_CAPACITY);

    let input_tx = events_tx.clone();
    let reader = thread::Builder::new()
        .name("mcp-stdin".into())
        .spawn(move || {
            let mut stdin = io::stdin().lock();
            loop {
                match read_input_line(&mut stdin) {
                    Ok(Some(InputLine::Message(line))) => {
                        if input_tx.send(Event::Input(line)).is_err() {
                            return;
                        }
                    }
                    Ok(Some(InputLine::TooLarge)) => {
                        if input_tx.send(Event::InputTooLarge).is_err() {
                            return;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        let _ = input_tx.send(Event::InputError(error.to_string()));
                        return;
                    }
                }
            }
            let _ = input_tx.send(Event::InputClosed);
        })?;

    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    let mut active = HashMap::<RequestId, ActiveCall>::new();
    let mut active_by_tool = vec![0usize; config.tools.len()];
    let mut task_workers = vec![0usize; config.tools.len()];
    let mut rate_limiters = config
        .tools
        .iter()
        .map(|definition| {
            FixedWindowRateLimiter::new(
                definition.rate_limit.count,
                definition.rate_limit.window_ms,
            )
        })
        .collect::<Vec<_>>();
    let mut closing = false;
    let mut failure = None::<String>;

    while !closing || !active.is_empty() || task_workers.iter().any(|count| *count != 0) {
        let event = if tasks.enabled() {
            match events_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(event) => event,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Err(error) = tasks.prune_all() {
                        failure = Some(format!("failed to prune expired tasks: {error}"));
                        closing = true;
                        cancel_all(&active, &mut tasks);
                    }
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("internal MCP event channel closed unexpectedly".into());
                }
            }
        } else {
            events_rx
                .recv()
                .map_err(|_| "internal MCP event channel closed unexpectedly")?
        };

        match event {
            Event::Input(_) if closing => {}
            Event::InputTooLarge if closing => {}
            Event::InputTooLarge => {
                let response = protocol::rpc_error(
                    None,
                    -32600,
                    "MCP stdio message exceeds the 8388608-byte limit",
                    None,
                );
                if let Err(error) = write_message(&mut stdout, &response) {
                    failure = Some(format!("failed to write MCP response: {error}"));
                    closing = true;
                    cancel_all(&active, &mut tasks);
                }
            }
            Event::Input(line) => match protocol::handle(&line, &config) {
                Action::Ignore => {}
                Action::Cancel(id) => {
                    if let Some(call) = active.get(&id) {
                        call.cancelled.store(true, Ordering::Release);
                    }
                }
                Action::Respond(response) => {
                    if let Err(error) = write_message(&mut stdout, &response) {
                        failure = Some(format!("failed to write MCP response: {error}"));
                        closing = true;
                        cancel_all(&active, &mut tasks);
                    }
                }
                Action::TaskGet { id, task_id } => {
                    let response = match tasks.get(&task_id) {
                        Ok(Some(fields)) => protocol::task_state_response(&id, &fields),
                        Ok(None) => unknown_task(&id),
                        Err(error) => task_store_error(&id, &error),
                    };
                    if let Err(error) = write_message(&mut stdout, &response) {
                        failure = Some(format!("failed to write MCP response: {error}"));
                        closing = true;
                        cancel_all(&active, &mut tasks);
                    }
                }
                Action::TaskUpdate { id, task_id } => {
                    let response = match tasks.acknowledge_update(&task_id) {
                        Ok(true) => protocol::task_ack(&id),
                        Ok(false) => unknown_task(&id),
                        Err(error) => task_store_error(&id, &error),
                    };
                    if let Err(error) = write_message(&mut stdout, &response) {
                        failure = Some(format!("failed to write MCP response: {error}"));
                        closing = true;
                        cancel_all(&active, &mut tasks);
                    }
                }
                Action::TaskCancel { id, task_id } => {
                    let response = match tasks.cancel(&task_id) {
                        Ok(true) => protocol::task_ack(&id),
                        Ok(false) => unknown_task(&id),
                        Err(error) => task_store_error(&id, &error),
                    };
                    if let Err(error) = write_message(&mut stdout, &response) {
                        failure = Some(format!("failed to write MCP response: {error}"));
                        closing = true;
                        cancel_all(&active, &mut tasks);
                    }
                }
                Action::Call {
                    id,
                    tool_index,
                    invocation,
                    task,
                } => {
                    if active.contains_key(&id) {
                        let response = protocol::rpc_error(
                            Some(&id),
                            -32600,
                            "request ID is already in flight",
                            None,
                        );
                        if let Err(error) = write_message(&mut stdout, &response) {
                            failure = Some(format!("failed to write MCP response: {error}"));
                            closing = true;
                            cancel_all(&active, &mut tasks);
                        }
                        continue;
                    }
                    let definition = &config.tools[tool_index];
                    let limits = definition.limits;
                    if let Err(retry_after_ms) = rate_limiters[tool_index].admit() {
                        let response = protocol::tool_error(
                            &id,
                            &format!(
                                "tool invocation rate limit reached; retry after approximately {retry_after_ms} ms"
                            ),
                        );
                        if let Err(error) = write_message(&mut stdout, &response) {
                            failure = Some(format!("failed to write MCP response: {error}"));
                            closing = true;
                            cancel_all(&active, &mut tasks);
                        }
                        continue;
                    }
                    if active_by_tool[tool_index] + task_workers[tool_index]
                        >= definition.limits.max_concurrency
                    {
                        let response = protocol::tool_error(
                            &id,
                            "process concurrency limit reached; retry after another call completes",
                        );
                        if let Err(error) = write_message(&mut stdout, &response) {
                            failure = Some(format!("failed to write MCP response: {error}"));
                            closing = true;
                            cancel_all(&active, &mut tasks);
                        }
                        continue;
                    }

                    if task {
                        let (task_id, cancelled, fields) = match tasks.create(tool_index) {
                            Ok(created) => created,
                            Err(error) => {
                                let response = protocol::tool_error(&id, &error);
                                if let Err(error) = write_message(&mut stdout, &response) {
                                    failure =
                                        Some(format!("failed to write MCP response: {error}"));
                                    closing = true;
                                    cancel_all(&active, &mut tasks);
                                }
                                continue;
                            }
                        };

                        let worker_tx = events_tx.clone();
                        let worker_task_id = task_id.clone();
                        let spawn =
                            thread::Builder::new()
                                .name("mcp-task".into())
                                .spawn(move || {
                                    let outcome = process::run(invocation, limits, cancelled);
                                    let _ = worker_tx.send(Event::TaskFinished {
                                        tool_index,
                                        task_id: worker_task_id,
                                        outcome,
                                    });
                                });

                        if let Err(error) = spawn {
                            let message = format!("failed to start task worker: {error}");
                            let response = match tasks.complete(
                                tool_index,
                                &task_id,
                                protocol::tool_error_result(&message),
                            ) {
                                Ok(Some(fields)) => protocol::task_response(&id, &fields),
                                Ok(None) => protocol::tool_error(&id, &message),
                                Err(store_error) => task_store_error(&id, &store_error),
                            };
                            if let Err(error) = write_message(&mut stdout, &response) {
                                failure = Some(format!("failed to write MCP response: {error}"));
                                closing = true;
                                cancel_all(&active, &mut tasks);
                            }
                            continue;
                        }
                        task_workers[tool_index] += 1;

                        let response = protocol::task_response(&id, &fields);
                        if let Err(error) = write_message(&mut stdout, &response) {
                            failure = Some(format!("failed to write MCP response: {error}"));
                            closing = true;
                            cancel_all(&active, &mut tasks);
                        }
                        continue;
                    }

                    let cancelled = Arc::new(AtomicBool::new(false));
                    active.insert(
                        id.clone(),
                        ActiveCall {
                            tool_index,
                            cancelled: Arc::clone(&cancelled),
                        },
                    );
                    active_by_tool[tool_index] += 1;

                    let worker_tx = events_tx.clone();
                    let worker_id = id.clone();
                    let spawn =
                        thread::Builder::new()
                            .name("mcp-tool-call".into())
                            .spawn(move || {
                                let outcome = process::run(invocation, limits, cancelled);
                                let _ = worker_tx.send(Event::CallFinished {
                                    id: worker_id,
                                    outcome,
                                });
                            });

                    if let Err(error) = spawn {
                        active.remove(&id);
                        active_by_tool[tool_index] -= 1;
                        let response = protocol::tool_error(
                            &id,
                            &format!("failed to start tool worker: {error}"),
                        );
                        if let Err(error) = write_message(&mut stdout, &response) {
                            failure = Some(format!("failed to write MCP response: {error}"));
                            closing = true;
                            cancel_all(&active, &mut tasks);
                        }
                    }
                }
            },
            Event::InputClosed => {
                closing = true;
                cancel_all(&active, &mut tasks);
            }
            Event::InputError(error) => {
                failure = Some(format!("failed to read MCP input: {error}"));
                closing = true;
                cancel_all(&active, &mut tasks);
            }
            Event::CallFinished { id, outcome } => {
                let call = active.remove(&id);
                if let Some(call) = &call {
                    active_by_tool[call.tool_index] =
                        active_by_tool[call.tool_index].saturating_sub(1);
                }
                let cancelled = call.is_some_and(|call| call.cancelled.load(Ordering::Acquire));
                if closing || cancelled {
                    continue;
                }

                let response = match outcome {
                    Ok(RunResult::Complete(output)) => protocol::tool_output(&id, output),
                    Ok(RunResult::Cancelled) => continue,
                    Err(error) => protocol::tool_error(&id, &error),
                };
                if let Err(error) = write_message(&mut stdout, &response) {
                    failure = Some(format!("failed to write MCP response: {error}"));
                    closing = true;
                    cancel_all(&active, &mut tasks);
                }
            }
            Event::TaskFinished {
                tool_index,
                task_id,
                outcome,
            } => {
                task_workers[tool_index] = task_workers[tool_index].saturating_sub(1);
                let persisted = match outcome {
                    Ok(RunResult::Complete(output)) => tasks
                        .complete(tool_index, &task_id, protocol::tool_output_result(output))
                        .map(|_| ()),
                    Ok(RunResult::Cancelled) => tasks.cancelled(tool_index, &task_id),
                    Err(error) => tasks
                        .complete(tool_index, &task_id, protocol::tool_error_result(&error))
                        .map(|_| ()),
                };
                if let Err(error) = persisted {
                    failure = Some(format!("failed to persist task result: {error}"));
                    closing = true;
                    cancel_all(&active, &mut tasks);
                }
            }
        }
    }

    drop(events_tx);
    drop(reader);

    if let Some(error) = failure {
        return Err(error.into());
    }
    Ok(())
}
