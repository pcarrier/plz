use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    env,
    error::Error,
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, IsTerminal, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{MetadataExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
    thread,
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const LIMIT: u64 = 1024 * 1024;
const DEFAULT_PROMPT: &str = "Catch likely shell mistakes: typos, wrong paths or flags, and \
    unintended destructive effects. Ask for confirmation when a command looks accidental; \
    otherwise allow it.";

#[derive(Serialize, Deserialize)]
struct Context {
    command: String,
    cwd: PathBuf,
    shell: String,
    path: String,
    os: String,
    uid: u32,
}

#[derive(Serialize, Deserialize)]
struct Request {
    context: Context,
    model: String,
    endpoint: String,
    api_key: String,
    #[serde(default)]
    prompt: Option<String>,
    // None checks the command; Some records the user's answer to a confirmation.
    answer: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Reply {
    verdict: String,
    error: Option<String>,
    #[serde(default)]
    cache_hit: Option<bool>,
    #[serde(default)]
    prompt_supported: bool,
}

fn state_dir() -> Result<PathBuf> {
    let dir = if let Some(dir) = env::var_os("PLZ_STATE_DIR") {
        PathBuf::from(dir)
    } else if let Some(dir) = env::var_os("XDG_STATE_HOME") {
        PathBuf::from(dir).join("plz")
    } else {
        PathBuf::from(env::var_os("HOME").ok_or("HOME is unset")?).join(".local/state/plz")
    };
    if !dir.is_absolute() {
        return Err("plz state directory must be absolute".into());
    }
    fs::create_dir_all(&dir)?;
    let metadata = fs::symlink_metadata(&dir)?;
    // SAFETY: geteuid has no preconditions.
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err("plz state directory must be a real directory owned by you".into());
    }
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

fn send(stream: &mut UnixStream, value: &impl Serialize) -> Result<()> {
    serde_json::to_writer(&mut *stream, value)?;
    stream.write_all(b"\n")?;
    Ok(())
}

fn receive<T: DeserializeOwned>(stream: &mut UnixStream) -> Result<T> {
    let mut line = String::new();
    BufReader::new(stream.take(LIMIT)).read_line(&mut line)?;
    if !line.ends_with('\n') {
        return Err("incomplete or oversized daemon message".into());
    }
    Ok(serde_json::from_str(&line)?)
}

fn connect(dir: &Path) -> Result<UnixStream> {
    let socket = dir.join("socket");
    if let Ok(stream) = UnixStream::connect(&socket) {
        return Ok(stream);
    }
    // The child detaches before doing any network or database work. Its lock
    // makes simultaneous first commands converge on one daemon.
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("daemon.log"))?;
    let status = Command::new(env::current_exe()?)
        .arg("__daemon")
        .env("PLZ_STATE_DIR", dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .status()?;
    if !status.success() {
        return Err("could not start daemon (see daemon.log)".into());
    }
    for _ in 0..100 {
        if let Ok(stream) = UnixStream::connect(&socket) {
            return Ok(stream);
        }
        thread::sleep(Duration::from_millis(20));
    }
    Err("daemon did not start (see daemon.log)".into())
}

fn rpc(dir: &Path, request: &Request, cache_hit: &mut Option<bool>) -> Result<Reply> {
    let mut stream = connect(dir)?;
    stream.set_read_timeout(Some(Duration::from_secs(120)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    send(&mut stream, request)?;
    let reply: Reply = receive(&mut stream)?;
    if request.prompt.is_some() && !reply.prompt_supported {
        return Err(format!(
            "daemon is outdated; restart it (PID file: {})",
            dir.join("pid").display()
        )
        .into());
    }
    if request.answer.is_none() {
        *cache_hit = reply.cache_hit;
    }
    if let Some(error) = &reply.error {
        return Err(error.clone().into());
    }
    Ok(reply)
}

fn timed_rpc(
    dir: &Path,
    request: &Request,
    waiting: &mut Duration,
    cache_hit: &mut Option<bool>,
) -> Result<Reply> {
    let started = Instant::now();
    let result = rpc(dir, request, cache_hit);
    *waiting += started.elapsed();
    result
}

fn database(dir: &Path) -> Result<Connection> {
    let db = Connection::open(dir.join("cache.sqlite3"))?;
    db.execute_batch(
        "PRAGMA journal_mode=WAL;
         CREATE TABLE IF NOT EXISTS settings (
             name TEXT PRIMARY KEY,
             value TEXT NOT NULL
          );
         CREATE TABLE IF NOT EXISTS verdicts (
             key TEXT PRIMARY KEY,
             verdict TEXT NOT NULL,
             response TEXT NOT NULL,
             always_allow INTEGER NOT NULL DEFAULT 0,
             created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
         );
         CREATE TABLE IF NOT EXISTS answers (
             id INTEGER PRIMARY KEY,
             key TEXT NOT NULL REFERENCES verdicts(key),
             answer TEXT NOT NULL,
              created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
           );
         CREATE TABLE IF NOT EXISTS checks (
             id INTEGER PRIMARY KEY,
             duration_ms REAL NOT NULL CHECK (duration_ms >= 0),
             cache_hit INTEGER CHECK (cache_hit IN (0, 1)),
             created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
         );
         CREATE VIEW IF NOT EXISTS usage AS
         WITH tokens AS (
             SELECT key, created_at,
                 CASE WHEN json_type(response, '$.usage.input_tokens') = 'integer'
                           AND json_extract(response, '$.usage.input_tokens') >= 0
                      THEN json_extract(response, '$.usage.input_tokens') END AS input_tokens,
                 CASE WHEN json_type(response, '$.usage.output_tokens') = 'integer'
                           AND json_extract(response, '$.usage.output_tokens') >= 0
                      THEN json_extract(response, '$.usage.output_tokens') END AS output_tokens
             FROM verdicts
         )
         SELECT *, input_tokens * 0.042 / 1000000.0 AS cost_usd FROM tokens;",
    )?;
    Ok(db)
}

fn set_enabled(enabled: bool) -> Result<bool> {
    database(&state_dir()?)?.execute(
        "INSERT INTO settings (name, value) VALUES ('enabled', ?1)
         ON CONFLICT(name) DO UPDATE SET value = excluded.value",
        [if enabled { "1" } else { "0" }],
    )?;
    eprintln!("plz {}", if enabled { "on" } else { "off" });
    Ok(true)
}

fn is_enabled(db: &Connection) -> Result<bool> {
    let enabled: Option<String> = db
        .query_row(
            "SELECT value FROM settings WHERE name = 'enabled'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(enabled.as_deref() != Some("0"))
}

fn prompt(db: &Connection) -> Result<String> {
    Ok(db
        .query_row(
            "SELECT value FROM settings WHERE name = 'prompt'",
            [],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or_else(|| DEFAULT_PROMPT.into()))
}

fn edit_prompt() -> Result<bool> {
    let dir = state_dir()?;
    let db = database(&dir)?;
    let path = dir.join(format!("prompt-edit-{}.txt", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    let result = (|| {
        writeln!(file, "{}", prompt(&db)?)?;
        drop(file);
        let editor = env::var("VISUAL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| env::var("EDITOR").ok().filter(|s| !s.trim().is_empty()))
            .unwrap_or_else(|| "vi".into());
        // The editor setting is a shell command; the filename is a separate argument.
        let status = Command::new("sh")
            .args(["-c", &format!("exec {editor} \"$1\""), "plz-editor"])
            .arg(&path)
            .status()?;
        if !status.success() {
            return Err("editor exited unsuccessfully; prompt not saved".into());
        }
        let mut text = String::new();
        File::open(&path)?.take(LIMIT).read_to_string(&mut text)?;
        if text.len() as u64 >= LIMIT || text.trim().is_empty() {
            return Err("prompt must be nonempty and shorter than 1 MiB".into());
        }
        db.execute(
            "INSERT INTO settings (name, value) VALUES ('prompt', ?1)
             ON CONFLICT(name) DO UPDATE SET value = excluded.value",
            [text.trim()],
        )?;
        eprintln!("plz: prompt saved");
        Ok(true)
    })();
    let _ = fs::remove_file(path);
    result
}

fn usage_totals(db: &Connection) -> Result<(i64, i64, f64, i64)> {
    Ok(db.query_row(
        "SELECT COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0),
                COALESCE(SUM(cost_usd), 0.0),
                COUNT(*) FILTER (WHERE input_tokens IS NULL OR output_tokens IS NULL)
         FROM usage",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?)
}

fn status() -> Result<bool> {
    let db = database(&state_dir()?)?;
    let (input, output, cost, missing) = usage_totals(&db)?;
    println!("plz {}", if is_enabled(&db)? { "on" } else { "off" });
    println!("prompt: {}", prompt(&db)?);
    println!("tokens: {input} in / {output} out");
    println!("estimated cost: ${cost:.8} ($0.042 / 1M input tokens; output free)");
    if missing > 0 {
        println!("incomplete usage: {missing} responses missing token counts");
    }
    let (hits, misses, unknown): (i64, i64, i64) = db.query_row(
        "SELECT COUNT(*) FILTER (WHERE cache_hit = 1),
                COUNT(*) FILTER (WHERE cache_hit = 0),
                COUNT(*) FILTER (WHERE cache_hit IS NULL) FROM checks",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let ratio = if hits + misses == 0 {
        "n/a".into()
    } else {
        format!("{:.1}%", 100.0 * hits as f64 / (hits + misses) as f64)
    };
    println!("cache: {hits} hits / {misses} misses / hit ratio {ratio}");
    if unknown > 0 {
        println!("cache: {unknown} checks without cache metadata (excluded from stats)");
    }
    let mut query =
        db.prepare("SELECT duration_ms FROM checks WHERE cache_hit = 0 ORDER BY duration_ms")?;
    let samples = query
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<f64>>>()?;
    println!(
        "uncached waiting: count {} / total {:.3}s",
        samples.len(),
        samples.iter().fold(0.0, |total, ms| total + ms) / 1000.0
    );
    if samples.is_empty() {
        println!("uncached waiting: min n/a / p50 n/a / p90 n/a / p99 n/a / max n/a");
    } else {
        // Nearest-rank percentiles select observed samples, including for small counts.
        let percentile = |p: usize| samples[(samples.len() * p).div_ceil(100) - 1];
        println!(
            "uncached waiting: min {:.3}ms / p50 {:.3}ms / p90 {:.3}ms / p99 {:.3}ms / max {:.3}ms",
            samples[0],
            percentile(50),
            percentile(90),
            percentile(99),
            percentile(100)
        );
    }
    Ok(true)
}

fn api_key(db: &Connection) -> Result<String> {
    if let Ok(key) = env::var("TYPESAFE_API_KEY")
        && !key.is_empty()
    {
        return Ok(key);
    }
    // Resolve in the client so changes also work with an already-running daemon.
    Ok(db
        .query_row(
            "SELECT value FROM settings WHERE name = 'api_key'",
            [],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or_default())
}

fn set_key() -> Result<bool> {
    let key = if io::stdin().is_terminal() {
        rpassword::prompt_password("TypeSafe API key: ")?
    } else {
        let mut key = String::new();
        io::stdin().take(8192).read_to_string(&mut key)?;
        if key.len() >= 8192 {
            return Err("API key input is too long".into());
        }
        key
    };
    let key = key.trim();
    if key.is_empty()
        || key.len() >= 8192
        || key.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err("API key must be a nonempty token shorter than 8192 bytes".into());
    }
    database(&state_dir()?)?.execute(
        "INSERT INTO settings (name, value) VALUES ('api_key', ?1)
         ON CONFLICT(name) DO UPDATE SET value = excluded.value",
        [key],
    )?;
    eprintln!("plz: API key saved");
    Ok(true)
}

fn verdict(response: &Value) -> Result<&'static str> {
    let answer = &response["answers"]["safety"];
    if answer["type"] != "choice" {
        return Err("provider returned an invalid safety answer".into());
    }
    match answer["choice"].as_str() {
        Some("allow") => Ok("allow"),
        Some("confirm") => Ok("confirm"),
        _ => Err("provider returned an unknown safety choice".into()),
    }
}

fn retryable(error: &ureq::Error) -> bool {
    if matches!(
        error.kind(),
        ureq::ErrorKind::Dns | ureq::ErrorKind::ConnectionFailed
    ) {
        return true;
    }
    // ureq and its JSON reader can wrap the underlying I/O error several times.
    let mut source: Option<&(dyn Error + 'static)> = Some(error);
    while let Some(error) = source {
        if let Some(error) = error.downcast_ref::<io::Error>()
            && matches!(
                error.kind(),
                io::ErrorKind::ConnectionReset
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::BrokenPipe
                    | io::ErrorKind::NotConnected
                    | io::ErrorKind::UnexpectedEof
                    | io::ErrorKind::TimedOut
                    | io::ErrorKind::WouldBlock
                    | io::ErrorKind::Interrupted
            )
        {
            return true;
        }
        source = error.source();
    }
    false
}

fn call_api(agent: &ureq::Agent, endpoint: &str, api_key: &str, body: &Value) -> Result<Value> {
    let mut retries = 0;
    loop {
        let result = agent
            .post(endpoint)
            .set("Authorization", &format!("Bearer {api_key}"))
            .send_json(body.clone())
            .map_err(Box::new)
            .and_then(|response| {
                // ureq's into_json stringifies errors, losing their I/O kind.
                serde_json::from_reader(response.into_reader()).map_err(|error| {
                    let kind = error.io_error_kind().unwrap_or(if error.is_eof() {
                        io::ErrorKind::UnexpectedEof
                    } else {
                        io::ErrorKind::InvalidData
                    });
                    Box::new(ureq::Error::from(io::Error::new(kind, error)))
                })
            });
        match result {
            Ok(response) => return Ok(response),
            Err(error) if retries < 3 && retryable(&error) => {
                thread::sleep(Duration::from_millis(100 << retries));
                retries += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

fn handle(
    db: &mut Connection,
    agent: &ureq::Agent,
    request: Request,
    cache_hit: &mut Option<bool>,
) -> Result<String> {
    // Pin the prompt for both the verdict and any subsequent confirmation reply.
    let prompt = match &request.prompt {
        Some(prompt) => prompt.clone(),
        None => prompt(db)?,
    };
    // Include the policy and provider configuration so upgrades don't reuse a
    // verdict made under different rules. Credentials aren't part of cached results.
    let key = serde_json::to_string(&json!({
        "context": request.context,
        "model": request.model,
        "endpoint": request.endpoint,
        "policy": prompt,
    }))?;
    let cached: Option<(String, bool)> = db
        .query_row(
            "SELECT verdict, always_allow FROM verdicts WHERE key = ?1",
            [&key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;

    if request.answer.is_none() {
        *cache_hit = Some(cached.is_some());
    }
    if let Some(answer) = request.answer {
        if !matches!(answer.as_str(), "once" | "always" | "no") {
            return Err("invalid confirmation answer".into());
        }
        if !matches!(&cached, Some((verdict, _)) if verdict == "confirm") {
            return Err("no cached confirmation to answer".into());
        }
        let tx = db.transaction()?;
        tx.execute(
            "INSERT INTO answers (key, answer) VALUES (?1, ?2)",
            params![key, answer],
        )?;
        if answer == "always" {
            tx.execute(
                "UPDATE verdicts SET always_allow = 1 WHERE key = ?1",
                [&key],
            )?;
        }
        tx.commit()?;
        return Ok(if answer == "no" { "deny" } else { "allow" }.into());
    }

    if let Some((verdict, always)) = cached {
        return Ok(if always { "allow".into() } else { verdict });
    }
    if request.api_key.is_empty() {
        return Err("run `plz set-key` or set TYPESAFE_API_KEY for uncached commands".into());
    }
    let response = call_api(
        agent,
        &request.endpoint,
        &request.api_key,
        &json!({
            "model": request.model,
            "state": request.context,
            "questions": { "safety": {
                "type": "choice",
                "instructions": prompt,
                "criteria": {
                    "allow": "Run without confirmation.",
                    "confirm": "Ask the user to confirm before running."
                }
            }}
        }),
    )?;
    let verdict = verdict(&response)?;
    db.execute(
        "INSERT INTO verdicts (key, verdict, response) VALUES (?1, ?2, ?3)",
        params![key, verdict, response.to_string()],
    )?;
    Ok(verdict.into())
}

fn daemon() -> Result<()> {
    // SAFETY: this fresh process is single-threaded. stdin/stdout are already
    // /dev/null; keep stderr attached to daemon.log. daemon changes cwd to /.
    if unsafe { libc::daemon(0, 1) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let dir = state_dir()?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("lock"))?;
    match lock.try_lock() {
        Ok(()) => (),
        Err(std::fs::TryLockError::WouldBlock) => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    let socket = dir.join("socket");
    match fs::remove_file(&socket) {
        Ok(()) => (),
        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    let mut db = database(&dir)?;
    let listener = UnixListener::bind(&socket)?;
    fs::write(dir.join("pid"), std::process::id().to_string())?;
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(20))
        .build();
    for stream in listener.incoming() {
        let mut stream = stream?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        let mut cache_hit = None;
        let result = receive(&mut stream)
            .and_then(|request| handle(&mut db, &agent, request, &mut cache_hit));
        let reply = match result {
            Ok(verdict) => Reply {
                verdict,
                error: None,
                cache_hit,
                prompt_supported: true,
            },
            Err(error) => Reply {
                verdict: "deny".into(),
                error: Some(error.to_string()),
                cache_hit,
                prompt_supported: true,
            },
        };
        // A closed client must not take down the daemon or lose a cached result.
        let _ = send(&mut stream, &reply);
    }
    Ok(())
}

struct TerminalInput {
    file: File,
    original: libc::termios,
}

impl TerminalInput {
    fn open() -> Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
        // SAFETY: termios consists of integer fields; tcgetattr initializes it.
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(file.as_raw_fd(), &mut original) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        // Fish key bindings can run us with a pipe on stdin while /dev/tty is
        // still raw. Enable line input, echo and Enter -> newline conversion.
        let mut mode = original;
        mode.c_lflag |= libc::ICANON | libc::ECHO | libc::ISIG;
        mode.c_iflag = (mode.c_iflag & !(libc::IGNCR | libc::INLCR)) | libc::ICRNL;
        // SAFETY: file is an open terminal and mode came from tcgetattr.
        if unsafe { libc::tcsetattr(file.as_raw_fd(), libc::TCSANOW, &mode) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(Self { file, original })
    }
}

impl Drop for TerminalInput {
    fn drop(&mut self) {
        // SAFETY: the terminal is still open and original came from tcgetattr.
        unsafe { libc::tcsetattr(self.file.as_raw_fd(), libc::TCSANOW, &self.original) };
    }
}

fn confirmation(command: &str) -> Result<String> {
    let mut terminal = TerminalInput::open()?;
    let tty = &mut terminal.file;
    let command = command.strip_suffix('\n').unwrap_or(command);
    // Debug formatting prevents terminal escape sequences in commands from
    // disguising the command being approved.
    write!(
        tty,
        "\nplz confirm {command:?}\n[O]nce, [A]lways, anything else for no: "
    )?;
    tty.flush()?;
    let mut answer = String::new();
    BufReader::new(tty).take(1024).read_line(&mut answer)?;
    Ok(match answer.trim().to_ascii_lowercase().as_str() {
        "o" => "once",
        "a" => "always",
        _ => "no",
    }
    .into())
}

fn check(shell: &str) -> Result<bool> {
    let mut command = String::new();
    io::stdin().take(LIMIT).read_to_string(&mut command)?;
    if command.len() as u64 >= LIMIT {
        return Err("command is too large".into());
    }
    // Local controls must work even when credentials or the provider are unavailable.
    if matches!(
        command.trim(),
        "" | "plz set-key" | "plz on" | "plz off" | "plz status" | "plz edit-prompt"
    ) {
        return Ok(true);
    }
    let dir = state_dir()?;
    let db = database(&dir)?;
    if !is_enabled(&db)? {
        return Ok(true);
    }
    let mut request = Request {
        context: Context {
            command,
            cwd: env::current_dir()?,
            shell: shell.into(),
            path: env::var("PATH").unwrap_or_default(),
            os: env::consts::OS.into(),
            // SAFETY: geteuid has no preconditions.
            uid: unsafe { libc::geteuid() },
        },
        model: env::var("TYPESAFE_MODEL").unwrap_or_else(|_| "jev-latest".into()),
        endpoint: env::var("TYPESAFE_URL")
            .unwrap_or_else(|_| "https://api.typesafe.ai/v1/systemone".into()),
        api_key: api_key(&db)?,
        prompt: Some(prompt(&db)?),
        answer: None,
    };
    let mut waiting = Duration::ZERO;
    let mut cache_hit = None;
    let result = (|| {
        let mut reply = timed_rpc(&dir, &request, &mut waiting, &mut cache_hit)?;
        if reply.verdict == "confirm" {
            request.answer = Some(confirmation(&request.context.command)?);
            reply = timed_rpc(&dir, &request, &mut waiting, &mut cache_hit)?;
        }
        Ok(reply.verdict == "allow")
    })();
    // One sample per check, including errors and both confirmation round trips.
    // Time spent reading the user's answer is deliberately outside timed_rpc.
    db.execute(
        "INSERT INTO checks (duration_ms, cache_hit) VALUES (?1, ?2)",
        params![waiting.as_secs_f64() * 1000.0, cache_hit],
    )?;
    result
}

fn run() -> Result<bool> {
    let args: Vec<String> = env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["init", "fish"] => {
            print!("{}", include_str!("../fish/init.fish"));
            Ok(true)
        }
        ["check", "--shell", shell] => check(shell),
        ["check"] => check("fish"),
        ["on"] => set_enabled(true),
        ["off"] => set_enabled(false),
        ["status"] => status(),
        ["edit-prompt"] => edit_prompt(),
        ["set-key"] => set_key(),
        ["unset-key"] => {
            database(&state_dir()?)?.execute("DELETE FROM settings WHERE name = 'api_key'", [])?;
            eprintln!("plz: saved API key removed");
            Ok(true)
        }
        ["__daemon"] => {
            daemon()?;
            Ok(true)
        }
        ["--help"] | ["-h"] | [] => {
            println!(
                "plz init fish | source\nplz check [--shell fish] < command\nplz on | plz off    # enable/disable checks across shells (saved in the DB)\nplz status    # on/off, prompt, tokens, estimated cost and waiting-time stats\nplz edit-prompt    # edit the saved prompt using VISUAL or EDITOR (default: vi)\nplz set-key    # hidden prompt, or read the key from stdin\nplz unset-key\n\nSave a key with plz set-key; TYPESAFE_API_KEY overrides the saved key.\nChecks default to on. The daemon starts automatically.\nConfirmation: O + Enter = once, A + Enter = always, anything else = no.\nExit status: 0 allowed, 1 denied, 2 error."
            );
            Ok(true)
        }
        _ => Err(
            "usage: plz init fish | plz check [--shell fish] | plz on | plz off | plz status | plz edit-prompt | plz set-key | plz unset-key".into(),
        ),
    }
}

fn main() -> ExitCode {
    // SAFETY: before any threads are created; all state files are private.
    unsafe {
        libc::umask(0o077);
    }
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => {
            eprintln!("plz: command not run");
            ExitCode::from(1)
        }
        Err(error) => {
            eprintln!("plz: {error}; command not run");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    enum Response {
        Reset,
        Truncated,
        Http(&'static str, &'static str),
    }

    fn server(responses: Vec<Response>) -> (String, thread::JoinHandle<Vec<Vec<u8>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let worker = thread::spawn(move || {
            let mut bodies = Vec::new();
            for response in responses {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error)
                            if error.kind() == io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            thread::sleep(Duration::from_millis(5))
                        }
                        Err(error) => panic!("mock server accept: {error}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new(&mut stream);
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    assert!(reader.read_line(&mut line).unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                drop(reader);
                bodies.push(body);
                match response {
                    Response::Reset => {
                        let linger = libc::linger {
                            l_onoff: 1,
                            l_linger: 0,
                        };
                        // SAFETY: stream is open and the option points to a valid linger.
                        assert_eq!(
                            unsafe {
                                libc::setsockopt(
                                    stream.as_raw_fd(),
                                    libc::SOL_SOCKET,
                                    libc::SO_LINGER,
                                    &linger as *const _ as *const _,
                                    std::mem::size_of_val(&linger) as libc::socklen_t,
                                )
                            },
                            0
                        );
                    }
                    Response::Truncated => {
                        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{\"ok\":").unwrap();
                    }
                    Response::Http(status, body) => {
                        write!(stream, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                    }
                }
            }
            bodies
        });
        (url, worker)
    }

    #[test]
    fn retries_reset_and_truncated_body() {
        let (url, worker) = server(vec![
            Response::Reset,
            Response::Truncated,
            Response::Http("200 OK", "{\"ok\":true}"),
        ]);
        let body = json!({"state": "pwd"});
        let result = call_api(&ureq::agent(), &url, "test-key", &body);
        let requests = worker.join().unwrap();
        assert_eq!(result.unwrap(), json!({"ok": true}));
        assert_eq!(requests.len(), 3);
        assert!(
            requests
                .iter()
                .all(|request| serde_json::from_slice::<Value>(request).unwrap() == body)
        );
    }

    #[test]
    fn limits_retries_and_preserves_error() {
        let (url, worker) = server((0..4).map(|_| Response::Reset).collect());
        let result = call_api(&ureq::agent(), &url, "test-key", &json!({}));
        assert_eq!(worker.join().unwrap().len(), 4);
        assert!(retryable(
            result.unwrap_err().downcast_ref::<ureq::Error>().unwrap()
        ));
    }

    #[test]
    fn does_not_retry_auth_or_invalid_json() {
        for (status, body) in [("401 Unauthorized", "{}"), ("200 OK", "not json")] {
            let (url, worker) = server(vec![Response::Http(status, body)]);
            let result = call_api(&ureq::agent(), &url, "test-key", &json!({}));
            assert_eq!(worker.join().unwrap().len(), 1);
            assert!(!retryable(
                result.unwrap_err().downcast_ref::<ureq::Error>().unwrap()
            ));
        }
    }
}
