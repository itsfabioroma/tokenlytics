use chrono::{DateTime, Duration, NaiveDate, Utc};
use notify::{Config as NotifyConfig, Event, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashMap, HashSet},
    env,
    fs::{self, OpenOptions},
    io::{IsTerminal, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{mpsc, Arc, Condvar, Mutex},
    thread,
    time::{Duration as StdDuration, Instant, SystemTime, UNIX_EPOCH},
};

const DASHBOARD_HTML: &str = include_str!("../dashboard.html");
// public host for the "compete globally" flow. swap this when the server moves.
const DEFAULT_GLOBAL_HOST: &str = "https://tokenlytics.ultracontext.com";
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");
// matches the figlet shown on the web dashboard header.
const FIGLET: &str = r"o-O-o  o-o  o  o o--o o   o o    o   o o-O-o o-O-o   o-o  o-o
  |   o   o | /  |    |\  | |     \ /    |     |    /    |
  |   |   | OO   O-o  | \ | |      O     |     |   O      o-o
  |   o   o | \  |    |  \| |      |     |     |    \        |
  o    o-o  o  o o--o o   o O---o  o     o   o-O-o   o-o o--o";
// brand colors. matches dashboard #f59e0b (claude) / #34d399 (codex).
const COLOR_CLAUDE: u8 = 214;
const COLOR_CODEX: u8 = 85;
// any submit from a client below this is rejected 426. bump on breaking changes.
const MIN_CLIENT_VERSION: &str = "0.1.0";
const UPGRADE_URL: &str = "https://ultracontext.com/tokenlytics.sh";
const EVENT_DEBOUNCE: StdDuration = StdDuration::from_millis(150);
const POLL_FALLBACK_INTERVAL: StdDuration = StdDuration::from_secs(1);
const ROLLING_REFRESH_INTERVAL: StdDuration = StdDuration::from_secs(60);
const SSE_KEEPALIVE_INTERVAL: StdDuration = StdDuration::from_secs(15);

#[derive(Clone)]
struct AppState {
    cache: Arc<Mutex<CacheState>>,
    cache_changed: Arc<Condvar>,
    paths: Paths,
    leaderboard: Arc<Mutex<HashMap<String, LeaderboardEntry>>>,
    leaderboard_path: Arc<PathBuf>,
    config: Arc<AppConfig>,
}

struct AppConfig {
    name: String,
    leaderboard_host: Option<String>,
    leaderboard_enabled: bool,
    server_mode: bool,
}

// persisted user config at ~/.tokenlytics/config.toml. created during onboarding.
#[derive(Default, Debug, Clone, Serialize, Deserialize)]
struct UserConfig {
    name: Option<String>,
    port: Option<u16>,
    #[serde(default)]
    leaderboard: LeaderboardCfg,
}

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
struct LeaderboardCfg {
    #[serde(default)]
    enabled: bool,
    host: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LeaderboardEntry {
    name: String,
    last_24h: u64,
    last_7d: u64,
    last_30d: u64,
    all_time: u64,
    updated_at: String,
}

#[derive(Clone)]
struct Paths {
    claude_dir: PathBuf,
    projects_dir: PathBuf,
    stats_cache: PathBuf,
    codex_dir: PathBuf,
    codex_sessions_dir: PathBuf,
}

#[derive(Default)]
struct CacheState {
    data: Option<UsageData>,
    version: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DataFingerprint {
    files: Vec<FileFingerprint>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct FileFingerprint {
    path: PathBuf,
    len: u64,
    modified_ns: u128,
}

#[derive(Debug, Deserialize)]
struct ClaudeJsonlEntry {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<String>,
    #[serde(rename = "requestId")]
    request_id: Option<String>,
    message: Option<ClaudeMessage>,
}

#[derive(Debug, Deserialize)]
struct ClaudeMessage {
    id: Option<String>,
    model: Option<String>,
    usage: Option<ClaudeUsage>,
}

#[derive(Debug, Deserialize)]
struct ClaudeUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct CodexJsonlEntry {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<String>,
    payload: Option<CodexPayload>,
}

#[derive(Debug, Deserialize)]
struct CodexPayload {
    #[serde(rename = "type")]
    kind: Option<String>,
    model: Option<String>,
    info: Option<CodexTokenInfo>,
}

#[derive(Debug, Deserialize)]
struct CodexTokenInfo {
    last_token_usage: Option<CodexTokenUsage>,
}

#[derive(Debug, Deserialize)]
struct CodexTokenUsage {
    input_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

#[derive(Clone, Debug)]
struct UsageEntry {
    timestamp_utc: DateTime<Utc>,
    source: UsageSource,
    model: String,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_creation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UsageSource {
    Claude,
    Codex,
}

struct UsageCandidate {
    dedupe_id: Option<String>,
    entry: UsageEntry,
}

struct ParsedUsageFile {
    sort_path: String,
    earliest_timestamp_ms: Option<i64>,
    entries: Vec<UsageCandidate>,
}

#[derive(Default, Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelUsageEntry {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_creation: u64,
    messages: u64,
}

type ModelUsage = BTreeMap<String, ModelUsageEntry>;

#[derive(Default)]
struct UsageAggregate {
    model_usage: ModelUsage,
    breakdown: TokenBreakdown,
    total_tokens: u64,
    claude_tokens: u64,
    codex_tokens: u64,
}

#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StatsCache {
    daily_activity: Option<Vec<DailyActivity>>,
    hour_counts: Option<BTreeMap<String, u64>>,
    total_sessions: Option<u64>,
    total_messages: Option<u64>,
    longest_session: Option<LongestSession>,
    first_session_date: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct DailyActivity {
    date: String,
    message_count: Option<u64>,
    session_count: Option<u64>,
}

#[derive(Clone, Default, Deserialize)]
struct LongestSession {
    duration: Option<f64>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct UsageData {
    windows: Windows,
    sparklines: Sparklines,
    model_usage: ModelUsage,
    streaks: Streaks,
    heatmap: Vec<HeatmapDay>,
    session_stats: SessionStats,
    favorite_model: Option<String>,
    peak_day: Option<String>,
    peak_hour: u32,
    last_updated: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Windows {
    last_24h: u64,
    last_7d: u64,
    last_30d: u64,
    all_time: u64,
    last_24h_breakdown: TokenBreakdown,
    last_7d_breakdown: TokenBreakdown,
    last_30d_breakdown: TokenBreakdown,
    all_time_breakdown: TokenBreakdown,
    prev_24h: u64,
    prev_7d: u64,
    prev_30d: u64,
    last_24h_claude: u64,
    last_24h_codex: u64,
    last_7d_claude: u64,
    last_7d_codex: u64,
    last_30d_claude: u64,
    last_30d_codex: u64,
    all_time_claude: u64,
    all_time_codex: u64,
}

#[derive(Clone, Copy, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct TokenBreakdown {
    input: u64,
    output: u64,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Sparklines {
    last_24h: Vec<SparklineBucket>,
    last_7d: Vec<SparklineBucket>,
    last_30d: Vec<SparklineBucket>,
}

#[derive(Clone, Copy, Default, Serialize)]
struct SparklineBucket {
    claude: u64,
    codex: u64,
}

#[cfg(test)]
impl SparklineBucket {
    fn total(self) -> u64 {
        self.claude + self.codex
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Streaks {
    current_streak: u64,
    longest_streak: u64,
}

#[derive(Clone, Serialize)]
struct HeatmapDay {
    date: String,
    messages: Option<u64>,
    sessions: Option<u64>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionStats {
    total_sessions: u64,
    total_messages: u64,
    longest_session: Option<String>,
    first_session_date: Option<String>,
    active_days: usize,
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let force_setup = args.iter().any(|a| a == "--reconfigure" || a == "--setup");
    let skip_setup = args.iter().any(|a| a == "--no-setup" || a == "-y");

    if args
        .iter()
        .any(|a| a == "-h" || a == "--help" || a == "help")
    {
        print_help();
        return;
    }
    if args
        .iter()
        .any(|a| a == "-V" || a == "--version" || a == "version")
    {
        println!("tokenlytics {CLIENT_VERSION}");
        return;
    }

    // first non-flag arg is the subcommand
    let cmd = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .cloned()
        .unwrap_or_default();

    let config_path = tokenlytics_dir().join("config.toml");
    let mut user_config = load_user_config(&config_path).unwrap_or_default();

    // wizard: first run on a server-like command, or explicit --reconfigure
    let server_like = matches!(cmd.as_str(), "" | "stats" | "on" | "start" | "serve");
    let needs_onboarding = force_setup || (!config_path.exists() && server_like);
    if needs_onboarding && !skip_setup && std::io::stdin().is_terminal() {
        match run_onboarding(&user_config) {
            Ok(new_cfg) => {
                if let Err(err) = save_user_config(&config_path, &new_cfg) {
                    eprintln!("warning: could not save {}: {err}", config_path.display());
                }
                user_config = new_cfg;
            }
            Err(err) => {
                eprintln!("setup cancelled: {err}");
                if force_setup {
                    return;
                }
            }
        }
    }

    let result = match cmd.as_str() {
        "on" | "start" => cmd_on(&user_config),
        "off" | "stop" => cmd_off(),
        "status" => cmd_status(&user_config),
        "update" | "upgrade" => cmd_update(),
        "serve" => cmd_serve(user_config),
        "" | "stats" => cmd_stats(&user_config),
        other => {
            eprintln!("unknown command: {other}");
            print_help();
            std::process::exit(2);
        }
    };

    if let Err(err) = result {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

// foreground server. `tokenlytics on` spawns this with stdio detached.
fn cmd_serve(user_config: UserConfig) -> std::io::Result<()> {
    // env vars override user config; user config overrides defaults
    let port = resolve_port(&user_config);
    let paths = Paths::from_home();

    let leaderboard_path = tokenlytics_dir().join("leaderboard.json");
    let leaderboard = load_leaderboard(&leaderboard_path);

    let leaderboard_host = env::var("LEADERBOARD_HOST")
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
        .or_else(|| user_config.leaderboard.host.clone());

    let leaderboard_enabled = leaderboard_host.is_some()
        || env::var("LEADERBOARD")
            .map(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "on" | "yes"
                )
            })
            .unwrap_or(false)
        || user_config.leaderboard.enabled;

    // server mode: API-only, suppresses dashboard page + auto-push of self.
    let server_mode = env::var("LEADERBOARD_SERVER")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "on" | "yes"))
        .unwrap_or(false);

    let config = AppConfig {
        name: env::var("TOKENLYTICS_NAME")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or(user_config.name.clone())
            .or_else(|| env::var("USER").ok())
            .unwrap_or_else(|| "you".to_string()),
        leaderboard_host,
        leaderboard_enabled,
        server_mode,
    };

    let state = AppState {
        cache: Arc::new(Mutex::new(CacheState::default())),
        cache_changed: Arc::new(Condvar::new()),
        paths,
        leaderboard: Arc::new(Mutex::new(leaderboard)),
        leaderboard_path: Arc::new(leaderboard_path),
        config: Arc::new(config),
    };

    let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|err| {
        std::io::Error::other(format!("failed to bind http://localhost:{port}: {err}"))
    })?;

    let preload_started = Instant::now();
    match refresh_cache(&state) {
        Ok(_) => println!(
            "Loaded usage cache in {:.0}ms",
            preload_started.elapsed().as_secs_f64() * 1000.0
        ),
        Err(err) => eprintln!("failed to preload usage cache: {err}"),
    }
    start_usage_watcher(state.clone());

    println!("tokenlytics {CLIENT_VERSION} serving on http://localhost:{port}");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let state = state.clone();
                thread::spawn(move || {
                    if let Err(err) = handle_connection(stream, &state) {
                        eprintln!("request failed: {err}");
                    }
                });
            }
            Err(err) => eprintln!("connection failed: {err}"),
        }
    }
    Ok(())
}

// daemon control: spawn self as `serve` with stdio redirected + new session.
fn cmd_on(user_config: &UserConfig) -> std::io::Result<()> {
    let port = resolve_port(user_config);
    let already_up = read_pid().filter(|p| process_alive(*p)).is_some();
    let pid = start_daemon_quietly(user_config)?;

    if already_up {
        println!("tokenlytics already running (pid {pid}) → http://localhost:{port}");
    } else {
        println!("tokenlytics started (pid {pid}) → http://localhost:{port}");
        println!("  logs: {}", log_file_path().display());
        println!("  stop: tokenlytics off");
    }
    Ok(())
}

// quiet sibling of cmd_on: spawn the daemon (or return existing pid) without printing.
fn start_daemon_quietly(_user_config: &UserConfig) -> std::io::Result<i32> {
    if let Some(pid) = read_pid() {
        if process_alive(pid) {
            return Ok(pid);
        }
        let _ = fs::remove_file(pid_file_path());
    }

    fs::create_dir_all(tokenlytics_dir())?;
    let log_path = log_file_path();
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let log_err = log.try_clone()?;

    let exe = env::current_exe()?;
    let child = unsafe {
        Command::new(&exe)
            .arg("serve")
            .arg("--no-setup")
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            })
            .spawn()?
    };

    let pid = child.id() as i32;
    fs::write(pid_file_path(), pid.to_string())?;

    // give the daemon a moment to bind the port + bail if it died
    thread::sleep(StdDuration::from_millis(500));
    if !process_alive(pid) {
        let _ = fs::remove_file(pid_file_path());
        return Err(std::io::Error::other(format!(
            "daemon exited immediately; check {}",
            log_path.display()
        )));
    }

    Ok(pid)
}

fn cmd_off() -> std::io::Result<()> {
    let Some(pid) = read_pid() else {
        println!("tokenlytics is not running");
        return Ok(());
    };

    if !process_alive(pid) {
        let _ = fs::remove_file(pid_file_path());
        println!("tokenlytics is not running (cleaned stale pid {pid})");
        return Ok(());
    }

    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }

    // wait up to 5s for graceful exit, then SIGKILL
    for _ in 0..50 {
        if !process_alive(pid) {
            break;
        }
        thread::sleep(StdDuration::from_millis(100));
    }
    if process_alive(pid) {
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
        thread::sleep(StdDuration::from_millis(200));
    }

    let _ = fs::remove_file(pid_file_path());
    println!("tokenlytics stopped (pid {pid})");
    Ok(())
}

fn cmd_status(user_config: &UserConfig) -> std::io::Result<()> {
    let port = resolve_port(user_config);
    match read_pid() {
        Some(pid) if process_alive(pid) => {
            println!("tokenlytics is running (pid {pid}) → http://localhost:{port}");
        }
        Some(pid) => {
            println!("tokenlytics is not running (stale pid {pid})");
        }
        None => {
            println!("tokenlytics is not running");
        }
    }
    Ok(())
}

fn cmd_stats(user_config: &UserConfig) -> std::io::Result<()> {
    let port = resolve_port(user_config);

    // happy path: daemon already up
    if let Ok((200, body)) = http_get_local(port, "/api/usage") {
        print_stats_block(&body, port);
        print_commands_hint(Some(port));
        return Ok(());
    }

    // daemon down → auto-start (daemon-by-default UX)
    let spinner = cliclack::spinner();
    spinner.start("starting daemon...");

    let pid = match start_daemon_quietly(user_config) {
        Ok(pid) => pid,
        Err(err) => {
            spinner.stop("failed");
            eprintln!("could not start daemon: {err}");
            print_not_running_block();
            print_commands_hint(None);
            return Ok(());
        }
    };

    // poll until cache loaded (cold first run can take a few seconds)
    let mut body_opt = None;
    for _ in 0..80 {
        thread::sleep(StdDuration::from_millis(150));
        if let Ok((200, body)) = http_get_local(port, "/api/usage") {
            body_opt = Some(body);
            break;
        }
    }

    spinner.stop(format!("daemon ready (pid {pid})"));

    match body_opt {
        Some(body) => {
            print_stats_block(&body, port);
            print_commands_hint(Some(port));
        }
        None => {
            eprintln!("daemon up but didn't respond in time. retry in a moment.");
            eprintln!("logs: {}", log_file_path().display());
        }
    }
    Ok(())
}

fn print_not_running_block() {
    use console::style;
    let rule = style("─".repeat(62)).color256(238);

    println!();
    for line in FIGLET.lines() {
        println!("  {}", style(line).white());
    }
    println!();
    let credit_visible = "by [ ultracontext ]".chars().count();
    print!("{}", center_pad(credit_visible));
    println!(
        "{}{}{}",
        style("by [ ").color256(238),
        style("ultracontext").color256(244),
        style(" ]").color256(238)
    );
    println!();
    println!("  {}", style(format!("v{CLIENT_VERSION}")).dim());
    println!("  {rule}");
    println!();
    println!(
        "       {}  {}",
        style("✗").bold().red(),
        style("daemon not running").bold()
    );
    println!(
        "          {} {}",
        style("start:").dim(),
        style("tokenlytics on").cyan()
    );
    println!();
    println!("  {rule}");
}

fn print_commands_hint(dashboard_port: Option<u16>) {
    use console::style;
    let cmds = ["on", "off", "status", "update", "--help"];
    let dot = style("·").color256(238);
    let line = cmds
        .iter()
        .map(|c| style(c.to_string()).cyan().to_string())
        .collect::<Vec<_>>()
        .join(&format!(" {dot} "));
    println!();
    println!("  {}  {}", style("commands").italic().color256(238), line);
    if let Some(port) = dashboard_port {
        println!(
            "  {}  {}",
            style("dashboard").italic().color256(238),
            style(format!("http://localhost:{port}"))
                .cyan()
                .underlined()
        );
    }
    println!();
}

fn print_stats_block(body: &str, port: u16) {
    use console::style;

    let value: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(err) => {
            eprintln!("invalid stats payload: {err}");
            return;
        }
    };
    let windows = value.get("windows");
    let g = |k: &str| {
        windows
            .and_then(|w| w.get(k))
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    };

    // figlet header (white) + centered "by [ ultracontext ]" credit
    println!();
    for line in FIGLET.lines() {
        println!("  {}", style(line).white());
    }
    println!();
    let credit_visible = "by [ ultracontext ]".chars().count();
    print!("{}", center_pad(credit_visible));
    println!(
        "{}{}{}",
        style("by [ ").color256(238),
        style("ultracontext").color256(244),
        style(" ]").color256(238)
    );

    // version + port row
    let version = format!("v{CLIENT_VERSION}");
    let badge = format!("→ :{port}");
    println!();
    println!(
        "  {}{}{}",
        style(&version).dim(),
        " ".repeat(56_usize.saturating_sub(version.len() + badge.len())),
        style(&badge).dim()
    );
    println!();

    // single table: rows = periods, columns = total + claude + codex + trend
    // colors implicitly label columns (amber=claude, green=codex), header reinforces
    let header_period = style(format!("{:<10}", "PERIOD")).color256(238).bold();
    let header_total = style(format!("{:>8}", "TOTAL")).color256(238).bold();
    let header_claude = style(format!("{:>9}", "CLAUDE"))
        .color256(COLOR_CLAUDE)
        .bold();
    let header_codex = style(format!("{:>9}", "CODEX"))
        .color256(COLOR_CODEX)
        .bold();
    let header_trend = style(format!("{:>8}", "TREND")).color256(238).bold();
    println!(
        "  {}  {}  {}  {}  {}",
        header_period, header_total, header_claude, header_codex, header_trend
    );
    let dash = |n: usize| style("─".repeat(n)).color256(238).to_string();
    println!(
        "  {}  {}  {}  {}  {}",
        dash(10),
        dash(8),
        dash(9),
        dash(9),
        dash(8)
    );

    let row = |period: &str, total: u64, claude: u64, codex: u64, trend: String| {
        println!(
            "  {}  {}  {}  {}  {}",
            style(format!("{period:<10}")).dim(),
            style(format!("{:>8}", compact_count(total))).bold(),
            style(format!("{:>9}", compact_count(claude))).color256(COLOR_CLAUDE),
            style(format!("{:>9}", compact_count(codex))).color256(COLOR_CODEX),
            trend,
        );
    };
    row(
        "last 24h",
        g("last24h"),
        g("last24hClaude"),
        g("last24hCodex"),
        trend_str(g("last24h"), g("prev24h")),
    );
    row(
        "last 7d",
        g("last7d"),
        g("last7dClaude"),
        g("last7dCodex"),
        trend_str(g("last7d"), g("prev7d")),
    );
    row(
        "last 30d",
        g("last30d"),
        g("last30dClaude"),
        g("last30dCodex"),
        trend_str(g("last30d"), g("prev30d")),
    );
    row(
        "all time",
        g("allTime"),
        g("allTimeClaude"),
        g("allTimeCodex"),
        trend_str(g("allTime"), 0),
    );
    println!();

    // single sparkline of combined claude+codex totals, centered, labeled below
    if let Some(spark) = value
        .get("sparklines")
        .and_then(|s| s.get("last24h"))
        .and_then(|s| s.as_array())
    {
        let totals: Vec<u64> = spark
            .iter()
            .map(|b| {
                let c = b.get("claude").and_then(|v| v.as_u64()).unwrap_or(0);
                let d = b.get("codex").and_then(|v| v.as_u64()).unwrap_or(0);
                c + d
            })
            .collect();
        let max = totals.iter().copied().max().unwrap_or(1).max(1);
        let chart = sparkline_chars_scaled(&totals, max);
        let chart_visible = chart.chars().count();
        print!("{}", center_pad(chart_visible));
        println!("{}", style(chart).white());
        let label = "last 24h";
        print!("{}", center_pad(label.chars().count()));
        println!("{}", style(label).color256(238).italic());
    }
}

// horizontal centering relative to the 62-char figlet width
fn center_pad(visible_len: usize) -> String {
    let pad = 62_usize.saturating_sub(visible_len) / 2;
    " ".repeat(2 + pad)
}

// Unicode block ramp for sparklines. 8 levels. max scaled to caller's choice.
fn sparkline_chars_scaled(values: &[u64], max: u64) -> String {
    if values.is_empty() {
        return String::new();
    }
    let max = max.max(1);
    const CHARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    values
        .iter()
        .map(|v| {
            let idx = ((*v as f64 / max as f64) * (CHARS.len() as f64 - 1.0)).round() as usize;
            CHARS[idx.min(CHARS.len() - 1)]
        })
        .collect()
}

// always returns 8 visible chars so the TREND column aligns under its header
fn trend_str(current: u64, previous: u64) -> String {
    use console::style;
    if previous == 0 {
        return style(format!("{:>8}", "—")).color256(238).to_string();
    }
    let pct = ((current as f64 - previous as f64) / previous as f64) * 100.0;
    if pct.abs() < 1.0 {
        return style(format!("{:>8}", "~ same")).dim().to_string();
    }
    let raw = if pct > 0.0 {
        format!("▲ {:>3.0}%", pct.abs())
    } else {
        format!("▼ {:>3.0}%", pct.abs())
    };
    let padded = format!("{:>8}", raw);
    if pct > 0.0 {
        style(padded).green().to_string()
    } else {
        style(padded).red().to_string()
    }
}

fn compact_count(n: u64) -> String {
    if n >= 1_000_000_000 {
        format!("{:.1}b", n as f64 / 1e9)
    } else if n >= 1_000_000 {
        format!("{:.1}m", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

fn read_pid() -> Option<i32> {
    fs::read_to_string(pid_file_path())
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn process_alive(pid: i32) -> bool {
    pid > 0 && unsafe { libc::kill(pid, 0) == 0 }
}

fn pid_file_path() -> PathBuf {
    tokenlytics_dir().join("tokenlytics.pid")
}

fn log_file_path() -> PathBuf {
    tokenlytics_dir().join("tokenlytics.log")
}

fn resolve_port(user_config: &UserConfig) -> u16 {
    env::var("PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .or(user_config.port)
        .unwrap_or(6969)
}

// minimal HTTP/1.0-style GET against the local daemon. enough to read /api/tokens.
fn http_get_local(port: u16, path: &str) -> std::io::Result<(u16, String)> {
    let addr: SocketAddr = format!("127.0.0.1:{port}")
        .parse()
        .map_err(|err| std::io::Error::other(format!("invalid addr: {err}")))?;
    let mut stream = TcpStream::connect_timeout(&addr, StdDuration::from_millis(500))?;
    stream.set_read_timeout(Some(StdDuration::from_secs(5)))?;

    let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes())?;

    let mut response = String::new();
    stream.read_to_string(&mut response)?;

    let mut parts = response.splitn(2, "\r\n\r\n");
    let head = parts.next().unwrap_or("");
    let body = parts.next().unwrap_or("").to_string();
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    Ok((status, body))
}

fn print_help() {
    println!("tokenlytics {CLIENT_VERSION} · token usage dashboard for Claude Code + Codex");
    println!();
    println!("USAGE:");
    println!("  tokenlytics              show your stats");
    println!("  tokenlytics on           start the background daemon");
    println!("  tokenlytics off          stop the background daemon");
    println!("  tokenlytics status       show daemon status");
    println!("  tokenlytics update       fetch the latest tokenlytics");
    println!("  tokenlytics serve        run in foreground (dev/CI)");
    println!("  tokenlytics --reconfigure   re-run the setup wizard");
    println!();
    println!("OPTIONS:");
    println!("  --no-setup, -y           skip first-run wizard");
    println!("  --version, -V            print version");
    println!("  --help, -h               this help");
}

// re-runs the install script. self-overwrites the binary in place.
fn cmd_update() -> std::io::Result<()> {
    println!("Updating tokenlytics from {UPGRADE_URL} ...");
    println!();
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!("curl -fsSL {UPGRADE_URL} | sh"))
        .status()?;
    if status.success() {
        println!();
        println!("✓ updated. restart with: tokenlytics off && tokenlytics on");
        Ok(())
    } else {
        eprintln!("update failed (exit {:?})", status.code());
        eprintln!("if you installed from source, run: cargo install --path .");
        Err(std::io::Error::other("update failed"))
    }
}

// trim leading 'v' if present, then split major.minor.patch into u32 tuple.
fn parse_semver(version: &str) -> Option<(u32, u32, u32)> {
    let mut parts = version.trim().trim_start_matches('v').splitn(3, '.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

fn version_too_old(client: &str, minimum: &str) -> bool {
    match (parse_semver(client), parse_semver(minimum)) {
        (Some(c), Some(m)) => c < m,
        _ => true, // unparseable client = treat as ancient
    }
}

impl Paths {
    fn from_home() -> Self {
        let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
        let home = PathBuf::from(home);
        let claude_dir = home.join(".claude");
        let codex_dir = home.join(".codex");
        Self {
            claude_dir: claude_dir.clone(),
            projects_dir: claude_dir.join("projects"),
            stats_cache: claude_dir.join("stats-cache.json"),
            codex_dir: codex_dir.clone(),
            codex_sessions_dir: codex_dir.join("sessions"),
        }
    }
}

fn handle_connection(mut stream: TcpStream, state: &AppState) -> std::io::Result<()> {
    let Some((request_line, body)) = read_http_request(&mut stream)? else {
        return Ok(());
    };

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let path = parts.next().unwrap_or("/");
    let path = path.split('?').next().unwrap_or("/");

    if method == "GET" && is_realtime_path(path) {
        return stream_usage_events(&mut stream, state);
    }

    let response = match method {
        "OPTIONS" => HttpResponse::empty(204, "No Content"),
        "GET" => route_get(path, state),
        "POST" => route_post(path, &body, state),
        _ => HttpResponse::json_error(405, "method not allowed"),
    };

    stream.write_all(&response.into_bytes())?;
    stream.flush()
}

// reads request line + headers + body (if Content-Length set). caps at 64KB body.
fn read_http_request(stream: &mut TcpStream) -> std::io::Result<Option<(String, Vec<u8>)>> {
    let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
    let mut tmp = [0u8; 8 * 1024];
    let header_end;

    loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_double_crlf(&buf) {
            header_end = pos;
            break;
        }
        if buf.len() > 32 * 1024 {
            return Ok(None);
        }
    }

    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let request_line = head.lines().next().unwrap_or("").to_string();
    let body_start = header_end + 4;

    let content_length = head
        .lines()
        .find_map(|line| {
            let lower = line.to_ascii_lowercase();
            lower
                .strip_prefix("content-length:")
                .and_then(|value| value.trim().parse::<usize>().ok())
        })
        .unwrap_or(0)
        .min(64 * 1024);

    while buf.len() < body_start + content_length {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }

    let body_end = (body_start + content_length).min(buf.len());
    let body = buf[body_start..body_end].to_vec();
    Ok(Some((request_line, body)))
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn is_realtime_path(path: &str) -> bool {
    matches!(path, "/api/stream" | "/api/realtime")
}

fn route_get(path: &str, state: &AppState) -> HttpResponse {
    match path {
        "/api/usage" => match get_data(state) {
            Ok(data) => HttpResponse::json(200, &data),
            Err(err) => HttpResponse::json_error(500, &err),
        },
        "/api/tokens" => match get_data(state) {
            Ok(data) => {
                let w = &data.windows;
                let summary = serde_json::json!({
                    "last24h": w.last_24h,
                    "last7d": w.last_7d,
                    "last30d": w.last_30d,
                    "allTime": w.all_time,
                    "trend24h": trend_percent(w.last_24h, w.prev_24h),
                    "trend7d": trend_percent(w.last_7d, w.prev_7d),
                    "trend30d": trend_percent(w.last_30d, w.prev_30d),
                });
                HttpResponse::json_value(200, summary)
            }
            Err(err) => HttpResponse::json_error(500, &err),
        },
        "/api/models" => match get_data(state) {
            Ok(data) => HttpResponse::json(200, &data.model_usage),
            Err(err) => HttpResponse::json_error(500, &err),
        },
        "/api/config" => HttpResponse::json_value(
            200,
            serde_json::json!({
                // empty name in server mode → dashboard JS guard skips auto-push
                "name": if state.config.server_mode { "" } else { state.config.name.as_str() },
                "leaderboardHost": state.config.leaderboard_host,
                "leaderboardEnabled": state.config.leaderboard_enabled,
                "clientVersion": CLIENT_VERSION,
                "serverMode": state.config.server_mode,
            }),
        ),
        "/api/version" => HttpResponse::json_value(
            200,
            serde_json::json!({
                "serverVersion": CLIENT_VERSION,
                "minClientVersion": MIN_CLIENT_VERSION,
                "upgradeUrl": UPGRADE_URL,
            }),
        ),
        "/api/leaderboard" => leaderboard_get(state),
        _ => {
            if state.config.server_mode {
                // public leaderboard server: hide dashboard, advertise API only
                HttpResponse::json_value(
                    200,
                    serde_json::json!({
                        "service": "tokenlytics-leaderboard",
                        "version": CLIENT_VERSION,
                        "endpoints": {
                            "version": "/api/version",
                            "config": "/api/config",
                            "leaderboard": "/api/leaderboard",
                            "submit": "POST /api/leaderboard/submit"
                        }
                    }),
                )
            } else {
                HttpResponse::html(200, DASHBOARD_HTML)
            }
        }
    }
}

fn route_post(path: &str, body: &[u8], state: &AppState) -> HttpResponse {
    match path {
        "/api/leaderboard/submit" => leaderboard_submit(body, state),
        "/api/self-update" => self_update_endpoint(),
        _ => HttpResponse::json_error(404, "not found"),
    }
}

// fired by the dashboard when a leaderboard submit returns 426. spawns the
// install script in the background, then re-execs the daemon with the freshly
// downloaded binary. responds 200 immediately so the caller doesn't hang.
fn self_update_endpoint() -> HttpResponse {
    thread::spawn(|| {
        eprintln!("self-update: fetching {UPGRADE_URL}");
        let status = Command::new("sh")
            .arg("-c")
            .arg(format!("curl -fsSL {UPGRADE_URL} | sh"))
            .status();
        match status {
            Ok(s) if s.success() => {
                eprintln!("self-update: install ok, re-exec'ing daemon");
                // brief pause so the HTTP response can flush before exec
                thread::sleep(StdDuration::from_millis(200));
                let exe = match env::current_exe() {
                    Ok(p) => p,
                    Err(err) => {
                        eprintln!("self-update: cannot locate exe: {err}");
                        return;
                    }
                };
                let args: Vec<_> = env::args().skip(1).collect();
                let err = Command::new(&exe).args(&args).exec();
                eprintln!("self-update: exec failed: {err}");
            }
            Ok(s) => eprintln!("self-update: install exited {:?}", s.code()),
            Err(err) => eprintln!("self-update: install spawn failed: {err}"),
        }
    });
    HttpResponse::json_value(200, serde_json::json!({ "status": "updating" }))
}

fn leaderboard_get(state: &AppState) -> HttpResponse {
    let guard = match state.leaderboard.lock() {
        Ok(guard) => guard,
        Err(_) => return HttpResponse::json_error(500, "leaderboard lock poisoned"),
    };
    let entries: Vec<&LeaderboardEntry> = guard.values().collect();
    HttpResponse::json(200, &entries)
}

fn leaderboard_submit(body: &[u8], state: &AppState) -> HttpResponse {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct SubmitInput {
        name: String,
        version: Option<String>,
        last_24h: u64,
        last_7d: u64,
        last_30d: u64,
        all_time: u64,
    }

    let input: SubmitInput = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(err) => return HttpResponse::json_error(400, &format!("invalid body: {err}")),
    };

    // gate on client version. clients before versioning sent no field — treated as ancient.
    let client_version = input.version.as_deref().unwrap_or("0.0.0");
    if version_too_old(client_version, MIN_CLIENT_VERSION) {
        return HttpResponse::json_value(
            426,
            serde_json::json!({
                "error": "client out of date",
                "yourVersion": client_version,
                "minVersion": MIN_CLIENT_VERSION,
                "upgrade": "tokenlytics update",
                "upgradeUrl": UPGRADE_URL,
            }),
        );
    }

    let name = input.name.trim().to_string();
    if name.is_empty() || name.chars().count() > 32 {
        return HttpResponse::json_error(400, "name must be 1..32 chars");
    }

    let entry = LeaderboardEntry {
        name: name.clone(),
        last_24h: input.last_24h,
        last_7d: input.last_7d,
        last_30d: input.last_30d,
        all_time: input.all_time,
        updated_at: Utc::now().to_rfc3339(),
    };

    let mut guard = match state.leaderboard.lock() {
        Ok(guard) => guard,
        Err(_) => return HttpResponse::json_error(500, "leaderboard lock poisoned"),
    };
    guard.insert(name.to_lowercase(), entry);
    if let Err(err) = save_leaderboard(&state.leaderboard_path, &guard) {
        eprintln!("failed to persist leaderboard: {err}");
    }

    HttpResponse::json_value(200, serde_json::json!({ "ok": true }))
}

fn load_leaderboard(path: &Path) -> HashMap<String, LeaderboardEntry> {
    let Ok(raw) = fs::read_to_string(path) else {
        return HashMap::new();
    };
    let entries: Vec<LeaderboardEntry> = serde_json::from_str(&raw).unwrap_or_default();
    entries
        .into_iter()
        .map(|entry| (entry.name.to_lowercase(), entry))
        .collect()
}

fn save_leaderboard(path: &Path, map: &HashMap<String, LeaderboardEntry>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let entries: Vec<&LeaderboardEntry> = map.values().collect();
    let raw = serde_json::to_vec_pretty(&entries)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, raw)?;
    fs::rename(tmp, path)
}

fn tokenlytics_dir() -> PathBuf {
    PathBuf::from(env::var("HOME").unwrap_or_else(|_| ".".to_string())).join(".tokenlytics")
}

// ─── persistent ledger (SQLite) ────────────────────────────────────────────
// Lives in ~/.tokenlytics/usage.db, separate from the binary. Survives every
// `tokenlytics update` and any cargo rebuild because the binary lives in a
// different directory entirely.

fn db_path() -> PathBuf {
    tokenlytics_dir().join("usage.db")
}

fn open_usage_db() -> rusqlite::Result<rusqlite::Connection> {
    let _ = fs::create_dir_all(tokenlytics_dir());
    let conn = rusqlite::Connection::open(db_path())?;
    // WAL gives concurrent readers + faster writes; safe for single-writer daemon
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
    init_db_schema(&conn)?;
    Ok(conn)
}

// schema migrations: each release that changes the schema bumps user_version
// and adds an idempotent block here. running this at every open is safe.
fn init_db_schema(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    let v: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if v < 1 {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS events (
                id TEXT PRIMARY KEY,
                ts INTEGER NOT NULL,
                src TEXT NOT NULL,
                model TEXT NOT NULL,
                input INTEGER NOT NULL,
                output INTEGER NOT NULL,
                cache_read INTEGER NOT NULL DEFAULT 0,
                cache_creation INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_events_ts ON events(ts);
            PRAGMA user_version = 1;",
        )?;
    }
    Ok(())
}

// content-derived stable ID — collisions require identical timestamp_ms +
// model + input + output, which is astronomically unlikely for distinct events.
fn entry_id(e: &UsageEntry) -> String {
    let src = match e.source {
        UsageSource::Claude => "claude",
        UsageSource::Codex => "codex",
    };
    format!(
        "{src}:{}:{}:{}:{}",
        e.timestamp_utc.timestamp_millis(),
        e.input,
        e.output,
        e.model
    )
}

fn upsert_events(
    conn: &mut rusqlite::Connection,
    entries: &[UsageEntry],
) -> rusqlite::Result<usize> {
    let tx = conn.transaction()?;
    let mut inserted = 0;
    {
        let mut stmt = tx.prepare(
            "INSERT OR IGNORE INTO events
                (id, ts, src, model, input, output, cache_read, cache_creation)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        for e in entries {
            let src = match e.source {
                UsageSource::Claude => "claude",
                UsageSource::Codex => "codex",
            };
            inserted += stmt.execute(rusqlite::params![
                entry_id(e),
                e.timestamp_utc.timestamp_millis(),
                src,
                e.model,
                e.input as i64,
                e.output as i64,
                e.cache_read as i64,
                e.cache_creation as i64,
            ])?;
        }
    }
    tx.commit()?;
    Ok(inserted)
}

fn load_events(conn: &rusqlite::Connection) -> rusqlite::Result<Vec<UsageEntry>> {
    let mut stmt = conn.prepare(
        "SELECT ts, src, model, input, output, cache_read, cache_creation
         FROM events ORDER BY ts",
    )?;
    let rows = stmt.query_map([], |r| {
        let ts_ms: i64 = r.get(0)?;
        let src: String = r.get(1)?;
        Ok(UsageEntry {
            timestamp_utc: DateTime::<Utc>::from_timestamp_millis(ts_ms).unwrap_or_default(),
            source: if src == "claude" {
                UsageSource::Claude
            } else {
                UsageSource::Codex
            },
            model: r.get(2)?,
            input: r.get::<_, i64>(3)? as u64,
            output: r.get::<_, i64>(4)? as u64,
            cache_read: r.get::<_, i64>(5)? as u64,
            cache_creation: r.get::<_, i64>(6)? as u64,
        })
    })?;
    rows.collect()
}

fn load_user_config(path: &Path) -> Option<UserConfig> {
    let raw = fs::read_to_string(path).ok()?;
    toml::from_str(&raw).ok()
}

fn save_user_config(path: &Path, cfg: &UserConfig) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let raw = toml::to_string_pretty(cfg)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err))?;
    fs::write(path, raw)
}

// first-run wizard: explains tokenlytics, asks for name, leaderboard mode, port.
fn run_onboarding(existing: &UserConfig) -> std::io::Result<UserConfig> {
    cliclack::intro("tokenlytics")?;

    cliclack::note(
        "wtf is this",
        "tokenlytics is an open source token tracker. watches\n\
         your ~/.claude and ~/.codex folders. all local. optionally\n\
         compete on tokenmaxing with your friends to see who becomes\n\
         the first token trillionaire.\n\n\
         Fabio Roma · [ ultracontext ]",
    )?;

    let default_name = existing
        .name
        .clone()
        .or_else(|| env::var("USER").ok())
        .unwrap_or_else(|| "you".to_string());

    let name: String = cliclack::input("Your display name")
        .placeholder(&default_name)
        .default_input(&default_name)
        .validate(|input: &String| {
            let trimmed = input.trim();
            if trimmed.is_empty() {
                Err("name cannot be empty")
            } else if trimmed.chars().count() > 32 {
                Err("name must be 32 chars or fewer")
            } else {
                Ok(())
            }
        })
        .interact()?;
    let name = name.trim().to_string();

    // map saved config back to the top-level mode so re-runs preselect the right option
    let initial_mode = match (
        existing.leaderboard.enabled,
        existing.leaderboard.host.as_deref(),
    ) {
        (false, _) => "off",
        (true, Some(host)) if host == DEFAULT_GLOBAL_HOST => "global",
        (true, _) => "friends",
    };

    let mode: &str = cliclack::select("Compete or not?")
        .initial_value(initial_mode)
        .item("off", "No", "just wanna track my shit locally")
        .item(
            "global",
            "Yes, but I don't have friends",
            "compete globally with other tokenchads",
        )
        .item(
            "friends",
            "Yes, I do have friends",
            "host or join a private leaderboard",
        )
        .interact()?;

    let (lb_enabled, lb_host) = match mode {
        "global" => (true, Some(DEFAULT_GLOBAL_HOST.to_string())),
        "friends" => prompt_friends_mode(existing)?,
        _ => (false, None),
    };

    let default_port = existing.port.unwrap_or(6969);
    let port = if should_prompt_port(mode, lb_enabled, lb_host.as_deref()) {
        prompt_port(default_port)?
    } else {
        default_port
    };

    let lb_summary = match (&lb_enabled, lb_host.as_deref()) {
        (true, Some(host)) if host == DEFAULT_GLOBAL_HOST => "global · everyone".to_string(),
        (true, Some(host)) => format!("join → {host}"),
        (true, None) => "host (friends point here)".to_string(),
        _ => "off · local only".to_string(),
    };
    let summary = format!("name      {name}\nport      {port}\nleaderboard  {lb_summary}");
    cliclack::note("saved to ~/.tokenlytics/config.toml", summary)?;
    cliclack::outro("setup done · starting tokenlytics")?;

    Ok(UserConfig {
        name: Some(name),
        port: Some(port),
        leaderboard: LeaderboardCfg {
            enabled: lb_enabled,
            host: lb_host,
        },
    })
}

fn prompt_port(default_port: u16) -> std::io::Result<u16> {
    // explain what this port is for — most users have never thought about it
    cliclack::log::info(
        "your local dashboard opens at http://localhost:PORT (6969 is fine unless something else uses it)",
    )?;
    let default_port = default_port.to_string();
    let port_str: String = cliclack::input("Local dashboard port")
        .placeholder(&default_port)
        .default_input(&default_port)
        .validate(|input: &String| match input.trim().parse::<u16>() {
            Ok(value) if value > 0 => Ok(()),
            _ => Err("port must be 1..=65535"),
        })
        .interact()?;
    Ok(port_str.trim().parse().unwrap_or(6969))
}

fn should_prompt_port(
    mode: &str,
    leaderboard_enabled: bool,
    leaderboard_host: Option<&str>,
) -> bool {
    mode == "friends" && leaderboard_enabled && leaderboard_host.is_none()
}

// sub-step: when user picks "Yes, I do have friends" → host this machine, or join one.
fn prompt_friends_mode(existing: &UserConfig) -> std::io::Result<(bool, Option<String>)> {
    let saved_friend_host = existing
        .leaderboard
        .host
        .as_deref()
        .filter(|host| *host != DEFAULT_GLOBAL_HOST);

    let initial_sub = if saved_friend_host.is_some() {
        "join"
    } else {
        "host"
    };

    let sub: &str = cliclack::select("Host or join?")
        .initial_value(initial_sub)
        .item("host", "Host", "friends point their tokenlytics at me")
        .item("join", "Join", "point at a friend who is hosting")
        .interact()?;

    if sub == "host" {
        return Ok((true, None));
    }

    let placeholder = saved_friend_host.unwrap_or("http://1.2.3.4:6969");
    let mut prompt = cliclack::input("Friend's tokenlytics URL")
        .placeholder(placeholder)
        .validate(|input: &String| {
            let trimmed = input.trim();
            if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
                Ok(())
            } else {
                Err("must start with http:// or https://")
            }
        });
    if let Some(saved) = saved_friend_host {
        prompt = prompt.default_input(saved);
    }
    let url: String = prompt.interact()?;
    Ok((true, Some(url.trim().trim_end_matches('/').to_string())))
}

fn stream_usage_events(stream: &mut TcpStream, state: &AppState) -> std::io::Result<()> {
    let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nCache-Control: no-store\r\nAccess-Control-Allow-Origin: *\r\nConnection: keep-alive\r\nX-Accel-Buffering: no\r\n\r\n";
    if !write_stream_chunk(stream, headers.as_bytes())? {
        return Ok(());
    }

    if let Err(err) = get_data(state) {
        let event = error_sse_event(&err);
        let _ = write_stream_chunk(stream, event.as_bytes())?;
    }

    let mut last_sent_version = 0;

    loop {
        match wait_for_cache_update(state, last_sent_version, SSE_KEEPALIVE_INTERVAL) {
            Ok(Some((version, data))) => {
                if version != last_sent_version {
                    let event = usage_sse_event(&data);
                    if !write_stream_chunk(stream, event.as_bytes())? {
                        return Ok(());
                    }
                    last_sent_version = version;
                }
            }
            Ok(None) => {
                if !write_stream_chunk(stream, b": keepalive\n\n")? {
                    return Ok(());
                }
            }
            Err(err) => {
                let event = error_sse_event(&err);
                if !write_stream_chunk(stream, event.as_bytes())? {
                    return Ok(());
                }
            }
        }
    }
}

fn usage_sse_event(data: &UsageData) -> String {
    let payload = serde_json::to_string(data)
        .unwrap_or_else(|err| serde_json::json!({ "error": err.to_string() }).to_string());
    format!(
        "event: usage\nid: {}\ndata: {}\n\n",
        data.last_updated, payload
    )
}

fn error_sse_event(message: &str) -> String {
    let payload = serde_json::json!({ "error": message }).to_string();
    format!("event: error\ndata: {payload}\n\n")
}

fn write_stream_chunk(stream: &mut TcpStream, bytes: &[u8]) -> std::io::Result<bool> {
    match stream.write_all(bytes).and_then(|_| stream.flush()) {
        Ok(()) => Ok(true),
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
            ) =>
        {
            Ok(false)
        }
        Err(err) => Err(err),
    }
}

fn get_data(state: &AppState) -> Result<UsageData, String> {
    {
        let guard = state
            .cache
            .lock()
            .map_err(|_| "cache lock poisoned".to_string())?;
        if let Some(data) = guard.data.as_ref() {
            return Ok(data.clone());
        }
    }

    refresh_cache(state)
}

fn refresh_cache(state: &AppState) -> Result<UsageData, String> {
    let data = get_usage_data(&state.paths)?;
    let mut guard = state
        .cache
        .lock()
        .map_err(|_| "cache lock poisoned".to_string())?;
    guard.data = Some(data.clone());
    guard.version = guard.version.saturating_add(1);
    state.cache_changed.notify_all();
    Ok(data)
}

fn wait_for_cache_update(
    state: &AppState,
    seen_version: u64,
    timeout: StdDuration,
) -> Result<Option<(u64, UsageData)>, String> {
    let guard = state
        .cache
        .lock()
        .map_err(|_| "cache lock poisoned".to_string())?;
    let (guard, _) = state
        .cache_changed
        .wait_timeout_while(guard, timeout, |cache| cache.version <= seen_version)
        .map_err(|_| "cache lock poisoned".to_string())?;

    if guard.version <= seen_version {
        return Ok(None);
    }

    Ok(guard
        .data
        .as_ref()
        .map(|data| (guard.version, data.clone())))
}

fn start_usage_watcher(state: AppState) {
    thread::spawn(move || {
        if let Err(err) = run_os_usage_watcher(state.clone()) {
            eprintln!("usage file watcher unavailable: {err}; falling back to fingerprint polling");
            run_polling_usage_watcher(state);
        }
    });
}

fn run_os_usage_watcher(state: AppState) -> Result<(), String> {
    let roots = usage_watch_roots(&state.paths);
    if roots.is_empty() {
        return Err("no Claude or Codex usage directories exist".to_string());
    }

    let (tx, rx) = mpsc::channel();
    let mut watcher = RecommendedWatcher::new(tx, NotifyConfig::default())
        .map_err(|err| format!("failed to start OS file watcher: {err}"))?;
    for root in &roots {
        watcher
            .watch(root, RecursiveMode::Recursive)
            .map_err(|err| format!("failed to watch {}: {err}", root.display()))?;
    }

    println!(
        "Watching usage files with OS events under {}",
        roots
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );

    let mut last_rolling_refresh = Instant::now();

    loop {
        let timeout = ROLLING_REFRESH_INTERVAL
            .checked_sub(last_rolling_refresh.elapsed())
            .unwrap_or_else(|| StdDuration::from_secs(0));

        match rx.recv_timeout(timeout) {
            Ok(Ok(event)) => {
                if !event_touches_usage(&event, &state.paths) {
                    continue;
                }
                coalesce_usage_events(&rx, &state.paths);
                if let Err(err) = refresh_cache(&state) {
                    eprintln!("failed to refresh usage cache: {err}");
                }
                last_rolling_refresh = Instant::now();
            }
            Ok(Err(err)) => eprintln!("usage file watcher event failed: {err}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Err(err) = refresh_cache(&state) {
                    eprintln!("failed to refresh rolling usage cache: {err}");
                }
                last_rolling_refresh = Instant::now();
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("watcher channel disconnected".to_string());
            }
        }
    }
}

fn coalesce_usage_events(rx: &mpsc::Receiver<notify::Result<Event>>, paths: &Paths) {
    thread::sleep(EVENT_DEBOUNCE);

    while let Ok(event) = rx.try_recv() {
        match event {
            Ok(event) => {
                let _ = event_touches_usage(&event, paths);
            }
            Err(err) => eprintln!("usage file watcher event failed: {err}"),
        }
    }
}

fn event_touches_usage(event: &Event, paths: &Paths) -> bool {
    event
        .paths
        .iter()
        .any(|path| path_touches_usage(path, paths))
}

fn path_touches_usage(path: &Path, paths: &Paths) -> bool {
    path == paths.stats_cache.as_path()
        || path == paths.claude_dir.as_path()
        || path.starts_with(&paths.projects_dir)
        || path == paths.codex_dir.as_path()
        || path.starts_with(&paths.codex_sessions_dir)
        || (path.starts_with(&paths.claude_dir)
            && path.file_name().and_then(|name| name.to_str()) == Some("stats-cache.json"))
}

fn usage_watch_roots(paths: &Paths) -> Vec<PathBuf> {
    [&paths.claude_dir, &paths.codex_dir]
        .into_iter()
        .filter(|path| path.is_dir())
        .cloned()
        .collect()
}

fn run_polling_usage_watcher(state: AppState) {
    let mut last_fingerprint = data_fingerprint(&state.paths).ok();

    loop {
        thread::sleep(POLL_FALLBACK_INTERVAL);

        let current_fingerprint = match data_fingerprint(&state.paths) {
            Ok(fingerprint) => fingerprint,
            Err(err) => {
                eprintln!("failed to scan usage files: {err}");
                continue;
            }
        };

        if last_fingerprint.as_ref() == Some(&current_fingerprint) {
            continue;
        }

        match refresh_cache(&state) {
            Ok(_) => {
                last_fingerprint = Some(current_fingerprint);
            }
            Err(err) => eprintln!("failed to refresh usage cache: {err}"),
        }
    }
}

fn data_fingerprint(paths: &Paths) -> Result<DataFingerprint, String> {
    let mut files = Vec::new();
    collect_usage_file_fingerprints(&paths.projects_dir, &mut files)?;
    collect_usage_file_fingerprints(&paths.codex_sessions_dir, &mut files)?;
    push_file_fingerprint(&paths.stats_cache, &mut files);
    files.sort();

    Ok(DataFingerprint { files })
}

fn collect_usage_file_fingerprints(
    dir: &Path,
    files: &mut Vec<FileFingerprint>,
) -> Result<(), String> {
    if !dir.is_dir() {
        return Ok(());
    }

    let entries =
        fs::read_dir(dir).map_err(|err| format!("failed to scan {}: {err}", dir.display()))?;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_usage_file_fingerprints(&path, files)?;
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
            push_file_fingerprint(&path, files);
        }
    }

    Ok(())
}

fn push_file_fingerprint(path: &Path, files: &mut Vec<FileFingerprint>) {
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };

    let modified_ns = metadata
        .modified()
        .ok()
        .and_then(system_time_to_unix_nanos)
        .unwrap_or(0);

    files.push(FileFingerprint {
        path: path.to_path_buf(),
        len: metadata.len(),
        modified_ns,
    });
}

fn system_time_to_unix_nanos(time: SystemTime) -> Option<u128> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_nanos())
}

fn get_usage_data(paths: &Paths) -> Result<UsageData, String> {
    get_usage_data_at(paths, Utc::now())
}

fn get_usage_data_at(paths: &Paths, now: DateTime<Utc>) -> Result<UsageData, String> {
    let cache = read_stats_cache(&paths.stats_cache)?;

    // 1. read whatever Claude/Codex still have on disk
    let mut live = read_project_messages(&paths.projects_dir, None)?;
    live.extend(read_codex_messages(&paths.codex_sessions_dir, None)?);

    // 2. mirror new events into our own SQLite ledger so we keep history past their retention
    let mut conn = open_usage_db().map_err(|e| format!("db open: {e}"))?;
    if !live.is_empty() {
        upsert_events(&mut conn, &live).map_err(|e| format!("db upsert: {e}"))?;
    }

    // 3. read full ledger — includes events that may have been deleted from .claude/.codex
    let messages = load_events(&conn).map_err(|e| format!("db load: {e}"))?;

    Ok(build_usage_data(cache, messages, now))
}

fn build_usage_data(cache: StatsCache, messages: Vec<UsageEntry>, now: DateTime<Utc>) -> UsageData {
    let current_end = now + Duration::seconds(1);
    let last_24h_start = now - Duration::hours(24);
    let prev_24h_start = now - Duration::hours(48);
    let last_7d_start = now - Duration::days(7);
    let prev_7d_start = now - Duration::days(14);
    let last_30d_start = now - Duration::days(30);
    let prev_30d_start = now - Duration::days(60);

    let agg_24h = aggregate_messages(messages_in_time_range(
        &messages,
        last_24h_start,
        current_end,
    ));
    let agg_7d = aggregate_messages(messages_in_time_range(
        &messages,
        last_7d_start,
        current_end,
    ));
    let agg_30d = aggregate_messages(messages_in_time_range(
        &messages,
        last_30d_start,
        current_end,
    ));
    let agg_prev_24h = aggregate_messages(messages_in_time_range(
        &messages,
        prev_24h_start,
        last_24h_start,
    ));
    let agg_prev_7d = aggregate_messages(messages_in_time_range(
        &messages,
        prev_7d_start,
        last_7d_start,
    ));
    let agg_prev_30d = aggregate_messages(messages_in_time_range(
        &messages,
        prev_30d_start,
        last_30d_start,
    ));

    let all_time = aggregate_messages(messages.iter());
    let all_time_tokens = all_time.total_tokens;
    let all_time_breakdown = all_time.breakdown;
    let all_time_claude = all_time.claude_tokens;
    let all_time_codex = all_time.codex_tokens;
    let all_time_model_usage = all_time.model_usage;

    let windows = Windows {
        last_24h: agg_24h.total_tokens,
        last_7d: agg_7d.total_tokens,
        last_30d: agg_30d.total_tokens,
        all_time: all_time_tokens,
        last_24h_breakdown: agg_24h.breakdown,
        last_7d_breakdown: agg_7d.breakdown,
        last_30d_breakdown: agg_30d.breakdown,
        all_time_breakdown,
        prev_24h: agg_prev_24h.total_tokens,
        prev_7d: agg_prev_7d.total_tokens,
        prev_30d: agg_prev_30d.total_tokens,
        last_24h_claude: agg_24h.claude_tokens,
        last_24h_codex: agg_24h.codex_tokens,
        last_7d_claude: agg_7d.claude_tokens,
        last_7d_codex: agg_7d.codex_tokens,
        last_30d_claude: agg_30d.claude_tokens,
        last_30d_codex: agg_30d.codex_tokens,
        all_time_claude,
        all_time_codex,
    };

    let spark_24h = rolling_sparkline(&messages, now, Duration::hours(24), 24);
    let spark_7d = rolling_sparkline(&messages, now, Duration::days(7), 7);
    let spark_30d = rolling_sparkline(&messages, now, Duration::days(30), 30);

    let daily_activity = cache.daily_activity.unwrap_or_default();
    let today = now.date_naive();
    let streaks = calculate_streaks(&daily_activity, agg_24h.total_tokens > 0, today);
    let heatmap = daily_activity
        .iter()
        .map(|day| HeatmapDay {
            date: day.date.clone(),
            messages: day.message_count,
            sessions: day.session_count,
        })
        .collect::<Vec<_>>();

    let session_stats = SessionStats {
        total_sessions: cache.total_sessions.unwrap_or(0),
        total_messages: cache.total_messages.unwrap_or(0),
        longest_session: cache
            .longest_session
            .and_then(|session| session.duration)
            .map(format_duration),
        first_session_date: cache
            .first_session_date
            .as_ref()
            .map(|value| value.chars().take(10).collect()),
        active_days: daily_activity.len(),
    };

    let peak_day = daily_activity
        .iter()
        .max_by_key(|day| day.message_count.unwrap_or(0))
        .map(|day| day.date.clone());

    let peak_hour = cache
        .hour_counts
        .unwrap_or_default()
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .and_then(|(hour, _)| hour.parse::<u32>().ok())
        .unwrap_or(0);

    let favorite_model = all_time_model_usage
        .iter()
        .max_by_key(|(_, usage)| usage.input + usage.output)
        .map(|(model, _)| clean_model_name(model));

    UsageData {
        windows,
        sparklines: Sparklines {
            last_24h: spark_24h,
            last_7d: spark_7d,
            last_30d: spark_30d,
        },
        model_usage: all_time_model_usage,
        streaks,
        heatmap,
        session_stats,
        favorite_model,
        peak_day,
        peak_hour,
        last_updated: now.to_rfc3339(),
    }
}

fn read_stats_cache(path: &Path) -> Result<StatsCache, String> {
    match fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).map_err(|err| format!("invalid stats cache: {err}")),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(StatsCache::default()),
        Err(err) => Err(format!("failed to read {}: {err}", path.display())),
    }
}

fn read_project_messages(
    projects_dir: &Path,
    min_timestamp: Option<DateTime<Utc>>,
) -> Result<Vec<UsageEntry>, String> {
    if !projects_dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    collect_jsonl_files(projects_dir, &mut files, min_timestamp)?;

    let mut parsed_files = files
        .into_iter()
        .filter_map(|path| read_usage_file(&path, min_timestamp))
        .collect::<Vec<_>>();
    parsed_files.sort_by(compare_usage_files);

    let mut messages = Vec::new();
    let mut seen_response_ids = HashSet::new();
    for file in parsed_files {
        for candidate in file.entries {
            if let Some(response_id) = candidate.dedupe_id {
                if !seen_response_ids.insert(response_id) {
                    continue;
                }
            }
            messages.push(candidate.entry);
        }
    }

    Ok(messages)
}

fn read_codex_messages(
    sessions_dir: &Path,
    min_timestamp: Option<DateTime<Utc>>,
) -> Result<Vec<UsageEntry>, String> {
    if !sessions_dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    collect_jsonl_files(sessions_dir, &mut files, min_timestamp)?;
    files.sort();

    let mut messages = Vec::new();
    for path in files {
        let raw = match fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(_) => continue,
        };
        let mut model = "codex".to_string();

        for line in raw.lines().filter(|line| !line.trim().is_empty()) {
            let Ok(entry) = serde_json::from_str::<CodexJsonlEntry>(line) else {
                continue;
            };
            let Some(payload) = entry.payload else {
                continue;
            };

            if entry.kind.as_deref() == Some("turn_context") {
                if let Some(next_model) = payload.model {
                    model = next_model;
                }
                continue;
            }

            if entry.kind.as_deref() != Some("event_msg")
                || payload.kind.as_deref() != Some("token_count")
            {
                continue;
            }

            let Some(timestamp) = entry.timestamp else {
                continue;
            };
            let Some(timestamp_utc) = parse_timestamp_utc(&timestamp) else {
                continue;
            };
            if min_timestamp.is_some_and(|min_timestamp| timestamp_utc < min_timestamp) {
                continue;
            }

            let Some(usage) = payload.info.and_then(|info| info.last_token_usage) else {
                continue;
            };
            let input = usage
                .input_tokens
                .unwrap_or(0)
                .saturating_sub(usage.cached_input_tokens.unwrap_or(0));
            let output = usage.output_tokens.unwrap_or(0);
            if input + output == 0 {
                continue;
            }

            messages.push(UsageEntry {
                timestamp_utc,
                source: UsageSource::Codex,
                model: model.clone(),
                input,
                output,
                cache_read: usage.cached_input_tokens.unwrap_or(0),
                cache_creation: 0,
            });
        }
    }

    Ok(messages)
}

fn read_usage_file(path: &Path, min_timestamp: Option<DateTime<Utc>>) -> Option<ParsedUsageFile> {
    let raw = fs::read_to_string(path).ok()?;
    let mut earliest_timestamp_ms: Option<i64> = None;
    let mut entries = Vec::new();

    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(timestamp_utc) = value
            .get("timestamp")
            .and_then(|timestamp| timestamp.as_str())
            .and_then(parse_timestamp_utc)
        {
            let timestamp_ms = timestamp_utc.timestamp_millis();
            earliest_timestamp_ms = Some(match earliest_timestamp_ms {
                Some(current) => current.min(timestamp_ms),
                None => timestamp_ms,
            });
        }

        let Ok(entry) = serde_json::from_value::<ClaudeJsonlEntry>(value) else {
            continue;
        };
        if entry.kind.as_deref() != Some("assistant") {
            continue;
        }

        let Some(timestamp) = entry.timestamp else {
            continue;
        };
        let Some(timestamp_utc) = parse_timestamp_utc(&timestamp) else {
            continue;
        };
        if min_timestamp.is_some_and(|min_timestamp| timestamp_utc < min_timestamp) {
            continue;
        }

        let Some(message) = entry.message else {
            continue;
        };
        let dedupe_id = message.id.as_ref().or(entry.request_id.as_ref()).cloned();
        let Some(usage) = message.usage else {
            continue;
        };
        let model = message.model.unwrap_or_else(|| "unknown".to_string());
        if model == "<synthetic>" {
            continue;
        }

        entries.push(UsageCandidate {
            dedupe_id,
            entry: UsageEntry {
                timestamp_utc,
                source: UsageSource::Claude,
                model,
                input: usage.input_tokens.unwrap_or(0),
                output: usage.output_tokens.unwrap_or(0),
                cache_read: usage.cache_read_input_tokens.unwrap_or(0),
                cache_creation: usage.cache_creation_input_tokens.unwrap_or(0),
            },
        });
    }

    Some(ParsedUsageFile {
        sort_path: path.to_string_lossy().into_owned(),
        earliest_timestamp_ms,
        entries,
    })
}

fn parse_timestamp_utc(timestamp: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

fn compare_usage_files(left: &ParsedUsageFile, right: &ParsedUsageFile) -> Ordering {
    match (left.earliest_timestamp_ms, right.earliest_timestamp_ms) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
    .then_with(|| left.sort_path.cmp(&right.sort_path))
}

fn collect_jsonl_files(
    dir: &Path,
    files: &mut Vec<PathBuf>,
    min_mtime: Option<DateTime<Utc>>,
) -> Result<(), String> {
    let entries =
        fs::read_dir(dir).map_err(|err| format!("failed to scan {}: {err}", dir.display()))?;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl_files(&path, files, min_mtime)?;
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
            && file_mtime_is_recent_enough(&path, min_mtime)
        {
            files.push(path);
        }
    }

    Ok(())
}

fn file_mtime_is_recent_enough(path: &Path, min_mtime: Option<DateTime<Utc>>) -> bool {
    let Some(min_mtime) = min_mtime else {
        return true;
    };

    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };

    let modified: DateTime<Utc> = modified.into();
    modified >= min_mtime
}

fn messages_in_time_range<'a>(
    messages: &'a [UsageEntry],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> impl Iterator<Item = &'a UsageEntry> {
    messages
        .iter()
        .filter(move |entry| entry.timestamp_utc >= start && entry.timestamp_utc < end)
}

fn aggregate_messages<'a>(messages: impl Iterator<Item = &'a UsageEntry>) -> UsageAggregate {
    let mut aggregate = UsageAggregate::default();

    for msg in messages {
        let total = msg.input + msg.output;
        aggregate.total_tokens += total;
        aggregate.breakdown.input += msg.input;
        aggregate.breakdown.output += msg.output;
        match msg.source {
            UsageSource::Claude => aggregate.claude_tokens += total,
            UsageSource::Codex => aggregate.codex_tokens += total,
        }
        let usage = aggregate.model_usage.entry(msg.model.clone()).or_default();
        usage.input += msg.input;
        usage.output += msg.output;
        usage.cache_read += msg.cache_read;
        usage.cache_creation += msg.cache_creation;
        usage.messages += 1;
    }

    aggregate
}

fn rolling_sparkline(
    messages: &[UsageEntry],
    now: DateTime<Utc>,
    window: Duration,
    buckets: usize,
) -> Vec<SparklineBucket> {
    if buckets == 0 {
        return Vec::new();
    }

    let mut sparkline = vec![SparklineBucket::default(); buckets];
    let start = now - window;
    let end = now + Duration::seconds(1);
    let window_ms = window.num_milliseconds().max(1);
    let bucket_ms = (window_ms / buckets as i64).max(1);

    for msg in messages_in_time_range(messages, start, end) {
        let offset_ms = msg
            .timestamp_utc
            .signed_duration_since(start)
            .num_milliseconds();
        if offset_ms < 0 {
            continue;
        }
        let index = (offset_ms / bucket_ms) as usize;
        let index = index.min(buckets - 1);
        let tokens = msg.input + msg.output;
        match msg.source {
            UsageSource::Claude => sparkline[index].claude += tokens,
            UsageSource::Codex => sparkline[index].codex += tokens,
        }
    }

    sparkline
}

fn calculate_streaks(
    daily_activity: &[DailyActivity],
    has_activity_today: bool,
    today: NaiveDate,
) -> Streaks {
    let mut active_dates = daily_activity
        .iter()
        .filter_map(|day| NaiveDate::parse_from_str(&day.date, "%Y-%m-%d").ok())
        .collect::<HashSet<_>>();

    if has_activity_today {
        active_dates.insert(today);
    }

    if active_dates.is_empty() {
        return Streaks {
            current_streak: 0,
            longest_streak: 0,
        };
    }

    let mut current_streak = 0;
    let mut check = today;
    while active_dates.contains(&check) {
        current_streak += 1;
        check -= Duration::days(1);
    }

    let mut sorted = active_dates.into_iter().collect::<Vec<_>>();
    sorted.sort();

    let mut longest_streak = 1;
    let mut temp_streak = 1;
    for pair in sorted.windows(2) {
        if pair[1].signed_duration_since(pair[0]).num_days() == 1 {
            temp_streak += 1;
        } else {
            longest_streak = longest_streak.max(temp_streak);
            temp_streak = 1;
        }
    }
    longest_streak = longest_streak.max(temp_streak);

    Streaks {
        current_streak,
        longest_streak,
    }
}

fn format_duration(ms: f64) -> String {
    let total_ms = ms.max(0.0) as u64;
    let day_ms = 24 * 60 * 60 * 1000;
    let hour_ms = 60 * 60 * 1000;
    let minute_ms = 60 * 1000;

    let days = total_ms / day_ms;
    let hours = (total_ms % day_ms) / hour_ms;
    let minutes = (total_ms % hour_ms) / minute_ms;

    let mut parts = Vec::new();
    if days > 0 {
        parts.push(format!("{days}d"));
    }
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    parts.push(format!("{minutes}m"));
    parts.join(" ")
}

fn clean_model_name(model: &str) -> String {
    let without_prefix = model.strip_prefix("claude-").unwrap_or(model);
    let parts = without_prefix.rsplit_once('-');
    if let Some((base, maybe_date)) = parts {
        if maybe_date.len() == 8 && maybe_date.chars().all(|ch| ch.is_ascii_digit()) {
            return base.to_string();
        }
    }
    without_prefix.to_string()
}

fn trend_percent(current: u64, previous: u64) -> Option<i64> {
    if previous == 0 {
        return None;
    }

    Some((((current as f64 - previous as f64) / previous as f64) * 100.0).round() as i64)
}

struct HttpResponse {
    status: u16,
    reason: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
}

impl HttpResponse {
    fn empty(status: u16, reason: &'static str) -> Self {
        Self {
            status,
            reason,
            content_type: "text/plain; charset=utf-8",
            body: Vec::new(),
        }
    }

    fn html(status: u16, body: &str) -> Self {
        Self {
            status,
            reason: status_reason(status),
            content_type: "text/html; charset=utf-8",
            body: body.as_bytes().to_vec(),
        }
    }

    fn json<T: Serialize>(status: u16, value: &T) -> Self {
        match serde_json::to_vec(value) {
            Ok(body) => Self {
                status,
                reason: status_reason(status),
                content_type: "application/json; charset=utf-8",
                body,
            },
            Err(err) => Self::json_error(500, &format!("failed to serialize response: {err}")),
        }
    }

    fn json_value(status: u16, value: serde_json::Value) -> Self {
        Self::json(status, &value)
    }

    fn json_error(status: u16, message: &str) -> Self {
        let body = serde_json::json!({ "error": message });
        Self::json_value(status, body)
    }

    fn into_bytes(self) -> Vec<u8> {
        let headers = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type\r\nConnection: close\r\n\r\n",
            self.status,
            self.reason,
            self.content_type,
            self.body.len()
        );

        let mut response = headers.into_bytes();
        response.extend_from_slice(&self.body);
        response
    }
}

fn status_reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        426 => "Upgrade Required",
        500 => "Internal Server Error",
        _ => "OK",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct LocalClaudeStats {
        daily_model_tokens: Vec<LocalDailyModelTokens>,
        last_computed_date: String,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct LocalDailyModelTokens {
        date: String,
        tokens_by_model: BTreeMap<String, u64>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct LocalCodexCcusageReport {
        totals: LocalCodexCcusageTotals,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct LocalClaudeCcusageReport {
        totals: LocalClaudeCcusageTotals,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct LocalClaudeCcusageTotals {
        input_tokens: u64,
        output_tokens: u64,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct LocalCodexCcusageTotals {
        input_tokens: u64,
        cached_input_tokens: u64,
        output_tokens: u64,
    }

    fn ts(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn entry(timestamp: &str, tokens: u64) -> UsageEntry {
        entry_parts(timestamp, tokens, 0)
    }

    fn entry_parts(timestamp: &str, input: u64, output: u64) -> UsageEntry {
        entry_parts_for_source(timestamp, UsageSource::Claude, input, output)
    }

    fn entry_parts_for_source(
        timestamp: &str,
        source: UsageSource,
        input: u64,
        output: u64,
    ) -> UsageEntry {
        let timestamp_utc = ts(timestamp);
        UsageEntry {
            timestamp_utc,
            source,
            model: "claude-opus-4-7".to_string(),
            input,
            output,
            cache_read: 0,
            cache_creation: 0,
        }
    }

    #[test]
    fn rolling_windows_use_elapsed_time_not_calendar_days() {
        let now = ts("2026-04-27T00:30:00Z");
        let messages = vec![
            entry("2026-04-27T00:00:00Z", 100),
            entry("2026-04-26T00:31:00Z", 10),
            entry("2026-04-26T00:29:59Z", 1_000),
            entry("2026-04-21T00:30:00Z", 200),
            entry("2026-04-20T00:29:59Z", 3_000),
            entry("2026-03-29T00:30:00Z", 400),
            entry("2026-03-28T00:29:59Z", 5_000),
        ];

        let data = build_usage_data(StatsCache::default(), messages, now);

        assert_eq!(data.windows.last_24h, 110);
        assert_eq!(data.windows.prev_24h, 1_000);
        assert_eq!(data.windows.last_7d, 1_310);
        assert_eq!(data.windows.prev_7d, 3_000);
        assert_eq!(data.windows.last_30d, 4_710);
        assert_eq!(data.windows.prev_30d, 5_000);
    }

    #[test]
    fn windows_expose_input_output_breakdowns() {
        let now = ts("2026-04-27T00:30:00Z");
        let messages = vec![
            entry_parts("2026-04-27T00:00:00Z", 100, 25),
            entry_parts("2026-04-26T00:31:00Z", 10, 5),
            entry_parts("2026-03-28T00:29:59Z", 1_000, 500),
        ];

        let data = build_usage_data(StatsCache::default(), messages, now);

        assert_eq!(data.windows.last_24h, 140);
        assert_eq!(data.windows.last_24h_breakdown.input, 110);
        assert_eq!(data.windows.last_24h_breakdown.output, 30);
        assert_eq!(data.windows.all_time, 1_640);
        assert_eq!(data.windows.all_time_breakdown.input, 1_110);
        assert_eq!(data.windows.all_time_breakdown.output, 530);
    }

    #[test]
    fn rolling_windows_include_start_boundary() {
        let now = ts("2026-04-27T00:30:00Z");
        let messages = vec![
            entry("2026-04-26T00:30:00Z", 24),
            entry("2026-04-20T00:30:00Z", 7),
            entry("2026-03-28T00:30:00Z", 30),
            entry("2026-04-27T00:30:00Z", 1),
        ];

        let data = build_usage_data(StatsCache::default(), messages, now);

        assert_eq!(data.windows.last_24h, 25);
        assert_eq!(data.windows.last_7d, 32);
        assert_eq!(data.windows.last_30d, 62);
    }

    #[test]
    fn all_time_uses_deduped_message_source() {
        let now = ts("2026-04-27T00:30:00Z");
        let messages = vec![
            entry("2026-04-26T00:30:00Z", 100),
            entry("2026-04-01T00:30:00Z", 200),
            entry("2026-02-01T00:30:00Z", 300),
        ];

        let data = build_usage_data(StatsCache::default(), messages, now);

        assert_eq!(data.windows.last_30d, 300);
        assert_eq!(data.windows.all_time, 600);
        assert!(data.windows.all_time >= data.windows.last_30d);
    }

    #[test]
    fn rolling_sparkline_assigns_points_to_elapsed_buckets() {
        let now = ts("2026-04-02T00:00:00Z");
        let messages = vec![
            entry("2026-04-01T00:00:00Z", 1),
            entry("2026-04-01T12:00:00Z", 2),
            entry("2026-04-01T23:59:59Z", 3),
        ];

        let sparkline = rolling_sparkline(&messages, now, Duration::hours(24), 24);

        assert_eq!(sparkline[0].total(), 1);
        assert_eq!(sparkline[12].total(), 2);
        assert_eq!(sparkline[23].total(), 3);
        assert_eq!(
            sparkline.iter().map(|bucket| bucket.total()).sum::<u64>(),
            6
        );
    }

    #[test]
    fn rolling_sparkline_splits_claude_and_codex_sources() {
        let now = ts("2026-04-02T00:00:00Z");
        let messages = vec![
            entry_parts_for_source("2026-04-01T00:00:00Z", UsageSource::Claude, 10, 5),
            entry_parts_for_source("2026-04-01T00:30:00Z", UsageSource::Codex, 20, 7),
            entry_parts_for_source("2026-04-01T12:00:00Z", UsageSource::Codex, 1, 2),
        ];

        let sparkline = rolling_sparkline(&messages, now, Duration::hours(24), 24);

        assert_eq!(sparkline[0].claude, 15);
        assert_eq!(sparkline[0].codex, 27);
        assert_eq!(sparkline[0].total(), 42);
        assert_eq!(sparkline[12].claude, 0);
        assert_eq!(sparkline[12].codex, 3);
    }

    #[test]
    fn deduplicates_repeated_claude_response_ids_while_reading_jsonl() -> Result<(), Box<dyn Error>>
    {
        let dir = env::temp_dir().join(format!(
            "tokenlytics-dedupe-{}-{}",
            std::process::id(),
            Utc::now().timestamp_micros()
        ));
        fs::create_dir_all(&dir)?;
        fs::write(
            dir.join("usage.jsonl"),
            r#"{"type":"assistant","timestamp":"2026-04-26T10:00:00Z","requestId":"req_a","message":{"id":"msg_a","model":"claude-opus-4-7","usage":{"input_tokens":10,"output_tokens":5}}}
{"type":"assistant","timestamp":"2026-04-26T10:00:01Z","requestId":"req_a","message":{"id":"msg_a","model":"claude-opus-4-7","usage":{"input_tokens":1000,"output_tokens":500}}}
{"type":"assistant","timestamp":"2026-04-26T10:01:00Z","requestId":"req_b","message":{"id":"msg_b","model":"claude-opus-4-7","usage":{"input_tokens":20,"output_tokens":7}}}
"#,
        )?;

        let messages = read_project_messages(&dir, None).map_err(std::io::Error::other)?;
        fs::remove_dir_all(&dir)?;
        let aggregate = aggregate_messages(messages.iter());

        assert_eq!(messages.len(), 2);
        assert_eq!(aggregate.total_tokens, 42);
        Ok(())
    }

    #[test]
    fn reads_earliest_jsonl_files_before_deduping() -> Result<(), Box<dyn Error>> {
        let dir = env::temp_dir().join(format!(
            "tokenlytics-stable-dedupe-{}-{}",
            std::process::id(),
            Utc::now().timestamp_micros()
        ));
        fs::create_dir_all(&dir)?;
        fs::write(
            dir.join("a.jsonl"),
            r#"{"type":"assistant","timestamp":"2026-04-26T10:00:01Z","requestId":"req_a","message":{"id":"msg_a","model":"claude-opus-4-7","usage":{"input_tokens":1000,"output_tokens":500}}}
{"type":"assistant","timestamp":"2026-04-26T10:01:00Z","requestId":"req_b","message":{"id":"msg_b","model":"claude-opus-4-7","usage":{"input_tokens":20,"output_tokens":7}}}
"#,
        )?;
        fs::write(
            dir.join("z.jsonl"),
            r#"{"type":"assistant","timestamp":"2026-04-26T10:00:00Z","requestId":"req_a","message":{"id":"msg_a","model":"claude-opus-4-7","usage":{"input_tokens":10,"output_tokens":5}}}
"#,
        )?;

        let messages = read_project_messages(&dir, None).map_err(std::io::Error::other)?;
        fs::remove_dir_all(&dir)?;
        let aggregate = aggregate_messages(messages.iter());

        assert_eq!(messages.len(), 2);
        assert_eq!(aggregate.total_tokens, 42);
        Ok(())
    }

    #[test]
    fn uses_response_timestamp_instead_of_session_start_date() -> Result<(), Box<dyn Error>> {
        let dir = env::temp_dir().join(format!(
            "tokenlytics-response-date-{}-{}",
            std::process::id(),
            Utc::now().timestamp_micros()
        ));
        fs::create_dir_all(&dir)?;
        fs::write(
            dir.join("usage.jsonl"),
            r#"{"type":"user","timestamp":"2026-04-20T10:00:00Z","isSidechain":false,"message":{"role":"user","content":"start"}}
{"type":"assistant","timestamp":"2026-04-27T10:00:00Z","isSidechain":false,"message":{"id":"msg_a","model":"claude-opus-4-7","usage":{"input_tokens":10,"output_tokens":5}}}
"#,
        )?;

        let messages = read_project_messages(&dir, None).map_err(std::io::Error::other)?;
        fs::remove_dir_all(&dir)?;
        let data = build_usage_data(StatsCache::default(), messages, ts("2026-04-27T12:00:00Z"));

        assert_eq!(data.windows.last_24h, 15);
        assert_eq!(data.windows.last_7d, 15);
        assert_eq!(data.windows.prev_7d, 0);
        Ok(())
    }

    #[test]
    fn reads_codex_token_count_events_without_cached_input() -> Result<(), Box<dyn Error>> {
        let dir = env::temp_dir().join(format!(
            "tokenlytics-codex-{}-{}",
            std::process::id(),
            Utc::now().timestamp_micros()
        ));
        fs::create_dir_all(&dir)?;
        fs::write(
            dir.join("rollout.jsonl"),
            r#"{"timestamp":"2026-04-27T10:00:00.000Z","type":"turn_context","payload":{"model":"gpt-5.5"}}
{"timestamp":"2026-04-27T10:00:01.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":7}}}}
{"timestamp":"2026-04-27T10:00:02.000Z","type":"event_msg","payload":{"type":"token_count","info":null}}
{"timestamp":"2026-04-27T10:00:03.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":30,"cached_input_tokens":0,"output_tokens":5}}}}
"#,
        )?;

        let messages = read_codex_messages(&dir, None).map_err(std::io::Error::other)?;
        fs::remove_dir_all(&dir)?;
        let aggregate = aggregate_messages(messages.iter());

        assert_eq!(messages.len(), 2);
        assert_eq!(aggregate.total_tokens, 102);
        assert_eq!(aggregate.breakdown.input, 90);
        assert_eq!(aggregate.breakdown.output, 12);
        assert_eq!(
            aggregate
                .model_usage
                .get("gpt-5.5")
                .map(|usage| usage.messages),
            Some(2)
        );
        Ok(())
    }

    #[test]
    fn data_fingerprint_changes_when_usage_file_changes() -> Result<(), Box<dyn Error>> {
        let dir = env::temp_dir().join(format!(
            "tokenlytics-fingerprint-{}-{}",
            std::process::id(),
            Utc::now().timestamp_micros()
        ));
        let projects_dir = dir.join("projects");
        let codex_sessions_dir = dir.join("codex").join("sessions");
        fs::create_dir_all(&projects_dir)?;
        fs::create_dir_all(&codex_sessions_dir)?;

        let paths = Paths {
            claude_dir: dir.clone(),
            projects_dir: projects_dir.clone(),
            stats_cache: dir.join("stats-cache.json"),
            codex_dir: dir.join("codex"),
            codex_sessions_dir: codex_sessions_dir.clone(),
        };
        let usage_file = projects_dir.join("usage.jsonl");
        fs::write(&usage_file, "one\n")?;

        let before = data_fingerprint(&paths).map_err(std::io::Error::other)?;
        fs::write(&usage_file, "one\ntwo\n")?;
        let after = data_fingerprint(&paths).map_err(std::io::Error::other)?;

        fs::remove_dir_all(&dir)?;

        assert_ne!(before, after);
        Ok(())
    }

    #[test]
    fn usage_path_filter_matches_project_and_stats_files() -> Result<(), Box<dyn Error>> {
        let dir = env::temp_dir().join(format!(
            "tokenlytics-path-filter-{}-{}",
            std::process::id(),
            Utc::now().timestamp_micros()
        ));
        let paths = Paths {
            claude_dir: dir.clone(),
            projects_dir: dir.join("projects"),
            stats_cache: dir.join("stats-cache.json"),
            codex_dir: dir.join("codex"),
            codex_sessions_dir: dir.join("codex").join("sessions"),
        };

        assert!(path_touches_usage(&paths.stats_cache, &paths));
        assert!(path_touches_usage(
            &paths.projects_dir.join("project/session.jsonl"),
            &paths
        ));
        assert!(path_touches_usage(
            &paths.codex_sessions_dir.join("2026/04/27/rollout.jsonl"),
            &paths
        ));
        assert!(!path_touches_usage(&dir.join("settings.json"), &paths));
        Ok(())
    }

    #[test]
    fn realtime_api_paths_are_explicit_aliases() {
        assert!(is_realtime_path("/api/stream"));
        assert!(is_realtime_path("/api/realtime"));
        assert!(!is_realtime_path("/api/usage"));
    }

    #[test]
    fn usage_sse_event_wraps_usage_payload() {
        let data = build_usage_data(
            StatsCache::default(),
            Vec::new(),
            ts("2026-04-27T12:00:00Z"),
        );
        let event = usage_sse_event(&data);

        assert!(event.starts_with("event: usage\n"));
        assert!(event.contains("id: 2026-04-27T12:00:00+00:00\n"));
        assert!(event.contains("\"last24h\":0"));
        assert!(event.ends_with("\n\n"));
    }

    #[test]
    fn claude_calendar_fixture_is_different_from_rolling_window() {
        let mut day_20 = BTreeMap::new();
        day_20.insert("claude-opus-4-7".to_string(), 1_000);
        let mut day_21 = BTreeMap::new();
        day_21.insert("claude-opus-4-7".to_string(), 100);
        let mut day_27 = BTreeMap::new();
        day_27.insert("claude-opus-4-7".to_string(), 10);
        let days = vec![
            LocalDailyModelTokens {
                date: "2026-04-20".to_string(),
                tokens_by_model: day_20,
            },
            LocalDailyModelTokens {
                date: "2026-04-21".to_string(),
                tokens_by_model: day_21,
            },
            LocalDailyModelTokens {
                date: "2026-04-27".to_string(),
                tokens_by_model: day_27,
            },
        ];

        let claude_calendar_total =
            calendar_last_7d_from_daily_model_tokens(&days, "2026-04-27").unwrap();

        assert_eq!(claude_calendar_total, 110);
    }

    #[test]
    fn normalized_rolling_window_matches_calendar_window() {
        let normalized_end = ts("2026-04-27T23:59:59Z");
        let messages = vec![
            entry("2026-04-20T23:59:58Z", 1_000),
            entry("2026-04-21T00:00:00Z", 100),
            entry("2026-04-27T23:59:59Z", 10),
            entry("2026-04-28T00:00:00Z", 2_000),
        ];

        let data = build_usage_data(StatsCache::default(), messages, normalized_end);

        assert_eq!(data.windows.last_7d, 110);
    }

    #[test]
    #[ignore = "reads local Claude Code data and compares normalized windows"]
    fn local_claude_code_comparison_matches_when_time_is_normalized() -> Result<(), Box<dyn Error>>
    {
        let paths = Paths::from_home();
        if !paths.stats_cache.exists() || !paths.projects_dir.exists() {
            eprintln!("No local Claude Code data found.");
            return Ok(());
        }

        let now = Utc::now();
        let raw = fs::read_to_string(&paths.stats_cache)?;
        let stats: LocalClaudeStats = serde_json::from_str(&raw)?;
        let claude_calendar_7d = calendar_last_7d_from_daily_model_tokens(
            &stats.daily_model_tokens,
            &stats.last_computed_date,
        )?;
        let normalized_end = ts(&format!("{}T23:59:59Z", stats.last_computed_date));
        let normalized_messages = daily_model_tokens_as_messages(&stats.daily_model_tokens);
        let tokenlytics_normalized_7d =
            build_usage_data(StatsCache::default(), normalized_messages, normalized_end)
                .windows
                .last_7d;
        let live = get_usage_data_at(&paths, now).map_err(std::io::Error::other)?;

        eprintln!("Tokenlytics all time: {}", live.windows.all_time);
        eprintln!("Tokenlytics rolling last 24h: {}", live.windows.last_24h);
        eprintln!("Tokenlytics rolling last 7d: {}", live.windows.last_7d);
        eprintln!("Tokenlytics rolling last 30d: {}", live.windows.last_30d);
        eprintln!(
            "Tokenlytics normalized calendar last 7d: {}",
            tokenlytics_normalized_7d
        );
        eprintln!("Claude Code calendar last 7d: {claude_calendar_7d}");

        assert_eq!(tokenlytics_normalized_7d, claude_calendar_7d);
        Ok(())
    }

    #[test]
    #[ignore = "requires local Claude data and npx ccusage"]
    fn local_claude_comparison_matches_ccusage_without_cache() -> Result<(), Box<dyn Error>> {
        let paths = Paths::from_home();
        if !paths.projects_dir.exists() {
            eprintln!("No local Claude Code data found.");
            return Ok(());
        }

        let tokenlytics_messages =
            read_project_messages(&paths.projects_dir, None).map_err(std::io::Error::other)?;
        let completed_day = completed_utc_day();
        let until = completed_day.format("%Y%m%d").to_string();
        let until_label = completed_day.to_string();
        let tokenlytics = aggregate_messages(
            tokenlytics_messages
                .iter()
                .filter(|entry| entry.timestamp_utc.date_naive() <= completed_day),
        );

        let output = std::process::Command::new("npx")
            .args([
                "--yes",
                "ccusage@latest",
                "daily",
                "--json",
                "--offline",
                "--timezone",
                "UTC",
                "--until",
                until.as_str(),
            ])
            .output()?;
        if !output.status.success() {
            return Err(std::io::Error::other(format!(
                "ccusage failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ))
            .into());
        }

        let ccusage: LocalClaudeCcusageReport = serde_json::from_slice(&output.stdout)?;
        let ccusage_input = ccusage.totals.input_tokens;
        let ccusage_output = ccusage.totals.output_tokens;

        eprintln!(
            "Tokenlytics Claude input+output without cache through {until_label} UTC: {}",
            tokenlytics.total_tokens,
        );
        eprintln!(
            "ccusage input+output without cache through {until_label} UTC: {}",
            ccusage_input + ccusage_output,
        );

        assert_eq!(tokenlytics.breakdown.input, ccusage_input);
        assert_eq!(tokenlytics.breakdown.output, ccusage_output);
        Ok(())
    }

    #[test]
    #[ignore = "requires local Codex data and npx @ccusage/codex"]
    fn local_codex_comparison_matches_ccusage_without_cache() -> Result<(), Box<dyn Error>> {
        let paths = Paths::from_home();
        if !paths.codex_sessions_dir.exists() {
            eprintln!("No local Codex data found.");
            return Ok(());
        }

        let tokenlytics_messages =
            read_codex_messages(&paths.codex_sessions_dir, None).map_err(std::io::Error::other)?;
        let completed_day = completed_utc_day();
        let until = completed_day.format("%Y%m%d").to_string();
        let until_label = completed_day.to_string();
        let tokenlytics = aggregate_messages(
            tokenlytics_messages
                .iter()
                .filter(|entry| entry.timestamp_utc.date_naive() <= completed_day),
        );

        let output = std::process::Command::new("npx")
            .args([
                "--yes",
                "@ccusage/codex@latest",
                "daily",
                "--json",
                "--offline",
                "--timezone",
                "UTC",
                "--until",
                until.as_str(),
            ])
            .output()?;
        if !output.status.success() {
            return Err(std::io::Error::other(format!(
                "@ccusage/codex failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ))
            .into());
        }

        let ccusage: LocalCodexCcusageReport = serde_json::from_slice(&output.stdout)?;
        let ccusage_input = ccusage
            .totals
            .input_tokens
            .saturating_sub(ccusage.totals.cached_input_tokens);
        let ccusage_output = ccusage.totals.output_tokens;

        eprintln!(
            "Tokenlytics Codex input+output without cache through {until_label} UTC: {}",
            tokenlytics.total_tokens,
        );
        eprintln!(
            "@ccusage/codex input+output without cache through {until_label} UTC: {}",
            ccusage_input + ccusage_output,
        );

        assert_eq!(tokenlytics.breakdown.input, ccusage_input);
        assert_eq!(tokenlytics.breakdown.output, ccusage_output);
        Ok(())
    }

    fn completed_utc_day() -> NaiveDate {
        (Utc::now() - Duration::days(1)).date_naive()
    }

    fn daily_model_tokens_as_messages(days: &[LocalDailyModelTokens]) -> Vec<UsageEntry> {
        let mut messages = Vec::new();
        for day in days {
            let timestamp_utc = ts(&format!("{}T12:00:00Z", day.date));
            for (model, tokens) in &day.tokens_by_model {
                messages.push(UsageEntry {
                    timestamp_utc,
                    source: UsageSource::Claude,
                    model: model.clone(),
                    input: *tokens,
                    output: 0,
                    cache_read: 0,
                    cache_creation: 0,
                });
            }
        }
        messages
    }

    fn calendar_last_7d_from_daily_model_tokens(
        days: &[LocalDailyModelTokens],
        last_computed_date: &str,
    ) -> Result<u64, Box<dyn Error>> {
        let end = NaiveDate::parse_from_str(last_computed_date, "%Y-%m-%d")?;
        let start = end - Duration::days(6);
        let total = days
            .iter()
            .filter_map(|entry| {
                let date = NaiveDate::parse_from_str(&entry.date, "%Y-%m-%d").ok()?;
                (date >= start && date <= end).then(|| entry.tokens_by_model.values().sum::<u64>())
            })
            .sum();
        Ok(total)
    }

    #[test]
    fn user_config_roundtrips_through_toml() {
        let cfg = UserConfig {
            name: Some("fabio".to_string()),
            port: Some(4000),
            leaderboard: LeaderboardCfg {
                enabled: true,
                host: Some("http://example.com:3456".to_string()),
            },
        };
        let raw = toml::to_string_pretty(&cfg).unwrap();
        let parsed: UserConfig = toml::from_str(&raw).unwrap();
        assert_eq!(parsed.name.as_deref(), Some("fabio"));
        assert_eq!(parsed.port, Some(4000));
        assert!(parsed.leaderboard.enabled);
        assert_eq!(
            parsed.leaderboard.host.as_deref(),
            Some("http://example.com:3456")
        );
    }

    #[test]
    fn user_config_defaults_when_fields_missing() {
        let parsed: UserConfig = toml::from_str("").unwrap();
        assert!(parsed.name.is_none());
        assert!(parsed.port.is_none());
        assert!(!parsed.leaderboard.enabled);
        assert!(parsed.leaderboard.host.is_none());
    }

    #[test]
    fn onboarding_only_prompts_for_port_when_hosting_friends() {
        assert!(!should_prompt_port(
            "global",
            true,
            Some(DEFAULT_GLOBAL_HOST)
        ));
        assert!(!should_prompt_port("off", false, None));
        assert!(!should_prompt_port(
            "friends",
            true,
            Some("http://friend.example:6969")
        ));
        assert!(should_prompt_port("friends", true, None));
    }

    #[test]
    fn semver_parses_normal_and_v_prefixed() {
        assert_eq!(parse_semver("0.1.0"), Some((0, 1, 0)));
        assert_eq!(parse_semver("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_semver("12.34.56"), Some((12, 34, 56)));
        assert_eq!(parse_semver("not.a.version"), None);
        assert_eq!(parse_semver("1.0"), None);
    }

    #[test]
    fn version_too_old_compares_correctly() {
        assert!(version_too_old("0.0.9", "0.1.0"));
        assert!(version_too_old("0.1.0", "0.2.0"));
        assert!(!version_too_old("0.1.0", "0.1.0"));
        assert!(!version_too_old("1.0.0", "0.9.9"));
        assert!(version_too_old("garbage", "0.1.0")); // unparseable = ancient
    }
}
