use crate::{Result, agents, batch::WORKSPACE_FILES, coordinator, now_ms};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    env, fmt, fs,
    io::{self, BufRead, BufReader, Write},
    net::{TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::Duration,
};

pub const STALL_LIMIT: u32 = 3;
const TIMEOUT: Duration = Duration::from_secs(30);
const PAUSE: Duration = Duration::from_secs(5);
const TOKENS: [&str; 4] = ["HARNESS_API_TOKEN", "HARNESS_JUDGE_TOKEN", "HARNESS_RUN_TOKEN", "HARNESS_JUDGMENT_TOKEN"];

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Subcommand {
    Session,
    Planner,
}

pub fn subcommand(args: &[String]) -> Result<Option<Subcommand>> {
    let (selected, input) = match args.get(1).map(String::as_str) {
        Some("session") => (Subcommand::Session, "HARNESS_WORKFLOW_REQUEST"),
        Some("planner") => (Subcommand::Planner, "HARNESS_WORKSPACE_PLANNER"),
        _ => return Ok(None),
    };
    if args.len() > 2 {
        return Err(format!("harness-bench {} takes no arguments; it reads {input} from the environment", args[1]).into());
    }
    Ok(Some(selected))
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64() != Some(0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(fields) => !fields.is_empty(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Step {
    Finished,
    Advance,
    Continue,
    Stop,
}
impl Step {
    pub fn name(self) -> &'static str {
        match self {
            Step::Finished => "finished",
            Step::Advance => "advance",
            Step::Continue => "continue",
            Step::Stop => "stop",
        }
    }
}

pub fn next_step(exit_code: Option<i64>, state: &Value, stalls: u32) -> (Step, u32) {
    if !truthy(&state["open"]) {
        return (Step::Finished, stalls);
    }
    if exit_code == Some(0) && truthy(&state["submitted"]) {
        return (Step::Advance, 0);
    }
    let stalls = stalls + 1;
    (if stalls >= STALL_LIMIT { Step::Stop } else { Step::Continue }, stalls)
}

pub fn can_still_run(task: &Value) -> bool {
    if truthy(&task["container"]) {
        return false;
    }
    truthy(&task["active"]) || !(task["stage"] == "done" || truthy(&task["stopped"]) || truthy(&task["blocked"]))
}

fn stripped(path: &str) -> String {
    path.to_lowercase().trim_start_matches(['\\', '?']).to_owned()
}

fn workspace_root(workspace: &Path) -> String {
    let resolved = fs::canonicalize(workspace).or_else(|_| std::path::absolute(workspace)).unwrap_or_else(|_| workspace.to_path_buf());
    stripped(&resolved.to_string_lossy())
}

pub fn active_tasks(tasks: &[Value], root: &str) -> Vec<Value> {
    tasks.iter().filter(|task| truthy(&task["active"]) && stripped(task["cwd"].as_str().unwrap_or("")).starts_with(root)).cloned().collect()
}

pub fn runnable_tasks(tasks: &[Value]) -> Vec<Value> {
    tasks.iter().filter(|task| can_still_run(task)).cloned().collect()
}

#[derive(Debug)]
pub enum HttpError {
    Transport(String),
    Status(u16, String),
    Invalid(String),
}
impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HttpError::Transport(message) | HttpError::Invalid(message) => f.write_str(message),
            HttpError::Status(status, body) => write!(f, "HTTP {status}: {body}"),
        }
    }
}
impl std::error::Error for HttpError {}

fn quote(text: &str) -> String {
    text.bytes().map(|b| if b.is_ascii_alphanumeric() || b"_.-~/".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

fn read_response(reader: &mut impl BufRead) -> io::Result<(u16, Vec<u8>)> {
    let invalid = |message: &str| io::Error::new(io::ErrorKind::InvalidData, message.to_owned());
    let mut line = String::new();
    loop {
        line.clear();
        reader.read_line(&mut line)?;
        let status: u16 = line.split_whitespace().nth(1).and_then(|code| code.parse().ok()).ok_or_else(|| invalid("malformed HTTP status line"))?;
        let (mut length, mut chunked) = (None, false);
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            let header = line.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    length = Some(value.trim().parse::<usize>().map_err(|_| invalid("bad Content-Length"))?);
                } else if name.eq_ignore_ascii_case("transfer-encoding") && value.to_ascii_lowercase().contains("chunked") {
                    chunked = true;
                }
            }
        }
        if (100..200).contains(&status) {
            continue;
        }
        let mut body = Vec::new();
        if chunked {
            loop {
                line.clear();
                reader.read_line(&mut line)?;
                let size = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16).map_err(|_| invalid("bad chunk size"))?;
                if size == 0 {
                    while reader.read_line(&mut line)? > 0 && !line.trim().is_empty() {
                        line.clear();
                    }
                    break;
                }
                let start = body.len();
                body.resize(start + size, 0);
                reader.read_exact(&mut body[start..])?;
                line.clear();
                reader.read_line(&mut line)?;
            }
        } else if let Some(length) = length {
            body.resize(length, 0);
            reader.read_exact(&mut body)?;
        } else {
            reader.read_to_end(&mut body)?;
        }
        return Ok((status, body));
    }
}

pub struct Client {
    pub url: String,
    token: String,
}
impl Client {
    pub fn new(url: &str, token: &str) -> Self {
        Self { url: url.trim_end_matches('/').to_owned(), token: token.to_owned() }
    }
    pub fn request(&self, method: &str, path: &str, body: Option<&Value>) -> std::result::Result<Value, HttpError> {
        let rest = self.url.strip_prefix("http://").ok_or_else(|| HttpError::Invalid(format!("unsupported URL {}", self.url)))?;
        let (authority, prefix) = rest.split_once('/').unwrap_or((rest, ""));
        let target = if prefix.is_empty() { path.to_owned() } else { format!("/{prefix}{path}") };
        let address = if authority.rsplit_once(':').is_some_and(|(_, port)| port.parse::<u16>().is_ok()) { authority.to_owned() } else { format!("{authority}:80") };
        let transport = |error: io::Error| HttpError::Transport(format!("{method} {}{path}: {error}", self.url));
        let mut last = io::Error::new(io::ErrorKind::NotFound, "no address");
        let mut connected = None;
        for candidate in address.to_socket_addrs().map_err(transport)? {
            match TcpStream::connect_timeout(&candidate, TIMEOUT) {
                Ok(stream) => {
                    connected = Some(stream);
                    break;
                }
                Err(error) => last = error,
            }
        }
        let mut stream = connected.ok_or_else(|| transport(last))?;
        stream.set_read_timeout(Some(TIMEOUT)).map_err(transport)?;
        stream.set_write_timeout(Some(TIMEOUT)).map_err(transport)?;
        let payload = body.map(|value| value.to_string().into_bytes());
        let mut head = format!(
            "{method} {target} HTTP/1.1\r\nHost: {authority}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nAccept-Encoding: identity\r\nConnection: close\r\n",
            self.token
        );
        if let Some(payload) = &payload {
            head += &format!("Content-Length: {}\r\n", payload.len());
        }
        head += "\r\n";
        stream.write_all(head.as_bytes()).map_err(transport)?;
        stream.write_all(payload.as_deref().unwrap_or_default()).map_err(transport)?;
        let (status, bytes) = read_response(&mut BufReader::new(stream)).map_err(transport)?;
        if !(200..300).contains(&status) {
            return Err(HttpError::Status(status, String::from_utf8_lossy(&bytes).into_owned()));
        }
        serde_json::from_slice(&bytes).map_err(|error| HttpError::Invalid(format!("{method} {}{path}: invalid JSON: {error}", self.url)))
    }
    fn tasks(&self, path: &str) -> std::result::Result<Vec<Value>, HttpError> {
        let mut value = self.request("GET", path, None)?;
        match value.get_mut("tasks").map(Value::take) {
            Some(Value::Array(tasks)) => Ok(tasks),
            _ => Err(HttpError::Invalid(format!("GET {}{path}: response has no tasks list", self.url))),
        }
    }
}

fn family() -> Option<&'static str> {
    agents::detect(|name| env::var(name).ok().filter(|value| !value.is_empty()))
}

pub fn resolve(selection: &Value, family: Option<&str>, retired: bool) -> Result<Value> {
    let name = selection.as_str().map_or_else(|| selection.to_string(), str::to_owned);
    let catalog = agents::catalog();
    let agent = selection.as_str().map(|id| catalog[id].clone()).filter(truthy).ok_or_else(|| format!("Unknown agent {name}"))?;
    let vendor = match (agent["provider"].as_str(), agent["harness"].as_str()) {
        (Some("openai"), Some("codex")) => "codex",
        (Some("anthropic"), Some("claude_code")) => "claude",
        _ => "",
    };
    if vendor.is_empty() || agent["model"].as_str().unwrap_or("").trim().is_empty() {
        return Err(format!("Agent {name} must run in its own vendor's harness").into());
    }
    if truthy(&agent["retired"]) && !retired {
        return Err(format!("Agent {name} is retired").into());
    }
    if Some(vendor) != family {
        return Err(format!("Agent {name} is not allowed when started by {}", family.unwrap_or("an undetected launcher")).into());
    }
    Ok(agent)
}

pub fn planner_command(command: &[String], agent: &Value) -> Vec<String> {
    let model = agent["model"].as_str().unwrap_or("");
    let effort = agent["effort"].as_str().unwrap_or("");
    command.iter().map(|arg| arg.replace("{model}", model).replace("{effort}", effort)).collect()
}

fn documents(directory: &Path, settings: &Value) -> Result<()> {
    for (role, filename) in [("project_planner", "PROJECT_PLANNER.md"), ("worker_protocol", "WORKER_PROTOCOL.md")] {
        let selected = &settings["prompts"][role];
        let candidate = selected["candidate"].as_str().unwrap_or("");
        if candidate.is_empty() || candidate == "." || candidate == ".." || candidate.contains(['/', '\\', ':']) {
            return Err("Workspace prompt candidate is required".into());
        }
        let path = directory.join(filename);
        if !fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
            return Err(format!("Workspace prompt must be a regular file: {}", path.display()).into());
        }
        let content = fs::read(&path)?;
        if !(1..=200).contains(&std::str::from_utf8(&content)?.split_whitespace().count()) {
            return Err(format!("Workspace prompt must contain 1..200 words: {}", path.display()).into());
        }
        if selected["sha256"] != format!("{:x}", Sha256::digest(&content)) {
            return Err(format!("Workspace prompt changed: {}", path.display()).into());
        }
    }
    Ok(())
}

fn exit_code(status: ExitStatus) -> Option<i64> {
    #[cfg(windows)]
    let code = status.code().map(|code| i64::from(code as u32));
    #[cfg(unix)]
    let code = {
        use std::os::unix::process::ExitStatusExt;
        status.code().map(i64::from).or_else(|| status.signal().map(|signal| -i64::from(signal)))
    };
    code
}

fn text_file(text: &str) -> String {
    if cfg!(windows) { text.replace('\n', "\r\n") } else { text.to_owned() }
}

struct Runner {
    root: PathBuf,
    workspace: PathBuf,
    command: Vec<String>,
    env: Vec<(String, Option<String>)>,
    message: Option<PathBuf>,
    progress: Value,
}
impl Runner {
    fn update(&mut self, fields: Value) {
        for (key, value) in fields.as_object().into_iter().flatten() {
            self.progress[key] = value.clone();
        }
    }
    fn save(&self) -> Result<()> {
        fs::write(self.root.join("progress.json"), self.progress.to_string())?;
        Ok(())
    }
    fn launch(&mut self, message: Option<&str>) -> Result<Child> {
        if let Some(message) = message {
            if !self.progress["messages"].is_array() {
                self.progress["messages"] = json!([]);
            }
            let messages = self.progress["messages"].as_array_mut().unwrap();
            let file = self.root.join(format!("planner-message-{}.txt", messages.len()));
            fs::write(&file, text_file(message))?;
            messages.push(json!({"file":file.to_string_lossy(),"at_ms":now_ms()}));
            self.message = Some(file);
        }
        let log = fs::OpenOptions::new().create(true).append(true).open(self.root.join("planner.log"))?;
        let mut command = Command::new(&self.command[0]);
        command.args(&self.command[1..]).current_dir(&self.workspace).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
        for (key, value) in &self.env {
            match value {
                Some(value) => command.env(key, value),
                None => command.env_remove(key),
            };
        }
        if let Some(file) = &self.message {
            command.env("HARNESS_PLANNER_MESSAGE", file);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let planner = command.spawn().map_err(|error| format!("could not start planner {:?}: {error}", self.command[0]))?;
        self.update(json!({"phase":"running","planner_pid":planner.id()}));
        self.save()?;
        Ok(planner)
    }
}

fn field<'a>(value: &'a Value, name: &str) -> Result<&'a str> {
    value[name].as_str().ok_or_else(|| format!("launch request is missing {name}").into())
}

pub fn session() -> Result<()> {
    let request = env::var_os("HARNESS_WORKFLOW_REQUEST").filter(|value| !value.is_empty()).ok_or("HARNESS_WORKFLOW_REQUEST is required")?;
    run_session(Path::new(&request), family(), PAUSE)
}

pub fn run_session(launch_path: &Path, family: Option<&str>, pause: Duration) -> Result<()> {
    let launch: Value = serde_json::from_slice(&fs::read(launch_path)?)?;
    let root = launch_path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let (profile, started) = (&launch["profile"], &launch["started"]);
    let settings = &profile["workspace"];
    let agent = resolve(&settings["planner"], family, false)?;
    let command = planner_command(&coordinator::command(&profile["planner_command"], "planner")?, &agent);
    let workspace_text = field(profile, "workspace_path")?;
    let workspace = PathBuf::from(workspace_text);
    let names = fs::read_dir(&workspace)?.map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned())).collect::<io::Result<BTreeSet<_>>>()?;
    if names != WORKSPACE_FILES.iter().map(|name| name.to_string()).collect() {
        return Err("planner workspace must contain only its instruction documents and LIMITS.json".into());
    }
    documents(&workspace, settings)?;
    let prompt_file = root.join("planner-prompt.txt");
    fs::write(&prompt_file, field(profile, "template_prompt")?)?;
    let coordinator = Client::new(field(profile, "coordinator_url")?, "");
    let run_id = field(&started["run"], "id")?;
    let handoff = started["handoff_token"].as_str().filter(|token| !token.is_empty());
    let mut env: Vec<(String, Option<String>)> = TOKENS.iter().map(|key| (key.to_string(), None)).collect();
    for (key, value) in profile["env"].as_object().into_iter().flatten() {
        env.push((key.clone(), Some(value.as_str().map_or_else(|| value.to_string(), str::to_owned))));
    }
    env.extend(["HARNESS_WORKFLOW_REQUEST", "CODEX_WINDOWS_REGISTERED_CORE"].map(|key| (key.to_owned(), None)));
    env.extend(
        [
            ("COORDINATOR_URL", coordinator.url.clone()),
            ("HARNESS_URL", field(&launch, "url")?.to_owned()),
            ("HARNESS_RUN_ID", run_id.to_owned()),
            ("HARNESS_RUN_TOKEN", field(started, "run_token")?.to_owned()),
            ("HARNESS_PLANNER_PROMPT", prompt_file.to_string_lossy().into_owned()),
            ("HARNESS_WORKSPACE_PLANNER", field(settings, "planner")?.to_owned()),
            ("AGENT_FAMILY", family.unwrap_or("").to_owned()),
        ]
        .map(|(key, value)| (key.to_owned(), Some(value))),
    );
    if handoff.is_some() {
        env.push(("HARNESS_PLANNER_SESSION".into(), Some(root.join("planner-session.json").to_string_lossy().into_owned())));
    }
    let progress = json!({"mode":"template","phase":"running","workspace":workspace_text,"coordinator_url":coordinator.url,"run_id":run_id});
    let mut runner = Runner { root, workspace: workspace.clone(), command, env, message: None, progress };
    let mut planner = runner.launch(None)?;
    let Some(handoff) = handoff else {
        let code = exit_code(planner.wait()?);
        runner.update(json!({"phase":"planner_exited","planner_exit_code":code}));
        runner.save()?;
        let path = format!("/api/tasks?workspace={}", quote(workspace_text));
        loop {
            match coordinator.tasks(&path) {
                Ok(tasks) if runnable_tasks(&tasks).is_empty() => {
                    runner.update(json!({"finished_at_ms":now_ms()}));
                    return runner.save();
                }
                Ok(_) | Err(HttpError::Transport(_)) => {}
                Err(error) => return Err(error.into()),
            }
            thread::sleep(pause);
        }
    };
    let supervisor = Client::new(field(&launch, "url")?, handoff);
    let path = format!("/v1/runs/{run_id}/handoff");
    let root_prefix = workspace_root(&workspace);
    let mut stalls = 0;
    loop {
        let code = exit_code(planner.wait()?);
        runner.update(json!({"phase":"waiting_for_workers","planner_exit_code":code}));
        runner.save()?;
        while !active_tasks(&coordinator.tasks("/api/state")?, &root_prefix).is_empty() {
            thread::sleep(pause);
        }
        let state = supervisor.request("POST", &path, Some(&json!({"action":"current"})))?;
        let (step, count) = next_step(code, &state, stalls);
        stalls = count;
        runner.update(json!({"request":state["request"],"stalls":stalls,"last_action":step.name()}));
        let message = match step {
            Step::Finished => break,
            Step::Stop => {
                supervisor.request("POST", &path, Some(&json!({"action":"stop"})))?;
                break;
            }
            Step::Advance => match supervisor.request("POST", &path, Some(&json!({"action":"advance"})))?["next"].as_str() {
                Some(next) => Some(next.to_owned()),
                None => break,
            },
            Step::Continue => state["continue"].as_str().map(str::to_owned),
        };
        planner = runner.launch(message.as_deref())?;
    }
    runner.update(json!({"phase":"finished"}));
    runner.save()
}

pub struct Plan {
    pub argv: Vec<String>,
    pub saved: Value,
    pub capture: bool,
}

fn codex_arguments(agent: &Value) -> [String; 4] {
    let (model, effort) = (agent["model"].as_str().unwrap_or(""), agent["effort"].as_str().unwrap_or(""));
    ["--model".into(), model.into(), "-c".into(), format!("model_reasoning_effort=\"{effort}\"")]
}

pub fn claude_arguments(agent: &Value, executable: &str) -> Vec<String> {
    let mut argv: Vec<String> = [executable, "-p", "--model", agent["model"].as_str().unwrap_or(""), "--output-format", "stream-json", "--verbose", "--no-session-persistence", "--dangerously-skip-permissions"]
        .map(str::to_owned)
        .into();
    if let Some(effort) = agent["effort"].as_str().filter(|effort| !effort.is_empty()) {
        argv.extend(["--effort".into(), effort.to_owned()]);
    }
    argv
}

pub fn plan(agent: &Value, persistent: bool, saved: &Value, message: bool, executable: &str, new_session: impl FnOnce() -> String) -> Plan {
    let session = if message { saved["session"].as_str() } else { None };
    if agent["harness"] == "codex" {
        let mut argv = vec![executable.to_owned(), "exec".into()];
        if let Some(session) = session.filter(|session| !session.is_empty()) {
            argv.extend(["resume", "--json", "--skip-git-repo-check", "--dangerously-bypass-approvals-and-sandbox"].map(str::to_owned));
            argv.extend(codex_arguments(agent));
            argv.extend([session.to_owned(), "-".into()]);
        } else {
            argv.push("--skip-git-repo-check".into());
            if !persistent {
                argv.push("--ephemeral".into());
            }
            argv.extend(["--json", "--sandbox", "danger-full-access", "-c", "approval_policy=\"never\""].map(str::to_owned));
            argv.extend(codex_arguments(agent));
            argv.push("-".into());
        }
        return Plan { argv, saved: saved.clone(), capture: persistent && session.is_none() };
    }
    let mut argv: Vec<String> = claude_arguments(agent, executable).into_iter().filter(|arg| !(persistent && arg == "--no-session-persistence")).collect();
    if persistent && session.is_none() {
        let id = new_session();
        argv.extend(["--session-id".into(), id.clone()]);
        return Plan { argv, saved: json!({"harness":"claude_code","session":id}), capture: false };
    }
    if let Some(session) = session.filter(|session| persistent && !session.is_empty()) {
        argv.extend(["--resume".into(), session.to_owned()]);
    }
    Plan { argv, saved: saved.clone(), capture: false }
}

pub fn hosted_claude_variable(name: &str) -> bool {
    let name = if cfg!(windows) { name.to_uppercase() } else { name.to_owned() };
    ["CLAUDECODE", "CLAUDE_PID", "CLAUDE_EFFORT"].contains(&name.as_str()) || ["CLAUDE_CODE_", "CLAUDE_AGENT_SDK", "CLAUDE_PREVIEW"].iter().any(|prefix| name.starts_with(prefix))
}

fn which(name: &str) -> Option<PathBuf> {
    let extensions: Vec<String> = if cfg!(windows) {
        env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD;.VBS;.JS;.WS;.MSC".into()).split(';').filter(|ext| !ext.is_empty()).map(str::to_owned).collect()
    } else {
        vec![String::new()]
    };
    env::split_paths(&env::var_os("PATH")?).find_map(|directory| {
        extensions.iter().map(|ext| directory.join(format!("{name}{ext}"))).find(|candidate| runnable_file(candidate))
    })
}

#[cfg(unix)]
fn runnable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}
#[cfg(not(unix))]
fn runnable_file(path: &Path) -> bool {
    path.is_file()
}

pub fn claude_executable(found: Option<PathBuf>) -> String {
    let Some(found) = found else { return "claude".into() };
    if found.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("cmd")) {
        let native = found.parent().unwrap_or(Path::new("")).join("node_modules").join("@anthropic-ai").join("claude-code").join("bin").join("claude.exe");
        if native.is_file() {
            return native.to_string_lossy().into_owned();
        }
    }
    found.to_string_lossy().into_owned()
}

pub fn thread_started(line: &[u8]) -> Option<Value> {
    let event: Value = serde_json::from_slice(line).ok()?;
    (event["type"] == "thread.started" && truthy(&event["thread_id"])).then(|| event["thread_id"].clone())
}

fn optional_path(name: &str) -> Option<PathBuf> {
    env::var_os(name).filter(|value| !value.is_empty()).map(PathBuf::from)
}

pub fn planner() -> Result<i32> {
    let selection = env::var("HARNESS_WORKSPACE_PLANNER").map_err(|_| "HARNESS_WORKSPACE_PLANNER is required")?;
    let agent = resolve(&json!(selection), family(), false)?;
    let prompt = fs::read(env::var_os("HARNESS_PLANNER_PROMPT").ok_or("HARNESS_PLANNER_PROMPT is required")?)?;
    let session_file = optional_path("HARNESS_PLANNER_SESSION");
    let message = optional_path("HARNESS_PLANNER_MESSAGE").map(fs::read).transpose()?;
    let saved = match &session_file {
        Some(file) if file.exists() => serde_json::from_slice(&fs::read(file)?)?,
        _ => json!({}),
    };
    let codex = agent["harness"] == "codex";
    let executable = if codex {
        env::var("CODEX_BIN").unwrap_or_else(|_| "codex".into())
    } else {
        env::var("CLAUDE_BIN").ok().filter(|bin| !bin.is_empty()).unwrap_or_else(|| claude_executable(which("claude")))
    };
    let Plan { argv, mut saved, capture } = plan(&agent, session_file.is_some(), &saved, message.is_some(), &executable, crate::id);
    if let Some(file) = session_file.as_ref().filter(|_| truthy(&saved)) {
        fs::write(file, saved.to_string())?;
    }
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]).stdin(Stdio::piped()).stdout(if capture { Stdio::piped() } else { Stdio::inherit() });
    for key in ["CODEX_WINDOWS_REGISTERED_CORE", "HARNESS_PLANNER_PROMPT", "HARNESS_PLANNER_SESSION", "HARNESS_PLANNER_MESSAGE"] {
        command.env_remove(key);
    }
    if !codex {
        for (key, _) in env::vars_os() {
            if hosted_claude_variable(&key.to_string_lossy()) {
                command.env_remove(key);
            }
        }
    }
    let mut child = command.spawn().map_err(|error| format!("could not start {:?}: {error}", argv[0]))?;
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(message.as_deref().unwrap_or(&prompt[..]))?;
    drop(stdin);
    if let (true, Some(stdout)) = (capture, child.stdout.take()) {
        let mut reader = BufReader::new(stdout);
        let mut out = io::stdout().lock();
        let mut line = Vec::new();
        while reader.read_until(b'\n', &mut line)? > 0 {
            out.write_all(&line)?;
            out.flush()?;
            if !truthy(&saved["session"]) {
                if let (Some(thread), Some(file)) = (thread_started(&line), &session_file) {
                    saved = json!({"harness":"codex","session":thread});
                    fs::write(file, saved.to_string())?;
                }
            }
            line.clear();
        }
    }
    Ok(exit_code(child.wait()?).unwrap_or(1) as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn hidden_subcommands_take_their_inputs_from_the_environment_only() {
        assert_eq!(subcommand(&args(&["harness-bench", "session"])).unwrap(), Some(Subcommand::Session));
        assert_eq!(subcommand(&args(&["harness-bench", "planner"])).unwrap(), Some(Subcommand::Planner));
        for other in [&["harness-bench"][..], &["harness-bench", "serve"][..], &["harness-bench", "--bind", "127.0.0.1:0"][..], &["harness-bench", "baseline", "random"][..]] {
            assert_eq!(subcommand(&args(other)).unwrap(), None);
        }
        assert!(subcommand(&args(&["harness-bench", "session", "launch.json"])).unwrap_err().to_string().contains("HARNESS_WORKFLOW_REQUEST"));
        assert!(subcommand(&args(&["harness-bench", "planner", "--help"])).unwrap_err().to_string().contains("HARNESS_WORKSPACE_PLANNER"));
    }

    #[test]
    fn clean_exit_with_a_new_submission_hands_off_and_stalls_stop_at_the_limit() {
        assert_eq!(next_step(Some(0), &json!({"open":true,"submitted":true}), 2), (Step::Advance, 0));
        let state = json!({"open":true,"submitted":false});
        assert_eq!(next_step(Some(0), &state, 0), (Step::Continue, 1));
        assert_eq!(next_step(Some(1), &json!({"open":true,"submitted":true}), 1), (Step::Continue, 2));
        assert_eq!(next_step(None, &json!({"open":true,"submitted":true}), 0), (Step::Continue, 1));
        assert_eq!(next_step(Some(0), &state, 2), (Step::Stop, 3));
        assert_eq!(next_step(Some(0), &json!({"open":false,"submitted":true}), 1), (Step::Finished, 1));
        assert_eq!(next_step(Some(0), &json!({"open":null,"submitted":true}), 0).0, Step::Finished);
        assert_eq!([Step::Finished, Step::Advance, Step::Continue, Step::Stop].map(Step::name), ["finished", "advance", "continue", "stop"]);
    }

    #[test]
    fn only_tasks_that_can_still_run_keep_a_case_open() {
        assert!(can_still_run(&json!({"stage":"implementation","active":"attempt"})));
        assert!(can_still_run(&json!({"stage":"implementation","active":null,"stopped":false,"blocked":false})));
        assert!(can_still_run(&json!({"stage":"review","active":null})));
        assert!(can_still_run(&json!({"stage":"done","active":"attempt"})));
        assert!(!can_still_run(&json!({"stage":"done","active":null})));
        assert!(!can_still_run(&json!({"stage":"implementation","active":null,"stopped":true})));
        assert!(!can_still_run(&json!({"stage":"replan","active":null,"blocked":true})));
        assert!(!can_still_run(&json!({"stage":"implementation","active":"attempt","container":true})));
        assert_eq!(runnable_tasks(&[json!({"stage":"done"}), json!({"stage":"review","id":"r"})]), vec![json!({"stage":"review","id":"r"})]);
    }

    #[test]
    fn only_active_tasks_inside_the_workspace_block_the_handoff() {
        let tasks = [
            json!({"active":"a1","cwd":r"\\?\D:\Work\WS1\maze_game"}),
            json!({"active":"a2","cwd":r"D:\work\ws2\maze_game"}),
            json!({"active":null,"cwd":r"D:\work\ws1\maze_game"}),
            json!({"active":"a4"}),
        ];
        let found = active_tasks(&tasks, &stripped(r"\\?\D:\work\ws1\maze_game"));
        assert_eq!(found.iter().map(|task| task["active"].clone()).collect::<Vec<_>>(), [json!("a1")]);
        assert_eq!(quote(r"D:\work\ws 1/maze_game~é"), "D%3A%5Cwork%5Cws%201/maze_game~%C3%A9");
    }

    #[test]
    fn agents_resolve_only_for_their_own_launcher_family() {
        assert_eq!(resolve(&json!("sol_6_1_light"), Some("codex"), false).unwrap()["model"], "gpt-6.1-sol");
        assert!(resolve(&json!("sol_6_1_light"), Some("claude"), false).unwrap_err().to_string().contains("not allowed when started by claude"));
        assert!(resolve(&json!("sol_6_1_light"), None, false).unwrap_err().to_string().contains("an undetected launcher"));
        assert!(resolve(&json!("sol_light"), Some("codex"), false).unwrap_err().to_string().contains("retired"));
        assert!(resolve(&json!("sol_light"), Some("codex"), true).is_ok());
        assert!(resolve(&json!("missing"), Some("codex"), false).unwrap_err().to_string().contains("Unknown agent missing"));
        assert!(resolve(&Value::Null, Some("codex"), false).is_err());
        let agent = resolve(&json!("claude_sonnet_5_5_low"), Some("claude"), false).unwrap();
        assert_eq!(planner_command(&args(&["x", "--model={model}", "{effort}"]), &agent), args(&["x", "--model=claude-sonnet-5-5", "low"]));
    }

    const CODEX: &str = r#"{"provider":"openai","harness":"codex","model":"gpt-6.1-sol","effort":"low"}"#;
    const CLAUDE: &str = r#"{"provider":"anthropic","harness":"claude_code","model":"claude-haiku-5-5","effort":"low"}"#;

    #[test]
    fn codex_runs_ephemeral_unless_resumable_and_resumes_by_thread() {
        let agent: Value = serde_json::from_str(CODEX).unwrap();
        let fresh = plan(&agent, false, &json!({}), false, "codex", crate::id);
        assert!(fresh.argv.contains(&"--ephemeral".into()) && !fresh.capture);
        assert_eq!(fresh.argv.last().unwrap(), "-");
        assert!(fresh.argv.contains(&"model_reasoning_effort=\"low\"".into()) && fresh.argv.contains(&"approval_policy=\"never\"".into()));
        let persistent = plan(&agent, true, &json!({}), false, "codex", crate::id);
        assert!(!persistent.argv.contains(&"--ephemeral".into()) && persistent.capture);
        let resumed = plan(&agent, true, &json!({"session":"thread-1"}), true, "C:/bin/codex.exe", crate::id);
        assert_eq!(resumed.argv[..3], args(&["C:/bin/codex.exe", "exec", "resume"]));
        assert_eq!(resumed.argv[resumed.argv.len() - 2..], args(&["thread-1", "-"]));
        assert!(resumed.argv.contains(&"gpt-6.1-sol".into()) && !resumed.capture);
        assert!(!plan(&agent, true, &json!({"session":"thread-1"}), false, "codex", crate::id).argv.contains(&"resume".into()));
        assert_eq!(thread_started(br#"{"type":"thread.started","thread_id":"t-9"}"#), Some(json!("t-9")));
        assert_eq!(thread_started(br#"{"type":"thread.started","thread_id":""}"#), None);
        assert_eq!(thread_started(b"not json"), None);
    }

    #[test]
    fn claude_gets_a_fixed_session_then_resumes_it() {
        let agent: Value = serde_json::from_str(CLAUDE).unwrap();
        let first = plan(&agent, true, &json!({}), false, "claude", || "session-1".into());
        assert_eq!(first.saved, json!({"harness":"claude_code","session":"session-1"}));
        assert_eq!(first.argv[first.argv.iter().position(|arg| arg == "--session-id").unwrap() + 1], "session-1");
        assert!(!first.argv.contains(&"--no-session-persistence".into()));
        let resumed = plan(&agent, true, &first.saved, true, "claude", || unreachable!());
        assert_eq!(resumed.argv[resumed.argv.iter().position(|arg| arg == "--resume").unwrap() + 1], "session-1");
        let single = plan(&agent, false, &json!({}), false, "claude", || unreachable!());
        assert!(single.argv.contains(&"--no-session-persistence".into()) && !single.argv.contains(&"--resume".into()));
        assert_eq!(single.argv[..2], args(&["claude", "-p"]));
        assert_eq!(single.argv[single.argv.len() - 2..], args(&["--effort", "low"]));
        assert!(hosted_claude_variable("CLAUDECODE") && hosted_claude_variable("CLAUDE_CODE_ENTRYPOINT") && hosted_claude_variable("CLAUDE_AGENT_SDK_VERSION"));
        assert!(!hosted_claude_variable("CLAUDE_BIN") && !hosted_claude_variable("PATH"));
        assert_eq!(claude_executable(None), "claude");
    }

    type Seen = Arc<Mutex<Vec<(String, String, Value, String)>>>;

    fn serve(respond: impl Fn(&str, &str, &Value) -> Value + Send + 'static) -> (String, Seen, Arc<tiny_http::Server>) {
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let url = format!("http://{}", server.server_addr());
        let seen: Seen = Arc::default();
        let (listener, record) = (server.clone(), seen.clone());
        thread::spawn(move || {
            for mut request in listener.incoming_requests() {
                let mut body = String::new();
                request.as_reader().read_to_string(&mut body).unwrap();
                let body: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                let auth = request.headers().iter().find(|h| h.field.equiv("Authorization")).map(|h| h.value.to_string()).unwrap_or_default();
                let (method, url) = (request.method().to_string(), request.url().to_owned());
                let reply = respond(&method, &url, &body);
                record.lock().unwrap().push((method, url, body, auth));
                let _ = request.respond(tiny_http::Response::from_string(reply.to_string()));
            }
        });
        (url, seen, server)
    }

    fn fixture(dir: &Path, url: &str, handoff: Option<&str>) -> (PathBuf, PathBuf) {
        let workspace = dir.join("ws").join("maze_game");
        fs::create_dir_all(&workspace).unwrap();
        let mut settings = json!({"planner":"sol_6_1_light","prompts":{},"limits":{"criteria":3}});
        fs::write(workspace.join("LIMITS.json"), r#"{"criteria":3}"#).unwrap();
        for (role, filename) in [("project_planner", "PROJECT_PLANNER.md"), ("worker_protocol", "WORKER_PROTOCOL.md")] {
            fs::write(workspace.join(filename), "Selected fixture instructions.").unwrap();
            settings["prompts"][role] = json!({"candidate":"fixture.md","sha256":format!("{:x}", Sha256::digest(b"Selected fixture instructions."))});
        }
        let run = dir.join("run");
        fs::create_dir(&run).unwrap();
        let exe = env::current_exe().unwrap().to_string_lossy().into_owned();
        let mut started = json!({"run":{"id":"run-1"},"run_token":"run-token"});
        if let Some(token) = handoff {
            started["handoff_token"] = json!(token);
        }
        let profile = json!({"workspace":settings,"workspace_path":workspace.to_string_lossy(),"coordinator_url":format!("{url}/"),
            "planner_command":[exe,"--list","{model}"],"template_prompt":"Fill maze_game","env":{"HARNESS_FIXTURE":"1"}});
        let launch = run.join("launch.json");
        fs::write(&launch, json!({"url":url,"started":started,"profile":profile}).to_string()).unwrap();
        (launch, workspace)
    }

    fn progress(launch: &Path) -> Value {
        serde_json::from_slice(&fs::read(launch.with_file_name("progress.json")).unwrap()).unwrap()
    }

    #[test]
    fn session_without_handoff_waits_until_no_task_can_still_run() {
        let dir = crate::test_support::dir();
        let polls = Arc::new(Mutex::new(0));
        let count = polls.clone();
        let (url, seen, server) = serve(move |_, _, _| {
            let mut count = count.lock().unwrap();
            *count += 1;
            let stage = if *count == 1 { "implementation" } else { "done" };
            json!({"tasks":[{"stage":stage,"active":null}]})
        });
        let (launch, workspace) = fixture(dir.path(), &url, None);
        assert!(run_session(&launch, Some("claude"), Duration::from_millis(10)).unwrap_err().to_string().contains("not allowed"));
        run_session(&launch, Some("codex"), Duration::from_millis(10)).unwrap();
        server.unblock();
        let state = progress(&launch);
        assert_eq!((state["mode"].as_str(), state["phase"].as_str(), state["planner_exit_code"].as_i64()), (Some("template"), Some("planner_exited"), Some(0)));
        assert_eq!((state["run_id"].as_str(), state["coordinator_url"].as_str()), (Some("run-1"), Some(url.as_str())));
        assert!(state["finished_at_ms"].is_i64() && state["planner_pid"].is_u64() && state.get("messages").is_none());
        assert_eq!(fs::read_to_string(launch.with_file_name("planner-prompt.txt")).unwrap(), "Fill maze_game");
        assert!(fs::read_to_string(launch.with_file_name("planner.log")).unwrap().contains("tests,"));
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].1, format!("/api/tasks?workspace={}", quote(&workspace.to_string_lossy())));
        fs::write(workspace.join("extra.txt"), "x").unwrap();
        assert!(run_session(&launch, Some("codex"), Duration::from_millis(10)).unwrap_err().to_string().contains("only its instruction documents"));
    }

    #[test]
    fn handoff_session_advances_repeats_and_stops_at_the_stall_limit() {
        let dir = crate::test_support::dir();
        let workspace_cwd = Arc::new(Mutex::new(String::new()));
        let cwd = workspace_cwd.clone();
        let calls = Arc::new(Mutex::new((0, 0)));
        let (url, seen, server) = serve(move |_, path, body| {
            let mut calls = calls.lock().unwrap();
            if path == "/api/state" {
                calls.0 += 1;
                let active = if calls.0 == 1 { json!("attempt") } else { Value::Null };
                return json!({"tasks":[{"active":active,"cwd":cwd.lock().unwrap().clone()},{"active":"elsewhere","cwd":r"Z:\other"}]});
            }
            match body["action"].as_str() {
                Some("current") => {
                    calls.1 += 1;
                    json!({"request":calls.1.min(2),"open":true,"submitted":calls.1 == 1,"continue":"Keep going\non request"})
                }
                Some("advance") => json!({"next":"Second request.\nDetails."}),
                _ => json!({"next":null}),
            }
        });
        let (launch, workspace) = fixture(dir.path(), &url, Some("handoff-secret"));
        *workspace_cwd.lock().unwrap() = fs::canonicalize(&workspace).unwrap().to_string_lossy().into_owned();
        run_session(&launch, Some("codex"), Duration::from_millis(10)).unwrap();
        server.unblock();
        let state = progress(&launch);
        assert_eq!((state["phase"].as_str(), state["last_action"].as_str(), state["stalls"].as_u64(), state["request"].as_u64()), (Some("finished"), Some("stop"), Some(3), Some(2)));
        let messages = state["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        let newline = if cfg!(windows) { "\r\n" } else { "\n" };
        assert_eq!(fs::read_to_string(messages[0]["file"].as_str().unwrap()).unwrap(), format!("Second request.{newline}Details."));
        assert_eq!(fs::read_to_string(launch.with_file_name("planner-message-2.txt")).unwrap(), format!("Keep going{newline}on request"));
        let seen = seen.lock().unwrap();
        let actions: Vec<_> = seen.iter().filter(|call| call.1 == "/v1/runs/run-1/handoff").map(|call| (call.2["action"].as_str().unwrap().to_owned(), call.3.clone())).collect();
        assert_eq!(actions.iter().map(|a| a.0.as_str()).collect::<Vec<_>>(), ["current", "advance", "current", "current", "current", "stop"]);
        assert!(actions.iter().all(|a| a.1 == "Bearer handoff-secret"));
        assert_eq!(seen.iter().filter(|call| call.1 == "/api/state").count(), 5);
        assert!(seen.iter().filter(|call| call.1 == "/api/state").all(|call| call.3.trim() == "Bearer"));
    }
}
