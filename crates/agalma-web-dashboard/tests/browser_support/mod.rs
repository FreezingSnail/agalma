//! Reusable headless-browser harness for the WEB.6 cockpit acceptance.
//!
//! This module is shared test support (not a product dependency): it starts a
//! real loopback HTTP server that serves the **compiled** dashboard assets plus
//! scripted JSON endpoints, runs the **installed** Chrome in headless mode
//! against it, and returns the serialized DOM a driver page wrote.
//!
//! - No browser, npm package or framework is downloaded. The executable is the
//!   installed `CHROME_BIN` (or a well-known local install path).
//! - Every run uses a unique profile directory, a unique loopback port and the
//!   real DOM/interaction surface; assertions come from the returned DOM.
//! - The server records every request so a test can prove, for example, that a
//!   refresh cycle generated zero POSTs.
//!
//! WEB.5/WEB.7/WEB.8 consume this harness later under a named ownership
//! transfer; they must not create an independent replacement.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// One captured HTTP request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub query: String,
    pub body: String,
}

/// One scripted HTTP response.
#[derive(Clone, Debug)]
pub struct Response {
    pub status: u16,
    pub content_type: String,
    pub body: String,
}

impl Response {
    pub fn json(status: u16, body: impl Into<String>) -> Self {
        Self { status, content_type: "application/json".into(), body: body.into() }
    }

    pub fn text(status: u16, content_type: &str, body: impl Into<String>) -> Self {
        Self { status, content_type: content_type.into(), body: body.into() }
    }

    pub fn unavailable(message: &str) -> Self {
        Self::json(503, format!("{{\"error\":\"read_unavailable\",\"detail\":{message:?}}}"))
    }
}

/// Router signature: (method, path, query, body) -> response, or `None` to use
/// the compiled-asset default.
pub type Router = Arc<dyn Fn(&str, &str, &str, &str) -> Option<Response> + Send + Sync>;

struct ServerInner {
    router: Router,
    extra: Mutex<BTreeMap<String, Response>>,
    log: Mutex<Vec<RecordedRequest>>,
    gets: AtomicUsize,
    posts: AtomicUsize,
    stopped: AtomicBool,
}

/// A running loopback fixture server.
pub struct FixtureServer {
    addr: SocketAddr,
    inner: Arc<ServerInner>,
    handle: Mutex<Option<JoinHandle<()>>>,
    _keepalive: TcpListener,
}

impl FixtureServer {
    /// Start a server whose JSON routes are decided by `router`. Compiled assets
    /// are always served for their exact request paths.
    pub fn start(router: Router) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let keepalive = listener.try_clone()?;
        let inner = Arc::new(ServerInner {
            router,
            extra: Mutex::new(BTreeMap::new()),
            log: Mutex::new(Vec::new()),
            gets: AtomicUsize::new(0),
            posts: AtomicUsize::new(0),
            stopped: AtomicBool::new(false),
        });
        let thread_inner = inner.clone();
        let handle = thread::spawn(move || {
            while !thread_inner.stopped.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let connection_inner = thread_inner.clone();
                        thread::spawn(move || {
                            let _ = serve_connection(stream, &connection_inner);
                        });
                    }
                    Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            addr,
            inner,
            handle: Mutex::new(Some(handle)),
            _keepalive: keepalive,
        })
    }

    /// Base URL `http://127.0.0.1:<port>` for the running server.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Register an extra static route (for example a driver page).
    pub fn register(&self, path: &str, response: Response) {
        self.inner.extra.lock().unwrap().insert(path.to_string(), response);
    }

    /// Every request the server has answered, in order.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.inner.log.lock().unwrap().clone()
    }

    /// Number of POST requests observed.
    pub fn post_count(&self) -> usize {
        self.inner.posts.load(Ordering::SeqCst)
    }

    /// Number of GET requests observed.
    pub fn get_count(&self) -> usize {
        self.inner.gets.load(Ordering::SeqCst)
    }

    /// Count requests for a path prefix.
    pub fn count_path(&self, prefix: &str) -> usize {
        self.requests().iter().filter(|r| r.path.starts_with(prefix)).count()
    }

    pub fn reset_log(&self) {
        self.inner.log.lock().unwrap().clear();
    }
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.inner.stopped.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.lock().unwrap().take() {
            let _ = handle.join();
        }
    }
}

fn compiled_asset(path: &str) -> Option<Response> {
    let (content_type, bytes) = agalma_web_dashboard::inventory::compiled_asset(path)?;
    Some(Response::text(200, content_type, String::from_utf8_lossy(bytes).into_owned()))
}

fn serve_connection(mut stream: TcpStream, inner: &ServerInner) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    // Read headers.
    let header_end;
    loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Ok(());
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = find_headers_end(&buffer) {
            header_end = position;
            break;
        }
        if buffer.len() > 64 * 1024 {
            return Ok(());
        }
    }
    let header_text = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let mut lines = header_text.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap_or("GET").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let mut content_length = 0usize;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let body_start = header_end + 4;
    while buffer.len() < body_start + content_length {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let body = String::from_utf8_lossy(&buffer[body_start..]).into_owned();

    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (target.clone(), String::new()),
    };

    let mut log = inner.log.lock().unwrap();
    log.push(RecordedRequest { method: method.clone(), path: path.clone(), query: query.clone(), body: body.clone() });
    drop(log);
    if method == "POST" {
        inner.posts.fetch_add(1, Ordering::SeqCst);
    } else {
        inner.gets.fetch_add(1, Ordering::SeqCst);
    }

    let response = (inner.router)(&method, &path, &query, &body)
        .or_else(|| compiled_asset(&path))
        .or_else(|| inner.extra.lock().unwrap().get(&path).cloned())
        .unwrap_or_else(|| Response::json(404, "{\"error\":\"not_found\"}"));

    write_response(&mut stream, &response)?;
    Ok(())
}

fn find_headers_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn write_response(stream: &mut TcpStream, response: &Response) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        response.status,
        status_text(response.status),
        response.content_type,
        response.body.len(),
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(response.body.as_bytes())?;
    stream.flush()
}

fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Status",
    }
}

/// Locate the installed Chrome executable without downloading anything.
pub fn resolve_chrome() -> Result<PathBuf, String> {
    if let Ok(explicit) = std::env::var("CHROME_BIN") {
        if !explicit.is_empty() {
            let path = PathBuf::from(&explicit);
            if path.is_file() {
                return Ok(path);
            }
            return Err(format!("CHROME_BIN={explicit} does not exist"));
        }
    }
    let candidates = [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/usr/bin/google-chrome",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
    ];
    for candidate in candidates {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return Ok(path);
        }
    }
    Err("no installed Chrome/Chromium found (set CHROME_BIN)".to_string())
}

/// The installed browser version string.
pub fn chrome_version(chrome: &Path) -> String {
    Command::new(chrome)
        .arg("--version")
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

/// One headless browser run result.
pub struct BrowserOutcome {
    pub dom: String,
    pub stderr: String,
    pub status: std::process::ExitStatus,
    pub profile: PathBuf,
}

impl BrowserOutcome {
    /// Extract the JSON the driver page wrote into `#harness-result`.
    pub fn result_json(&self) -> serde_json::Value {
        self.try_result_json().unwrap_or_else(|error| panic!("{error}"))
    }

    /// Fallible form used by the retry wrapper: an empty or truncated result is
    /// a browser-run failure that may be retried with a fresh profile.
    pub fn try_result_json(&self) -> Result<serde_json::Value, String> {
        let marker = "<pre id=\"harness-result\">";
        let start = self
            .dom
            .find(marker)
            .ok_or_else(|| format!("driver DOM has no harness result; dom={}", self.dom))?
            + marker.len();
        let rest = &self.dom[start..];
        let end = rest.find("</pre>").unwrap_or(rest.len());
        let raw = unescape_html(&rest[..end]);
        if raw.trim().is_empty() {
            return Err(format!(
                "driver result is empty; status={:?} stderr={}",
                self.status, self.stderr
            ));
        }
        serde_json::from_str(&raw)
            .map_err(|error| format!("driver result is not JSON ({error}); raw={raw:?}"))
    }
}

fn unescape_html(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// Serializes real browser launches inside one test binary.
///
/// The runner may execute several cases concurrently in one process. Two
/// independent headless Chrome launches do not share state, but concurrent
/// `--dump-dom` runs under a bounded virtual-time budget occasionally serialize
/// an empty document. Holding this lock for one launch keeps every case honest
/// (each still runs a real browser) without relying on scheduler luck.
static BROWSER_LAUNCH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static BROWSER_PROFILE_SEQ: AtomicUsize = AtomicUsize::new(0);

/// Run the installed Chrome headless against `url` and return the dumped DOM.
///
/// `budget_ms` is the virtual-time budget: timers and awaited fetches run to
/// completion quickly while `--dump-dom` still serializes the settled DOM.
pub fn run_browser(chrome: &Path, url: &str, budget_ms: u64, label: &str) -> BrowserOutcome {
    run_browser_sized(chrome, url, budget_ms, label, 1280, 2000)
}

/// Run the installed Chrome headless with an explicit viewport so responsive
/// layout checks use the real CSS engine instead of a simulated width.
pub fn run_browser_sized(
    chrome: &Path,
    url: &str,
    budget_ms: u64,
    label: &str,
    width: u32,
    height: u32,
) -> BrowserOutcome {
    run_browser_sized_theme(chrome, url, budget_ms, label, width, height, None)
}

/// Deterministic theme args: Chrome's Blink setting accepts an explicit
/// preferred color scheme even when the host desktop is the other theme.
pub fn theme_args(theme: &str) -> Vec<&'static str> {
    match theme {
        "light" => vec!["--blink-settings=preferredColorScheme=1"],
        "dark" => vec!["--blink-settings=preferredColorScheme=0"],
        _ => Vec::new(),
    }
}

/// Sized run with an explicit `light`/`dark` emulated color scheme.
pub fn run_browser_sized_theme(
    chrome: &Path,
    url: &str,
    budget_ms: u64,
    label: &str,
    width: u32,
    height: u32,
    theme: Option<&str>,
) -> BrowserOutcome {
    let _launch_guard = BROWSER_LAUNCH_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let seq = BROWSER_PROFILE_SEQ.fetch_add(1, Ordering::SeqCst);
    let profile = std::env::temp_dir().join(format!(
        "agalma-web6-{label}-{}-{seq}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&profile);
    std::fs::create_dir_all(&profile).expect("create browser profile dir");
    let mut command = Command::new(chrome);
    command.args([
        "--headless=new",
        "--disable-gpu",
        "--no-first-run",
        "--no-default-browser-check",
        "--disable-extensions",
        "--disable-background-networking",
        "--disable-sync",
        "--mute-audio",
        "--hide-scrollbars",
    ]);
    for arg in theme_args(theme.unwrap_or("")) {
        command.arg(arg);
    }
    let output = command
        .arg(format!("--window-size={width},{height}"))
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg(format!("--virtual-time-budget={budget_ms}"))
        .args(["--dump-dom", url])
        .output()
        .expect("spawn installed Chrome");
    BrowserOutcome {
        dom: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        status: output.status,
        profile,
    }
}

/// Capture one real browser screenshot (inspection artifact, not a pixel
/// snapshot). Chrome's headless viewport has a 500 CSS px minimum for `innerWidth`;
/// narrow (<500 px) artifacts are therefore captured through the harness iframe
/// and cropped to the requested CSS width with the installed `sips` tool.
pub fn capture_screenshot(
    chrome: &Path,
    url: &str,
    path: &Path,
    budget_ms: u64,
    label: &str,
    width: u32,
    height: u32,
    theme: &str,
) -> BrowserOutcome {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create screenshot directory");
    }
    let launch_width = width.max(500);
    let _launch_guard = BROWSER_LAUNCH_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let seq = BROWSER_PROFILE_SEQ.fetch_add(1, Ordering::SeqCst);
    let profile = std::env::temp_dir().join(format!(
        "agalma-web6-shot-{label}-{}-{seq}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&profile);
    std::fs::create_dir_all(&profile).expect("create browser profile dir");
    let mut command = Command::new(chrome);
    command.args([
        "--headless=new",
        "--disable-gpu",
        "--no-first-run",
        "--no-default-browser-check",
        "--disable-extensions",
        "--disable-background-networking",
        "--disable-sync",
        "--mute-audio",
        "--hide-scrollbars",
    ]);
    for arg in theme_args(theme) {
        command.arg(arg);
    }
    let output = command
        .arg(format!("--window-size={launch_width},{height}"))
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg(format!("--virtual-time-budget={budget_ms}"))
        .arg(format!("--screenshot={}", path.display()))
        .arg(url)
        .output()
        .expect("spawn installed Chrome for screenshot");
    if width < 500 && path.is_file() {
        // The iframe page lays the cockpit out at the exact CSS width; crop the
        // unused 500 px gutter so the artifact is exactly the requested width.
        let cropped = path.with_extension("cropped.png");
        let status = Command::new("sips")
            .args([
                "-c",
                &height.to_string(),
                &width.to_string(),
                "--cropOffset",
                "0",
                &format!("{}", (launch_width - width) / 2),
                &path.to_string_lossy().into_owned(),
                "--out",
                &cropped.to_string_lossy().into_owned(),
            ])
            .status();
        if matches!(status, Ok(value) if value.success()) {
            let _ = std::fs::rename(&cropped, path);
        } else {
            let _ = std::fs::remove_file(&cropped);
        }
    }
    BrowserOutcome {
        dom: String::new(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        status: output.status,
        profile,
    }
}

/// Run `url` in the real browser and return the driver's parsed JSON result.
///
/// Retries once with a fresh profile when headless Chrome serialized an empty
/// document (a launch flake, never a test assertion). A driver that ran and
/// wrote `{"harnessError": ...}` is returned as-is so the caller asserts it.
pub fn run_browser_result(
    chrome: &Path,
    url: &str,
    budget_ms: u64,
    label: &str,
) -> (BrowserOutcome, serde_json::Value) {
    run_browser_result_sized(chrome, url, budget_ms, label, 1280, 2000)
}

/// Sized variant used by responsive shell assertions.
pub fn run_browser_result_sized(
    chrome: &Path,
    url: &str,
    budget_ms: u64,
    label: &str,
    width: u32,
    height: u32,
) -> (BrowserOutcome, serde_json::Value) {
    run_browser_result_sized_theme(chrome, url, budget_ms, label, width, height, "")
}

/// Sized result run with an explicit emulated color scheme.
pub fn run_browser_result_sized_theme(
    chrome: &Path,
    url: &str,
    budget_ms: u64,
    label: &str,
    width: u32,
    height: u32,
    theme: &str,
) -> (BrowserOutcome, serde_json::Value) {
    let mut last_error = String::new();
    for attempt in 0..2 {
        let outcome = run_browser_sized_theme(
            chrome,
            url,
            budget_ms,
            &format!("{label}-{attempt}"),
            width,
            height,
            Some(theme),
        );
        match outcome.try_result_json() {
            Ok(value) => return (outcome, value),
            Err(error) => last_error = format!("{error} (attempt {attempt})"),
        }
    }
    panic!("browser run {label} produced no driver result: {last_error}");
}

/// Wrap driver JavaScript into a page that imports the production modules and
/// records its JSON result in `#harness-result`.
///
/// `body` is the body of an async arrow function returning the JSON-serializable
/// result. It runs after the production modules are imported, so it drives the
/// real renderer, scheduler and controls.
pub fn driver_page(body: &str) -> String {
    // A fixture may embed literal markup such as `</script>` (the injection
    // fixture does). Inside an inline `<script>` element the HTML parser would
    // close the element at that text, truncating the module. `\/` is a valid
    // JavaScript escape for `/`, so escaping every `</` keeps the module intact
    // while the string values (and `JSON.parse` of them) are unchanged.
    let body = body.replace("</", "<\\/");
    let body = body.as_str();
    format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>harness</title>
<link rel="stylesheet" href="/dashboard.css"></head>
<body><main id="app"></main><pre id="harness-result"></pre>
<script type="module">
import {{ startCockpit, Cockpit, boundedText, formatTimestamp, FIELD_TEXT_LIMIT, LIST_ROW_LIMIT, sourceFreshnessModel, runtimeModel, repositoryIdentity, summaryCards, currentWorkModel, costModel, componentPageModel, componentSeverity, deriveAttention, coverageGaps, timelineModel, formatComponentHash }} from '/app.js';
import {{ DashboardApi, validateOverview, validateComponents, validateFactory, validateControl, validateComponentStatus, ApiError, isExactU64, isAmount }} from '/api.js';
import {{ Refresher }} from '/refresh.js';
import {{ ControlPanel, RESUME_CONFIRMATION }} from '/controls.js';
import {{ timestampModel, componentStatusModel }} from '/ui.js';
import {{ parseHash, formatHash }} from '/router.js';
const setResult = (value) => {{ document.getElementById('harness-result').textContent = JSON.stringify(value); }};
try {{
  const result = await (async () => {{
{body}
  }})();
  setResult(result ?? {{ ok: true }});
}} catch (error) {{
  setResult({{ harnessError: String(error && error.stack ? error.stack : error) }});
}}
</script></body></html>
"#
    )
}
