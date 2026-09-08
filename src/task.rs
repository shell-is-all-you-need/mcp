use crate::json;
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

pub(super) const EXTENSION_ID: &str = "io.modelcontextprotocol/tasks";

const LOCK_FILE: &str = ".store.lock";
const RECORD_SUFFIX: &str = ".jsonl";

#[derive(Clone, Default)]
pub(super) struct Config {
    pub(super) store_dir: Option<String>,
    pub(super) ttl_ms: Option<u64>,
    pub(super) poll_interval_ms: Option<u64>,
}

impl Config {
    pub(super) fn enabled(&self) -> bool {
        self.store_dir.is_some()
    }
}

#[derive(Clone)]
struct Record {
    task_id: String,
    status: Status,
    status_message: Option<String>,
    created_at: String,
    last_updated_at: String,
    created_unix_ms: u64,
    ttl_ms: Option<u64>,
    poll_interval_ms: Option<u64>,
    cancel_requested: bool,
    cancelled: Arc<AtomicBool>,
    result: Option<String>,
    error: Option<String>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Status {
    Working,
    Completed,
    Failed,
    Cancelled,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "working" => Some(Self::Working),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

pub(super) struct Store {
    config: Config,
    directory: Option<PathBuf>,
    _lock: Option<File>,
    records: HashMap<String, Record>,
}

impl Store {
    #[cfg(test)]
    pub(super) fn new(config: Config) -> Result<Self, String> {
        let directory = canonical_directory(&config)?;
        Self::new_prepared(config, directory)
    }

    pub(super) fn new_prepared(config: Config, directory: Option<PathBuf>) -> Result<Self, String> {
        if !config.enabled() {
            return Ok(Self {
                config,
                directory: None,
                _lock: None,
                records: HashMap::new(),
            });
        }

        let directory = directory.ok_or("enabled task store requires a prepared directory")?;
        let lock = acquire_lock(&directory)?;
        let mut probe = [0_u8; 1];
        random_bytes(&mut probe)?;

        let mut store = Self {
            config,
            directory: Some(directory),
            _lock: Some(lock),
            records: HashMap::new(),
        };
        store.load()?;
        store.prune_expired()?;
        store.recover_interrupted()?;
        Ok(store)
    }

    #[cfg(test)]
    pub(super) fn enabled(&self) -> bool {
        self.config.enabled()
    }

    pub(super) fn task_ids(&self) -> impl Iterator<Item = &str> {
        self.records.keys().map(String::as_str)
    }

    #[cfg(test)]
    pub(super) fn forget_for_test(&mut self, task_id: &str) -> Result<(), String> {
        self.remove_file(task_id)?;
        self.records.remove(task_id);
        Ok(())
    }

    pub(super) fn create(&mut self) -> Result<(String, Arc<AtomicBool>, String), String> {
        let (created_unix_ms, now) = now()?;
        let ttl_ms = self.config.ttl_ms;
        if let Some(ttl_ms) = ttl_ms {
            created_unix_ms
                .checked_add(ttl_ms)
                .ok_or("task TTL is too large for the current clock value")?;
        }

        loop {
            let task_id = task_id()?;
            if self.records.contains_key(&task_id) {
                continue;
            }
            let cancelled = Arc::new(AtomicBool::new(false));
            let record = Record {
                task_id: task_id.clone(),
                status: Status::Working,
                status_message: None,
                created_at: now.clone(),
                last_updated_at: now.clone(),
                created_unix_ms,
                ttl_ms,
                poll_interval_ms: self.config.poll_interval_ms,
                cancel_requested: false,
                cancelled: Arc::clone(&cancelled),
                result: None,
                error: None,
            };

            match self.persist_new(&record)? {
                true => {
                    let fields = record.fields();
                    self.records.insert(task_id.clone(), record);
                    return Ok((task_id, cancelled, fields));
                }
                false => continue,
            }
        }
    }

    pub(super) fn get(&mut self, task_id: &str) -> Result<Option<String>, String> {
        Ok(self.records.get(task_id).map(Record::fields))
    }

    pub(super) fn cancel(&mut self, task_id: &str) -> Result<bool, String> {
        let Some(current) = self.records.get(task_id) else {
            return Ok(false);
        };
        if current.status != Status::Working {
            return Ok(true);
        }

        let mut next = current.clone();
        next.cancel_requested = true;
        self.persist_append(&next)?;
        next.cancelled.store(true, Ordering::Release);
        self.records.insert(task_id.to_owned(), next);
        Ok(true)
    }

    pub(super) fn acknowledge_update(&mut self, task_id: &str) -> Result<bool, String> {
        Ok(self.records.contains_key(task_id))
    }

    pub(super) fn complete(
        &mut self,
        task_id: &str,
        result: String,
    ) -> Result<Option<String>, String> {
        let Some(current) = self.records.get(task_id) else {
            return Ok(None);
        };
        if current.status != Status::Working {
            return Ok(Some(current.fields()));
        }

        let mut next = current.clone();
        next.status = Status::Completed;
        next.result = Some(result);
        next.error = None;
        next.status_message = None;
        next.touch()?;
        self.persist_append(&next)?;
        let fields = next.fields();
        self.records.insert(task_id.to_owned(), next);
        Ok(Some(fields))
    }

    pub(super) fn cancelled(&mut self, task_id: &str) -> Result<(), String> {
        let Some(current) = self.records.get(task_id) else {
            return Ok(());
        };
        if current.status != Status::Working {
            return Ok(());
        }

        let mut next = current.clone();
        next.status = Status::Cancelled;
        next.result = None;
        next.error = None;
        next.status_message = None;
        next.touch()?;
        self.persist_append(&next)?;
        self.records.insert(task_id.to_owned(), next);
        Ok(())
    }

    pub(super) fn cancel_all(&mut self) -> Result<(), String> {
        let ids = self
            .records
            .iter()
            .filter_map(|(task_id, record)| {
                if record.status == Status::Working {
                    record.cancelled.store(true, Ordering::Release);
                    Some(task_id.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        for task_id in ids {
            self.cancel(&task_id)?;
        }
        Ok(())
    }

    fn load(&mut self) -> Result<(), String> {
        let directory = self.directory()?.to_path_buf();
        for entry in fs::read_dir(&directory)
            .map_err(|error| format!("cannot read task store {}: {error}", directory.display()))?
        {
            let entry = entry.map_err(|error| format!("cannot read task store entry: {error}"))?;
            let file_type = entry
                .file_type()
                .map_err(|error| format!("cannot inspect task store entry: {error}"))?;
            if !file_type.is_file() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name == LOCK_FILE {
                continue;
            }
            let Some(task_id) = name.strip_suffix(RECORD_SUFFIX) else {
                continue;
            };
            if !valid_task_id(task_id) {
                continue;
            }

            let Some(record) = load_record(&entry.path())? else {
                fs::remove_file(entry.path()).map_err(|error| {
                    format!(
                        "cannot remove uncommitted task record {}: {error}",
                        entry.path().display()
                    )
                })?;
                sync_directory(&directory)?;
                continue;
            };
            if record.task_id != task_id {
                return Err(format!(
                    "task store record {} contains mismatched taskId {:?}",
                    entry.path().display(),
                    record.task_id
                ));
            }
            if self.records.insert(task_id.to_owned(), record).is_some() {
                return Err(format!("duplicate task record {task_id:?}"));
            }
        }
        Ok(())
    }

    fn recover_interrupted(&mut self) -> Result<(), String> {
        let ids = self
            .records
            .iter()
            .filter_map(|(task_id, record)| {
                (record.status == Status::Working).then_some(task_id.clone())
            })
            .collect::<Vec<_>>();

        for task_id in ids {
            let current = self.records.get(&task_id).expect("collected task ID");
            let mut next = current.clone();
            if next.cancel_requested {
                next.status = Status::Cancelled;
                next.status_message =
                    Some("Cancellation was pending when the server stopped.".into());
                next.error = None;
            } else {
                next.status = Status::Failed;
                next.status_message = Some(
                    "Task execution was interrupted before the server could persist a final result."
                        .into(),
                );
                next.error = Some(
                    "{\"code\":-32603,\"message\":\"Task execution interrupted by server restart\"}"
                        .into(),
                );
            }
            next.result = None;
            next.touch()?;
            self.persist_append(&next)?;
            self.records.insert(task_id, next);
        }
        Ok(())
    }

    pub(super) fn prune_expired(&mut self) -> Result<Vec<String>, String> {
        let now_ms = unix_ms()?;
        let expired = self
            .records
            .iter()
            .filter_map(|(task_id, record)| {
                let ttl_ms = record.ttl_ms?;
                let expires_at = record.created_unix_ms.checked_add(ttl_ms)?;
                (now_ms >= expires_at).then_some(task_id.clone())
            })
            .collect::<Vec<_>>();

        for task_id in &expired {
            if let Some(record) = self.records.get(task_id) {
                record.cancelled.store(true, Ordering::Release);
            }
            self.remove_file(task_id)?;
            self.records.remove(task_id);
        }
        Ok(expired)
    }

    fn persist_new(&self, record: &Record) -> Result<bool, String> {
        let path = self.record_path(&record.task_id)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = match options.open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Ok(false);
            }
            Err(error) => {
                return Err(format!(
                    "cannot create task record {}: {error}",
                    path.display()
                ));
            }
        };
        write_snapshot(&mut file, record, &path)?;
        sync_directory(self.directory()?)?;
        Ok(true)
    }

    fn persist_append(&self, record: &Record) -> Result<(), String> {
        let path = self.record_path(&record.task_id)?;
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(|error| format!("cannot open task record {}: {error}", path.display()))?;
        write_snapshot(&mut file, record, &path)
    }

    fn remove_file(&self, task_id: &str) -> Result<(), String> {
        let path = self.record_path(task_id)?;
        match fs::remove_file(&path) {
            Ok(()) => sync_directory(self.directory()?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!(
                "cannot remove expired task record {}: {error}",
                path.display()
            )),
        }
    }

    fn directory(&self) -> Result<&Path, String> {
        self.directory
            .as_deref()
            .ok_or_else(|| "MCP Tasks are not enabled".into())
    }

    fn record_path(&self, task_id: &str) -> Result<PathBuf, String> {
        if !valid_task_id(task_id) {
            return Err("invalid internal task ID".into());
        }
        Ok(self.directory()?.join(format!("{task_id}{RECORD_SUFFIX}")))
    }
}

pub(super) fn canonical_directory(config: &Config) -> Result<Option<PathBuf>, String> {
    config
        .store_dir
        .as_deref()
        .map(|directory| prepare_directory(Path::new(directory)))
        .transpose()
}

impl Record {
    fn touch(&mut self) -> Result<(), String> {
        self.last_updated_at = now()?.1;
        Ok(())
    }

    fn fields(&self) -> String {
        let ttl = self
            .ttl_ms
            .map(|value| value.to_string())
            .unwrap_or_else(|| "null".into());
        let poll = self
            .poll_interval_ms
            .map(|value| format!(",\"pollIntervalMs\":{value}"))
            .unwrap_or_default();
        let status_message = self
            .status_message
            .as_ref()
            .map(|value| format!(",\"statusMessage\":{}", json::quote(value)))
            .unwrap_or_default();
        let result = self
            .result
            .as_ref()
            .map(|value| format!(",\"result\":{value}"))
            .unwrap_or_default();
        let error = self
            .error
            .as_ref()
            .map(|value| format!(",\"error\":{value}"))
            .unwrap_or_default();
        format!(
            ",\"taskId\":{},\"status\":{},\"createdAt\":{},\"lastUpdatedAt\":{},\"ttlMs\":{ttl}{poll}{status_message}{result}{error}",
            json::quote(&self.task_id),
            json::quote(self.status.as_str()),
            json::quote(&self.created_at),
            json::quote(&self.last_updated_at),
        )
    }

    fn snapshot(&self) -> String {
        let ttl = self
            .ttl_ms
            .map(|value| value.to_string())
            .unwrap_or_else(|| "null".into());
        let poll = self
            .poll_interval_ms
            .map(|value| value.to_string())
            .unwrap_or_else(|| "null".into());
        let status_message = self
            .status_message
            .as_ref()
            .map(|value| json::quote(value))
            .unwrap_or_else(|| "null".into());
        let result = self.result.as_deref().unwrap_or("null");
        let error = self.error.as_deref().unwrap_or("null");
        format!(
            "{{\"taskId\":{},\"status\":{},\"statusMessage\":{status_message},\"createdAt\":{},\"lastUpdatedAt\":{},\"createdUnixMs\":{},\"ttlMs\":{ttl},\"pollIntervalMs\":{poll},\"cancelRequested\":{},\"result\":{result},\"error\":{error}}}",
            json::quote(&self.task_id),
            json::quote(self.status.as_str()),
            json::quote(&self.created_at),
            json::quote(&self.last_updated_at),
            self.created_unix_ms,
            self.cancel_requested,
        )
    }
}

fn prepare_directory(path: &Path) -> Result<PathBuf, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder
            .create(path)
            .map_err(|error| format!("cannot create task store {}: {error}", path.display()))?;
    }
    #[cfg(not(unix))]
    fs::create_dir_all(path)
        .map_err(|error| format!("cannot create task store {}: {error}", path.display()))?;

    let canonical = fs::canonicalize(path)
        .map_err(|error| format!("cannot resolve task store {}: {error}", path.display()))?;
    if !canonical.is_dir() {
        return Err(format!("task store is not a directory: {}", path.display()));
    }
    Ok(canonical)
}

#[cfg(unix)]
fn acquire_lock(directory: &Path) -> Result<File, String> {
    use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};

    unsafe extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }
    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;

    let path = directory.join(LOCK_FILE);
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).mode(0o600);
    let file = options
        .open(&path)
        .map_err(|error| format!("cannot open task store lock {}: {error}", path.display()))?;
    // SAFETY: `file` owns a valid descriptor and flock does not dereference pointers.
    if unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        return Err(format!(
            "cannot exclusively lock task store {}: {error}",
            directory.display()
        ));
    }
    Ok(file)
}

#[cfg(windows)]
fn acquire_lock(directory: &Path) -> Result<File, String> {
    use std::os::windows::fs::OpenOptionsExt;

    let path = directory.join(LOCK_FILE);
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .share_mode(0)
        .open(&path)
        .map_err(|error| {
            format!(
                "cannot exclusively lock task store {}: {error}",
                directory.display()
            )
        })
}

#[cfg(not(any(unix, windows)))]
fn acquire_lock(_directory: &Path) -> Result<File, String> {
    Err("durable MCP Tasks require task-store locking support on Unix or Windows".into())
}

fn write_snapshot(file: &mut File, record: &Record, path: &Path) -> Result<(), String> {
    let snapshot = record.snapshot();
    file.write_all(snapshot.as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("cannot persist task record {}: {error}", path.display()))
}

#[cfg(unix)]
fn sync_directory(directory: &Path) -> Result<(), String> {
    File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(|error| format!("cannot sync task store {}: {error}", directory.display()))
}

#[cfg(not(unix))]
fn sync_directory(_directory: &Path) -> Result<(), String> {
    Ok(())
}

fn load_record(path: &Path) -> Result<Option<Record>, String> {
    let bytes = fs::read(path)
        .map_err(|error| format!("cannot read task record {}: {error}", path.display()))?;

    let committed_len = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    if committed_len < bytes.len() {
        let file = OpenOptions::new()
            .write(true)
            .open(path)
            .map_err(|error| format!("cannot repair task record {}: {error}", path.display()))?;
        file.set_len(committed_len as u64)
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("cannot repair task record {}: {error}", path.display()))?;
    }
    if committed_len == 0 {
        return Ok(None);
    }

    let text = std::str::from_utf8(&bytes[..committed_len])
        .map_err(|_| format!("task record is not UTF-8: {}", path.display()))?;
    let mut record = None;
    for (index, line) in text.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        record = Some(parse_record(line).map_err(|error| {
            format!(
                "invalid task record {} at line {}: {error}",
                path.display(),
                index + 1
            )
        })?);
    }
    Ok(record)
}

fn parse_record(raw: &str) -> Result<Record, String> {
    let entries = json::object_entries(raw).ok_or("snapshot must be a JSON object")?;
    const FIELDS: [&str; 11] = [
        "taskId",
        "status",
        "statusMessage",
        "createdAt",
        "lastUpdatedAt",
        "createdUnixMs",
        "ttlMs",
        "pollIntervalMs",
        "cancelRequested",
        "result",
        "error",
    ];
    if entries.len() != FIELDS.len()
        || entries
            .iter()
            .any(|(name, _)| !FIELDS.contains(&name.as_str()))
    {
        return Err("snapshot fields do not match the current task-store format".into());
    }

    let get = |name: &str| {
        entries
            .iter()
            .find_map(|(key, value)| (key == name).then_some(*value))
            .ok_or_else(|| format!("missing field {name:?}"))
    };

    let task_id = json::string(get("taskId")?).ok_or("taskId must be a string")?;
    if !valid_task_id(&task_id) {
        return Err("taskId has an invalid format".into());
    }
    let status = json::string(get("status")?)
        .and_then(|value| Status::parse(&value))
        .ok_or("status is invalid")?;
    let status_message = nullable_string(get("statusMessage")?)?;
    let created_at = json::string(get("createdAt")?).ok_or("createdAt must be a string")?;
    let last_updated_at =
        json::string(get("lastUpdatedAt")?).ok_or("lastUpdatedAt must be a string")?;
    let created_unix_ms = u64_value(get("createdUnixMs")?, "createdUnixMs")?;
    let ttl_ms = nullable_u64(get("ttlMs")?, "ttlMs")?;
    let poll_interval_ms = nullable_u64(get("pollIntervalMs")?, "pollIntervalMs")?;
    let cancel_requested =
        json::boolean(get("cancelRequested")?).ok_or("cancelRequested must be a boolean")?;
    let result = nullable_object(get("result")?, "result")?;
    let error = nullable_object(get("error")?, "error")?;

    match status {
        Status::Working | Status::Cancelled if result.is_some() || error.is_some() => {
            return Err("working/cancelled tasks cannot contain result or error".into());
        }
        Status::Completed if result.is_none() || error.is_some() => {
            return Err("completed tasks must contain result and no error".into());
        }
        Status::Failed if error.is_none() || result.is_some() => {
            return Err("failed tasks must contain error and no result".into());
        }
        _ => {}
    }

    Ok(Record {
        task_id,
        status,
        status_message,
        created_at,
        last_updated_at,
        created_unix_ms,
        ttl_ms,
        poll_interval_ms,
        cancel_requested,
        cancelled: Arc::new(AtomicBool::new(false)),
        result,
        error,
    })
}

fn nullable_string(raw: &str) -> Result<Option<String>, String> {
    if raw.trim() == "null" {
        return Ok(None);
    }
    json::string(raw)
        .map(Some)
        .ok_or_else(|| "expected string or null".into())
}

fn u64_value(raw: &str, name: &str) -> Result<u64, String> {
    if !json::integer(raw) {
        return Err(format!("{name} must be a non-negative integer"));
    }
    raw.trim()
        .parse::<u64>()
        .map_err(|_| format!("{name} must fit in u64"))
}

fn nullable_u64(raw: &str, name: &str) -> Result<Option<u64>, String> {
    if raw.trim() == "null" {
        return Ok(None);
    }
    let value = u64_value(raw, name)?;
    if value > 9_007_199_254_740_991 {
        return Err(format!("{name} exceeds the MCP safe integer maximum"));
    }
    Ok(Some(value))
}

fn nullable_object(raw: &str, name: &str) -> Result<Option<String>, String> {
    if raw.trim() == "null" {
        return Ok(None);
    }
    if json::object_entries(raw).is_none() {
        return Err(format!("{name} must be an object or null"));
    }
    Ok(Some(raw.to_owned()))
}

fn valid_task_id(value: &str) -> bool {
    value.len() == 37
        && value.starts_with("task-")
        && value[5..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn task_id() -> Result<String, String> {
    let mut bytes = [0_u8; 16];
    random_bytes(&mut bytes)?;
    let mut output = String::with_capacity(37);
    output.push_str("task-");
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    Ok(output)
}

#[cfg(unix)]
fn random_bytes(output: &mut [u8]) -> Result<(), String> {
    use std::io::Read as _;
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(output))
        .map_err(|error| format!("cannot obtain OS randomness for task ID: {error}"))
}

#[cfg(windows)]
fn random_bytes(output: &mut [u8]) -> Result<(), String> {
    use std::ffi::c_void;

    #[link(name = "bcrypt")]
    unsafe extern "system" {
        #[link_name = "BCryptGenRandom"]
        fn bcrypt_gen_random(
            algorithm: *mut c_void,
            buffer: *mut u8,
            length: u32,
            flags: u32,
        ) -> i32;
    }

    const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;
    let length = u32::try_from(output.len()).map_err(|_| "random buffer is too large")?;
    let status = unsafe {
        bcrypt_gen_random(
            std::ptr::null_mut(),
            output.as_mut_ptr(),
            length,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(format!(
            "cannot obtain OS randomness for task ID: BCryptGenRandom returned 0x{:08x}",
            status as u32
        ))
    }
}

#[cfg(not(any(unix, windows)))]
fn random_bytes(_output: &mut [u8]) -> Result<(), String> {
    Err("MCP Tasks require an operating-system random source on this platform".into())
}

fn unix_ms() -> Result<u64, String> {
    let milliseconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "system clock is before the Unix epoch")?
        .as_millis();
    u64::try_from(milliseconds).map_err(|_| "system time is out of range".into())
}

fn now() -> Result<(u64, String), String> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "system clock is before the Unix epoch")?;
    let milliseconds =
        u64::try_from(duration.as_millis()).map_err(|_| "system time is out of range")?;
    let seconds = duration.as_secs();
    let days = i64::try_from(seconds / 86_400).map_err(|_| "system time is out of range")?;
    let seconds_of_day = seconds % 86_400;
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    let (year, month, day) = civil_from_days(days);
    let timestamp = format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{:03}Z",
        duration.subsec_millis()
    );
    Ok((milliseconds, timestamp))
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += if month <= 2 { 1 } else { 0 };
    (year, month, day)
}
