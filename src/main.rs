use chrono::{DateTime, Duration, NaiveDate, Utc};
use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashSet},
    env, fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{mpsc, Arc, Condvar, Mutex},
    thread,
    time::{Duration as StdDuration, Instant, SystemTime, UNIX_EPOCH},
};

const DASHBOARD_HTML: &str = include_str!("../dashboard.html");
const EVENT_DEBOUNCE: StdDuration = StdDuration::from_millis(150);
const POLL_FALLBACK_INTERVAL: StdDuration = StdDuration::from_secs(1);
const ROLLING_REFRESH_INTERVAL: StdDuration = StdDuration::from_secs(60);
const SSE_KEEPALIVE_INTERVAL: StdDuration = StdDuration::from_secs(15);

#[derive(Clone)]
struct AppState {
    cache: Arc<Mutex<CacheState>>,
    cache_changed: Arc<Condvar>,
    paths: Paths,
}

#[derive(Clone)]
struct Paths {
    claude_dir: PathBuf,
    projects_dir: PathBuf,
    stats_cache: PathBuf,
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

#[derive(Clone, Debug)]
struct UsageEntry {
    timestamp_utc: DateTime<Utc>,
    model: String,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_creation: u64,
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
    total_tokens: u64,
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
    prev_24h: u64,
    prev_7d: u64,
    prev_30d: u64,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Sparklines {
    last_24h: Vec<u64>,
    last_7d: Vec<u64>,
    last_30d: Vec<u64>,
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
    let port = env::var("PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(3456);

    let paths = Paths::from_home();
    let state = AppState {
        cache: Arc::new(Mutex::new(CacheState::default())),
        cache_changed: Arc::new(Condvar::new()),
        paths,
    };

    let listener = TcpListener::bind(("127.0.0.1", port))
        .unwrap_or_else(|err| panic!("failed to bind http://localhost:{port}: {err}"));

    let preload_started = Instant::now();
    match refresh_cache(&state) {
        Ok(_) => {
            println!(
                "Loaded usage cache in {:.0}ms",
                preload_started.elapsed().as_secs_f64() * 1000.0
            );
        }
        Err(err) => eprintln!("failed to preload usage cache: {err}"),
    }
    start_usage_watcher(state.clone());

    println!("Tokenlytics running at http://localhost:{port}");
    println!();
    println!("API endpoints:");
    println!("  GET /api/usage   - full usage data");
    println!("  GET /api/tokens  - token counts + trends");
    println!("  GET /api/models  - per-model breakdown");
    println!("  GET /api/stream  - realtime SSE stream");
    println!("  GET /api/realtime - realtime SSE stream alias");

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
}

impl Paths {
    fn from_home() -> Self {
        let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
        let claude_dir = PathBuf::from(home).join(".claude");
        Self {
            claude_dir: claude_dir.clone(),
            projects_dir: claude_dir.join("projects"),
            stats_cache: claude_dir.join("stats-cache.json"),
        }
    }
}

fn handle_connection(mut stream: TcpStream, state: &AppState) -> std::io::Result<()> {
    let mut buffer = [0_u8; 16 * 1024];
    let bytes_read = stream.read(&mut buffer)?;
    if bytes_read == 0 {
        return Ok(());
    }

    let request = String::from_utf8_lossy(&buffer[..bytes_read]);
    let request_line = request.lines().next().unwrap_or_default();
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
        _ => HttpResponse::json_error(405, "method not allowed"),
    };

    stream.write_all(&response.into_bytes())?;
    stream.flush()
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
        _ => HttpResponse::html(200, DASHBOARD_HTML),
    }
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
    if !state.paths.claude_dir.is_dir() {
        return Err(format!(
            "{} does not exist",
            state.paths.claude_dir.display()
        ));
    }

    let (tx, rx) = mpsc::channel();
    let mut watcher = RecommendedWatcher::new(tx, Config::default())
        .map_err(|err| format!("failed to start OS file watcher: {err}"))?;
    watcher
        .watch(&state.paths.claude_dir, RecursiveMode::Recursive)
        .map_err(|err| {
            format!(
                "failed to watch {}: {err}",
                state.paths.claude_dir.display()
            )
        })?;

    println!(
        "Watching Claude usage files with OS events under {}",
        state.paths.claude_dir.display()
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
        || (path.starts_with(&paths.claude_dir)
            && path.file_name().and_then(|name| name.to_str()) == Some("stats-cache.json"))
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
    let messages = read_project_messages(&paths.projects_dir, None)?;

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
    let all_time_model_usage = all_time.model_usage;

    let windows = Windows {
        last_24h: agg_24h.total_tokens,
        last_7d: agg_7d.total_tokens,
        last_30d: agg_30d.total_tokens,
        all_time: all_time_tokens,
        prev_24h: agg_prev_24h.total_tokens,
        prev_7d: agg_prev_7d.total_tokens,
        prev_30d: agg_prev_30d.total_tokens,
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
        aggregate.total_tokens += msg.input + msg.output;
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
) -> Vec<u64> {
    if buckets == 0 {
        return Vec::new();
    }

    let mut sparkline = vec![0_u64; buckets];
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
        sparkline[index] += msg.input + msg.output;
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
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type\r\nConnection: close\r\n\r\n",
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
        405 => "Method Not Allowed",
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

    fn ts(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn entry(timestamp: &str, tokens: u64) -> UsageEntry {
        let timestamp_utc = ts(timestamp);
        UsageEntry {
            timestamp_utc,
            model: "claude-opus-4-7".to_string(),
            input: tokens,
            output: 0,
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

        assert_eq!(sparkline[0], 1);
        assert_eq!(sparkline[12], 2);
        assert_eq!(sparkline[23], 3);
        assert_eq!(sparkline.iter().sum::<u64>(), 6);
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
    fn data_fingerprint_changes_when_usage_file_changes() -> Result<(), Box<dyn Error>> {
        let dir = env::temp_dir().join(format!(
            "tokenlytics-fingerprint-{}-{}",
            std::process::id(),
            Utc::now().timestamp_micros()
        ));
        let projects_dir = dir.join("projects");
        fs::create_dir_all(&projects_dir)?;

        let paths = Paths {
            claude_dir: dir.clone(),
            projects_dir: projects_dir.clone(),
            stats_cache: dir.join("stats-cache.json"),
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
        };

        assert!(path_touches_usage(&paths.stats_cache, &paths));
        assert!(path_touches_usage(
            &paths.projects_dir.join("project/session.jsonl"),
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

    fn daily_model_tokens_as_messages(days: &[LocalDailyModelTokens]) -> Vec<UsageEntry> {
        let mut messages = Vec::new();
        for day in days {
            let timestamp_utc = ts(&format!("{}T12:00:00Z", day.date));
            for (model, tokens) in &day.tokens_by_model {
                messages.push(UsageEntry {
                    timestamp_utc,
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
}
