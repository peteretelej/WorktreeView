//! The headless server: one binary, one store, one repos directory. It
//! boots without a Tauri runtime, applies the store migrations in its own
//! home, wires the four push sinks to a broadcast channel instead of the
//! webview, and starts the agent endpoint with the SSE stream on top. The
//! agent face is the desktop's, unchanged; only the surroundings differ.

use crate::home;
use crate::identity;
use crate::transport::{self, ListenerConfig, McpStatusHandle, PushEvent, TransportDeps};
use sqlx::{sqlite::SqliteConnectOptions, SqlitePool};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::broadcast;

// How far a subscriber may fall behind before its stream is cut and the
// client reconnects with backoff (docs/server.md).
const EVENT_CHANNEL_CAPACITY: usize = 256;

pub(crate) const SERVER_ENV_VAR: &str = "WORKTREEVIEW_SERVER_HOME";
const SERVER_DEFAULT_HOME: &str = ".worktreeview-server";
const SERVER_STORE_NAME: &str = "worktreeview-server.sqlite3";
// Loopback, one port above the desktop's 9888, so a desktop app and a
// server coexist on one host without configuration.
const DEFAULT_BIND: &str = "127.0.0.1:9890";
const USAGE: &str = "usage: worktreeview-server serve [--home <dir>] [--bind <addr:port>]\n       worktreeview-server create-admin <name> [--home <dir>]";
const CREATE_ADMIN_USAGE: &str =
    "usage: worktreeview-server create-admin <name> [--home <dir>]";

#[derive(Debug)]
struct ServeOptions {
    home: Option<String>,
    bind: String,
}

#[derive(Debug)]
struct CreateAdminOptions {
    home: Option<String>,
    name: String,
}

// The face this invocation runs: the listener, or the one-shot identity
// bootstrap.
#[derive(Debug)]
enum Command {
    Serve(ServeOptions),
    CreateAdmin(CreateAdminOptions),
}

impl Command {
    fn home(&self) -> Option<&str> {
        match self {
            Command::Serve(options) => options.home.as_deref(),
            Command::CreateAdmin(options) => options.home.as_deref(),
        }
    }
}// The user profile root without a Tauri runtime to ask.
#[cfg(windows)]
fn home_root() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(PathBuf::from)
}

#[cfg(not(windows))]
fn home_root() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

// Launch override wins over the environment, which wins over the default
// under the user profile; the same precedence the desktop home resolves
// with, under a server-specific name.
fn server_home_dir(home_root: &Path, arg: Option<&str>, env: Option<&str>) -> PathBuf {
    let chosen = arg
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| env.map(str::trim).filter(|value| !value.is_empty()));
    match chosen {
        Some(value) => PathBuf::from(value),
        None => home_root.join(SERVER_DEFAULT_HOME),
    }
}

// The `--bind <addr:port>` override, from either the spaced or the
// `--bind=<addr:port>` form; later flags lose to the first, mirroring the
// home argument's parsing.
fn bind_arg_from<I>(args: I) -> Option<String>
where
    I: IntoIterator<Item = std::ffi::OsString>,
{
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let Ok(arg) = arg.into_string() else {
            continue;
        };
        if let Some(value) = arg.strip_prefix("--bind=") {
            if !value.trim().is_empty() {
                return Some(value.trim().to_string());
            }
        } else if arg == "--bind" {
            let value = match args.next() {
                Some(value) => value,
                None => continue,
            };
            let Ok(value) = value.into_string() else {
                continue;
            };
            if !value.trim().is_empty() && !value.starts_with('-') {
                return Some(value.trim().to_string());
            }
        }
    }
    None
}

fn split_bind(bind: &str) -> Result<(String, u16), String> {
    let Some((address, port)) = bind.rsplit_once(':') else {
        return Err(format!("--bind expects <addr:port>, got \"{bind}\""));
    };
    if address.trim().is_empty() {
        return Err(format!("--bind expects <addr:port>, got \"{bind}\""));
    }
    let port: u16 = port
        .trim()
        .parse()
        .map_err(|_| format!("--bind expects a port from 0 to 65535, got \"{bind}\""))?;
    Ok((address.trim().to_string(), port))
}

fn serve_options(args: &[std::ffi::OsString]) -> Result<ServeOptions, String> {
    if args.get(1).and_then(|arg| arg.to_str()) != Some("serve") {
        return Err(USAGE.to_string());
    }
    Ok(ServeOptions {
        home: home::home_arg_from(args.iter().cloned()),
        bind: bind_arg_from(args.iter().cloned()).unwrap_or_else(|| DEFAULT_BIND.to_string()),
    })
}

// The name is the first positional argument; name validation (trim,
// non-empty, length) belongs to the store accessor that owns the users
// table, so a leading dash only means "no name given" here.
fn create_admin_options(args: &[std::ffi::OsString]) -> Result<CreateAdminOptions, String> {
    let name = match args.get(2).and_then(|arg| arg.to_str()) {
        Some(name) if !name.starts_with('-') => name,
        _ => return Err(CREATE_ADMIN_USAGE.to_string()),
    };
    Ok(CreateAdminOptions {
        home: home::home_arg_from(args.iter().cloned()),
        name: name.to_string(),
    })
}

fn command_from(args: &[std::ffi::OsString]) -> Result<Command, String> {
    match args.get(1).and_then(|arg| arg.to_str()) {
        Some("serve") => serve_options(args).map(Command::Serve),
        Some("create-admin") => create_admin_options(args).map(Command::CreateAdmin),
        _ => Err(USAGE.to_string()),
    }
}

// The server store opens with the same migrations. A store written by a
// newer binary refuses untouched: no dialog can ask here, and rebuilding
// over schema this build cannot read would destroy it.
async fn open_store(db_path: &Path) -> Result<SqlitePool, String> {
    let options = SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(true);
    let pool = sqlx::SqlitePool::connect_with(options)
        .await
        .map_err(|error| format!("Could not connect to server storage: {error}"))?;
    match sqlx::migrate!().run(&pool).await {
        Ok(()) => Ok(pool),
        Err(sqlx::migrate::MigrateError::VersionMissing(_)) => {
            pool.close().await;
            Err(format!(
                "Server storage {} was written by a newer version of WorktreeView; update the server binary.",
                db_path.display()
            ))
        }
        Err(error) => {
            pool.close().await;
            Err(format!("Could not migrate server storage: {error}"))
        }
    }
}

// One push frame; the payload JSON is what the desktop bridge emits for
// the same event name.
fn push(events: &broadcast::Sender<PushEvent>, kind: &'static str, payload: &impl serde::Serialize) {
    match serde_json::to_value(payload) {
        Ok(payload) => {
            let _ = events.send(PushEvent { kind, payload });
        }
        Err(error) => log::warn!("could not serialize the {kind} event: {error}"),
    }
}

// Open the store, wire the sinks to the broadcast channel, and start the
// listener. A bind failure is fatal: a headless server has nothing else
// to do, while the desktop app would surface it in Settings.
async fn serve(data_dir: PathBuf, address: String, port: u16) -> i32 {
    let pool = match open_store(&data_dir.join(SERVER_STORE_NAME)).await {
        Ok(pool) => pool,
        Err(message) => {
            eprintln!("{message}");
            return 1;
        }
    };
    let (events, _) = broadcast::channel::<PushEvent>(EVENT_CHANNEL_CAPACITY);
    let arrivals: transport::ArrivalSink = {
        let events = events.clone();
        Arc::new(move |arrival| push(&events, "submission-received", &arrival))
    };
    let refreshes: transport::RefreshSink = {
        let events = events.clone();
        Arc::new(move |repo_path| {
            push(&events, "project-refreshed", &serde_json::json!({ "repo_path": repo_path }))
        })
    };
    let comment_changes: transport::CommentSink = {
        let events = events.clone();
        Arc::new(move |change| push(&events, "comment-changed", &change))
    };
    let request_changes: transport::RequestChangeSink = {
        let events = events.clone();
        Arc::new(move |change| push(&events, "review-request-changed", &change))
    };
    let config = ListenerConfig { enabled: true, address, port };
    let status = McpStatusHandle::for_config(&config);
    status.set_paths(&data_dir);
    let deps = TransportDeps {
        data_dir,
        arrivals,
        refreshes,
        comment_changes,
        request_changes,
    };
    match transport::start_with_events(pool, deps, config, status.clone(), Some(events)).await {
        Ok(Some(_listener)) => {
            let live = status.lock_status();
            eprintln!("worktreeview-server listening on {}:{}", live.address, live.port);
            eprintln!("agent endpoint config: {}", live.config_path);
            drop(live);
            // No graceful-shutdown channel exists in this phase: the
            // process parks and the listener thread owns the sockets until
            // the process is stopped. The next start rewrites the config
            // file, so a stale one after a stop is survivable (clients
            // re-read on refusal).
            loop {
                std::thread::park();
            }
        }
        Ok(None) => {
            let error = status
                .lock_status()
                .error
                .clone()
                .unwrap_or_else(|| "the listener did not start".to_string());
            eprintln!("worktreeview-server could not start: {error}");
            1
        }
        Err(message) => {
            eprintln!("worktreeview-server could not start: {message}");
            1
        }
    }
}

// The one-shot identity bootstrap: opens the store, creates the first
// admin with their initial token, and prints the secret once to stdout.
// Refusals (a user already exists, an invalid name) are clear stderr
// reasons and a nonzero exit.
async fn create_admin(data_dir: PathBuf, name: &str) -> i32 {
    let pool = match open_store(&data_dir.join(SERVER_STORE_NAME)).await {
        Ok(pool) => pool,
        Err(message) => {
            eprintln!("{message}");
            return 1;
        }
    };
    let created = identity::create_first_admin_in_pool(&pool, name).await;
    pool.close().await;
    match created {
        Ok(created) => {
            println!("created admin {}", created.user.name);
            println!("bearer token (printed once): {}", created.secret);
            0
        }
        Err(error) => {
            eprintln!("{}", error.message);
            1
        }
    }
}

// Shared boot for both subcommands: resolve and create the server home,
// then build the runtime the store and listener run on.
fn server_runtime(home: Option<&str>) -> Result<(PathBuf, tokio::runtime::Runtime), i32> {
    let Some(home_root) = home_root() else {
        eprintln!("Could not resolve the user home directory; pass --home with the server home directory.");
        return Err(1);
    };
    let data_dir = server_home_dir(&home_root, home, std::env::var(SERVER_ENV_VAR).ok().as_deref());
    if let Err(error) = std::fs::create_dir_all(&data_dir) {
        eprintln!(
            "Could not create the server home directory {}: {error}",
            data_dir.display()
        );
        return Err(1);
    }
    match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => Ok((data_dir, runtime)),
        Err(error) => {
            eprintln!("Could not start the server runtime: {error}");
            Err(1)
        }
    }
}

// The server binary's entry point; the desktop's `run()` is never called.
// Returns only on a fatal error, with the process exit code.
pub fn run_server() -> i32 {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let command = match command_from(&args) {
        Ok(command) => command,
        Err(usage) => {
            eprintln!("{usage}");
            return 2;
        }
    };
    let (data_dir, runtime) = match server_runtime(command.home()) {
        Ok(pair) => pair,
        Err(code) => return code,
    };
    match command {
        Command::Serve(options) => {
            let (address, port) = match split_bind(&options.bind) {
                Ok(bind) => bind,
                Err(message) => {
                    eprintln!("{message}");
                    eprintln!("{USAGE}");
                    return 2;
                }
            };
            runtime.block_on(serve(data_dir, address, port))
        }
        Command::CreateAdmin(options) => runtime.block_on(create_admin(data_dir, &options.name)),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        bind_arg_from, command_from, serve_options, server_home_dir, split_bind,
        Command, CREATE_ADMIN_USAGE, DEFAULT_BIND, SERVER_DEFAULT_HOME, SERVER_ENV_VAR, USAGE,
    };
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn server_home_resolves_arg_over_env_over_default() {
        let root = Path::new("/Users/dev");
        assert_eq!(
            server_home_dir(root, Some("/tmp/srv"), Some("/tmp/env")),
            PathBuf::from("/tmp/srv")
        );
        assert_eq!(
            server_home_dir(root, None, Some("/tmp/env")),
            PathBuf::from("/tmp/env")
        );
        assert_eq!(
            server_home_dir(root, None, None),
            root.join(SERVER_DEFAULT_HOME)
        );
        // Empty values count as unset, as on the desktop.
        assert_eq!(
            server_home_dir(root, Some("  "), Some("/tmp/env")),
            PathBuf::from("/tmp/env")
        );
    }

    #[test]
    fn bind_parses_both_flag_forms() {
        assert_eq!(
            bind_arg_from(args(&["worktreeview-server", "serve", "--bind", "127.0.0.1:9999"])),
            Some("127.0.0.1:9999".into())
        );
        assert_eq!(
            bind_arg_from(args(&["worktreeview-server", "serve", "--bind=127.0.0.1:9999"])),
            Some("127.0.0.1:9999".into())
        );
        assert_eq!(
            bind_arg_from(args(&["worktreeview-server", "serve"])),
            None,
            "the default bind needs no flag"
        );
        assert_eq!(
            bind_arg_from(args(&["worktreeview-server", "serve", "--bind", "--home", "/x"])),
            None,
            "a following flag is never the value"
        );
    }

    #[test]
    fn split_bind_needs_address_and_valid_port() {
        assert_eq!(
            split_bind("127.0.0.1:9890").unwrap(),
            ("127.0.0.1".to_string(), 9890)
        );
        assert_eq!(split_bind("localhost:0").unwrap(), ("localhost".to_string(), 0));
        assert!(split_bind("9890").is_err());
        assert!(split_bind(":9890").is_err());
        assert!(split_bind("127.0.0.1:notaport").is_err());
        assert!(split_bind("127.0.0.1:99999").is_err());
    }

    #[test]
    fn unknown_subcommands_are_refused_and_known_ones_dispatch() {
        assert_eq!(command_from(&args(&["worktreeview-server"])).unwrap_err(), USAGE);
        assert_eq!(
            command_from(&args(&["worktreeview-server", "daemonize"])).unwrap_err(),
            USAGE
        );
        assert!(matches!(
            command_from(&args(&["worktreeview-server", "serve"])).unwrap(),
            Command::Serve(_)
        ));
        let options = serve_options(&args(&["worktreeview-server", "serve"])).unwrap();
        assert_eq!(options.home, None);
        assert_eq!(options.bind, DEFAULT_BIND);
    }

    #[test]
    fn create_admin_reads_the_name_and_home_flags() {
        let command = command_from(&args(&["worktreeview-server", "create-admin", "ops"])).unwrap();
        let Command::CreateAdmin(options) = command else {
            panic!("create-admin must parse into the create-admin command");
        };
        assert_eq!(options.name, "ops");
        assert_eq!(options.home, None);

        let command = command_from(&args(&[
            "worktreeview-server",
            "create-admin",
            "ops",
            "--home=/tmp/srv",
        ]))
        .unwrap();
        let Command::CreateAdmin(options) = command else {
            panic!("create-admin must parse into the create-admin command");
        };
        assert_eq!(options.name, "ops");
        assert_eq!(options.home.as_deref(), Some("/tmp/srv"));
    }

    #[test]
    fn create_admin_needs_a_name() {
        assert_eq!(
            command_from(&args(&["worktreeview-server", "create-admin"])).unwrap_err(),
            CREATE_ADMIN_USAGE
        );
        // A leading dash is a forgotten name, not a name.
        assert_eq!(
            command_from(&args(&["worktreeview-server", "create-admin", "--home", "/tmp/srv"]))
                .unwrap_err(),
            CREATE_ADMIN_USAGE
        );
    }

    #[test]
    fn serve_options_read_home_and_bind_flags() {
        let options = serve_options(&args(&[
            "worktreeview-server",
            "serve",
            "--home",
            "/tmp/srv",
            "--bind=127.0.0.1:9999",
        ]))
        .unwrap();
        assert_eq!(options.home.as_deref(), Some("/tmp/srv"));
        assert_eq!(options.bind, "127.0.0.1:9999");
    }

    #[test]
    fn server_env_var_is_the_server_one() {
        // The desktop's WORKTREEVIEW_DATA_DIR must not move the server
        // home; the override names are distinct.
        assert_ne!(SERVER_ENV_VAR, crate::home::HOME_ENV_VAR);
    }
}
