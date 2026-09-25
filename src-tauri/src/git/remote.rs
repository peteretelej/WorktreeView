use super::exec::{git_execution_error, read_bounded};
use super::parse::{parse_framed_output, FragmentOutput};
use super::validate::is_not_a_repository_diagnostic;
use crate::CommandError;
use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use tokio::io::AsyncRead;
use tokio::process::Command;
use tokio::time::{timeout, Duration};

// A remote project target: the user's ssh reaches `host` as `user` and the
// repository lives at POSIX `path` there. `port` is set only for a
// non-default port; 22 collapses so the identity stays scp-shaped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteTarget {
    pub(crate) user: Option<String>,
    pub(crate) host: String,
    pub(crate) port: Option<u16>,
    pub(crate) path: String,
}

const DEFAULT_PORT: u16 = 22;

impl RemoteTarget {
    // IPC entry: typed fields from the add-project form, normalized here so
    // the webview never owns the identity shape.
    pub(crate) fn from_parts(
        user: Option<&str>,
        host: &str,
        port: Option<u16>,
        path: &str,
    ) -> Result<Self, CommandError> {
        let user = user.map(str::trim).filter(|user| !user.is_empty());
        if let Some(user) = user {
            validate_user(user)?;
        }
        let host = host.trim();
        validate_host(host)?;
        if port == Some(0) {
            return Err(malformed("The port must be between 1 and 65535."));
        }
        let port = port.filter(|port| *port != DEFAULT_PORT);
        let path = path.trim();
        validate_path(path)?;
        Ok(Self {
            user: user.map(str::to_string),
            host: host.to_string(),
            port,
            path: path.to_string(),
        })
    }

    // Parses the identity string users type (`user@host:path` or
    // `ssh://[user@]host[:port]/path`) into the canonical target.
    pub(crate) fn parse_identity(input: &str) -> Result<Self, CommandError> {
        let input = input.trim();
        if input.is_empty() {
            return Err(malformed("Enter a remote target like user@host:path."));
        }
        let lowered = input.to_ascii_lowercase();
        if let Some(rest) = lowered.strip_prefix("ssh://") {
            return Self::from_url(&input[input.len() - rest.len()..]);
        }
        if lowered.contains("://") {
            return Err(malformed("Only ssh remote targets are supported."));
        }
        Self::from_scp(input)
    }

    // `ssh://[user@]host[:port]/path`: the path starts at the first slash.
    fn from_url(rest: &str) -> Result<Self, CommandError> {
        let Some(slash) = rest.find('/') else {
            return Err(malformed("The ssh URL needs a path after the host."));
        };
        let (authority, path) = rest.split_at(slash);
        let (user, hostport) = match authority.split_once('@') {
            Some((user, hostport)) => (Some(user), hostport),
            None => (None, authority),
        };
        let (host, port) = split_host_port(hostport)?;
        Self::from_parts(user, host, port, path)
    }

    // `[user@]host:path`: the separator is the first colon outside brackets,
    // so bracketed IPv6 hosts keep their colons; scp syntax carries no port.
    fn from_scp(input: &str) -> Result<Self, CommandError> {
        let separator = find_scp_separator(input).ok_or_else(|| {
            malformed("Enter a remote target like user@host:path.")
        })?;
        let (left, path) = input.split_at(separator);
        let path = &path[1..];
        let (user, host) = match left.split_once('@') {
            Some((user, host)) => (Some(user), host),
            None => (None, left),
        };
        Self::from_parts(user, host, None, path)
    }

    // The stored identity string: canonical and total, so rendering a parsed
    // target reproduces it byte-for-byte. A non-default port keeps the
    // ssh:// form because scp syntax cannot carry a port.
    pub(crate) fn identity(&self) -> String {
        let host = &self.host;
        let path = &self.path;
        match (self.user.as_deref(), self.port) {
            (Some(user), None) => format!("{user}@{host}:{path}"),
            (None, None) => format!("{host}:{path}"),
            (Some(user), Some(port)) => format!("ssh://{user}@{host}:{port}{path}"),
            (None, Some(port)) => format!("ssh://{host}:{port}{path}"),
        }
    }

    // The display name mirrors the local open's file_name: the path's final
    // segment. Parsing guarantees one exists.
    pub(crate) fn name(&self) -> &str {
        let trimmed = self.path.trim_end_matches('/');
        trimmed.rsplit('/').next().unwrap_or(trimmed)
    }
}

fn malformed(detail: &str) -> CommandError {
    CommandError::new("invalid_remote_target", detail)
}

fn validate_user(user: &str) -> Result<(), CommandError> {
    if user.is_empty()
        || user.starts_with('-')
        || user
            .chars()
            .any(|character| {
                character.is_whitespace()
                    || character.is_control()
                    || matches!(character, '@' | ':' | '/' | '\\')
            })
    {
        return Err(malformed("The user name is not a valid login name."));
    }
    Ok(())
}

fn validate_host(host: &str) -> Result<(), CommandError> {
    if host.is_empty()
        || host.starts_with('-')
        || host.chars().any(|character| {
            character.is_whitespace()
                || character.is_control()
                || matches!(character, '@' | '/' | '\\')
        })
    {
        return Err(malformed("The host name is not valid."));
    }
    let bracketed = host.starts_with('[');
    if bracketed || host.ends_with(']') {
        // A bracketed IPv6 literal must be exactly [..]; colons are only
        // allowed inside the brackets.
        if !(bracketed && host.ends_with(']') && host.len() >= 3) {
            return Err(malformed("The host name is not valid."));
        }
    } else if host.contains(':') || host.contains('[') || host.contains(']') {
        return Err(malformed(
            "An IPv6 host must be written in bracket form, like [::1].",
        ));
    }
    Ok(())
}

fn validate_path(path: &str) -> Result<(), CommandError> {
    // Remote hosts are POSIX: a backslash means a local Windows path was
    // pasted by mistake, and refusing it beats storing a target that can
    // never validate.
    if path.is_empty()
        || path.contains('\\')
        || path.chars().any(|character| character.is_control())
    {
        return Err(malformed(
            "The path must be a non-empty POSIX path on the host.",
        ));
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(malformed("The path must name a repository folder."));
    }
    Ok(())
}

fn find_scp_separator(input: &str) -> Option<usize> {
    let mut bracketed = false;
    for (index, character) in input.char_indices() {
        match character {
            '[' => bracketed = true,
            ']' => bracketed = false,
            ':' if !bracketed => return Some(index),
            _ => {}
        }
    }
    None
}

fn split_host_port(hostport: &str) -> Result<(&str, Option<u16>), CommandError> {
    if let Some(stripped) = hostport.strip_prefix('[') {
        let Some(close) = stripped.find(']') else {
            return Err(malformed("The IPv6 host must close its bracket."));
        };
        let host = &hostport[..=close + 1];
        let tail = &hostport[close + 2..];
        let port = match tail.strip_prefix(':') {
            Some(port) => Some(parse_port(port)?),
            None if tail.is_empty() => None,
            None => return Err(malformed("The host name is not valid.")),
        };
        return Ok((host, port));
    }
    match hostport.rsplit_once(':') {
        Some((host, port)) => Ok((host, Some(parse_port(port)?))),
        None => Ok((hostport, None)),
    }
}

fn parse_port(port: &str) -> Result<u16, CommandError> {
    port.parse::<u16>()
        .ok()
        .filter(|port| *port >= 1)
        .ok_or_else(|| malformed("The port must be between 1 and 65535."))
}

// One tested POSIX shell-quoting helper: every variable slot (paths,
// refnames, any repo- or user-derived argv element) passes through it
// before entering a composed command, because the host interprets the
// command through the login shell.
pub(crate) fn shell_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for character in value.chars() {
        if character == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(character);
        }
    }
    quoted.push('\'');
    quoted
}

// Sentinel output framing: one ssh invocation can carry several commands.
// Each allowlisted fragment rides in a subshell (so its own `exit` cannot
// end the batch) between two nonce sentinel lines, and the closing line
// carries the fragment's exit code. The nonce is per-invocation random, so
// fragment output can never forge framing lines it cannot predict.
pub(crate) fn new_nonce() -> Result<String, CommandError> {
    const HEX_DIGITS: &[u8] = b"0123456789abcdef";
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes)
        .map_err(|_| CommandError::new("remote_execution", "Could not generate a batch nonce."))?;
    let mut nonce = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        nonce.push(HEX_DIGITS[usize::from(byte >> 4)] as char);
        nonce.push(HEX_DIGITS[usize::from(byte & 0x0f)] as char);
    }
    Ok(nonce)
}

fn sentinel(nonce: &str) -> String {
    format!("WTV-{nonce}")
}

pub(crate) fn frame_fragment(nonce: &str, fragment: &str) -> String {
    let sentinel = sentinel(nonce);
    format!("echo {sentinel}-begin\n( {fragment} )\necho {sentinel}-exit-$?")
}

// The remote allowlist: the single match deciding which git argv may run on
// the host, mirroring the read-only plumbing set of the local call sites.
// The runner's entry point is the only caller; anything not matched here is
// refused with a typed error. `git diff --no-index` and the bounded read
// command are deliberate later additions, not absent by omission.
fn allowlisted(args: &[String]) -> Result<(), CommandError> {
    let args = leading_globals_and_configs(args)?;
    let Some((subcommand, rest)) = args.split_first() else {
        return Err(refused("The remote command is empty."));
    };
    match subcommand.as_str() {
        "rev-parse" => flags(
            rest,
            &["--is-inside-work-tree", "--verify", "--quiet"],
            &[],
        ),
        "log" => flags(
            rest,
            &["-1", "--decorate=full"],
            &["--skip=", "--max-count=", "--format="],
        ),
        "diff" => {
            // ext-diff and textconv execute repository-defined converters;
            // both neutralizations are required, not optional.
            if !rest.iter().any(|arg| arg == "--no-ext-diff")
                || !rest.iter().any(|arg| arg == "--no-textconv")
            {
                return Err(refused("diff requires --no-ext-diff and --no-textconv."));
            }
            flags(
                rest,
                &[
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-color",
                    "--numstat",
                    "--name-status",
                    "-z",
                    "-R",
                    "-U3",
                    "--",
                ],
                &[],
            )
        }
        "status" => {
            // A bare `status` may opportunistically refresh the remote
            // index; the read-only probe shape always carries --porcelain=v1.
            if !rest.iter().any(|arg| arg == "--porcelain=v1") {
                return Err(refused("status must run with --porcelain=v1."));
            }
            flags(
                rest,
                &["--porcelain=v1", "--no-renames", "--untracked-files=all", "-z"],
                &[],
            )
        }
        "worktree" => {
            let allowed = ["list", "--porcelain"];
            if rest.len() != allowed.len()
                || !rest.iter().zip(allowed).all(|(arg, expected)| arg == expected)
            {
                return Err(refused("only `worktree list --porcelain` is allowed."));
            }
            Ok(())
        }
        "for-each-ref" => flags(rest, &[], &["--format=", "--merged="]),
        "rev-list" => flags(rest, &["--no-walk", "--not", "--left-right", "--count"], &[]),
        "merge-base" => operands(rest),
        "cat-file" => match rest {
            [kind, tail @ ..] if kind == "blob" => operands(tail),
            _ => Err(refused("cat-file only reads blobs.")),
        },
        "ls-tree" => flags(rest, &["-z", "--"], &[]),
        "ls-files" => flags(rest, &["--others", "--exclude-standard", "-z"], &[]),
        "remote" if rest.is_empty() => Ok(()),
        // config without a read subcommand writes (`config user.name x`);
        // only the two read forms pass.
        "config" => match rest.first().map(String::as_str) {
            Some("--get") | Some("--get-regexp") => operands(&rest[1..]),
            _ => Err(refused("config only reads with --get/--get-regexp.")),
        },
        "hash-object" => {
            if rest != ["-t", "tree", "--stdin"] {
                return Err(refused("hash-object only computes the empty tree."));
            }
            Ok(())
        }
        "check-attr" => {
            let allowed_operands = ["filter"];
            for arg in rest {
                if arg.starts_with('-') {
                    if !matches!(arg.as_str(), "-z" | "--stdin") {
                        return Err(refused("unexpected check-attr flag."));
                    }
                } else if !allowed_operands.contains(&arg.as_str()) {
                    return Err(refused("unexpected check-attr attribute."));
                }
            }
            Ok(())
        }
        _ => Err(refused("the subcommand is not in the remote allowlist.")),
    }
}

fn refused(detail: &str) -> CommandError {
    CommandError::new(
        "remote_command_refused",
        format!("The remote command is not allowed: {detail}"),
    )
}

// git's leading global options and -c config pairs precede the subcommand;
// only the shapes the local call sites produce pass.
fn leading_globals_and_configs(args: &[String]) -> Result<&[String], CommandError> {
    let mut rest = args;
    if rest.first().map(String::as_str) == Some("--no-optional-locks") {
        rest = &rest[1..];
    }
    while rest.first().map(String::as_str) == Some("-c") {
        let Some(value) = rest.get(1) else {
            return Err(refused("a -c config pair is missing its value."));
        };
        if !neutralized_config(value) {
            return Err(refused("this -c override is not a neutralization."));
        }
        rest = &rest[2..];
    }
    Ok(rest)
}

// Config overrides may only restate the filter neutralization (identity
// `cat` filters, non-required) or quotePath display; the value is
// shell-quoted downstream regardless.
fn neutralized_config(value: &str) -> bool {
    if value == "core.quotePath=false" {
        return true;
    }
    let Some((key, set)) = value.split_once('=') else {
        return false;
    };
    let Some(name) = key.strip_prefix("filter.") else {
        return false;
    };
    let safe_name = !name.is_empty()
        && name
            .chars()
            .all(|character| !character.is_whitespace() && !character.is_control());
    match set {
        "cat" => {
            safe_name && (name.ends_with(".clean") || name.ends_with(".process"))
        }
        "false" => safe_name && name.ends_with(".required"),
        _ => false,
    }
}

// Every option-shaped element must be exactly listed (or match a listed
// prefix form); operands pass the match and are shell-quoted downstream.
fn flags(
    args: &[String],
    exact: &[&str],
    prefixes: &[&str],
) -> Result<(), CommandError> {
    for arg in args {
        if !arg.starts_with('-') {
            continue;
        }
        let known = exact.contains(&arg.as_str())
            || prefixes.iter().any(|prefix| arg.starts_with(prefix));
        if !known {
            return Err(refused(&format!("unexpected flag {arg}.")));
        }
    }
    Ok(())
}

fn operands(args: &[String]) -> Result<(), CommandError> {
    if let Some(arg) = args.iter().find(|arg| arg.starts_with('-')) {
        return Err(refused(&format!("unexpected flag {arg}.")));
    }
    Ok(())
}

// Composes the remote command string: the same neutralizations the local
// hardened builder applies (lazy-fetch guard, fsmonitor/autoRefreshIndex
// off, empty attributes file plus GIT_ATTR_NOSYSTEM so repository-defined
// filters never run), with the locale pinned when the output is
// machine-parsed. Every git argv element is quoted, so all variable slots
// necessarily pass the quoting helper.
pub(crate) fn git_command_string(
    target: &RemoteTarget,
    args: &[String],
    machine_parsed: bool,
) -> String {
    let mut command = String::new();
    if machine_parsed {
        command.push_str("LC_ALL=C LANG=C ");
    }
    command.push_str("GIT_NO_LAZY_FETCH=1 GIT_ATTR_NOSYSTEM=1 git");
    for config in ["core.fsmonitor=false", "diff.autoRefreshIndex=false", "core.attributesFile="] {
        command.push_str(" -c ");
        command.push_str(&shell_quote(config));
    }
    command.push_str(" -C ");
    command.push_str(&shell_quote(&target.path));
    for arg in args {
        command.push(' ');
        command.push_str(&shell_quote(arg));
    }
    command
}

// ControlMaster multiplexes connections where the platform supports it;
// Windows OpenSSH has none (Win32-OpenSSH issue #1328), so the options are
// gated on a non-Windows host. The batched reads pass the per-target socket.
pub(crate) fn control_master_options(socket: Option<&str>) -> Vec<String> {
    if cfg!(windows) {
        return Vec::new();
    }
    let Some(socket) = socket else {
        return Vec::new();
    };
    vec![
        "-o".into(),
        "ControlMaster=auto".into(),
        "-o".into(),
        format!("ControlPath={socket}"),
        "-o".into(),
        "ControlPersist=60".into(),
    ]
}

// Everything the spawner needs: ssh's resolved program and its explicit
// argument array. No shell ever interpolates these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SshSpawn {
    pub(crate) program: String,
    pub(crate) args: Vec<String>,
}

fn ssh_invocation(
    target: &RemoteTarget,
    program: &str,
    remote_command: &str,
    multiplex_socket: Option<&str>,
) -> SshSpawn {
    let mut args: Vec<String> = vec!["-o".into(), "BatchMode=yes".into()];
    args.extend(control_master_options(multiplex_socket));
    if let Some(port) = target.port {
        args.push("-p".into());
        args.push(port.to_string());
    }
    args.push(match &target.user {
        Some(user) => format!("{user}@{}", target.host),
        None => target.host.clone(),
    });
    args.push(remote_command.to_string());
    SshSpawn {
        program: program.to_string(),
        args,
    }
}

type BoxReader = Box<dyn AsyncRead + Unpin + Send>;
type BoxWait<'a> =
    Pin<Box<dyn Future<Output = std::io::Result<std::process::ExitStatus>> + Send + 'a>>;
type BoxKill<'a> = Pin<Box<dyn Future<Output = std::io::Result<()>> + Send + 'a>>;

// One spawned remote command. Implementations own kill-on-drop: dropping
// the value ends the remote command.
pub(crate) trait RemoteChild: Send {
    fn stdout(&mut self) -> BoxReader;
    fn stderr(&mut self) -> BoxReader;
    fn wait(&mut self) -> BoxWait<'_>;
    fn kill(&mut self) -> BoxKill<'_>;
}

type BoxChild = Box<dyn RemoteChild>;
type SpawnResult = Pin<Box<dyn Future<Output = Result<BoxChild, CommandError>> + Send>>;

// The crate-internal spawner parameter: the only production implementation
// shells to ssh; tests inject a fake runner at this seam.
pub(crate) type Spawner = Arc<dyn Fn(SshSpawn) -> SpawnResult + Send + Sync>;

struct SshChild {
    child: tokio::process::Child,
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
}

impl SshChild {
    fn new(mut child: tokio::process::Child) -> Self {
        Self {
            stdout: child.stdout.take(),
            stderr: child.stderr.take(),
            child,
        }
    }
}

impl Drop for SshChild {
    fn drop(&mut self) {
        // Killing on drop ends the remote command even when a caller
        // abandons the child; tokio's kill_on_drop covers the raw Child,
        // this keeps the guarantee behind the RemoteChild seam.
        let _ = self.child.start_kill();
    }
}

impl RemoteChild for SshChild {
    fn stdout(&mut self) -> BoxReader {
        Box::new(self.stdout.take().expect("remote stdout read once"))
    }

    fn stderr(&mut self) -> BoxReader {
        Box::new(self.stderr.take().expect("remote stderr read once"))
    }

    fn wait(&mut self) -> BoxWait<'_> {
        Box::pin(self.child.wait())
    }

    fn kill(&mut self) -> BoxKill<'_> {
        Box::pin(self.child.kill())
    }
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(windows)]
fn hide_console(command: &mut Command) {
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_console(_command: &mut Command) {}

fn process_spawner_impl() -> Spawner {
    static SPAWNER: OnceLock<Spawner> = OnceLock::new();
    SPAWNER
        .get_or_init(|| {
            Arc::new(|spawn: SshSpawn| {
                Box::pin(async move {
                    let mut command = Command::new(&spawn.program);
                    command
                        .args(&spawn.args)
                        .stdin(Stdio::null())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .kill_on_drop(true);
                    hide_console(&mut command);
                    let child = command.spawn().map_err(|_| {
                        CommandError::new(
                            "remote_spawn",
                            "The ssh client could not be started.",
                        )
                    })?;
                    Ok(Box::new(SshChild::new(child)) as BoxChild)
                }) as SpawnResult
            })
        })
        .clone()
}

#[cfg(windows)]
fn ssh_program() -> String {
    find_on_path("ssh.exe")
        .or_else(|| beside_git("ssh.exe"))
        .unwrap_or_else(|| r"C:\Windows\System32\OpenSSH\ssh.exe".to_string())
}

#[cfg(not(windows))]
fn ssh_program() -> String {
    // Resolved from PATH by the process spawn itself.
    "ssh".to_string()
}

#[cfg(windows)]
fn find_on_path(program: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(program))
        .find(|candidate| candidate.is_file())
        .map(|candidate| candidate.to_string_lossy().into_owned())
}

// Git-for-Windows ships its own ssh beside git.exe; System32's OpenSSH is
// the last resort.
#[cfg(windows)]
fn beside_git(program: &str) -> Option<String> {
    let git = find_on_path("git.exe")?;
    let candidate = std::path::Path::new(&git).parent()?.join(program);
    candidate
        .is_file()
        .then(|| candidate.to_string_lossy().into_owned())
}

// Deadlines sized to include connection setup: 60 s bounds a hung remote
// read without tripping the phase-5 latency gate threshold (8 s); the
// remote fetch matches the local fetch budget.
const REMOTE_TIMEOUT: Duration = Duration::from_secs(60);
#[allow(dead_code)] // consumed by the remote fetch (phase 3)
const REMOTE_FETCH_TIMEOUT: Duration = Duration::from_secs(300);

async fn run_remote_git_budgeted(
    spawner: &Spawner,
    target: &RemoteTarget,
    args: &[String],
    machine_parsed: bool,
    budget: Duration,
) -> Result<(i32, Vec<u8>, Vec<u8>), CommandError> {
    allowlisted(args)?;
    let remote_command = git_command_string(target, args, machine_parsed);
    let spawn = ssh_invocation(target, &ssh_program(), &remote_command, None);
    let child = (spawner)(spawn).await?;
    run_child(child, budget).await
}

async fn join_read(
    task: tokio::task::JoinHandle<Result<Vec<u8>, CommandError>>,
) -> Result<Vec<u8>, CommandError> {
    task.await
        .map_err(|_| CommandError::new("remote_execution", "The remote output could not be read."))?
}

async fn kill_and_reap(child: &mut dyn RemoteChild) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

async fn run_child(
    mut child: BoxChild,
    budget: Duration,
) -> Result<(i32, Vec<u8>, Vec<u8>), CommandError> {
    let stdout_task = tokio::spawn(read_bounded(child.stdout()));
    let stderr_task = tokio::spawn(read_bounded(child.stderr()));
    let result = timeout(budget, async {
        let status = child
            .wait()
            .await
            .map_err(|_| CommandError::new("remote_execution", "The remote command did not finish."))?;
        let stdout = join_read(stdout_task).await?;
        let stderr = join_read(stderr_task).await?;
        Ok::<_, CommandError>((status, stdout, stderr))
    })
    .await;
    match result {
        Ok(Ok((status, stdout, stderr))) => Ok((status.code().unwrap_or(-1), stdout, stderr)),
        Ok(Err(error)) => {
            kill_and_reap(child.as_mut()).await;
            Err(error)
        }
        Err(_) => {
            kill_and_reap(child.as_mut()).await;
            log::warn!("remote command timed out after {budget:?}");
            Err(CommandError::new(
                "remote_timeout",
                "The remote host did not respond in time. Check the host and your ssh config; try again if the host was busy.",
            ))
        }
    }
}

// Stable per-target socket path so ControlMaster=auto multiplexes the
// app's invocations on platforms that support it; the option builder
// drops it on Windows, where every batch runs one-shot.
fn multiplex_socket(target: &RemoteTarget) -> Option<String> {
    if cfg!(windows) {
        return None;
    }
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in target.identity().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Some(
        std::env::temp_dir()
            .join(format!("worktreeview-ssh-{hash:016x}.sock"))
            .to_string_lossy()
            .into_owned(),
    )
}

// One allowlisted git command inside a read group. `cwd` addresses a
// specific worktree on the host instead of the target's repository path
// (per-worktree probes); `stdin` feeds NUL-separated operands through the
// host shell's printf so stdin-reading plumbing (check-attr) keeps its
// exact allowlisted argv inside the batch.
pub(crate) struct BatchFragment {
    pub(crate) cwd: Option<String>,
    pub(crate) args: Vec<String>,
    pub(crate) stdin: Option<Vec<String>>,
}

// One ssh invocation's demultiplexed result: per-fragment outputs plus the
// batch's combined stderr, which carries a failing fragment's diagnostic.
#[derive(Debug)]
pub(crate) struct BatchOutput {
    pub(crate) fragments: Vec<FragmentOutput>,
    pub(crate) stderr: Vec<u8>,
}

impl BatchOutput {
    // The fragment's output, or the git execution error its nonzero exit
    // and the batch stderr describe.
    pub(crate) fn ok(&self, index: usize) -> Result<&FragmentOutput, CommandError> {
        let fragment = &self.fragments[index];
        if fragment.exit_code != 0 {
            return Err(git_execution_error(&self.stderr));
        }
        Ok(fragment)
    }

    // The raw exit code, for callers that distinguish codes (config reads
    // treat 1 as "no matches").
    pub(crate) fn code(&self, index: usize) -> i32 {
        self.fragments[index].exit_code
    }
}

// Runs every read group through the production spawner.
pub(crate) fn process_spawner() -> Spawner {
    process_spawner_impl()
}

// Runs every read group through the production spawner.
pub(crate) async fn run_remote_batch(
    target: &RemoteTarget,
    fragments: &[BatchFragment],
) -> Result<BatchOutput, CommandError> {
    run_remote_batch_with(&process_spawner_impl(), target, fragments).await
}

// One ssh invocation per read group: the fragments compose into one framed
// script, the allowlist gates each fragment's argv before anything spawns,
// and the demultiplexer returns per-fragment outputs. Connection-level
// failures (ssh refused, dropped mid-batch, timed out) surface as the typed
// offline classifications; host-side git failures stay per-fragment exit
// codes the caller maps to git execution errors.
pub(crate) async fn run_remote_batch_with(
    spawner: &Spawner,
    target: &RemoteTarget,
    fragments: &[BatchFragment],
) -> Result<BatchOutput, CommandError> {
    let nonce = new_nonce()?;
    let mut script = String::new();
    for fragment in fragments {
        allowlisted(&fragment.args)?;
        let effective = match &fragment.cwd {
            Some(cwd) => RemoteTarget {
                user: target.user.clone(),
                host: target.host.clone(),
                port: target.port,
                path: cwd.clone(),
            },
            None => target.clone(),
        };
        let mut command = git_command_string(&effective, &fragment.args, true);
        if let Some(operands) = &fragment.stdin {
            let mut feed = String::from("printf '%s\\000'");
            for operand in operands {
                feed.push(' ');
                feed.push_str(&shell_quote(operand));
            }
            command = format!("{feed} | {command}");
        }
        if !script.is_empty() {
            script.push('\n');
        }
        script.push_str(&frame_fragment(&nonce, &command));
    }
    let spawn = ssh_invocation(
        target,
        &ssh_program(),
        &script,
        multiplex_socket(target).as_deref(),
    );
    let child = (spawner)(spawn).await?;
    let (exit_code, stdout, stderr) = run_child(child, REMOTE_TIMEOUT).await?;
    if exit_code == 255 {
        return Err(ssh_failure(&stderr));
    }
    let outputs = parse_framed_output(&nonce, &stdout)?;
    if outputs.len() != fragments.len() {
        return Err(CommandError::new(
            "remote_framing",
            "The remote batch output was truncated or malformed.",
        ));
    }
    Ok(BatchOutput {
        fragments: outputs,
        stderr,
    })
}

// Adding a remote project validates with the same probe the local open
// uses, run on the host. Failures land in the three typed errors the
// frontend renders inline; ssh reserves exit 255 for its own failures
// (git never exits it here), which is the classification seam.
pub(crate) async fn validate_work_tree(target: &RemoteTarget) -> Result<(), CommandError> {
    validate_work_tree_with(&process_spawner_impl(), target).await
}

pub(crate) async fn validate_work_tree_with(
    spawner: &Spawner,
    target: &RemoteTarget,
) -> Result<(), CommandError> {
    let args = [
        "rev-parse".to_string(),
        "--is-inside-work-tree".to_string(),
    ];
    let (exit_code, stdout, stderr) =
        run_remote_git_budgeted(spawner, target, &args, true, REMOTE_TIMEOUT).await?;
    if exit_code == 255 {
        return Err(ssh_failure(&stderr));
    }
    if exit_code != 0 && !is_not_a_repository_diagnostic(&stderr) {
        return Err(git_execution_error(&stderr));
    }
    if exit_code != 0 || std::str::from_utf8(&stdout).map(str::trim) != Ok("true") {
        return Err(CommandError::new(
            "not_a_work_tree",
            "The remote path is not a Git work tree and was not added.",
        ));
    }
    Ok(())
}

fn ssh_failure(stderr: &[u8]) -> CommandError {
    let text = String::from_utf8_lossy(stderr);
    let lowered = text.to_ascii_lowercase();
    // BatchMode turns passphrase and host-key prompts into these failures:
    // point at the terminal-side fixes instead of hanging on a prompt the
    // app can never answer.
    if lowered.contains("permission denied")
        || lowered.contains("host key verification failed")
        || lowered.contains("no supported authentication")
        || lowered.contains("too many authentication failures")
    {
        return CommandError::new(
            "auth_failed",
            format!(
                "The host refused authentication. Add the key to your ssh-agent and accept the host key once from your own terminal ssh; the app never answers passphrase or host-key prompts. Host: {}",
                text.trim()
            ),
        );
    }
    CommandError::new(
        "unreachable_host",
        format!(
            "The remote host could not be reached. Check the host name and your ssh config. Host: {}",
            text.trim()
        ),
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use super::super::batch_fragment;
    use std::sync::atomic::{AtomicBool, Ordering};

// A fake spawner for read-group tests: it records every composed ssh
// invocation, extracts the batch nonce from the script's echo lines, and
// emits one canned framed output per begin marker, so module tests can
// drive full remote loads without a host. Recorded scripts are the
// composition assertions: one entry per invocation, one frame per fragment.
#[cfg(test)]
pub(crate) fn batch_fake_spawner(
    per_invocation: Vec<Vec<(i32, Vec<u8>)>>,
) -> (Spawner, Arc<std::sync::Mutex<Vec<String>>>) {
    let scripts: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorder = scripts.clone();
    let spawner: Spawner = Arc::new(move |spawn: SshSpawn| {
        let script = spawn.args.last().cloned().unwrap_or_default();
        let nonce = script
            .lines()
            .find_map(|line| {
                line.strip_prefix("echo WTV-")
                    .and_then(|rest| rest.strip_suffix("-begin"))
            })
            .unwrap_or_default()
            .to_string();
        let index = recorder.lock().unwrap().len();
        let frames = per_invocation.get(index).cloned().unwrap_or_default();
        let mut output = String::new();
        for (exit_code, body) in frames {
            output.push_str(&format!("WTV-{nonce}-begin\n"));
            output.push_str(&String::from_utf8_lossy(&body));
            // The host's exit echo glues onto fragment output that lacks a
            // trailing newline, exactly as the real shell produces it.
            output.push_str(&format!("WTV-{nonce}-exit-{exit_code}\n"));
        }
        recorder.lock().unwrap().push(script);
        let child = canned_child(0, output.as_bytes(), b"");
        Box::pin(async move { Ok(child) }) as SpawnResult
    });
    (spawner, scripts)
}

    fn target() -> RemoteTarget {
        RemoteTarget::from_parts(Some("dev"), "host.example", None, "/srv/repo").unwrap()
    }

    #[test]
    fn parses_scp_identity_and_round_trips() {
        for identity in [
            "user@host:/srv/repo",
            "host:/srv/repo",
            "user@host:repos/demo",
            "user@[2001:db8::1]:/srv/repo",
            "user@host:/srv/re po",
            "user@host:~/repos/demo",
        ] {
            let parsed = RemoteTarget::parse_identity(identity).unwrap();
            assert_eq!(parsed.identity(), identity, "{identity}");
            assert_eq!(RemoteTarget::parse_identity(&parsed.identity()).unwrap(), parsed);
        }
        let parsed = RemoteTarget::parse_identity("dev@host.example:/srv/repo").unwrap();
        assert_eq!(parsed.user.as_deref(), Some("dev"));
        assert_eq!(parsed.host, "host.example");
        assert_eq!(parsed.port, None);
        assert_eq!(parsed.path, "/srv/repo");
    }

    #[test]
    fn collapses_default_port_to_scp_form() {
        let parsed = RemoteTarget::parse_identity("ssh://user@host:22/srv/repo").unwrap();
        assert_eq!(parsed.port, None);
        assert_eq!(parsed.identity(), "user@host:/srv/repo");
        let from_parts = RemoteTarget::from_parts(Some("user"), "host", Some(DEFAULT_PORT), "/srv/repo").unwrap();
        assert_eq!(from_parts.identity(), "user@host:/srv/repo");
    }

    #[test]
    fn keeps_non_default_port_in_ssh_url_form() {
        let parsed = RemoteTarget::parse_identity("ssh://user@host:2200/srv/repo").unwrap();
        assert_eq!(parsed.port, Some(2200));
        assert_eq!(parsed.identity(), "ssh://user@host:2200/srv/repo");
        assert_eq!(
            RemoteTarget::parse_identity(&parsed.identity()).unwrap(),
            parsed
        );
        let without_user = RemoteTarget::parse_identity("ssh://host:2200/srv/repo").unwrap();
        assert_eq!(without_user.identity(), "ssh://host:2200/srv/repo");
    }

    #[test]
    fn keeps_ipv6_hosts_bracketed() {
        let scp = RemoteTarget::parse_identity("user@[2001:db8::1]:/srv/repo").unwrap();
        assert_eq!(scp.host, "[2001:db8::1]");
        assert_eq!(scp.identity(), "user@[2001:db8::1]:/srv/repo");
        let url = RemoteTarget::parse_identity("ssh://user@[2001:db8::1]:2222/srv/repo").unwrap();
        assert_eq!(url.port, Some(2222));
        assert_eq!(url.identity(), "ssh://user@[2001:db8::1]:2222/srv/repo");
        let no_port_url = RemoteTarget::parse_identity("ssh://[::1]/srv/repo").unwrap();
        assert_eq!(no_port_url.identity(), "[::1]:/srv/repo");
    }

    #[test]
    fn derives_repo_name_from_the_path() {
        assert_eq!(target().name(), "repo");
        assert_eq!(
            RemoteTarget::parse_identity("user@host:/home/dev/repos/demo/").unwrap().name(),
            "demo"
        );
        assert_eq!(
            RemoteTarget::parse_identity("user@host:~/repos/demo").unwrap().name(),
            "demo"
        );
    }

    #[test]
    fn rejects_malformed_targets() {
        for bad in [
            "",
            "   ",
            "host",
            "host:",
            ":/repo",
            "user@:repo",
            "user@host:",
            "-host:/repo",
            "-user@host:/repo",
            r"user@host:\repo",
            r"C:\repos\demo",
            "user name@host:/repo",
            "user@ho st:/repo",
            "user@host:/re\u{0}po",
            "ssh://host",
            "ssh://host:",
            "ssh://host:abc/repo",
            "ssh://host:99999/repo",
            "ssh://host:0/repo",
            "ssh://[::1/repo",
            "ssh://user@/repo",
            "file://host/repo",
        ] {
            let error = RemoteTarget::parse_identity(bad).unwrap_err();
            assert_eq!(error.code, "invalid_remote_target", "{bad}");
        }
        let zero_port = RemoteTarget::from_parts(Some("u"), "host", Some(0), "/repo").unwrap_err();
        assert_eq!(zero_port.code, "invalid_remote_target");
    }

    #[test]
    fn shell_quote_escapes_metacharacters() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote("a b$(rm -rf)/;x|y"), "'a b$(rm -rf)/;x|y'");
        assert_eq!(shell_quote("new\nline"), "'new\nline'");
    }

    #[test]
    fn composed_command_quotes_every_variable_slot() {
        let target =
            RemoteTarget::from_parts(Some("dev"), "host.example", None, "/srv/my repo").unwrap();
        let machine =
            git_command_string(&target, &["log".to_string(), "it's".to_string()], true);
        assert!(machine.starts_with(
            "LC_ALL=C LANG=C GIT_NO_LAZY_FETCH=1 GIT_ATTR_NOSYSTEM=1 git"
        ));
        assert!(machine.contains("-C '/srv/my repo'"));
        assert!(machine.contains("'it'\\''s'"));
        assert!(machine.contains("-c 'core.fsmonitor=false'"));
        assert!(machine.contains("-c 'core.attributesFile='"));
        let plain = git_command_string(&target, &[], false);
        assert!(plain.starts_with("GIT_NO_LAZY_FETCH=1"));
        assert!(!plain.starts_with("LC_ALL"));
    }

    #[test]
    fn nonces_are_random_hex() {
        let first = new_nonce().unwrap();
        let second = new_nonce().unwrap();
        assert_ne!(first, second);
        assert_eq!(first.len(), 16);
        assert!(first.chars().all(|character| character.is_ascii_hexdigit()));
    }

    fn args_of(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn allowlists_the_read_only_plumbing_set() {
        let accepted: &[&[&str]] = &[
            &["rev-parse", "--is-inside-work-tree"],
            &["rev-parse", "--verify", "--quiet", "abc123"],
            &["rev-parse", "--verify", "abc123^{{commit}}"],
            &[
                "log",
                "--skip=0",
                "--max-count=51",
                "--decorate=full",
                "--format=%H%x1f%s",
                "abc123",
            ],
            // The exact shape `commit_detail` spawns for the history quick look.
            &[
                "log",
                "-1",
                "--format=%H%x00%an%x00%cd%x00%P%x00%s%x00%b",
                "abc123",
            ],
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                "--name-status",
                "-z",
                "a...b",
                "--",
            ],
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--numstat",
                "-z",
                "-R",
                "a...b",
                "--",
                "src/file.rs",
            ],
            &[
                "status",
                "--porcelain=v1",
                "--no-renames",
                "--untracked-files=all",
                "-z",
            ],
            &["worktree", "list", "--porcelain"],
            &["for-each-ref", "refs/heads", "refs/remotes", "--format=%(refname)"],
            &["for-each-ref", "refs/heads", "--merged=abc123", "--format=%(refname)"],
            &["rev-list", "--no-walk", "a", "b", "--not", "c"],
            &["rev-list", "--left-right", "--count", "a...b"],
            &["merge-base", "a", "b"],
            &["cat-file", "blob", "abc123"],
            &["ls-tree", "-z", "abc123", "--", "src/file.rs"],
            &["ls-files", "--others", "--exclude-standard", "-z"],
            &["ls-files", "--others", "--exclude-standard", "-z", ":(literal)name"],
            &["remote"],
            &["config", "--get-regexp", r"^remote\..*\.promisor$"],
            &["config", "--get", "remote.origin.url"],
            &["hash-object", "-t", "tree", "--stdin"],
            &["check-attr", "-z", "--stdin", "filter"],
            &[
                "--no-optional-locks",
                "status",
                "--porcelain=v1",
                "--no-renames",
                "--untracked-files=all",
                "-z",
            ],
            &["-c", "core.quotePath=false", "worktree", "list", "--porcelain"],
            &[
                "-c",
                "filter.x.clean=cat",
                "-c",
                "filter.x.process=cat",
                "-c",
                "filter.x.required=false",
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--name-status",
                "-z",
                "a...b",
                "--",
            ],
        ];
        for argv in accepted {
            assert!(
                allowlisted(&args_of(argv)).is_ok(),
                "expected allowlisted: {argv:?}"
            );
        }
    }

    #[test]
    fn refuses_everything_outside_the_allowlist() {
        let refused: &[&[&str]] = &[
            &["push", "origin"],
            &["fetch", "--all"],
            &["clone", "https://example.com/repo.git"],
            &["worktree", "add", "/tmp/elsewhere"],
            &["worktree", "list"],
            &["remote", "-v"],
            &["remote", "add", "origin", "https://example.com/repo.git"],
            &["config", "--unset", "user.name"],
            &["config", "user.name", "someone"],
            &["status", "--porcelain=v2", "-z"],
            &["cat-file", "commit", "abc123"],
            &["hash-object", "-t", "blob", "--stdin"],
            &["rev-parse", "--upload-pack", "evil"],
            // Missing the required diff neutralizations.
            &["diff", "--numstat", "-z", "a...b"],
            // The phase-3 addition is not allowlisted yet.
            &["diff", "--no-index", "--no-ext-diff", "--no-textconv", "a", "b"],
            &["check-attr", "-z", "diff", "--", "file"],
            &["-c", "core.fsmonitor=true", "status", "--porcelain=v1"],
            &["-c", "filter.x.clean=evil", "diff", "--no-ext-diff", "--no-textconv"],
            &["-c", "filter.x.clean", "diff", "--no-ext-diff", "--no-textconv"],
            &["status"],
            &[],
        ];
        for argv in refused {
            let error = allowlisted(&args_of(argv)).unwrap_err();
            assert_eq!(error.code, "remote_command_refused", "{argv:?}");
        }
    }

    #[test]
    fn control_master_options_stay_gated() {
        let options = control_master_options(Some("/tmp/wtv-ssh.sock"));
        if cfg!(windows) {
            assert!(options.is_empty());
        } else {
            let text = options.join(" ");
            assert!(text.contains("ControlMaster=auto"));
            assert!(text.contains("ControlPath=/tmp/wtv-ssh.sock"));
            assert!(text.contains("ControlPersist=60"));
        }
        assert!(control_master_options(None).is_empty());
    }

    fn exit_status(code: i32) -> std::process::ExitStatus {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(code << 8)
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(code as u32)
        }
    }

    fn canned_child(exit_code: i32, stdout: &[u8], stderr: &[u8]) -> BoxChild {
        struct Canned {
            stdout: std::io::Cursor<Vec<u8>>,
            stderr: std::io::Cursor<Vec<u8>>,
            status: std::process::ExitStatus,
        }
        impl RemoteChild for Canned {
            fn stdout(&mut self) -> BoxReader {
                Box::new(self.stdout.clone())
            }
            fn stderr(&mut self) -> BoxReader {
                Box::new(self.stderr.clone())
            }
            fn wait(&mut self) -> BoxWait<'_> {
                let status = self.status;
                Box::pin(async move { Ok(status) })
            }
            fn kill(&mut self) -> BoxKill<'_> {
                Box::pin(async { Ok(()) })
            }
        }
        Box::new(Canned {
            stdout: std::io::Cursor::new(stdout.to_vec()),
            stderr: std::io::Cursor::new(stderr.to_vec()),
            status: exit_status(exit_code),
        })
    }

    fn canned_spawner(exit_code: i32, stdout: &'static [u8], stderr: &'static [u8]) -> Spawner {
        Arc::new(move |_spawn: SshSpawn| {
            let child = canned_child(exit_code, stdout, stderr);
            Box::pin(async move { Ok(child) }) as SpawnResult
        })
    }

    // A fake runner that spawns a real local process in ssh's place, for
    // the pipeline properties (bounded output, deadline) that need a live
    // child.
    fn process_spawning_fake(program: &str, args: &[&str]) -> Spawner {
        let program = program.to_string();
        let args: Vec<String> = args.iter().map(|argument| argument.to_string()).collect();
        Arc::new(move |_spawn: SshSpawn| {
            let program = program.clone();
            let args = args.clone();
            Box::pin(async move {
                let mut command = Command::new(&program);
                command
                    .args(&args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true);
                let child = command.spawn().map_err(|_| {
                    CommandError::new("remote_spawn", "The fake child could not start.")
                })?;
                Ok(Box::new(SshChild::new(child)) as BoxChild)
            }) as SpawnResult
        })
    }

    #[tokio::test]
    async fn spawn_composes_batchmode_port_and_quoted_command() {
        let target = RemoteTarget::from_parts(
            Some("dev"),
            "host.example",
            Some(2200),
            "/srv/re po",
        )
        .unwrap();
        let git_args = args_of(&["rev-parse", "--is-inside-work-tree"]);
        let recorded: Arc<std::sync::Mutex<Option<SshSpawn>>> =
            Arc::new(std::sync::Mutex::new(None));
        let recorder = recorded.clone();
        let spawner: Spawner = Arc::new(move |spawn: SshSpawn| {
            *recorder.lock().unwrap() = Some(spawn);
            let child = canned_child(0, b"true\n", b"");
            Box::pin(async move { Ok(child) }) as SpawnResult
        });
        let outcome = run_remote_git_budgeted(
            &spawner,
            &target,
            &git_args,
            true,
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(outcome.0, 0);
        let spawn = recorded.lock().unwrap().take().unwrap();
        assert_eq!(spawn.args[0], "-o");
        assert_eq!(spawn.args[1], "BatchMode=yes");
        let port_at = spawn.args.iter().position(|argument| argument == "-p").unwrap();
        assert_eq!(spawn.args[port_at + 1], "2200");
        assert_eq!(spawn.args[spawn.args.len() - 2], "dev@host.example");
        assert_eq!(
            spawn.args.last().unwrap(),
            &git_command_string(&target, &git_args, true)
        );
        assert!(spawn.args.last().unwrap().contains("'/srv/re po'"));
    }

    #[tokio::test]
    async fn allowlist_refusal_happens_before_any_spawn() {
        let spawned = Arc::new(AtomicBool::new(false));
        let flag = spawned.clone();
        let spawner: Spawner = Arc::new(move |_spawn: SshSpawn| {
            flag.store(true, Ordering::Relaxed);
            let child = canned_child(0, b"", b"");
            Box::pin(async move { Ok(child) }) as SpawnResult
        });
        let error = run_remote_git_budgeted(
            &spawner,
            &target(),
            &args_of(&["push", "origin"]),
            false,
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "remote_command_refused");
        assert!(!spawned.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn output_beyond_the_bound_is_refused() {
        let dir = crate::testutil::test_path("remote-runner-bound");
        std::fs::create_dir_all(&dir).unwrap();
        let big = dir.join("big.txt");
        let content = "0123456789abcdef\n".repeat(65536 * 17);
        std::fs::write(&big, &content).unwrap();
        let path = big.to_string_lossy().into_owned();
        let spawner = if cfg!(windows) {
            process_spawning_fake("cmd", &["/c", "type", &path])
        } else {
            process_spawning_fake("cat", &[&path])
        };
        let child = (spawner)(SshSpawn {
            program: String::new(),
            args: Vec::new(),
        })
        .await
        .unwrap();
        let error = run_child(child, Duration::from_secs(60)).await.unwrap_err();
        assert_eq!(error.code, "git_output_too_large");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn deadline_aborts_a_hung_remote_command() {
        let spawner = if cfg!(windows) {
            process_spawning_fake("ping", &["-n", "30", "127.0.0.1"])
        } else {
            process_spawning_fake("sleep", &["30"])
        };
        let child = (spawner)(SshSpawn {
            program: String::new(),
            args: Vec::new(),
        })
        .await
        .unwrap();
        let started = std::time::Instant::now();
        let error = run_child(child, Duration::from_millis(700)).await.unwrap_err();
        assert_eq!(error.code, "remote_timeout");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn kill_on_drop_ends_the_child() {
        struct Dropped {
            killed: Arc<AtomicBool>,
        }
        impl RemoteChild for Dropped {
            fn stdout(&mut self) -> BoxReader {
                Box::new(std::io::Cursor::new(Vec::new()))
            }
            fn stderr(&mut self) -> BoxReader {
                Box::new(std::io::Cursor::new(Vec::new()))
            }
            fn wait(&mut self) -> BoxWait<'_> {
                let status = exit_status(0);
                Box::pin(async move { Ok(status) })
            }
            fn kill(&mut self) -> BoxKill<'_> {
                self.killed.store(true, Ordering::Relaxed);
                Box::pin(async { Ok(()) })
            }
        }
        impl Drop for Dropped {
            fn drop(&mut self) {
                // Mirrors SshChild's kill-on-drop contract.
                self.killed.store(true, Ordering::Relaxed);
            }
        }
        let killed = Arc::new(AtomicBool::new(false));
        let child: BoxChild = Box::new(Dropped {
            killed: killed.clone(),
        });
        drop(child);
        assert!(killed.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn validation_accepts_a_work_tree() {
        let spawner = canned_spawner(0, b"true\n", b"");
        validate_work_tree_with(&spawner, &target()).await.unwrap();
    }

    #[tokio::test]
    async fn validation_rejects_a_non_work_tree() {
        let spawner = canned_spawner(
            128,
            b"",
            b"fatal: not a git repository (or any of the parent directories): .git\n",
        );
        let error = validate_work_tree_with(&spawner, &target()).await.unwrap_err();
        assert_eq!(error.code, "not_a_work_tree");
        // Any other git failure keeps git's own diagnostic.
        let spawner = canned_spawner(1, b"", b"fatal: something else\n");
        let error = validate_work_tree_with(&spawner, &target()).await.unwrap_err();
        assert_eq!(error.code, "git_execution");
    }

    #[tokio::test]
    async fn validation_classifies_host_and_auth_failures() {
        let unreachable = canned_spawner(
            255,
            b"",
            b"ssh: Could not resolve hostname host.example: Name or service not known\n",
        );
        let error = validate_work_tree_with(&unreachable, &target())
            .await
            .unwrap_err();
        assert_eq!(error.code, "unreachable_host");

        let refused = canned_spawner(
            255,
            b"",
            b"host.example: Permission denied (publickey,password).\n",
        );
        let error = validate_work_tree_with(&refused, &target())
            .await
            .unwrap_err();
        assert_eq!(error.code, "auth_failed");
        assert!(error.message.contains("ssh-agent"));

        let host_key = canned_spawner(255, b"", b"Host key verification failed.\n");
        let error = validate_work_tree_with(&host_key, &target())
            .await
            .unwrap_err();
        assert_eq!(error.code, "auth_failed");
        assert!(error.message.contains("host key"));
    }

    #[tokio::test]
    async fn batch_runs_one_invocation_per_group_and_demultiplexes() {
        let (spawner, scripts) = batch_fake_spawner(vec![vec![
            (0, b"one\n".to_vec()),
            (3, b"two".to_vec()),
            (0, Vec::new()),
        ]]);
        let output = run_remote_batch_with(
            &spawner,
            &target(),
            &[
                batch_fragment(args_of(&["worktree", "list", "--porcelain"])),
                batch_fragment(args_of(&["status", "--porcelain=v1"])),
                batch_fragment(args_of(&["remote"])),
            ],
        )
        .await
        .unwrap();
        // Exactly one ssh invocation carries the whole read group.
        assert_eq!(scripts.lock().unwrap().len(), 1);
        assert_eq!(scripts.lock().unwrap()[0].matches("-begin").count(), 3);
        // Outputs come back per fragment with their own exit codes.
        assert_eq!(output.fragments.len(), 3);
        assert_eq!(output.fragments[0].exit_code, 0);
        assert_eq!(output.fragments[0].stdout, b"one\n");
        assert_eq!(output.fragments[1].exit_code, 3);
        assert_eq!(output.fragments[2].exit_code, 0);
        assert!(output.ok(0).is_ok());
        // A failing fragment maps onto the git execution error.
        assert_eq!(output.ok(1).unwrap_err().code, "git_execution");
        assert!(output.ok(2).is_ok());
    }

    #[tokio::test]
    async fn batch_refuses_non_allowlisted_fragments_before_spawning() {
        let (spawner, scripts) = batch_fake_spawner(vec![]);
        let error = run_remote_batch_with(
            &spawner,
            &target(),
            &[
                batch_fragment(args_of(&["remote"])),
                batch_fragment(args_of(&["push", "origin"])),
            ],
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "remote_command_refused");
        assert!(scripts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn batch_composes_quoted_fragments_and_stdin_feeds() {
        let (spawner, scripts) =
            batch_fake_spawner(vec![vec![(0, b"a.bin\0filter\0lfs\0".to_vec())]]);
        run_remote_batch_with(
            &spawner,
            &target(),
            &[BatchFragment {
                cwd: Some("/srv/re po".into()),
                args: args_of(&["check-attr", "-z", "--stdin", "filter"]),
                stdin: Some(vec!["it's a file".into()]),
            }],
        )
        .await
        .unwrap();
        let script = scripts.lock().unwrap()[0].clone();
        // Every variable slot rides the quoting helper.
        assert!(script.contains("'/srv/re po'"));
        assert!(script.contains("'it'\\''s a file'"));
        // The stdin feed pipes NUL-separated operands into the allowlisted
        // command, and machine-parsed output keeps the locale pin.
        assert!(script.contains("printf '%s\\000' 'it'\\''s a file' | "));
        assert!(script.contains("LC_ALL=C LANG=C"));
        assert!(script.contains("GIT_NO_LAZY_FETCH=1"));
    }

    #[tokio::test]
    async fn batch_maps_ssh_failures_and_output_truncation() {
        // ssh's own exit 255 classifies through the phase-1 failure map.
        let spawner = canned_spawner(255, b"", b"host.example: Permission denied.\n");
        let error = run_remote_batch_with(
            &spawner,
            &target(),
            &[batch_fragment(args_of(&["remote"]))],
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "auth_failed");

        // Fewer closing frames than fragments is truncation, never a
        // silent partial result.
        let (spawner, _scripts) = batch_fake_spawner(vec![vec![(0, b"partial".to_vec())]]);
        let error = run_remote_batch_with(
            &spawner,
            &target(),
            &[
                batch_fragment(args_of(&["remote"])),
                batch_fragment(args_of(&["worktree", "list", "--porcelain"])),
            ],
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "remote_framing");
    }
}
