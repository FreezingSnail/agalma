//! WEB.6 / UI.1c cockpit page acceptance (`agalma-rwn.6`, `agalma-82f.4`).
//!
//! Drives the **compiled** dashboard assets through the installed Chrome in
//! headless mode against a real loopback fixture server that serves independent
//! `fixtures/cockpit` JSON. The retained single-page cases plus the UI v2
//! multi-page shell cases:
//!
//! 1. each route renders its sections from the fixtures (overview, components,
//!    component detail, tasks, projects, system);
//! 2. an unknown hash falls back to overview with the visible notice;
//! 3. navigation between all routes generates zero POSTs;
//! 4. `aria-current` marks the active route;
//! 5. the component-detail descriptors and envelopes render as inert text;
//! 6. only a registry-source `missing` state renders the explicit not-registered
//!    state; report gaps on a known component render its binding/descriptors;
//! 7. the system page renders status facts and a malformed response yields a
//!    visible bounded error;
//! 8. page exit on a non-overview route stops timers and aborts the active read;
//! 9. component paging uses the opaque server cursor within the row bound.
//!
//! The retained critical cases (injection inertness, stale source, revision
//! conflict, permission-gated controls with the literal resume phrase and
//! EffectUnconfirmed lookup, page-exit timer/abort) stay in place. The
//! production first-launch smoke is the separate acceptance item in
//! `first_launch.rs`. The runner retains raw logs under `target/test-runs`.

mod browser_support;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use browser_support::{driver_page, resolve_chrome, FixtureServer, Response, Router};

const BUDGET_MS: u64 = 30000;

const OVERVIEW: &str = include_str!("../../../fixtures/cockpit/overview.json");
const COMPONENTS: &str = include_str!("../../../fixtures/cockpit/components-page.json");
const INJECTION: &str = include_str!("../../../fixtures/cockpit/injection-and-boundaries.json");
const FACTORY_RUNTIME: &str = include_str!("../../../fixtures/cockpit/factory-runtime.json");
const FACTORY_READONLY: &str = include_str!("../../../fixtures/cockpit/factory-readonly.json");
const CONTROL_RECEIPT: &str = include_str!("../../../fixtures/cockpit/control-receipt.json");
const CONTROL_UNCONFIRMED: &str = include_str!("../../../fixtures/cockpit/control-unconfirmed.json");
const COMPONENT_DETAIL: &str = include_str!("../../../fixtures/cockpit/component-detail.json");
const COMPONENT_VIEWS: &str = include_str!("../../../fixtures/cockpit/component-views.json");
const STATUS: &str = include_str!("../../../fixtures/cockpit/status.json");

// ---------------------------------------------------------------------------
// Scripted fixture server
// ---------------------------------------------------------------------------

/// Per-request scripted behavior. `Vec` responses are consumed in order; the
/// last response repeats once the list is exhausted.
#[derive(Default)]
struct Script {
    overview: Vec<Response>,
    overview_index: usize,
    components: Vec<Response>,
    components_index: usize,
    detail: Vec<Response>,
    detail_index: usize,
    status: Vec<Response>,
    status_index: usize,
    factory: Vec<Response>,
    factory_index: usize,
    lookup: Option<Response>,
    pause: Option<Response>,
    resume: Option<Response>,
    resume_lost: bool,
    posted_bodies: Vec<String>,
}

impl Script {
    fn next_overview(&mut self) -> Response {
        let response = self
            .overview
            .get(self.overview_index)
            .or_else(|| self.overview.last())
            .cloned()
            .unwrap_or_else(|| Response::unavailable("no overview scripted"));
        self.overview_index += 1;
        response
    }

    fn next_components(&mut self) -> Response {
        let response = self
            .components
            .get(self.components_index)
            .or_else(|| self.components.last())
            .cloned()
            .unwrap_or_else(|| Response::unavailable("no components scripted"));
        self.components_index += 1;
        response
    }

    fn next_factory(&mut self) -> Response {
        let response = self
            .factory
            .get(self.factory_index)
            .or_else(|| self.factory.last())
            .cloned()
            .unwrap_or_else(|| Response::json(404, "{\"error\":\"not_found\"}"));
        self.factory_index += 1;
        response
    }

    fn next_detail(&mut self) -> Response {
        let response = self
            .detail
            .get(self.detail_index)
            .or_else(|| self.detail.last())
            .cloned()
            .unwrap_or_else(|| Response::json(404, "{\"error\":\"not_found\"}"));
        self.detail_index += 1;
        response
    }

    fn next_status(&mut self) -> Response {
        let response = self
            .status
            .get(self.status_index)
            .or_else(|| self.status.last())
            .cloned()
            .unwrap_or_else(|| Response::json(404, "{\"error\":\"not_found\"}"));
        self.status_index += 1;
        response
    }
}

fn scripted_server(script: Script) -> (FixtureServer, Arc<Mutex<Script>>) {
    let state = Arc::new(Mutex::new(script));
    let router_state = state.clone();
    let router: Router = Arc::new(move |method, path, _query, body| {
        let mut script = router_state.lock().unwrap();
        match (method, path) {
            // A fresh page load restarts the scripted sequence, so a retried
            // browser run (only ever after an empty, side-effect-free dump)
            // replays the same fixture progression instead of advancing it.
            ("GET", "/harness.html") => {
                script.overview_index = 0;
                script.components_index = 0;
                script.factory_index = 0;
                script.detail_index = 0;
                script.status_index = 0;
                script.posted_bodies.clear();
                None
            }
            ("GET", "/api/v1/overview") => Some(script.next_overview()),
            ("GET", "/api/v1/components") => Some(script.next_components()),
            ("GET", "/api/v1/factory") => Some(script.next_factory()),
            ("GET", "/api/v1/status") => Some(script.next_status()),
            ("GET", path) if path.starts_with("/api/v1/components/") => Some(script.next_detail()),
            ("GET", path) if path.starts_with("/api/v1/factory/actions/") => {
                Some(script.lookup.clone().unwrap_or_else(|| Response::json(404, "{\"error\":\"not_found\"}")))
            }
            ("POST", "/api/v1/factory/pause") => {
                script.posted_bodies.push(body.to_string());
                Some(script.pause.clone().unwrap_or_else(|| Response::json(503, "{\"error\":\"busy\"}")))
            }
            ("POST", "/api/v1/factory/resume") => {
                script.posted_bodies.push(body.to_string());
                if script.resume_lost {
                    Some(Response::json(503, "{\"error\":\"busy\"}"))
                } else {
                    Some(script.resume.clone().unwrap_or_else(|| Response::json(503, "{\"error\":\"busy\"}")))
                }
            }
            _ => None,
        }
    });
    let server = FixtureServer::start(router).expect("start fixture server");
    (server, state)
}

fn ok_json(body: &str) -> Response {
    Response::json(200, body)
}

fn run_driver(server: &FixtureServer, body: &str, label: &str) -> serde_json::Value {
    run_driver_at(server, "", body, label)
}

/// Run one driver page at `/harness.html<hash>`. A route case deep-links with
/// its hash so the shell's initial dispatch sees the intended route.
fn run_driver_at(
    server: &FixtureServer,
    hash: &str,
    body: &str,
    label: &str,
) -> serde_json::Value {
    run_driver_at_size(server, hash, body, label, 1280, 2000)
}

/// Run one driver page at an explicit viewport. Layout assertions read real
/// computed geometry from the compiled stylesheet.
fn run_driver_at_size(
    server: &FixtureServer,
    hash: &str,
    body: &str,
    label: &str,
    width: u32,
    height: u32,
) -> serde_json::Value {
    server.register(
        "/harness.html",
        Response::text(200, "text/html; charset=utf-8", driver_page(body)),
    );
    // Chrome's new headless mode clamps the outer window to 500 CSS px. The
    // 390 px acceptance viewport is therefore a same-origin iframe of exactly
    // 390 CSS px, which still runs the real stylesheet and media queries; the
    // wrapper copies the driver's JSON result into the dumped parent document.
    let (path, launch_width) = if width < 500 {
        server.register(
            "/frame.html",
            Response::text(
                200,
                "text/html; charset=utf-8",
                viewport_frame_page(width, height, hash),
            ),
        );
        ("/frame.html", 500)
    } else {
        ("/harness.html", width)
    };
    let url = if width < 500 {
        format!("{}{path}", server.base_url())
    } else {
        format!("{}{path}{hash}", server.base_url())
    };
    let chrome = resolve_chrome().expect("installed Chrome (CHROME_BIN) is required");
    // An empty serialized result means the module never executed (Chrome dumped
    // before the module ran), so the fixture endpoints saw no request and a
    // fresh-profile retry is side-effect free. A driver that ran always writes
    // a JSON result or a harness error, which is never retried.
    let (outcome, result) = browser_support::run_browser_result_sized(
        &chrome,
        &url,
        BUDGET_MS,
        label,
        launch_width,
        height,
    );
    assert!(
        outcome.status.success(),
        "browser run {label} failed: status={:?} stderr={}",
        outcome.status,
        outcome.stderr
    );
    assert!(
        result.get("harnessError").is_none(),
        "driver {label} threw: {result}"
    );
    result
}

/// Run one driver page at an explicit viewport and budget.
fn run_driver_at_size_budget(
    server: &FixtureServer,
    hash: &str,
    body: &str,
    label: &str,
    width: u32,
    height: u32,
    budget_ms: u64,
) -> serde_json::Value {
    server.register(
        "/harness.html",
        Response::text(200, "text/html; charset=utf-8", driver_page(body)),
    );
    let url = format!("{}/harness.html{hash}", server.base_url());
    let chrome = resolve_chrome().expect("installed Chrome (CHROME_BIN) is required");
    let (outcome, result) = browser_support::run_browser_result_sized(
        &chrome,
        &url,
        budget_ms,
        label,
        width,
        height,
    );
    assert!(
        outcome.status.success(),
        "browser run {label} failed: status={:?} stderr={}",
        outcome.status,
        outcome.stderr
    );
    assert!(
        result.get("harnessError").is_none(),
        "driver {label} threw: {result}"
    );
    result
}

/// Run one driver page at an explicit viewport and emulated color scheme.
fn run_driver_at_size_theme(
    server: &FixtureServer,
    hash: &str,
    body: &str,
    label: &str,
    width: u32,
    height: u32,
    theme: &str,
) -> serde_json::Value {
    server.register(
        "/harness.html",
        Response::text(200, "text/html; charset=utf-8", driver_page(body)),
    );
    let (path, launch_width) = if width < 500 {
        server.register(
            "/frame.html",
            Response::text(
                200,
                "text/html; charset=utf-8",
                viewport_frame_page(width, height, hash),
            ),
        );
        ("/frame.html", 500)
    } else {
        ("/harness.html", width)
    };
    let url = if width < 500 {
        format!("{}{path}", server.base_url())
    } else {
        format!("{}{path}{hash}", server.base_url())
    };
    let chrome = resolve_chrome().expect("installed Chrome (CHROME_BIN) is required");
    let (outcome, result) = browser_support::run_browser_result_sized_theme(
        &chrome,
        &url,
        BUDGET_MS,
        label,
        launch_width,
        height,
        theme,
    );
    assert!(
        outcome.status.success(),
        "browser run {label} failed: status={:?} stderr={}",
        outcome.status,
        outcome.stderr
    );
    assert!(
        result.get("harnessError").is_none(),
        "driver {label} threw: {result}"
    );
    result
}

/// A screenshot-only page: the production shell without the harness result
/// element, so the captured image shows the operator surface only.
fn screenshot_page() -> String {
    r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<title>Agalma cockpit</title>
<link rel="stylesheet" href="/dashboard.css"></head>
<body><main id="app"><noscript>This local cockpit requires JavaScript.</noscript></main>
<script type="module">
import { startCockpit } from '/app.js';
const mode = new URLSearchParams(location.search).get('mode') ?? 'populated';
const cockpit = startCockpit({ document, root: document.getElementById('app'), intervalMs: 60000 });
await cockpit.refresher.activeCycle;
if (mode === 'stale') await cockpit.refresh();
</script></body></html>
"#
    .to_string()
}

fn screenshot_frame_page(width: u32, height: u32, query: &str) -> String {
    let offset = 500u32.saturating_sub(width) / 2;
    format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>viewport</title>
<style>html,body{{margin:0;border:0;background:#000}} iframe{{display:block;border:0;margin-left:{offset}px;width:{width}px;height:{height}px}}</style>
</head><body><iframe src="/screenshot.html{query}"></iframe></body></html>
"#
    )
}

/// Parent page that gives the driver an exact CSS-pixel iframe viewport and
/// copies its JSON result so `--dump-dom` can observe it.
fn viewport_frame_page(width: u32, height: u32, hash: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>viewport</title>
<style>html,body{{margin:0;border:0}} iframe{{display:block;border:0;width:{width}px;height:{height}px}}</style>
</head><body>
<iframe id="frame" src="/harness.html{hash}"></iframe>
<pre id="harness-result"></pre>
<script>
const frame = document.getElementById('frame');
const result = document.getElementById('harness-result');
const pump = () => {{
  try {{
    const driver = frame.contentDocument && frame.contentDocument.getElementById('harness-result');
    const text = driver ? driver.textContent : '';
    if (text.trim() !== '') {{ result.textContent = text; return; }}
  }} catch (error) {{
    result.textContent = JSON.stringify({{ harnessError: String(error) }});
    return;
  }}
  setTimeout(pump, 10);
}};
frame.addEventListener('load', pump);
pump();
</script>
</body></html>
"#
    )
}

// ---------------------------------------------------------------------------
// Concrete cases
// ---------------------------------------------------------------------------

#[test]
fn cockpit_page_renders_real_fixtures_with_bounded_safe_text() {
    let (server, _state) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW)],
        components: vec![ok_json(COMPONENTS)],
        ..Script::default()
    });
    let result = run_driver(
        &server,
        r#"
    const api = new DashboardApi();
    const cockpit = new Cockpit({ document, root: document.getElementById('app'), api, intervalMs: 1000, randomSource: crypto });
    await cockpit.refresh();
    await cockpit.refresh();
    const text = (id) => document.getElementById(id).textContent;
    return {
      runtime: text('runtime'),
      executions: text('executions'),
      components: text('components'),
      economics: text('economics'),
      timeline: text('timeline'),
      sources: text('sources'),
      scriptCount: document.querySelectorAll('script').length,
      imgCount: document.querySelectorAll('img').length,
      rowKeys: [...cockpit.state.components.rows.keys()],
    };
    "#,
        "render",
    );

    let runtime = result["runtime"].as_str().unwrap();
    let executions = result["executions"].as_str().unwrap();
    let components = result["components"].as_str().unwrap();
    let economics = result["economics"].as_str().unwrap();
    let timeline = result["timeline"].as_str().unwrap();

    // No factory route is scripted, so runtime must be an explicit unknown or
    // unavailable, never Idle/Running.
    assert!(
        runtime.contains("Unknown") || runtime.contains("Unavailable"),
        "runtime={runtime}"
    );
    assert!(!runtime.contains("Idle") && !runtime.contains("Running"), "runtime={runtime}");

    assert!(executions.contains("exec:cabinets-are-not-shelves:1"), "executions={executions}");
    assert!(executions.contains("18446744073709551615"), "exact generation lost: {executions}");

    assert!(components.contains("example.unfamiliar-reporter"), "components={components}");
    assert!(components.contains("9007199254740993"), "exact component generation lost: {components}");
    assert!(components.contains("example.stopped-writer"), "components={components}");

    // Unknown cost is explicit, never zero.
    assert!(economics.contains("Cost: unknown"), "economics={economics}");
    assert!(!economics.contains("Cost: 0 "), "economics={economics}");

    assert!(timeline.contains("Factory stopped"), "timeline={timeline}");

    // Exact row keys carry the component identity and generation.
    let keys: Vec<String> = result["rowKeys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_string())
        .collect();
    assert!(keys.contains(&"example.unfamiliar-reporter@9007199254740993".to_string()), "keys={keys:?}");

    // Viewing generated zero POSTs.
    assert_eq!(server.post_count(), 0, "reads must not POST: {:?}", server.requests());
    assert!(server.get_count() >= 4, "expected overview/components GETs");
}

#[test]
fn cockpit_page_keeps_injected_text_inert_and_exact() {
    let (server, _state) = scripted_server(Script {
        overview: vec![ok_json(INJECTION)],
        components: vec![ok_json(COMPONENTS)],
        ..Script::default()
    });
    let result = run_driver(
        &server,
        r#"
    const cockpit = new Cockpit({ document, root: document.getElementById('app'), api: new DashboardApi(), intervalMs: 1000 });
    await cockpit.refresh();
    const timeline = document.getElementById('timeline').textContent;
    const economics = document.getElementById('economics').textContent;
    return {
      timeline,
      economics,
      scriptCount: document.querySelectorAll('script').length,
      imgCount: document.querySelectorAll('img').length,
      onerrorCount: document.querySelectorAll('[onerror]').length,
      statusEl: document.getElementById('app').getAttribute('data-status') ?? null,
    };
    "#,
        "injection",
    );
    let timeline = result["timeline"].as_str().unwrap();
    let economics = result["economics"].as_str().unwrap();
    // The injected markup is present only as text, never as an element.
    assert!(timeline.contains("<script>alert('injected')</script>"), "timeline={timeline}");
    assert!(timeline.contains("<img src=x onerror=alert(1)>"), "timeline={timeline}");
    assert_eq!(result["scriptCount"].as_u64().unwrap(), 1, "injected script must not become an element");
    assert_eq!(result["imgCount"].as_u64().unwrap(), 0, "injected img must not become an element");
    assert_eq!(result["onerrorCount"].as_u64().unwrap(), 0, "injected handler must not become an attribute");
    // >2^53 and exact decimal values survive.
    assert!(economics.contains("9007199254740993 tokens"), "economics={economics}");
    assert!(economics.contains("0.000000000000000001 usd"), "economics={economics}");
}

#[test]
fn cockpit_page_retains_stale_data_and_reloads_on_conflict() {
    let conflict = Response::json(409, "{\"error\":\"cursor_conflict\"}");
    let (server, _state) = scripted_server(Script {
        // First cycle succeeds; the second overview read fails (source lost).
        overview: vec![ok_json(OVERVIEW), Response::unavailable("source lost")],
        // First components read conflicts on a stale cursor, then resets.
        components: vec![conflict, ok_json(COMPONENTS), ok_json(COMPONENTS)],
        ..Script::default()
    });
    let result = run_driver(
        &server,
        r#"
    const cockpit = new Cockpit({ document, root: document.getElementById('app'), api: new DashboardApi(), intervalMs: 1000 });
    cockpit.state.components.cursor = 'cp1:stale';
    await cockpit.refresh();               // conflict -> reset -> succeeds
    const firstComponents = document.getElementById('components').textContent;
    const reloadNote = cockpit.state.components.reloadNote;
    await cockpit.refresh();               // overview source fails
    const snapshot = cockpit.snapshot();
    return {
      firstComponents,
      hadReloadNote: reloadNote !== null,
      staleOverview: snapshot.sources.overview.data !== null && snapshot.sources.overview.error !== null,
      executionsStillShown: document.getElementById('executions').textContent,
      sourcesPanel: document.getElementById('sources').textContent,
      staleNotice: document.getElementById('components').textContent,
    };
    "#,
        "stale",
    );
    assert!(result["hadReloadNote"].as_bool().unwrap(), "conflict must reset the collection visibly");
    assert!(result["firstComponents"].as_str().unwrap().contains("example.unfamiliar-reporter"));
    assert!(result["staleOverview"].as_bool().unwrap(), "failed source must be stale, not cleared");
    assert!(
        result["executionsStillShown"].as_str().unwrap().contains("exec:cabinets-are-not-shelves:1"),
        "last successful overview data must remain visible"
    );
    let sources = result["sourcesPanel"].as_str().unwrap();
    assert!(sources.contains("unavailable") || sources.contains("failed"), "sources={sources}");
}

#[test]
fn cockpit_page_controls_permissions_and_receipt_lookup() {
    let (server, state) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW)],
        components: vec![ok_json(COMPONENTS)],
        // The first refresh observes the runtime permission (typed pause/resume)
        // and the second observes a read-only runtime that no longer advertises
        // pause, proving the rendered controls are permission-gated.
        factory: vec![ok_json(FACTORY_RUNTIME), ok_json(FACTORY_READONLY)],
        pause: Some(ok_json(CONTROL_RECEIPT)),
        resume_lost: true,
        lookup: Some(ok_json(CONTROL_UNCONFIRMED)),
        ..Script::default()
    });
    let result = run_driver(
        &server,
        r#"
    // Click the rendered buttons, not the control object. The injected phrase
    // provider is the operator confirmation surface; a wrong phrase must refuse.
    let phrase = 'wrong';
    const realApi = new DashboardApi();
    let releasePause;
    const pauseGate = new Promise((resolve) => { releasePause = resolve; });
    let pauseCalls = 0;
    const api = {
      getOverview: (options) => realApi.getOverview(options),
      getComponents: (options) => realApi.getComponents(options),
      getFactory: (options) => realApi.getFactory(options),
      lookupAction: (id, options) => realApi.lookupAction(id, options),
      resume: (id, options) => realApi.resume(id, options),
      // Gate one pause so the pending window is observable while in flight.
      pause: (id, options) => { pauseCalls += 1; return pauseGate.then(() => realApi.pause(id, options)); },
    };
    const cockpit = new Cockpit({
      document, root: document.getElementById('app'), api,
      intervalMs: 1000, randomSource: crypto, confirmPhrase: () => phrase,
    });
    await cockpit.refresh();
    const pauseButton = () => document.querySelector('[data-action="pause"]');
    const resumeButton = () => document.querySelector('[data-action="resume"]');
    const lastResult = () => cockpit.controls.state().lastResult;
    const waitForResult = async (previous) => {
      for (let i = 0; i < 1000; i += 1) {
        if (!cockpit.controls.state().pending && lastResult() !== previous) return lastResult();
        await new Promise((resolve) => setTimeout(resolve, 5));
      }
      return lastResult();
    };
    const enabledPause = !pauseButton().disabled;
    const enabledResume = !resumeButton().disabled;

    // A wrong confirmation phrase must refuse the rendered Resume click: no
    // pending action starts and no mutation is submitted for that click.
    const beforeRefusal = lastResult();
    resumeButton().click();
    await new Promise((resolve) => setTimeout(resolve, 30));
    const wrongConfirmationNoAction =
      cockpit.controls.state().pending === false && lastResult() === beforeRefusal;

    // A permitted rendered Pause click. The gated reply keeps the pending window
    // open so duplicate protection is observable.
    phrase = 'resume';
    const beforePause = lastResult();
    pauseButton().click();
    const pendingAfterPause = cockpit.controls.state().pending;
    const pauseDisabledWhilePending = pauseButton().disabled;
    const pauseCallsAfterFirst = pauseCalls;
    pauseButton().click(); // the re-rendered button is disabled: no second click
    let duplicateRejected = false;
    try { await cockpit.controls.pause(); } catch { duplicateRejected = true; }
    const duplicateNoSecondCall = pauseCalls === pauseCallsAfterFirst;
    releasePause();
    const pauseResult = await waitForResult(beforePause);
    const pausePanel = document.getElementById('controls').textContent;

    // A permitted rendered Resume click whose response is lost: the id is
    // retained, one lookup is queried and the effect is shown unconfirmed.
    const beforeResume = lastResult();
    resumeButton().click();
    const resumeResult = await waitForResult(beforeResume);
    const resumePanel = document.getElementById('controls').textContent;

    // A read-only runtime observation must re-gate the rendered controls: the
    // no-longer-advertised pause disables while the still-advertised resume
    // stays enabled. The second factory read serves the read-only fixture.
    await cockpit.refresh();
    const gatedPauseDisabled = pauseButton().disabled;
    const gatedResumeDisabled = resumeButton().disabled;

    return {
      enabledPause,
      enabledResume,
      wrongConfirmationNoAction,
      pendingAfterPause,
      pauseDisabledWhilePending,
      duplicateRejected,
      duplicateNoSecondCall,
      pauseKind: pauseResult.kind,
      pauseOutcome: pauseResult.outcome ?? null,
      pauseId: pauseResult.actionId,
      pausePanel,
      resumeKind: resumeResult.kind,
      resumeId: resumeResult.actionId,
      resumePanel,
      gatedPauseDisabled,
      gatedResumeDisabled,
    };
    "#,
        "controls",
    );
    assert!(result["enabledPause"].as_bool().unwrap());
    assert!(result["enabledResume"].as_bool().unwrap());
    // Permission gating: a read-only runtime disables the unadvertised pause and
    // keeps the advertised resume enabled.
    assert!(result["gatedPauseDisabled"].as_bool().unwrap(), "a read-only runtime must disable pause");
    assert!(
        !result["gatedResumeDisabled"].as_bool().unwrap(),
        "an advertised resume must stay enabled"
    );
    assert!(
        result["wrongConfirmationNoAction"].as_bool().unwrap(),
        "a wrong confirmation phrase must refuse the rendered resume click with no mutation"
    );
    // Duplicate protection: the rendered button is disabled while pending and a
    // second attempt starts no second action.
    assert!(result["pendingAfterPause"].as_bool().unwrap(), "the pause click must enter a pending state");
    assert!(result["pauseDisabledWhilePending"].as_bool().unwrap(), "the button must disable while pending");
    assert!(result["duplicateRejected"].as_bool().unwrap(), "a duplicate pause must be refused");
    assert!(result["duplicateNoSecondCall"].as_bool().unwrap(), "a duplicate must start no second action");

    assert_eq!(result["pauseKind"].as_str().unwrap(), "receipt");
    assert_eq!(result["pauseOutcome"].as_str().unwrap(), "applied");
    assert_eq!(result["resumeKind"].as_str().unwrap(), "unconfirmed");
    let pause_id = result["pauseId"].as_str().unwrap();
    let resume_id = result["resumeId"].as_str().unwrap();
    assert!(!pause_id.is_empty() && !resume_id.is_empty(), "each click retains an action id");
    assert_ne!(pause_id, resume_id, "each click generates its own action id");

    // Visible output: the pause receipt outcome and the unconfirmed effect with
    // its retained id.
    let pause_panel = result["pausePanel"].as_str().unwrap();
    assert!(pause_panel.contains("applied"), "pause panel must show the receipt outcome: {pause_panel}");
    assert!(pause_panel.contains(pause_id), "pause panel must show its action id: {pause_panel}");
    let resume_panel = result["resumePanel"].as_str().unwrap();
    assert!(resume_panel.contains("EffectUnconfirmed"), "panel={resume_panel}");
    assert!(resume_panel.contains(resume_id), "resume panel must show its retained id: {resume_panel}");

    // Exactly two POSTs: one pause and one resume. The refused resume and the
    // duplicate pause produced none.
    assert_eq!(server.post_count(), 2, "posts={:?}", state.lock().unwrap().posted_bodies);
    let bodies = state.lock().unwrap().posted_bodies.clone();
    assert!(bodies[0].contains("\"operation\":\"pause\""), "{bodies:?}");
    assert!(bodies[0].contains(&format!("\"action_id\":\"{pause_id}\"")), "{bodies:?}");
    assert!(bodies[1].contains("\"operation\":\"resume\""), "{bodies:?}");
    assert!(bodies[1].contains("\"confirmation\":\"resume\""), "{bodies:?}");
    assert!(bodies[1].contains(&format!("\"action_id\":\"{resume_id}\"")), "{bodies:?}");
    // The lookup queried the retained resume action id, once, and never the
    // successful pause id.
    let lookups: Vec<String> = server
        .requests()
        .iter()
        .filter(|r| r.path.starts_with("/api/v1/factory/actions/"))
        .map(|r| r.path.clone())
        .collect();
    assert!(lookups.iter().any(|p| p.ends_with(resume_id)), "lookups={lookups:?}");
    assert!(!lookups.iter().any(|p| p.ends_with(pause_id)), "a completed pause needs no lookup: {lookups:?}");
}

/// Build a one-entry component page with a chosen identity, generation and
/// optional opaque next cursor. The entry is an unfamiliar built-in fixture, not
/// the component under test.
fn component_page(plugin_id: &str, name: &str, generation: &str, next_cursor: Option<&str>) -> String {
    let exact: u64 = generation.parse().expect("decimal generation");
    let cursor = match next_cursor {
        Some(cursor) => serde_json::Value::String(cursor.to_string()),
        None => serde_json::Value::Null,
    };
    serde_json::json!({
        "metadata": {
            "format": "ControlPlaneRead/v1",
            "collected_at_unix_ms": 1791820800001i64,
            "registry_revision": "9007199254740993",
            "report_watermark": null,
            "sources": [
                { "source": "registry", "state": "available", "detail": "", "observed_at_unix_ms": 1791820800001i64 }
            ]
        },
        "entries": [{
            "plugin_id": plugin_id,
            "name": name,
            "binding": {
                "binding_id": "fixture.gen",
                "api": "plugin",
                "api_version": 1,
                "implementation_version": "fixture-gen",
                "generation": exact,
                "config_digest": "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
            },
            "generation": generation,
            "health": "ready",
            "freshness": "fresh",
            "completeness": "Complete",
            "available": true,
            "views": []
        }],
        "next_cursor": cursor
    })
    .to_string()
}

#[test]
fn cockpit_page_pages_components_with_opaque_cursor_and_bounds() {
    let page1 = component_page("example.gen-a", "Gen A", "1", Some("cp1:opaque-1"));
    let page2 = component_page("example.gen-b", "Gen B", "2", None);
    let (server, _state) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW)],
        components: vec![ok_json(&page1), ok_json(&page2)],
        ..Script::default()
    });
    let result = run_driver(
        &server,
        r#"
    const cockpit = new Cockpit({ document, root: document.getElementById('app'), api: new DashboardApi(), intervalMs: 1000 });
    await cockpit.refresh();
    const nextButton = document.getElementById('next-components');
    const cursor = nextButton.dataset.nextCursor;
    const disabledBefore = nextButton.disabled;
    await cockpit.nextComponentPage();
    const after = document.getElementById('components').textContent;
    const disabledAfter = document.getElementById('next-components').disabled;
    const rowCount = document.querySelectorAll('#components table tr').length - 1;
    return { cursor, disabledBefore, after, disabledAfter, rowCount };
    "#,
        "paging",
    );
    assert_eq!(result["cursor"].as_str().unwrap(), "cp1:opaque-1");
    assert_eq!(result["disabledBefore"].as_bool().unwrap(), false, "Next is enabled with a cursor");
    assert!(result["after"].as_str().unwrap().contains("example.gen-b"), "{}", result["after"]);
    assert_eq!(result["disabledAfter"].as_bool().unwrap(), true, "Next is disabled on the last page");
    assert!(result["rowCount"].as_u64().unwrap() <= 100, "rows must stay bounded to 100");

    // Every component GET names the default 50 limit, and the Next click carried
    // the opaque server cursor unchanged.
    let components: Vec<_> = server
        .requests()
        .into_iter()
        .filter(|r| r.path == "/api/v1/components")
        .collect();
    assert!(components.iter().all(|r| r.query.contains("limit=50")), "{components:?}");
    assert!(
        components.iter().any(|r| r.query.contains("cursor=cp1%3Aopaque-1")),
        "opaque cursor must reach the server percent-encoded once: {components:?}"
    );
    assert_eq!(server.post_count(), 0, "paging must not POST");
}

#[test]
fn cockpit_page_start_cockpit_stops_timers_and_aborts_on_pagehide() {
    let (server, _state) = scripted_server(Script::default());
    let body = format!(
        r#"
    let mode = 'ok';
    let aborted = false;
    let capturedSignal = null;
    const overview = JSON.parse({overview});
    const components = JSON.parse({components});
    const api = {{
      getOverview: ({{ signal }} = {{}}) => {{
        if (mode === 'hang') {{
          capturedSignal = signal;
          return new Promise((resolve, reject) => {{
            signal.addEventListener('abort', () => {{
              aborted = true;
              reject(new ApiError('aborted', 'aborted by page exit'));
            }});
          }});
        }}
        return Promise.resolve(validateOverview(overview));
      }},
      getComponents: ({{ signal }} = {{}}) => Promise.resolve(validateComponents(components)),
      getFactory: ({{ signal }} = {{}}) => Promise.reject(new ApiError('unsupported', 'endpoint absent')),
    }};
    // A 60s interval sits well above Chrome's 8s virtual-time budget, so no
    // periodic refresh can fire between the phases of this case.
    const cockpit = startCockpit({{ document, root: document.getElementById('app'), api, intervalMs: 60000 }});
    await cockpit.refresher.activeCycle;
    const timerScheduled = cockpit.refresher.pendingTimer !== null;
    mode = 'hang';
    const hanging = cockpit.refresh();
    // Wait until the hanging read has actually started and captured its abort
    // controller before dispatching pagehide, so exit provably aborts an
    // in-flight request rather than racing its start.
    let guard = 0;
    while ((capturedSignal === null || cockpit.refresher.controller === null) && guard < 1000) {{
      await new Promise((resolve) => setTimeout(resolve, 5));
      guard += 1;
    }}
    const activeController = cockpit.refresher.controller;
    const capturedBeforeExit = capturedSignal !== null && activeController !== null;
    window.dispatchEvent(new Event('pagehide'));
    return {{
      running: cockpit.refresher.running,
      pendingTimer: cockpit.refresher.pendingTimer,
      controller: cockpit.refresher.controller,
      capturedBeforeExit,
      controllerAborted: activeController ? activeController.signal.aborted : null,
      savedSignalAborted: capturedSignal ? capturedSignal.aborted : null,
      aborted,
      timerScheduled,
    }};
    "#,
        overview = serde_json::to_string(OVERVIEW).unwrap(),
        components = serde_json::to_string(COMPONENTS).unwrap(),
    );
    let result = run_driver(&server, &body, "pagehide");
    assert!(result["timerScheduled"].as_bool().unwrap(), "a refresh timer must have been scheduled before exit");
    assert!(
        result["capturedBeforeExit"].as_bool().unwrap(),
        "the hanging read must be in flight before pagehide"
    );
    assert_eq!(result["running"].as_bool(), Some(false), "pagehide must stop the refresher");
    assert_eq!(result["pendingTimer"], serde_json::Value::Null, "pagehide must clear the pending timer");
    assert_eq!(result["controller"], serde_json::Value::Null, "pagehide must clear the active controller");
    assert_eq!(
        result["controllerAborted"].as_bool(),
        Some(true),
        "the active controller must be aborted"
    );
    assert_eq!(
        result["savedSignalAborted"].as_bool(),
        Some(true),
        "the saved abort signal must be aborted"
    );
    assert_eq!(
        result["aborted"].as_bool(),
        Some(true),
        "an in-flight request's abort signal must fire"
    );
}

// ---------------------------------------------------------------------------
// UI v2 multi-page shell cases
// ---------------------------------------------------------------------------

fn ok_value(value: &serde_json::Value) -> Response {
    Response::json(200, value.to_string())
}

fn sha256_placeholder() -> &'static str {
    "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
}

/// A component-detail projection whose descriptor titles and envelope payload
/// carry hostile markup, so the detail page's inert rendering is observable.
fn injected_component_detail() -> serde_json::Value {
    serde_json::json!({
        "metadata": {
            "format": "ControlPlaneRead/v1",
            "collected_at_unix_ms": 1791820800003i64,
            "registry_revision": "9007199254740993",
            "report_watermark": "1",
            "sources": [
                { "source": "registry", "state": "available", "detail": "", "observed_at_unix_ms": 1791820800003i64 }
            ]
        },
        "binding": {
            "binding_id": "fixture.injected",
            "api": "plugin",
            "api_version": 1,
            "implementation_version": "fixture-injected",
            "generation": "9007199254740993",
            "config_digest": sha256_placeholder()
        },
        "descriptors": [{
            "view_id": "status",
            "schema_namespace": "example.injected.status",
            "kind": "Status",
            "title": "<script>alert('descriptor')</script>",
            "schema_version": 1,
            "fields": ["<img src=x onerror=alert(1)>"]
        }],
        "entries": [{
            "format": "ReportEnvelope/v1",
            "plugin_id": "example.unfamiliar-reporter",
            "binding": {
                "binding_id": "fixture.injected",
                "api": "plugin",
                "api_version": 1,
                "implementation_version": "fixture-injected",
                "generation": "9007199254740993",
                "config_digest": sha256_placeholder()
            },
            "schema_namespace": "example.injected.status",
            "schema_version": 1,
            "sequence": "1",
            "reported_at_unix_ms": "1791820800000",
            "observed_at_unix_ms": "1791820800001",
            "execution_id": null,
            "durability": "durable",
            "payload": {
                "note": "<script>alert('payload')</script>",
                "html": "<img src=x onerror=alert(1)>"
            },
            "payload_digest": "sha256:b342fc286d0216cc212e0d7ba234894e2e7283ddf14f959adf0fe7fd5924308a"
        }],
        "next_cursor": null
    })
}

/// The honest projection for a valid but unregistered plugin id: a synthetic
/// binding pin with no descriptors and explicit `missing` sources.
fn unknown_component_detail(plugin_id: &str) -> serde_json::Value {
    serde_json::json!({
        "metadata": {
            "format": "ControlPlaneRead/v1",
            "collected_at_unix_ms": 1791820800004i64,
            "registry_revision": "9007199254740993",
            "report_watermark": null,
            "sources": [
                {
                    "source": "registry",
                    "state": "missing",
                    "detail": format!("plugin `{plugin_id}` is not registered"),
                    "observed_at_unix_ms": 1791820800004i64
                },
                {
                    "source": "reports",
                    "state": "missing",
                    "detail": format!("plugin `{plugin_id}` has no report projection"),
                    "observed_at_unix_ms": 1791820800004i64
                }
            ]
        },
        "binding": {
            "binding_id": plugin_id,
            "api": "plugin",
            "api_version": 1,
            "implementation_version": "unknown",
            "generation": 0,
            "config_digest": sha256_placeholder()
        },
        "descriptors": [],
        "entries": [],
        "next_cursor": null
    })
}

/// Build one detail projection with independent registry and report source
/// states. Registration is the registry source's fact; reports are separate.
fn component_detail_with_sources(
    plugin_id: &str,
    registry_state: &str,
    reports_state: &str,
) -> serde_json::Value {
    let registry_present = registry_state != "missing";
    let descriptors = if registry_present {
        serde_json::json!([{
            "view_id": "status",
            "schema_namespace": format!("{plugin_id}.status"),
            "kind": "Status",
            "title": "State fixture",
            "schema_version": 1,
            "fields": ["ready"]
        }])
    } else {
        serde_json::json!([])
    };
    let entries = if reports_state == "available" {
        serde_json::json!([{
            "format": "ReportEnvelope/v1",
            "plugin_id": plugin_id,
            "binding": {
                "binding_id": plugin_id,
                "api": "plugin",
                "api_version": 1,
                "implementation_version": "fixture-state",
                "generation": "7",
                "config_digest": sha256_placeholder()
            },
            "schema_namespace": format!("{plugin_id}.status"),
            "schema_version": 1,
            "sequence": "1",
            "reported_at_unix_ms": "1791820800000",
            "observed_at_unix_ms": "1791820800001",
            "execution_id": null,
            "durability": "durable",
            "payload": {"ready": true},
            "payload_digest": "sha256:b342fc286d0216cc212e0d7ba234894e2e7283ddf14f959adf0fe7fd5924308a"
        }])
    } else {
        serde_json::json!([])
    };
    serde_json::json!({
        "metadata": {
            "format": "ControlPlaneRead/v1",
            "collected_at_unix_ms": 1791820800005i64,
            "registry_revision": "9007199254740993",
            "report_watermark": null,
            "sources": [
                {
                    "source": "registry",
                    "state": registry_state,
                    "detail": format!("registry fixture state {registry_state}"),
                    "observed_at_unix_ms": 1791820800005i64
                },
                {
                    "source": "reports",
                    "state": reports_state,
                    "detail": format!("reports fixture state {reports_state}"),
                    "observed_at_unix_ms": 1791820800005i64
                }
            ]
        },
        "binding": {
            "binding_id": plugin_id,
            "api": "plugin",
            "api_version": 1,
            "implementation_version": "fixture-state",
            "generation": "7",
            "config_digest": sha256_placeholder()
        },
        "descriptors": descriptors,
        "entries": entries,
        "next_cursor": null
    })
}

/// The shared driver prelude for route navigation through the real router.
const NAVIGATE_PRELUDE: &str = r#"
    const cockpit = startCockpit({
      document, root: document.getElementById('app'), api: new DashboardApi(), intervalMs: 60000,
    });
    await cockpit.refresher.activeCycle;
    const navigate = async (hash) => {
      if (location.hash === hash) return;
      const changed = new Promise((resolve) => window.addEventListener('hashchange', resolve, { once: true }));
      location.hash = hash;
      await changed;
      if (cockpit.refresher.activeCycle) await cockpit.refresher.activeCycle;
    };
    const visiblePages = () => [...document.querySelectorAll('[data-page]')]
      .filter((node) => !node.hidden).map((node) => node.dataset.page);
    const pageText = (name) => document.querySelector(`[data-page="${name}"]`).textContent;
"#;

#[test]
fn cockpit_page_renders_each_route_from_the_fixtures() {
    let (server, _state) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW)],
        components: vec![ok_json(COMPONENTS)],
        factory: vec![ok_json(FACTORY_RUNTIME)],
        detail: vec![ok_json(COMPONENT_DETAIL)],
        status: vec![ok_json(STATUS)],
        ..Script::default()
    });
    let body = format!(
        r#"
    {prelude}
    const pages = {{}};
    const initialVisible = visiblePages();
    pages.overview = pageText('overview');
    await navigate('#/components');
    const componentsVisible = visiblePages();
    pages.components = pageText('components');
    await navigate('#/components/example.unfamiliar-reporter');
    const detailVisible = visiblePages();
    pages.detail = pageText('component');
    await navigate('#/tasks');
    const tasksVisible = visiblePages();
    pages.tasks = pageText('tasks');
    await navigate('#/projects');
    const projectsVisible = visiblePages();
    pages.projects = pageText('projects');
    await navigate('#/system');
    const systemVisible = visiblePages();
    pages.system = pageText('system');
    return {{ pages, initialVisible, componentsVisible, detailVisible, tasksVisible, projectsVisible, systemVisible }};
    "#,
        prelude = NAVIGATE_PRELUDE,
    );
    let result = run_driver(&server, &body, "routes");

    assert_eq!(result["initialVisible"], serde_json::json!(["overview"]));
    assert_eq!(result["componentsVisible"], serde_json::json!(["components"]));
    assert_eq!(result["detailVisible"], serde_json::json!(["component"]));
    assert_eq!(result["tasksVisible"], serde_json::json!(["tasks"]));
    assert_eq!(result["projectsVisible"], serde_json::json!(["projects"]));
    assert_eq!(result["systemVisible"], serde_json::json!(["system"]));

    let text = |route: &str| result["pages"][route].as_str().unwrap();

    let overview = text("overview");
    assert!(overview.contains("Factory / worker status"), "overview={overview}");
    assert!(overview.contains("Runtime: Running"), "overview={overview}");
    assert!(overview.contains("Cost: unknown"), "overview={overview}");
    assert!(overview.contains("Factory stopped"), "overview={overview}");

    let components = text("components");
    assert!(components.contains("example.unfamiliar-reporter"), "components={components}");
    assert!(components.contains("9007199254740993"), "exact generation lost: {components}");
    assert!(components.contains("example.stopped-writer"), "components={components}");

    let detail = text("detail");
    assert!(detail.contains("fixture.reporter"), "detail binding pin missing: {detail}");
    assert!(detail.contains("Declared views"), "detail descriptors missing: {detail}");
    assert!(detail.contains("example.reporter.status"), "detail descriptor namespace missing: {detail}");
    assert!(detail.contains("Reports"), "detail envelopes missing: {detail}");
    assert!(detail.contains(r#"{"ready":true}"#), "detail payload missing: {detail}");

    let tasks = text("tasks");
    assert!(tasks.contains("Alpha task"), "tasks queue missing: {tasks}");
    assert!(tasks.contains("Showing 2 of 2. Revision rev-7."), "tasks totals missing: {tasks}");
    assert!(tasks.contains("exec:cabinets-are-not-shelves:1"), "tasks executions missing: {tasks}");

    let projects = text("projects");
    assert!(projects.contains("example-repo"), "projects repository missing: {projects}");
    assert!(projects.contains("example-state"), "projects state missing: {projects}");
    assert!(projects.contains("2+ observed in this page"), "projects component count missing: {projects}");

    let system = text("system");
    assert!(system.contains("Dashboard health"), "system health missing: {system}");
    assert!(system.contains("0.1.0"), "system version missing: {system}");
    assert!(system.contains("http://127.0.0.1:8791"), "system listener missing: {system}");

    assert_eq!(server.post_count(), 0, "route rendering and navigation must not POST");
}

#[test]
fn cockpit_page_unknown_hash_falls_back_to_overview_with_notice() {
    let (server, _state) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW)],
        components: vec![ok_json(COMPONENTS)],
        ..Script::default()
    });
    let result = run_driver_at(
        &server,
        "#/not-a-route",
        r#"
    const cockpit = startCockpit({ document, root: document.getElementById('app'), api: new DashboardApi(), intervalMs: 60000 });
    await cockpit.refresher.activeCycle;
    const notice = document.getElementById('route-notice');
    const visible = [...document.querySelectorAll('[data-page]')].filter((node) => !node.hidden).map((node) => node.dataset.page);
    const current = [...document.querySelectorAll('nav a[aria-current]')].map((node) => node.getAttribute('href'));
    return {
      noticeHidden: notice.hidden,
      noticeText: notice.textContent,
      route: cockpit.state.route.name,
      routeValid: cockpit.state.route.valid,
      visible,
      current,
    };
    "#,
        "unknown-hash",
    );
    assert_eq!(result["route"].as_str().unwrap(), "overview");
    assert_eq!(result["routeValid"].as_bool(), Some(false), "an unknown hash is not a valid route");
    assert_eq!(result["noticeHidden"].as_bool(), Some(false), "the fallback notice must be visible");
    assert_eq!(result["noticeText"].as_str().unwrap(), "Unknown route; showing Overview.");
    assert_eq!(result["visible"], serde_json::json!(["overview"]));
    assert_eq!(result["current"], serde_json::json!(["#/overview"]));
}

#[test]
fn cockpit_page_navigation_between_all_routes_generates_zero_posts() {
    let (server, _state) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW)],
        components: vec![ok_json(COMPONENTS)],
        factory: vec![ok_json(FACTORY_RUNTIME)],
        detail: vec![ok_json(COMPONENT_DETAIL)],
        status: vec![ok_json(STATUS)],
        ..Script::default()
    });
    let body = format!(
        r#"
    {prelude}
    const hashes = [
      '#/overview', '#/components', '#/components/example.unfamiliar-reporter',
      '#/tasks', '#/projects', '#/system', '#/not-a-route', '#/overview',
    ];
    const visited = [];
    for (const hash of hashes) {{
      await navigate(hash);
      visited.push(`${{cockpit.state.route.name}}:${{location.hash}}`);
    }}
    return {{ visited, posts: 0 }};
    "#,
        prelude = NAVIGATE_PRELUDE,
    );
    let result = run_driver(&server, &body, "nav-posts");
    assert_eq!(result["visited"].as_array().unwrap().len(), 8);
    assert_eq!(server.post_count(), 0, "navigation must never POST: {:?}", server.requests());
    let requests = server.requests();
    assert!(
        requests.iter().all(|request| request.method == "GET"),
        "navigation must only read: {requests:?}"
    );
    assert!(
        requests.iter().any(|request| request.path.starts_with("/api/v1/components/")),
        "the detail route must read its projection: {requests:?}"
    );
    assert!(
        requests.iter().any(|request| request.path == "/api/v1/status"),
        "the system route must read the status projection: {requests:?}"
    );
}

#[test]
fn cockpit_page_aria_current_marks_the_active_route() {
    let (server, _state) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW)],
        components: vec![ok_json(COMPONENTS)],
        factory: vec![ok_json(FACTORY_RUNTIME)],
        detail: vec![ok_json(COMPONENT_DETAIL)],
        status: vec![ok_json(STATUS)],
        ..Script::default()
    });
    let body = format!(
        r#"
    {prelude}
    const current = () => [...document.querySelectorAll('nav a[aria-current]')]
      .map((node) => node.getAttribute('href'));
    const marked = {{}};
    for (const hash of ['#/overview', '#/components', '#/components/example.unfamiliar-reporter', '#/tasks', '#/projects', '#/system', '#/bogus']) {{
      await navigate(hash);
      marked[hash] = current();
    }}
    const links = [...document.querySelectorAll('nav a')].length;
    return {{ marked, links }};
    "#,
        prelude = NAVIGATE_PRELUDE,
    );
    let result = run_driver(&server, &body, "aria-current");
    let marked = result["marked"].as_object().unwrap();
    let expect = |hash: &str, href: &str| {
        assert_eq!(
            marked[hash],
            serde_json::json!([href]),
            "aria-current for {hash}"
        );
    };
    expect("#/overview", "#/overview");
    expect("#/components", "#/components");
    expect("#/components/example.unfamiliar-reporter", "#/components");
    expect("#/tasks", "#/tasks");
    expect("#/projects", "#/projects");
    expect("#/system", "#/system");
    expect("#/bogus", "#/overview");
    assert_eq!(result["links"].as_u64().unwrap(), 5, "the nav offers every non-detail route");
}

#[test]
fn cockpit_page_component_detail_renders_inert_text() {
    let (server, _state) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW)],
        components: vec![ok_json(COMPONENTS)],
        detail: vec![ok_value(&injected_component_detail())],
        ..Script::default()
    });
    let result = run_driver_at(
        &server,
        "#/components/example.unfamiliar-reporter",
        r#"
    const cockpit = startCockpit({ document, root: document.getElementById('app'), api: new DashboardApi(), intervalMs: 60000 });
    await cockpit.refresher.activeCycle;
    const page = document.querySelector('[data-page="component"]');
    return {
      route: cockpit.state.route.name,
      visible: !page.hidden,
      text: page.textContent,
      scriptCount: document.querySelectorAll('script').length,
      imgCount: document.querySelectorAll('img').length,
      onerrorCount: document.querySelectorAll('[onerror]').length,
      pageScripts: page.querySelectorAll('script').length,
      pageImgs: page.querySelectorAll('img').length,
    };
    "#,
        "detail-inert",
    );
    assert_eq!(result["route"].as_str().unwrap(), "component");
    assert_eq!(result["visible"].as_bool(), Some(true));
    let text = result["text"].as_str().unwrap();
    assert!(text.contains("<script>alert('descriptor')</script>"), "descriptor markup must stay text: {text}");
    assert!(text.contains("<script>alert('payload')</script>"), "payload markup must stay text: {text}");
    assert!(text.contains("<img src=x onerror=alert(1)>"), "payload markup must stay text: {text}");
    assert_eq!(result["scriptCount"].as_u64().unwrap(), 1, "hostile script text must not become an element");
    assert_eq!(result["imgCount"].as_u64().unwrap(), 0, "hostile img text must not become an element");
    assert_eq!(result["onerrorCount"].as_u64().unwrap(), 0, "a hostile handler must not become an attribute");
    assert_eq!(result["pageScripts"].as_u64().unwrap(), 0);
    assert_eq!(result["pageImgs"].as_u64().unwrap(), 0);
}

#[test]
fn cockpit_page_component_detail_unknown_plugin_is_not_registered() {
    let plugin_id = "example.not-registered";
    let (server, _state) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW)],
        components: vec![ok_json(COMPONENTS)],
        detail: vec![ok_value(&unknown_component_detail(plugin_id))],
        ..Script::default()
    });
    let result = run_driver_at(
        &server,
        "#/components/example.not-registered",
        r#"
    const cockpit = startCockpit({ document, root: document.getElementById('app'), api: new DashboardApi(), intervalMs: 60000 });
    await cockpit.refresher.activeCycle;
    const page = document.querySelector('[data-page="component"]');
    return {
      route: cockpit.state.route.name,
      routeId: cockpit.state.route.params.id,
      visible: !page.hidden,
      text: page.textContent,
    };
    "#,
        "detail-unknown",
    );
    assert_eq!(result["route"].as_str().unwrap(), "component");
    assert_eq!(result["routeId"].as_str().unwrap(), plugin_id);
    assert_eq!(result["visible"].as_bool(), Some(true));
    let text = result["text"].as_str().unwrap();
    assert!(
        text.contains("This component is not registered in the hub registry."),
        "an unknown plugin must render the explicit not-registered state: {text}"
    );
    assert!(!text.contains("No report envelopes."), "an unknown plugin must not render a healthy empty detail: {text}");
    assert!(!text.contains("Component detail unknown."), "the page must resolve the projection: {text}");
}

#[test]
fn cockpit_page_component_detail_source_state_property_has_no_false_absence() {
    const REGISTRY_STATES: [&str; 3] = ["available", "missing", "unreadable"];
    const REPORT_STATES: [&str; 4] = ["available", "empty", "missing", "unknown"];

    // Deterministic 3 x 4 matrix: 12 cases, one browser launch.
    let mut detail = Vec::new();
    let mut cases = Vec::new();
    for registry_state in REGISTRY_STATES {
        for reports_state in REPORT_STATES {
            let plugin_id = format!("case.{registry_state}.{reports_state}");
            detail.push(ok_value(&component_detail_with_sources(
                &plugin_id,
                registry_state,
                reports_state,
            )));
            cases.push(serde_json::json!({
                "id": plugin_id,
                "registry": registry_state,
                "reports": reports_state,
            }));
        }
    }
    assert_eq!(cases.len(), 12, "registry x reports property budget");

    let (server, _state) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW)],
        components: vec![ok_json(COMPONENTS)],
        detail,
        ..Script::default()
    });
    let first = cases[0]["id"].as_str().unwrap();
    let body = format!(
        r#"
    {prelude}
    const cases = {cases};
    const results = [];
    for (const item of cases) {{
      await navigate(`#/components/${{item.id}}`);
      const page = document.querySelector('[data-page="component"]');
      results.push({{ id: item.id, registry: item.registry, reports: item.reports, text: page.textContent }});
    }}
    return {{ results }};
    "#,
        prelude = NAVIGATE_PRELUDE,
        cases = serde_json::to_string(&cases).unwrap(),
    );
    let result = run_driver_at(
        &server,
        &format!("#/components/{first}"),
        &body,
        "detail-source-state-property",
    );
    let results = result["results"].as_array().unwrap();
    assert_eq!(results.len(), 12, "all matrix cases must render");

    for item in results {
        let registry_state = item["registry"].as_str().unwrap();
        let reports_state = item["reports"].as_str().unwrap();
        let plugin_id = item["id"].as_str().unwrap();
        let text = item["text"].as_str().unwrap();
        if registry_state == "missing" {
            assert!(
                text.contains("This component is not registered in the hub registry."),
                "registry missing must render absent: {plugin_id} {text}"
            );
            assert!(
                !text.contains("Binding")
                    && !text.contains("Declared views")
                    && !text.contains("No report envelopes."),
                "registry missing must not render a healthy detail: {plugin_id} {text}"
            );
        } else {
            assert!(
                !text.contains("not registered in the hub registry"),
                "registry {registry_state} must not be falsely absent: {plugin_id} {text}"
            );
            assert!(
                text.contains("Binding") && text.contains("Declared views"),
                "registry {registry_state} must render binding/descriptors: {plugin_id} {text}"
            );
            if reports_state == "available" {
                assert!(
                    text.contains(&format!("{plugin_id}.status")),
                    "available reports must render the entry: {plugin_id} {text}"
                );
            } else {
                assert!(
                    text.contains("No report envelopes."),
                    "reports {reports_state} must render the honest empty state: {plugin_id} {text}"
                );
            }
        }
    }
}

#[test]
fn cockpit_page_component_detail_builtin_without_reports_renders_binding() {
    let plugin_id = "component:harness";
    let mut detail = component_detail_with_sources(plugin_id, "available", "missing");
    detail["descriptors"] = serde_json::json!([]);
    let (server, _state) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW)],
        components: vec![ok_json(COMPONENTS)],
        detail: vec![ok_value(&detail)],
        ..Script::default()
    });
    let result = run_driver_at(
        &server,
        "#/components/component:harness",
        r#"
    const cockpit = startCockpit({ document, root: document.getElementById('app'), api: new DashboardApi(), intervalMs: 60000 });
    await cockpit.refresher.activeCycle;
    const page = document.querySelector('[data-page="component"]');
    return { text: page.textContent };
    "#,
        "detail-builtin-no-reports",
    );
    let text = result["text"].as_str().unwrap();
    assert!(text.contains("Plugin: component:harness"), "plugin identity missing: {text}");
    assert!(text.contains("Binding") && text.contains("component:harness"), "builtin binding missing: {text}");
    assert!(text.contains("Declared views"), "builtin descriptor section missing: {text}");
    assert!(text.contains("No declared views."), "builtin descriptor empty state missing: {text}");
    assert!(text.contains("No report envelopes."), "builtin no-report state missing: {text}");
    assert!(
        !text.contains("not registered in the hub registry"),
        "a registered builtin must not render absent: {text}"
    );
}

#[test]
fn cockpit_page_system_renders_status_facts() {
    let (server, _state) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW)],
        components: vec![ok_json(COMPONENTS)],
        status: vec![ok_json(STATUS)],
        ..Script::default()
    });
    let result = run_driver_at(
        &server,
        "#/system",
        r#"
    const cockpit = startCockpit({ document, root: document.getElementById('app'), api: new DashboardApi(), intervalMs: 60000 });
    await cockpit.refresher.activeCycle;
    const page = document.querySelector('[data-page="system"]');
    return {
      visible: !page.hidden,
      status: document.getElementById('status').textContent,
      sources: document.getElementById('sources').textContent,
    };
    "#,
        "system-facts",
    );
    assert_eq!(result["visible"].as_bool(), Some(true));
    let status = result["status"].as_str().unwrap();
    assert!(status.contains("Version: 0.1.0"), "status={status}");
    assert!(status.contains("Health: ready"), "status={status}");
    assert!(status.contains("Listener: ready"), "status={status}");
    assert!(status.contains("http://127.0.0.1:8791"), "status={status}");
    assert!(status.contains("Binding: ready"), "status={status}");
    let sources = result["sources"].as_str().unwrap();
    assert!(sources.contains("refresh:status"), "sources={sources}");
    assert!(sources.contains("Interval: 60000 ms"), "sources={sources}");
}

#[test]
fn cockpit_page_system_malformed_status_shows_bounded_error() {
    let malformed = r#"{"format":"DashboardStatus/v1","version":"0.1.0","health_state":"ready"}"#;
    let (server, _state) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW)],
        components: vec![ok_json(COMPONENTS)],
        status: vec![ok_json(malformed)],
        ..Script::default()
    });
    let result = run_driver_at(
        &server,
        "#/system",
        r#"
    const cockpit = startCockpit({ document, root: document.getElementById('app'), api: new DashboardApi(), intervalMs: 60000 });
    await cockpit.refresher.activeCycle;
    const page = document.querySelector('[data-page="system"]');
    const status = document.getElementById('status').textContent;
    return { visible: !page.hidden, status, pageText: page.textContent };
    "#,
        "system-malformed",
    );
    assert_eq!(result["visible"].as_bool(), Some(true));
    let status = result["status"].as_str().unwrap();
    assert!(
        status.contains("status read unavailable") && status.contains("malformed"),
        "a malformed status response must surface a visible bounded error: {status}"
    );
    assert!(status.len() <= 4200, "the error text must stay bounded: {} bytes", status.len());
    assert!(!status.contains("Dashboard status unknown."), "the error is explicit, not unknown: {status}");
    assert!(
        result["pageText"].as_str().unwrap().contains("Dashboard health"),
        "the system page must not be blank"
    );
}

#[test]
fn cockpit_page_pagehide_on_tasks_route_stops_timers_and_aborts() {
    let (server, _state) = scripted_server(Script::default());
    let body = format!(
        r#"
    let mode = 'ok';
    let aborted = false;
    let capturedSignal = null;
    const overview = JSON.parse({overview});
    const components = JSON.parse({components});
    const api = {{
      getOverview: ({{ signal }} = {{}}) => {{
        if (mode === 'hang') {{
          capturedSignal = signal;
          return new Promise((resolve, reject) => {{
            signal.addEventListener('abort', () => {{
              aborted = true;
              reject(new ApiError('aborted', 'aborted by page exit'));
            }});
          }});
        }}
        return Promise.resolve(validateOverview(overview));
      }},
      getComponents: ({{ signal }} = {{}}) => Promise.resolve(validateComponents(components)),
      getFactory: ({{ signal }} = {{}}) => Promise.reject(new ApiError('unsupported', 'endpoint absent')),
    }};
    // A 60s interval sits well above Chrome's 8s virtual-time budget, so no
    // periodic refresh can fire between the phases of this case.
    const cockpit = startCockpit({{ document, root: document.getElementById('app'), api, intervalMs: 60000 }});
    await cockpit.refresher.activeCycle;
    const timerScheduled = cockpit.refresher.pendingTimer !== null;
    const activePage = [...document.querySelectorAll('[data-page]')]
      .filter((node) => !node.hidden).map((node) => node.dataset.page);
    mode = 'hang';
    const hanging = cockpit.refresh();
    let guard = 0;
    while ((capturedSignal === null || cockpit.refresher.controller === null) && guard < 1000) {{
      await new Promise((resolve) => setTimeout(resolve, 5));
      guard += 1;
    }}
    const activeController = cockpit.refresher.controller;
    const capturedBeforeExit = capturedSignal !== null && activeController !== null;
    window.dispatchEvent(new Event('pagehide'));
    return {{
      route: cockpit.state.route.name,
      activePage,
      running: cockpit.refresher.running,
      pendingTimer: cockpit.refresher.pendingTimer,
      controller: cockpit.refresher.controller,
      capturedBeforeExit,
      controllerAborted: activeController ? activeController.signal.aborted : null,
      savedSignalAborted: capturedSignal ? capturedSignal.aborted : null,
      aborted,
      timerScheduled,
    }};
    "#,
        overview = serde_json::to_string(OVERVIEW).unwrap(),
        components = serde_json::to_string(COMPONENTS).unwrap(),
    );
    let result = run_driver_at(&server, "#/tasks", &body, "pagehide-tasks");
    assert_eq!(result["route"].as_str().unwrap(), "tasks");
    assert_eq!(result["activePage"], serde_json::json!(["tasks"]));
    assert!(result["timerScheduled"].as_bool().unwrap(), "a refresh timer must have been scheduled before exit");
    assert!(
        result["capturedBeforeExit"].as_bool().unwrap(),
        "the hanging read must be in flight before pagehide"
    );
    assert_eq!(result["running"].as_bool(), Some(false), "pagehide must stop the refresher on a non-overview route");
    assert_eq!(result["pendingTimer"], serde_json::Value::Null, "pagehide must clear the pending timer");
    assert_eq!(result["controller"], serde_json::Value::Null, "pagehide must clear the active controller");
    assert_eq!(result["controllerAborted"].as_bool(), Some(true), "the active controller must be aborted");
    assert_eq!(result["savedSignalAborted"].as_bool(), Some(true), "the saved abort signal must be aborted");
    assert_eq!(result["aborted"].as_bool(), Some(true), "an in-flight request's abort signal must fire");
}

// ---------------------------------------------------------------------------
// Shell geometry, visibility and lifecycle repair regressions
// ---------------------------------------------------------------------------

/// Deterministic property matrix: every route state at every accepted viewport
/// class. The case budget is four browser launches x (bare URL + six routes).
/// There is no random seed; the generation is the fixed width/route cross
/// product below.
#[test]
fn cockpit_page_shell_viewport_route_property_matrix() {
    const VIEWPORTS: [u32; 4] = [390, 720, 1280, 1920];
    const ROUTES: [(&str, &str, bool); 6] = [
        ("#/overview", "overview", true),
        ("#/components", "components", true),
        ("#/tasks", "tasks", true),
        ("#/projects", "projects", true),
        ("#/system", "system", true),
        ("#/not-a-route", "overview", false),
    ];
    let (server, _state) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW)],
        components: vec![ok_json(COMPONENTS)],
        factory: vec![ok_json(FACTORY_RUNTIME)],
        detail: vec![ok_json(COMPONENT_DETAIL)],
        status: vec![ok_json(STATUS)],
        ..Script::default()
    });
    let body = r#"
    const cockpit = startCockpit({
      document, root: document.getElementById('app'), api: new DashboardApi(), intervalMs: 60000,
    });
    await cockpit.refresher.activeCycle;
    const rect = (node) => {
      const r = node.getBoundingClientRect();
      return { left: r.left, right: r.right, top: r.top, bottom: r.bottom, width: r.width, height: r.height };
    };
    const hit = (node) => {
      const r = node.getBoundingClientRect();
      if (r.width === 0 || r.height === 0) return false;
      const found = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
      return !!found && (found === node || node.contains(found));
    };
    const snapshot = () => {
      const pages = [...document.querySelectorAll('[data-page]')].map((node) => ({
        name: node.dataset.page,
        hidden: node.hidden,
        display: getComputedStyle(node).display,
        rect: rect(node),
      }));
      const notice = document.getElementById('route-notice');
      const error = document.getElementById('errors');
      const pause = document.querySelector('[data-action="pause"]');
      const resume = document.querySelector('[data-action="resume"]');
      return {
        route: cockpit.state.route.name,
        valid: cockpit.state.route.valid,
        innerWidth,
        scrollWidth: document.documentElement.scrollWidth,
        bodyScrollWidth: document.body.scrollWidth,
        overflowing: [...document.querySelectorAll('body *')]
          .filter((node) => {
            const r = node.getBoundingClientRect();
            return r.width > 0 && (r.left < -1 || r.right > innerWidth + 1);
          })
          .slice(0, 12)
          .map((node) => ({
            tag: node.tagName,
            id: node.id,
            cls: String(node.className || ''),
            rect: rect(node),
          })),
        nav: rect(document.querySelector('nav')),
        content: rect(document.querySelector('.content')),
        pages,
        notice: {
          hidden: notice.hidden,
          display: getComputedStyle(notice).display,
          rect: rect(notice),
          text: notice.textContent,
        },
        error: {
          hidden: error.hidden,
          display: getComputedStyle(error).display,
          rect: rect(error),
        },
        pause: pause ? { hidden: pause.hidden, rect: rect(pause), hit: hit(pause), disabled: pause.disabled } : null,
        resume: resume ? { hidden: resume.hidden, rect: rect(resume), hit: hit(resume), disabled: resume.disabled } : null,
      };
    };
    const navigate = async (hash) => {
      if (location.hash === hash) return;
      const changed = new Promise((resolve) => window.addEventListener('hashchange', resolve, { once: true }));
      location.hash = hash;
      await changed;
      if (cockpit.refresher.activeCycle) await cockpit.refresher.activeCycle;
    };
    const observations = { initial: snapshot(), routes: {} };
    for (const hash of ['#/overview', '#/components', '#/tasks', '#/projects', '#/system', '#/not-a-route']) {
      await navigate(hash);
      observations.routes[hash] = snapshot();
    }
    return observations;
    "#;

    for width in VIEWPORTS {
        let result = run_driver_at_size(
            &server,
            "",
            body,
            &format!("shell-matrix-{width}"),
            width,
            2000,
        );
        assert_eq!(
            result["initial"]["innerWidth"].as_f64().unwrap(),
            f64::from(width),
            "Chrome must honour the requested viewport"
        );
        // Bare URL: valid Overview landing without the unknown-route notice.
        assert_eq!(result["initial"]["route"].as_str().unwrap(), "overview");
        assert_eq!(result["initial"]["valid"].as_bool(), Some(true), "a bare URL is the valid default");
        assert_eq!(result["initial"]["notice"]["hidden"].as_bool(), Some(true));
        assert_eq!(result["initial"]["notice"]["display"].as_str().unwrap(), "none");

        for (hash, expected, valid) in ROUTES {
            let observation = &result["routes"][hash];
            assert_eq!(observation["route"].as_str().unwrap(), expected, "{width}px {hash}");
            assert_eq!(observation["valid"].as_bool(), Some(valid), "{width}px {hash}");
            assert!(
                observation["scrollWidth"].as_f64().unwrap() <= f64::from(width) + 1.0,
                "{width}px {hash} must not overflow the page: scrollWidth={} overflowing={}",
                observation["scrollWidth"],
                observation["overflowing"]
            );
            assert!(
                observation["bodyScrollWidth"].as_f64().unwrap() <= f64::from(width) + 1.0,
                "{width}px {hash} body must not overflow: bodyScrollWidth={}",
                observation["bodyScrollWidth"]
            );

            let pages = observation["pages"].as_array().unwrap();
            let visible: Vec<&str> = pages
                .iter()
                .filter(|page| page["display"].as_str() != Some("none"))
                .map(|page| page["name"].as_str().unwrap())
                .collect();
            assert_eq!(visible, vec![expected], "{width}px {hash} renders only the active page");
            for page in pages {
                let name = page["name"].as_str().unwrap();
                if name != expected {
                    assert_eq!(page["display"].as_str().unwrap(), "none", "{width}px {hash}: {name} must be display:none");
                    assert_eq!(page["rect"]["width"].as_f64().unwrap(), 0.0, "{width}px {hash}: {name} occupies width");
                    assert_eq!(page["rect"]["height"].as_f64().unwrap(), 0.0, "{width}px {hash}: {name} occupies height");
                }
            }

            // Unknown notice only on the malformed hash. Hidden notices and
            // errors occupy no layout space.
            if valid {
                assert_eq!(observation["notice"]["hidden"].as_bool(), Some(true), "{width}px {hash}");
                assert_eq!(observation["notice"]["display"].as_str().unwrap(), "none", "{width}px {hash}");
                assert_eq!(
                    observation["notice"]["rect"]["height"].as_f64().unwrap(),
                    0.0,
                    "{width}px {hash} hidden notice takes space"
                );
            } else {
                assert_eq!(observation["notice"]["hidden"].as_bool(), Some(false), "{width}px {hash}");
                assert_eq!(
                    observation["notice"]["text"].as_str().unwrap(),
                    "Unknown route; showing Overview."
                );
            }
            assert_eq!(observation["error"]["hidden"].as_bool(), Some(true), "{width}px {hash}");
            assert_eq!(observation["error"]["display"].as_str().unwrap(), "none", "{width}px {hash}");
            assert_eq!(observation["error"]["rect"]["height"].as_f64().unwrap(), 0.0, "{width}px {hash} hidden error takes space");

            let nav = &observation["nav"];
            let content = &observation["content"];
            if width >= 1280 {
                assert!(
                    content["left"].as_f64().unwrap() >= nav["right"].as_f64().unwrap() - 0.5,
                    "{width}px {hash}: content must clear the fixed sidebar: nav={nav:?} content={content:?}"
                );
            } else {
                assert!(
                    nav["left"].as_f64().unwrap() >= -0.5
                        && content["left"].as_f64().unwrap() >= -0.5,
                    "{width}px {hash}: navigation and content stay inside the viewport"
                );
            }
            assert!(
                content["right"].as_f64().unwrap() <= f64::from(width) + 1.0,
                "{width}px {hash}: content must stay inside the viewport"
            );

            if hash == "#/overview" || hash == "#/not-a-route" {
                for action in ["pause", "resume"] {
                    let control = &observation[action];
                    assert_eq!(control["hidden"].as_bool(), Some(false), "{width}px {hash}: {action}");
                    assert_eq!(control["disabled"].as_bool(), Some(false), "{width}px {hash}: {action} permission");
                    let rect = &control["rect"];
                    assert!(
                        rect["left"].as_f64().unwrap() >= content["left"].as_f64().unwrap() - 0.5
                            && rect["right"].as_f64().unwrap() <= content["right"].as_f64().unwrap() + 0.5,
                        "{width}px {hash}: {action} must remain inside the content column: {rect:?}"
                    );
                    assert_eq!(control["hit"].as_bool(), Some(true), "{width}px {hash}: {action} must be hit-testable");
                }
            }
        }
    }
    assert_eq!(server.post_count(), 0, "layout, route and refresh checks must never POST");
}

#[test]
fn cockpit_page_visibility_and_persisted_pageshow_resume_one_loop() {
    let (server, _state) = scripted_server(Script::default());
    let body = r#"
    let visibility = 'visible';
    Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => visibility });    let overviewCalls = 0;
    let componentsCalls = 0;
    let factoryCalls = 0;
    let mode = 'ok';
    let aborted = false;
    let capturedSignal = null;
    const api = {
      getOverview: ({ signal } = {}) => {
        if (mode === 'hang') {
          capturedSignal = signal;
          return new Promise((resolve, reject) => {
            signal.addEventListener('abort', () => {
              aborted = true;
              reject(new ApiError('aborted', 'hidden'));
            });
          });
        }
        overviewCalls += 1;
        return Promise.resolve({ economics: null, timeline: [] });
      },
      getComponents: () => { componentsCalls += 1; return Promise.resolve({ entries: [], next_cursor: null }); },
      getFactory: () => { factoryCalls += 1; return Promise.reject(new ApiError('unsupported', 'endpoint absent')); },
    };
    const cockpit = startCockpit({ document, root: document.getElementById('app'), api, intervalMs: 60000 });
    await cockpit.refresher.activeCycle;
    const start = { overview: overviewCalls, running: cockpit.refresher.running, timer: cockpit.refresher.pendingTimer !== null };

    // Hide while an active read is in flight: the read must abort, not linger.
    mode = 'hang';
    const hanging = cockpit.refresh();
    let guard = 0;
    while ((capturedSignal === null || cockpit.refresher.controller === null) && guard < 1000) {
      await new Promise((resolve) => setTimeout(resolve, 5));
      guard += 1;
    }
    const activeController = cockpit.refresher.controller;
    visibility = 'hidden';
    document.dispatchEvent(new Event('visibilitychange'));
    await hanging;
    const hidden = {
      overview: overviewCalls,
      running: cockpit.refresher.running,
      timer: cockpit.refresher.pendingTimer,
      controller: cockpit.refresher.controller,
      controllerAborted: activeController ? activeController.signal.aborted : null,
      savedSignalAborted: capturedSignal ? capturedSignal.aborted : null,
      aborted,
    };

    mode = 'ok';
    visibility = 'visible';
    document.dispatchEvent(new Event('visibilitychange'));
    await cockpit.refresher.activeCycle;
    const visible = { overview: overviewCalls, running: cockpit.refresher.running, timer: cockpit.refresher.pendingTimer !== null };

    // A duplicate visible event must not start a second loop or read.
    document.dispatchEvent(new Event('visibilitychange'));
    await Promise.resolve();
    const duplicate = { overview: overviewCalls, running: cockpit.refresher.running };

    window.dispatchEvent(new Event('pagehide'));
    const pageHidden = {
      overview: overviewCalls,
      running: cockpit.refresher.running,
      timer: cockpit.refresher.pendingTimer,
      controller: cockpit.refresher.controller,
    };

    window.dispatchEvent(new PageTransitionEvent('pageshow', { persisted: true }));
    await cockpit.refresher.activeCycle;
    const pageShown = { overview: overviewCalls, running: cockpit.refresher.running, timer: cockpit.refresher.pendingTimer !== null };
    return { start, hidden, visible, duplicate, pageHidden, pageShown, componentsCalls, factoryCalls };
    "#;
    let result = run_driver_at_size(&server, "", body, "lifecycle", 1280, 2000);

    assert_eq!(result["start"]["running"].as_bool(), Some(true));
    assert_eq!(result["start"]["timer"].as_bool(), Some(true));
    let first = result["start"]["overview"].as_u64().unwrap();

    assert_eq!(result["hidden"]["running"].as_bool(), Some(false), "hidden pages stop polling");
    assert_eq!(result["hidden"]["timer"], serde_json::Value::Null, "hidden pages clear the timer");
    assert_eq!(result["hidden"]["controller"], serde_json::Value::Null, "hidden pages abort the active read");
    assert_eq!(
        result["hidden"]["controllerAborted"].as_bool(),
        Some(true),
        "hiding must abort the active controller"
    );
    assert_eq!(
        result["hidden"]["savedSignalAborted"].as_bool(),
        Some(true),
        "the in-flight read's signal must abort on hide"
    );
    assert_eq!(result["hidden"]["aborted"].as_bool(), Some(true));
    assert_eq!(result["hidden"]["overview"].as_u64().unwrap(), first, "hiding must not read");

    assert_eq!(result["visible"]["running"].as_bool(), Some(true), "visible pages resume polling");
    assert_eq!(result["visible"]["timer"].as_bool(), Some(true));
    assert_eq!(
        result["visible"]["overview"].as_u64().unwrap(),
        first + 1,
        "becoming visible reads immediately exactly once"
    );
    assert_eq!(
        result["duplicate"]["overview"].as_u64().unwrap(),
        first + 1,
        "a duplicate visible event must not create another loop"
    );
    assert_eq!(result["duplicate"]["running"].as_bool(), Some(true));

    window_assert_stopped(&result["pageHidden"], "pagehide");
    assert_eq!(
        result["pageShown"]["overview"].as_u64().unwrap(),
        first + 2,
        "persisted pageshow resumes exactly one read"
    );
    assert_eq!(result["pageShown"]["running"].as_bool(), Some(true));
    assert_eq!(result["pageShown"]["timer"].as_bool(), Some(true));
    assert!(result["componentsCalls"].as_u64().unwrap() >= 1);
    assert!(result["factoryCalls"].as_u64().unwrap() >= 1);
    assert_eq!(server.post_count(), 0, "lifecycle and refresh must never POST");
}

fn window_assert_stopped(value: &serde_json::Value, label: &str) {
    assert_eq!(value["running"].as_bool(), Some(false), "{label} must stop polling");
    assert_eq!(value["timer"], serde_json::Value::Null, "{label} must clear the timer");
    assert_eq!(value["controller"], serde_json::Value::Null, "{label} must abort the active read");
}

#[test]
fn cockpit_page_refresh_preserves_keyboard_focus() {
    let (server, _state) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW)],
        components: vec![ok_json(COMPONENTS)],
        factory: vec![ok_json(FACTORY_RUNTIME)],
        ..Script::default()
    });
    let body = r#"
    const cockpit = startCockpit({
      document, root: document.getElementById('app'), api: new DashboardApi(), intervalMs: 60000,
    });
    await cockpit.refresher.activeCycle;
    const pause = document.querySelector('[data-action="pause"]');
    pause.focus();
    const pauseFocusedBefore = document.activeElement === pause;
    await cockpit.refresh();
    const pauseFocusedAfter = document.activeElement?.dataset?.action === 'pause';

    const componentsLink = document.querySelector('nav a[data-focus-key="nav:components"]');
    componentsLink.focus();
    const navFocusedBefore = document.activeElement === componentsLink;
    await cockpit.refresh();
    const navFocusedAfter = document.activeElement?.dataset?.focusKey === 'nav:components';
    return {
      pauseFocusedBefore,
      pauseFocusedAfter,
      navFocusedBefore,
      navFocusedAfter,
      activeTag: document.activeElement?.tagName ?? null,
    };
    "#;
    let result = run_driver_at_size(&server, "", body, "focus", 1280, 2000);
    assert_eq!(result["pauseFocusedBefore"].as_bool(), Some(true));
    assert_eq!(result["pauseFocusedAfter"].as_bool(), Some(true), "a refresh must keep focus on the rendered Pause control");
    assert_eq!(result["navFocusedBefore"].as_bool(), Some(true));
    assert_eq!(result["navFocusedAfter"].as_bool(), Some(true), "a refresh must keep focus on the same navigation link");
    assert_eq!(server.post_count(), 0);
}

// ---------------------------------------------------------------------------
// UI.3 information, appearance and navigation properties
// ---------------------------------------------------------------------------

/// Deterministic property matrix: 4 source observation states x 3 exact queue
/// totals x 3 component-page shapes = 36 independent model scenarios. The
/// generation is the fixed cross product below; there is no random seed.
#[test]
fn cockpit_information_state_property_matrix() {
    let (server, _state) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW)],
        components: vec![ok_json(COMPONENTS)],
        factory: vec![ok_json(FACTORY_RUNTIME)],
        ..Script::default()
    });
    let body = format!(
        r#"
    const NOW = 1791820805000;
    const STATES = ['known', 'empty', 'first_read_unavailable', 'retained_stale'];
    const TOTALS = ['0', null, '18446744073709551615'];
    const SHAPES = ['empty', 'populated_final', 'populated_continuation'];
    const failures = [];
    const check = (id, condition, detail) => {{ if (!condition) failures.push({{ id, detail }}); }};
    const entry = (suffix, health, freshness, completeness, available) => ({{
      plugin_id: `example.${{suffix}}`, name: `Example ${{suffix}}`, generation: String(10 + suffix.length),
      health, freshness, completeness, available, views: [],
    }});
    const entriesFor = (shape) => {{
      if (shape === 'empty') return [];
      return [
        entry('ready', 'ready', 'fresh', 'Complete', true),
        entry('down', 'unavailable', 'stale', 'Partial', false),
        entry('unknown', 'unknown', 'unknown', 'Unknown', true),
      ];
    }};
    const source = (data, state) => {{
      if (state === 'first_read_unavailable') {{
        return {{ data: null, lastSuccessAt: null, lastAttemptAt: NOW, error: {{ kind: 'unavailable', message: 'down' }}, status: 'failed', stale: false }};
      }}
      if (state === 'retained_stale') {{
        return {{ data, lastSuccessAt: NOW - 60000, lastAttemptAt: NOW, error: {{ kind: 'unavailable', message: 'down' }}, status: 'failed', stale: true }};
      }}
      return {{ data, lastSuccessAt: NOW - 1000, lastAttemptAt: NOW, error: null, status: 'available', stale: false }};
    }};
    const scenarios = [];
    for (const state of STATES) {{
      for (let totalIndex = 0; totalIndex < TOTALS.length; totalIndex += 1) {{
        const total = TOTALS[totalIndex];
        for (const shape of SHAPES) {{
          const id = `state=${{state}};total=${{totalIndex}};shape=${{shape}}`;
          const executions = [
            {{ execution_id: 'exec:current', task_id: 'task-current', generation: '8', phase: 'Build', state: 'Running', attempt: '99', updated_at_unix_ms: NOW - 1000 }},
            {{ execution_id: 'exec:current', task_id: 'task-current', generation: '7', phase: 'Build', state: 'Running', attempt: '7', updated_at_unix_ms: NOW - 1000 }},
          ];
          const emptyState = state === 'empty';
          const currentExecution = emptyState ? null : {{
            execution_id: 'exec:current', task_id: 'task-current', generation: '7', phase: 'Build', state: 'Running',
            updated_at_unix_ms: NOW - 1000,
          }};
          const overview = {{
            metadata: {{ sources: [{{ source: 'ledger', state: 'available', detail: '', observed_at_unix_ms: NOW }}] }},
            factory: {{ stopped: false, reason: null, observed_at_unix_ms: NOW }},
            executions: emptyState ? [] : executions,
            economics: {{ completeness: 'Complete', usage: '123456789012345.678901', usage_unit: 'tokens', cost: '0.000000000000000001', allowance: '5.00', cost_unit: 'usd', observed_at_unix_ms: NOW }},
            timeline: emptyState ? [] : [{{ at_unix_ms: NOW - 2000, kind: 'note', title: 'Observed', detail: 'fixture' }}],
          }};
          const factory = {{
            repository_id: 'repo-a', state_id: 'state-a', status: 'Running', reason: null,
            stop: {{ stopped: false, reason: null, observed_at_unix_ms: NOW }},
            current_execution: currentExecution,
            eligible: {{ entries: emptyState ? [] : [{{ task_id: 'alpha', title: 'Alpha', priority: 'normal', family: 'feature' }}], revision: 'rev-1', total, next_cursor: null }},
            permissions: {{ pause: true, resume: true }},
          }};
          const page = {{ entries: entriesFor(shape), next_cursor: shape === 'populated_continuation' ? 'cp:next' : null }};
          const snapshot = {{
            renderNow: NOW,
            sources: {{
              overview: source(overview, state),
              factory: source(factory, state),
              components: source(page, state),
            }},
          }};
          const cards = summaryCards(snapshot);
          const byId = Object.fromEntries(cards.map((card) => [card.id, card]));
          const current = currentWorkModel(snapshot);
          const componentPage = componentPageModel(snapshot);
          const attention = deriveAttention(snapshot);
          const keys = attention.items.map((item) => item.key);
          const unique = new Set(keys).size === keys.length;
          const rank = {{ error: 0, warning: 1, unknown: 2 }};
          let ordered = true;
          for (let index = 1; index < attention.items.length; index += 1) {{
            const a = attention.items[index - 1];
            const b = attention.items[index];
            if (rank[a.severity] > rank[b.severity]) ordered = false;
            if (rank[a.severity] === rank[b.severity] && a.key > b.key) ordered = false;
          }}

          const unavailable = state === 'first_read_unavailable';
          check(id + ':cards', cards.length === 4);
          check(
            id + ':eligible-total',
            byId['summary-eligible'].value === (unavailable || total === null ? 'unknown' : total),
            byId['summary-eligible'],
          );
          check(
            id + ':cost-exact',
            unavailable
              ? byId['summary-cost'].value === 'unknown'
              : byId['summary-cost'].value === '0.000000000000000001 usd',
          );
          check(id + ':component-sums', componentPage.ready + componentPage.attention + componentPage.unknown === componentPage.observed);
          check(
            id + ':component-page-scope',
            unavailable
              ? componentPage.continuation === false
              : componentPage.continuation === (shape === 'populated_continuation'),
          );
          check(id + ':attention-bounded', attention.items.length <= 10 && attention.hiddenCount === Math.max(0, attention.total - 10));
          check(id + ':attention-deduped', unique);
          check(id + ':attention-order', ordered);
          check(id + ':no-post', true);

          if (state === 'first_read_unavailable') {{
            check(id + ':unknown-factory', byId['summary-factory'].value !== 'Running' && byId['summary-factory'].value !== 'Idle');
            check(id + ':unknown-eligible', byId['summary-eligible'].value === 'unknown');
            check(id + ':unknown-cost', byId['summary-cost'].value === 'unknown');
            check(id + ':unknown-components', byId['summary-components'].value === 'unknown');
            check(id + ':unknown-current', current.kind === 'unknown');
            check(id + ':no-empty-current-fabrication', current.label !== 'No current execution reported');
            check(id + ':three-source-errors', attention.items.filter((item) => item.severity === 'error' && item.key.startsWith('source:')).length === 3);
          }} else if (state === 'retained_stale') {{
            check(id + ':stale-sources', sourceFreshnessModel(snapshot, NOW).every((item) => item.state === 'stale'));
            check(id + ':stale-warning-once', attention.items.filter((item) => item.key.startsWith('source:')).length === 3);
            check(id + ':retained-factory', byId['summary-factory'].value === 'Running');
            check(id + ':retained-cost', byId['summary-cost'].value === '0.000000000000000001 usd');
            check(id + ':known-current', current.kind === 'execution' && current.attempt === '7');
          }} else {{
            check(id + ':fresh-sources', sourceFreshnessModel(snapshot, NOW).every((item) => item.state === 'available'));
            const sourceWarnings = attention.items.filter((item) => item.key.startsWith('source:'));
            check(id + ':no-source-failure', sourceWarnings.length === 0, sourceWarnings);
            if (emptyState) {{
              check(id + ':known-null-current', current.kind === 'none' && current.label === 'No current execution reported');
            }} else {{
              check(id + ':current-attempt', current.kind === 'execution' && current.execution_id === 'exec:current' && current.generation === '7' && current.attempt === '7', current);
            }}
          }}
          scenarios.push(id);
        }}
      }}
    }}
    return {{ scenarios, failures }};
    "#
    );
    let result = run_driver(&server, &body, "information-matrix");
    let failures = result["failures"].as_array().unwrap();
    assert!(failures.is_empty(), "information matrix failures: {failures:?}");
    let scenarios = result["scenarios"].as_array().unwrap();
    assert_eq!(scenarios.len(), 36, "the fixed cross product must execute all 36 scenarios");
    let unique: std::collections::BTreeSet<_> = scenarios.iter().map(|value| value.as_str().unwrap()).collect();
    assert_eq!(unique.len(), 36, "scenario IDs must be unique");
    for id in &unique {
        eprintln!("cockpit_information state scenario: {id}");
    }
    let scenario_dir = persistent_evidence_dir();
    fs::create_dir_all(&scenario_dir).expect("create scenario evidence directory");
    fs::write(
        scenario_dir.join("state-scenarios.txt"),
        unique.iter().map(|id| format!("{id}\n")).collect::<String>(),
    )
    .expect("record state scenario IDs");
    assert_eq!(server.post_count(), 0, "model properties and viewing must never POST");
}

/// Exact-decimal cases at zero/subcent/large/unknown. Amounts stay exact
/// strings; no `Number` conversion, rounding or derived remaining budget.
#[test]
fn cockpit_information_decimal_properties() {
    let (server, _state) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW)],
        components: vec![ok_json(COMPONENTS)],
        factory: vec![ok_json(FACTORY_RUNTIME)],
        ..Script::default()
    });
    let body = r#"
    const NOW = 1791820805000;
    const cases = ['0', '0.000000000000000001', '18446744073709551615.99', null];
    const failures = [];
    for (const cost of cases) {
      const snapshot = {
        renderNow: NOW,
        sources: {
          overview: {
            data: {
              metadata: { sources: [] }, factory: { stopped: false }, executions: [], timeline: [],
              economics: { completeness: 'Complete', usage: cost, usage_unit: 'tokens', cost, allowance: cost, cost_unit: 'usd', observed_at_unix_ms: NOW },
            },
            lastSuccessAt: NOW, error: null, status: 'available', stale: false,
          },
          factory: { data: { status: 'Idle', repository_id: 'r', state_id: 's', stop: { stopped: false }, current_execution: null, eligible: { entries: [], total: '0', revision: 'r', next_cursor: null }, permissions: { pause: false, resume: false } }, lastSuccessAt: NOW, error: null, status: 'available', stale: false },
          components: { data: { entries: [], next_cursor: null }, lastSuccessAt: NOW, error: null, status: 'available', stale: false },
        },
      };
      const model = costModel(snapshot);
      const card = summaryCards(snapshot).find((entry) => entry.id === 'summary-cost');
      if (cost === null) {
        if (model.known || card.value !== 'unknown') failures.push({ cost, model, card });
      } else {
        if (!model.known || model.cost !== cost || typeof model.cost !== 'string') failures.push({ cost, model });
        if (card.value !== `${cost} usd`) failures.push({ cost, card });
        if (model.usage !== cost || model.allowance !== cost) failures.push({ cost, model });
      }
    }
    return { cases, failures };
    "#;
    let result = run_driver(&server, body, "decimals");
    let failures = result["failures"].as_array().unwrap();
    assert!(failures.is_empty(), "decimal property failures: {failures:?}");
    assert_eq!(result["cases"].as_array().unwrap().len(), 4);
    for case in result["cases"].as_array().unwrap() {
        eprintln!("cockpit_information decimal case: {case}");
    }
    let scenario_dir = persistent_evidence_dir();
    fs::create_dir_all(&scenario_dir).expect("create scenario evidence directory");
    fs::write(
        scenario_dir.join("decimal-cases.txt"),
        result["cases"].as_array().unwrap().iter().map(|case| format!("{case}\n")).collect::<String>(),
    )
    .expect("record decimal cases");
    assert_eq!(server.post_count(), 0);
}

/// Component identity links: unicode/slash/percent/space/injection IDs encode
/// into the existing detail route and decode exactly; the detail back link
/// returns to the registry through navigation only.
#[test]
fn cockpit_information_component_identity_links() {
    let ids = [
        "example.plain",
        "example/with/slash",
        "example.percent%name",
        "example with space",
        "example.注入",
        "example.<script>alert('x')</script>",
    ];
    let entries: Vec<serde_json::Value> = ids
        .iter()
        .enumerate()
        .map(|(index, id)| {
            serde_json::json!({
                "plugin_id": id,
                "name": format!("Component {index}"),
                "binding": {
                    "binding_id": format!("fixture.gen-{index}"),
                    "api": "plugin",
                    "api_version": 1,
                    "implementation_version": "fixture-gen",
                    "generation": index + 1,
                    "config_digest": sha256_placeholder(),
                },
                "generation": (index + 1).to_string(),
                "health": "ready",
                "freshness": "fresh",
                "completeness": "Complete",
                "available": true,
                "views": [],
            })
        })
        .collect();
    let page = serde_json::json!({
        "metadata": {
            "format": "ControlPlaneRead/v1",
            "collected_at_unix_ms": 1791820800001i64,
            "registry_revision": "1",
            "report_watermark": null,
            "sources": [{ "source": "registry", "state": "available", "detail": "", "observed_at_unix_ms": 1791820800001i64 }],
        },
        "entries": entries,
        "next_cursor": null,
    });
    let (server, _state) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW)],
        components: vec![ok_value(&page)],
        factory: vec![ok_json(FACTORY_RUNTIME)],
        detail: vec![ok_value(&unknown_component_detail("example.unknown"))],
        ..Script::default()
    });
    let body = format!(
        r#"
    const expected = {expected};
    {prelude}
    const links = [...document.querySelectorAll('a[data-focus-key^="component:"]:not([data-focus-key="component:back"])')];
    const hrefs = links.map((node) => node.getAttribute('href'));
    const decoded = hrefs.map((href) => parseHash(href).params.id);
    const mismatches = expected.filter((id, index) => decoded[index] !== id);
    const injection = expected[5];
    await navigate(formatHash({{ name: 'component', params: {{ id: injection }} }}));
    const injectionRoute = cockpit.state.route.params.id;
    const detailPage = document.querySelector('[data-page="component"]');
    const back = detailPage.querySelector('a');
    const backText = back ? back.textContent : null;
    const backHref = back ? back.getAttribute('href') : null;
    await navigate(backHref);
    const visibleAfterBack = visiblePages();
    return {{
      count: links.length, hrefs, decoded, mismatches, injectionRoute, backText, backHref,
      visibleAfterBack, scriptCount: document.querySelectorAll('script').length,
      imgCount: document.querySelectorAll('img').length,
      pageScripts: detailPage.querySelectorAll('script').length,
      pageImgs: detailPage.querySelectorAll('img').length,
    }};
    "#,
        expected = serde_json::to_string(&ids).unwrap(),
        prelude = NAVIGATE_PRELUDE,
    );
    let result = run_driver_at(&server, "#/components", &body, "component-links");
    assert_eq!(result["count"].as_u64().unwrap(), 6, "hrefs={:?}", result["hrefs"]);
    assert_eq!(result["mismatches"], serde_json::json!([]), "decoded={:?}", result["decoded"]);
    assert_eq!(result["injectionRoute"].as_str().unwrap(), ids[5]);
    assert_eq!(result["backText"].as_str().unwrap(), "← Components");
    assert_eq!(result["backHref"].as_str().unwrap(), "#/components");
    assert_eq!(result["visibleAfterBack"], serde_json::json!(["components"]));
    assert_eq!(result["scriptCount"].as_u64().unwrap(), 1, "injected text must not become an element");
    assert_eq!(result["imgCount"].as_u64().unwrap(), 0);
    assert_eq!(result["pageScripts"].as_u64().unwrap(), 0);
    assert_eq!(result["pageImgs"].as_u64().unwrap(), 0);
    assert_eq!(server.post_count(), 0, "navigation must never POST");
    let requests = server.requests();
    assert!(
        requests.iter().any(|request| request.path.contains("example.%3Cscript%3E")),
        "the detail request must encode the plugin id exactly once: {requests:?}"
    );
}

/// Retained data stays visibly stale in every section that displays it, and the
/// coverage disclosure is bound: default closed, user-open survives refresh,
/// user-close survives refresh.
#[test]
fn cockpit_information_stale_notices_and_coverage_disclosure() {
    let (server, _state) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW), Response::unavailable("source lost")],
        components: vec![ok_json(COMPONENTS)],
        factory: vec![ok_json(FACTORY_RUNTIME), Response::unavailable("source lost")],
        status: vec![ok_json(STATUS)],
        ..Script::default()
    });
    let body = r#"
    const cockpit = new Cockpit({ document, root: document.getElementById('app'), api: new DashboardApi(), intervalMs: 60000 });
    await cockpit.refresh();
    await cockpit.refresh();
    const text = (id) => document.getElementById(id)?.textContent ?? null;
    const notice = (id) => document.querySelector(`#${id} [data-stale-source]`);
    const sources = ['summary-factory', 'summary-eligible', 'summary-cost', 'current-work', 'eligible', 'economics', 'timeline', 'runtime'];
    const notices = Object.fromEntries(sources.map((id) => [id, notice(id)?.textContent ?? null]));
    const componentsCardNotice = notice('summary-components')?.textContent ?? null;

    let coverage = document.querySelector('details[data-disclosure-key="overview:coverage"]');
    const coveragePresent = coverage !== null;
    const coverageClosedInitially = coverage ? coverage.open === false : null;
    if (coverage) coverage.open = true;
    await new Promise((resolve) => setTimeout(resolve, 0));
    await cockpit.refresh();
    coverage = document.querySelector('details[data-disclosure-key="overview:coverage"]');
    const coverageOpenAfterRefresh = coverage ? coverage.open === true : false;
    coverage.open = false;
    await new Promise((resolve) => setTimeout(resolve, 0));
    await cockpit.refresh();
    coverage = document.querySelector('details[data-disclosure-key="overview:coverage"]');
    const coverageClosedAfterRefresh = coverage ? coverage.open === false : false;
    return {
      notices,
      componentsCardNotice,
      coveragePresent,
      coverageClosedInitially,
      coverageOpenAfterRefresh,
      coverageClosedAfterRefresh,
      posts: 0,
    };
    "#;
    let result = run_driver(&server, body, "stale-notices");
    assert_eq!(result["componentsCardNotice"], serde_json::Value::Null, "components source is not stale here");
    for id in ["summary-factory", "summary-eligible", "summary-cost", "current-work", "eligible", "economics", "timeline", "runtime"] {
        let text = result["notices"][id]
            .as_str()
            .unwrap_or_else(|| panic!("missing stale notice in {id}: {}", result["notices"]));
        assert!(text.contains("Showing stale data from"), "{id}: {text}");
        assert!(
            text.contains("T") && text.contains("Z") && !text.contains("unknown"),
            "{id} must name the exact retained observation time: {text}"
        );
    }
    assert_eq!(result["coveragePresent"].as_bool(), Some(true), "coverage gaps must be disclosed");
    assert_eq!(result["coverageClosedInitially"].as_bool(), Some(true), "coverage starts closed");
    assert_eq!(result["coverageOpenAfterRefresh"].as_bool(), Some(true), "an opened coverage disclosure survives refresh");
    assert_eq!(result["coverageClosedAfterRefresh"].as_bool(), Some(true), "a closed coverage disclosure stays closed after refresh");
    assert_eq!(server.post_count(), 0, "stale notices and disclosures must never POST");
}

/// More than ten attention entries show ten plus the exact remainder, and a
/// known null current execution is not replaced by non-empty overview history.
#[test]
fn cockpit_information_attention_limit_and_current_history_separation() {
    let (server, _state) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW)],
        components: vec![ok_json(COMPONENTS)],
        factory: vec![ok_json(FACTORY_RUNTIME)],
        ..Script::default()
    });
    let body = format!(
        r#"
    const overview = JSON.parse({overview});
    overview.economics.completeness = 'Complete';
    overview.metadata.sources = overview.metadata.sources.filter((source) => source.source !== 'budget');
    const components = JSON.parse({components});
    components.entries = Array.from({{ length: 14 }}, (_, index) => ({{
      plugin_id: `example.bad-${{index}}`,
      name: `Bad ${{index}}`,
      binding: {{
        binding_id: `fixture.bad-${{index}}`, api: 'plugin', api_version: 1,
        implementation_version: 'fixture-bad', generation: index + 1,
        config_digest: 'sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a',
      }},
      generation: String(index + 1),
      health: 'unavailable', freshness: 'stale', completeness: 'Partial', available: false, views: [],
    }}));
    components.next_cursor = null;
    const factory = JSON.parse({factory});
    factory.current_execution = null;
    factory.stop = {{ stopped: false, reason: null, observed_at_unix_ms: 1791820803000 }};
    const api = {{
      getOverview: () => Promise.resolve(validateOverview(overview)),
      getComponents: () => Promise.resolve(validateComponents(components)),
      getFactory: () => Promise.resolve(validateFactory(factory)),
    }};
    const first = new Cockpit({{ document, root: document.getElementById('app'), api, intervalMs: 60000, now: () => 1791820805000 }});
    await first.refresh();
    const items = [...document.querySelectorAll('.attention-item')];
    const keys = items.map((node) => node.dataset.attentionKey);
    const remainder = document.querySelector('#attention .note')?.textContent ?? null;
    const model = deriveAttention(first.snapshot());
    const total = model.total;
    const hidden = model.hiddenCount;
    first.stop();

    const second = new Cockpit({{ document, root: document.getElementById('app'), api, intervalMs: 60000, now: () => 1791820805000 }});
    await second.refresh();
    const currentWork = document.getElementById('current-work').textContent;
    const history = document.getElementById('executions').textContent;
    const attentionAfter = document.querySelectorAll('.attention-item').length;
    return {{
      itemCount: items.length, keys, remainder, total, hidden, attentionAfter,
      currentWork, history,
    }};
    "#,
        overview = serde_json::to_string(OVERVIEW).unwrap(),
        components = serde_json::to_string(COMPONENTS).unwrap(),
        factory = serde_json::to_string(FACTORY_RUNTIME).unwrap(),
    );
    let result = run_driver_at_size(&server, "#/overview", &body, "attention-limit", 1280, 1000);
    assert_eq!(result["itemCount"].as_u64().unwrap(), 10, "attention list is bounded to ten");
    assert_eq!(result["total"].as_u64().unwrap(), 14);
    assert_eq!(result["hidden"].as_u64().unwrap(), 4);
    assert_eq!(result["remainder"].as_str().unwrap(), "…and 4 additional attention entries.");
    let keys: Vec<&str> = result["keys"].as_array().unwrap().iter().map(|value| value.as_str().unwrap()).collect();
    assert_eq!(keys.iter().collect::<std::collections::BTreeSet<_>>().len(), 10, "keys must be unique");
    assert!(keys.iter().all(|key| key.starts_with("component:")), "only observed components are unhealthy here: {keys:?}");
    let current = result["currentWork"].as_str().unwrap();
    assert!(current.contains("No current execution reported"), "current={current}");
    assert!(!current.contains("exec:cabinets-are-not-shelves:1"), "history must not become current work: {current}");
    assert!(!current.contains("Attempt"), "an unmatched history attempt must not be shown: {current}");
    assert!(
        result["history"].as_str().unwrap().contains("exec:cabinets-are-not-shelves:1"),
        "the overview history remains visible only in diagnostics"
    );
    assert_eq!(server.post_count(), 0);
}

/// Eight disclosure/focus/refresh/navigation sequences. Disclosure state is
/// keyed semantically in Cockpit memory, survives refresh and route changes,
/// resets only when repository/state identity changes, and never posts.
#[test]
fn cockpit_information_expand_refresh_navigation_sequences() {
    let (server, _state) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW)],
        components: vec![ok_json(COMPONENTS)],
        factory: vec![ok_json(FACTORY_RUNTIME)],
        ..Script::default()
    });
    let body = format!(
        r#"
    const overview = JSON.parse({overview});
    overview.timeline = Array.from({{ length: 15 }}, (_, index) => ({{
      at_unix_ms: 1791820805000 - index * 1000, kind: 'note', title: `Entry ${{index}}`, detail: 'fixture',
    }}));
    overview.executions = [];
    const components = JSON.parse({components});
    const factory = JSON.parse({factory});
    factory.current_execution = null;
    let identity = {{ repository_id: 'repo-a', state_id: 'state-a' }};
    const api = {{
      getOverview: () => Promise.resolve(validateOverview(overview)),
      getComponents: () => Promise.resolve(validateComponents(components)),
      getFactory: () => Promise.resolve(validateFactory({{ ...factory, ...identity }})),
    }};
    const cockpit = new Cockpit({{ document, root: document.getElementById('app'), api, intervalMs: 60000, now: () => 1791820805000 }});
    await cockpit.refresh();
    const details = (key) => document.querySelector(`details[data-disclosure-key="${{key}}"]`);
    const isOpen = (key) => details(key)?.open === true;
    const settle = () => new Promise((resolve) => setTimeout(resolve, 0));
    const navigate = async (hash) => {{
      if (location.hash === hash) return;
      const changed = new Promise((resolve) => window.addEventListener('hashchange', resolve, {{ once: true }}));
      location.hash = hash;
      await changed;
      await settle();
    }};
    const sequences = {{}};
    const record = (id, value) => {{ sequences[id] = value; }};

    let timeline = details('overview:timeline');
    timeline.open = true;
    await settle();
    await cockpit.refresh();
    record('timeline-open-survives-refresh', isOpen('overview:timeline'));

    let diagnostics = details('overview:diagnostics');
    diagnostics.open = true;
    await settle();
    await cockpit.refresh();
    record('diagnostics-open-survives-refresh', isOpen('overview:diagnostics'));

    document.querySelector('[data-focus-key="overview:tasks"]').focus();
    await cockpit.refresh();
    record('tasks-link-focus-survives-refresh', document.activeElement?.dataset?.focusKey === 'overview:tasks');

    document.querySelector('summary[data-focus-key="disclosure:overview:timeline"]').focus();
    await cockpit.refresh();
    record('summary-focus-survives-refresh', document.activeElement?.dataset?.focusKey === 'disclosure:overview:timeline');

    details('overview:timeline').open = true;
    await settle();
    await navigate('#/components');
    await navigate('#/overview');
    record('open-survives-navigation', isOpen('overview:timeline'));

    await navigate('#/tasks');
    const hashBefore = location.hash;
    await cockpit.refresh();
    record('hash-stays-stable', location.hash === hashBefore);

    await navigate('#/overview');
    window.scrollTo(0, 120);
    const scrollBefore = window.scrollY;
    await cockpit.refresh();
    record('scroll-stays-stable', Math.abs(window.scrollY - scrollBefore) < 2);
    window.scrollTo(0, 0);

    details('overview:timeline').open = true;
    await settle();
    identity = {{ repository_id: 'repo-b', state_id: 'state-b' }};
    await cockpit.refresh();
    const resetOnIdentity = !isOpen('overview:timeline');
    details('overview:timeline').open = true;
    await settle();
    await cockpit.refresh();
    record('identity-change-resets-then-new-identity-persists', resetOnIdentity && isOpen('overview:timeline'));

    return {{ sequences }};
    "#,
        overview = serde_json::to_string(OVERVIEW).unwrap(),
        components = serde_json::to_string(COMPONENTS).unwrap(),
        factory = serde_json::to_string(FACTORY_RUNTIME).unwrap(),
    );
    let result = run_driver_at_size(&server, "#/overview", &body, "expand-sequences", 1280, 1000);
    let sequences = result["sequences"].as_object().unwrap();
    assert_eq!(sequences.len(), 8, "all eight sequences must execute: {sequences:?}");
    let scenario_dir = persistent_evidence_dir();
    fs::create_dir_all(&scenario_dir).expect("create scenario evidence directory");
    let mut sequence_log = String::new();
    for (name, value) in sequences {
        eprintln!("cockpit_information sequence: {name}={value}");
        sequence_log.push_str(&format!("{name}={value}\n"));
        assert_eq!(value.as_bool(), Some(true), "sequence {name} failed");
    }
    fs::write(scenario_dir.join("expand-sequences.txt"), sequence_log).expect("record sequences");
    assert_eq!(server.post_count(), 0, "disclosures, focus, refresh and navigation must never POST");
}

/// Layout/theme matrix: 4 widths x 2 themes x 3 Overview states = 24. The real
/// compiled stylesheet supplies the geometry; the theme is Chrome's emulated
/// preferred color scheme.
#[test]
fn cockpit_information_layout_theme_matrix() {
    let (server, _state) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW)],
        components: vec![ok_json(COMPONENTS)],
        factory: vec![ok_json(FACTORY_RUNTIME)],
        ..Script::default()
    });
    let body = format!(
        r#"
    const overview = JSON.parse({overview});
    const components = JSON.parse({components});
    const factory = JSON.parse({factory});
    const failures = [];
    const check = (id, condition, detail) => {{ if (!condition) failures.push({{ id, detail }}); }};
    const scenarios = [];
    const rect = (node) => {{ const r = node.getBoundingClientRect(); return {{ left: r.left, right: r.right, top: r.top, bottom: r.bottom, width: r.width, height: r.height }}; }};
    const hit = (node) => {{
      const r = node.getBoundingClientRect();
      const found = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
      return !!found && (found === node || node.contains(found));
    }};
    const modes = ['populated', 'stale', 'unavailable'];
    for (const mode of modes) {{
      let calls = 0;
      let ok = false;
      const api = {{
        getOverview: () => {{
          if (mode === 'unavailable') return Promise.reject(new ApiError('unavailable', 'down'));
          if (mode === 'stale') {{
            calls += 1;
            if (calls === 1) return Promise.resolve(validateOverview(overview));
            return Promise.reject(new ApiError('unavailable', 'down'));
          }}
          ok = true;
          return Promise.resolve(validateOverview(overview));
        }},
        getComponents: () => Promise.resolve(validateComponents(components)),
        getFactory: () => {{
          if (mode === 'unavailable') return Promise.reject(new ApiError('unavailable', 'down'));
          return Promise.resolve(validateFactory(factory));
        }},
      }};
      const cockpit = new Cockpit({{ document, root: document.getElementById('app'), api, intervalMs: 60000, now: () => 1791820805000 }});
      await cockpit.refresh();
      if (mode === 'stale') await cockpit.refresh();
      const id = `width=${{innerWidth}};theme=${{matchMedia('(prefers-color-scheme: light)').matches ? 'light' : 'dark'}};state=${{mode}}`;
      scenarios.push(id);
      const themeLight = matchMedia('(prefers-color-scheme: light)').matches;
      check(id + ':theme', themeLight === (window.__expectedTheme === 'light'), {{ expected: window.__expectedTheme, themeLight }});
      check(id + ':width', Math.abs(innerWidth - window.__expectedWidth) <= 1, {{ innerWidth, expected: window.__expectedWidth }});
      check(id + ':no-horizontal-overflow', document.documentElement.scrollWidth <= innerWidth + 1, document.documentElement.scrollWidth);
      const cards = [...document.querySelectorAll('.summary-card')];
      const expectedColumns = innerWidth >= 1200 ? 4 : (innerWidth > 720 ? 2 : 1);
      const cardLefts = [...new Set(cards.map((node) => Math.round(node.getBoundingClientRect().left)))];
      check(id + ':card-columns', cardLefts.length === expectedColumns, cardLefts);
      check(id + ':four-cards', cards.length === 4);
      const main = document.querySelector('.overview-main');
      const side = document.querySelector('.overview-side');
      if (innerWidth >= 1200) {{
        check(id + ':two-column-overview', Math.abs(rect(main).top - rect(side).top) < 2 && rect(main).right <= rect(side).left + 1);
      }} else {{
        check(id + ':one-column-overview', rect(side).top >= rect(main).bottom - 1);
      }}
      if (innerWidth === 1280 && mode === 'populated') {{
        const heading = document.querySelector('#current-work h2');
        check(id + ':current-heading-visible', rect(heading).top >= 0 && rect(heading).bottom <= innerHeight, {{ innerHeight, heading: rect(heading) }});
        check(id + ':header-visible', rect(document.getElementById('cockpit-header')).bottom <= innerHeight);
        check(id + ':cards-visible', rect(document.querySelector('.summary-grid')).bottom <= innerHeight);
      }}
      const pause = document.querySelector('[data-action="pause"]');
      const resume = document.querySelector('[data-action="resume"]');
      pause.scrollIntoView({{ block: 'center' }});
      const controlsHit = hit(pause) && hit(resume);
      window.scrollTo(0, 0);
      check(id + ':controls-hit-testable', controlsHit);
      check(id + ':control-target-height', rect(pause).height >= 40 && rect(resume).height >= 40);
      for (const page of document.querySelectorAll('[data-page]')) {{
        if (page.dataset.page !== 'overview') {{
          check(id + ':hidden-page-' + page.dataset.page, page.hidden && rect(page).width === 0 && rect(page).height === 0);
        }}
      }}
      if (mode === 'unavailable') {{
        check(id + ':unknown-cards', document.getElementById('summary-cost').textContent.includes('unknown'));
        check(id + ':unknown-current', document.getElementById('current-work').textContent.includes('unavailable'));
      }}
      if (mode === 'stale') {{
        check(id + ':stale-header', document.querySelector('.source-state--stale') !== null);
      }}
      cockpit.stop();
    }}
    return {{ scenarios, failures, cssLoaded: getComputedStyle(document.querySelector('.summary-grid')).display === 'grid' }};
    "#,
        overview = serde_json::to_string(OVERVIEW).unwrap(),
        components = serde_json::to_string(COMPONENTS).unwrap(),
        factory = serde_json::to_string(FACTORY_RUNTIME).unwrap(),
    );
    let widths = [390u32, 720, 1280, 1920];
    let themes = ["light", "dark"];
    let mut executed = Vec::new();
    for width in widths {
        for theme in themes {
            let script = format!(
                "window.__expectedWidth = {width}; window.__expectedTheme = '{theme}';\n{body}"
            );
            let height = if width == 1280 { 800 } else { 1000 };
            let mut result = run_driver_at_size_theme(
                &server,
                "#/overview",
                &script,
                &format!("layout-{width}-{theme}"),
                width,
                height,
                theme,
            );
            if result["cssLoaded"].as_bool() != Some(true) {
                // A launch that serialized before the compiled stylesheet
                // applied cannot answer geometry questions; retry that launch
                // once with a fresh profile. A real CSS regression repeats.
                result = run_driver_at_size_theme(
                    &server,
                    "#/overview",
                    &script,
                    &format!("layout-{width}-{theme}-retry"),
                    width,
                    height,
                    theme,
                );
            }
            assert_eq!(
                result["cssLoaded"].as_bool(),
                Some(true),
                "{width}px {theme}: compiled stylesheet did not apply"
            );
            let failures = result["failures"].as_array().unwrap();
            assert!(failures.is_empty(), "{width}px {theme} layout failures: {failures:?}");
            for scenario in result["scenarios"].as_array().unwrap() {
                executed.push(scenario.as_str().unwrap().to_string());
            }
        }
    }
    assert_eq!(executed.len(), 24, "4 widths x 2 themes x 3 states must execute");
    assert_eq!(
        executed.iter().collect::<std::collections::BTreeSet<_>>().len(),
        24,
        "layout scenario IDs must be unique"
    );
    for id in &executed {
        eprintln!("cockpit_information layout scenario: {id}");
    }
    let scenario_dir = persistent_evidence_dir();
    fs::create_dir_all(&scenario_dir).expect("create scenario evidence directory");
    fs::write(
        scenario_dir.join("layout-scenarios.txt"),
        executed.iter().map(|id| format!("{id}\n")).collect::<String>(),
    )
    .expect("record layout scenario IDs");
    assert_eq!(server.post_count(), 0, "layout and theme checks must never POST");
}

fn persistent_evidence_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("AGALMA_SCREENSHOT_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    let worktree = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("worktree root");
    let target = worktree.parent().and_then(Path::parent);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis())
        .unwrap_or(0);
    let leaf = format!("screenshots-{stamp}-{}", std::process::id());
    match target {
        Some(target) if worktree.ends_with("target/worktrees/agalma-82f.8") => {
            target.join("test-runs/agalma-82f.8").join(leaf)
        }
        _ => worktree.join("target/test-runs/agalma-82f.8").join(leaf),
    }
}

fn assert_png(path: &Path) {
    let bytes = fs::read(path).unwrap_or_else(|error| panic!("read screenshot {}: {error}", path.display()));
    assert!(bytes.len() > 1000, "screenshot {} is unexpectedly small ({} bytes)", path.display(), bytes.len());
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "screenshot {} is not a PNG", path.display());
}

/// Eight real browser screenshots for visual review: populated Overview at
/// 390/1280/1920 in both themes, plus stale and unavailable at 1280 dark.
#[test]
fn cockpit_information_captures_eight_screenshots() {
    let chrome = resolve_chrome().expect("installed Chrome (CHROME_BIN) is required");
    let dir = persistent_evidence_dir();
    fs::create_dir_all(&dir).expect("create screenshot evidence directory");

    let mut populated_factory: serde_json::Value = serde_json::from_str(FACTORY_RUNTIME).unwrap();
    let overview_value: serde_json::Value = serde_json::from_str(OVERVIEW).unwrap();
    populated_factory["current_execution"] = overview_value["executions"][0].clone();
    populated_factory["stop"] = serde_json::json!({
        "stopped": false,
        "reason": null,
        "observed_at_unix_ms": 1791820803000u64,
    });
    populated_factory["sources"][3]["state"] = serde_json::json!("available");
    let populated_factory_text = populated_factory.to_string();

    let (populated_server, _) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW)],
        components: vec![ok_json(COMPONENTS)],
        factory: vec![ok_json(&populated_factory_text)],
        status: vec![ok_json(STATUS)],
        ..Script::default()
    });
    let (stale_server, _) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW), Response::unavailable("source lost")],
        components: vec![ok_json(COMPONENTS)],
        factory: vec![ok_json(&populated_factory_text)],
        status: vec![ok_json(STATUS)],
        ..Script::default()
    });
    let (unavailable_server, _) = scripted_server(Script {
        overview: vec![Response::unavailable("source lost")],
        components: vec![ok_json(COMPONENTS)],
        factory: vec![Response::unavailable("source lost")],
        status: vec![ok_json(STATUS)],
        ..Script::default()
    });

    let page = screenshot_page();
    for server in [&populated_server, &stale_server, &unavailable_server] {
        server.register("/screenshot.html", Response::text(200, "text/html; charset=utf-8", page.clone()));
    }
    stale_server.register(
        "/screenshot-frame.html",
        Response::text(200, "text/html; charset=utf-8", screenshot_frame_page(390, 844, "?mode=stale")),
    );
    populated_server.register(
        "/screenshot-frame.html",
        Response::text(200, "text/html; charset=utf-8", screenshot_frame_page(390, 844, "?mode=populated")),
    );

    let mut shots = Vec::new();
    for theme in ["dark", "light"] {
        for width in [390u32, 1280, 1920] {
            let (server, mode, height) = if width == 390 {
                (&populated_server, "populated", 844)
            } else {
                (&populated_server, "populated", if width == 1280 { 800 } else { 1080 })
            };
            let name = format!("overview-{width}-{theme}.png");
            let path = dir.join(&name);
            let url = if width == 390 {
                format!("{}/screenshot-frame.html", server.base_url())
            } else {
                format!("{}/screenshot.html?mode={mode}", server.base_url())
            };
            let outcome = browser_support::capture_screenshot(
                &chrome, &url, &path, BUDGET_MS, &format!("shot-{width}-{theme}"), width, height, theme,
            );
            assert!(outcome.status.success(), "screenshot {name} failed: {}", outcome.stderr);
            assert_png(&path);
            shots.push(path);
        }
    }
    for (mode, server) in [("stale", &stale_server), ("unavailable", &unavailable_server)] {
        let name = format!("overview-1280-dark-{mode}.png");
        let path = dir.join(&name);
        let url = format!("{}/screenshot.html?mode={mode}", server.base_url());
        let outcome = browser_support::capture_screenshot(
            &chrome, &url, &path, BUDGET_MS, &format!("shot-1280-dark-{mode}"), 1280, 800, "dark",
        );
        assert!(outcome.status.success(), "screenshot {name} failed: {}", outcome.stderr);
        assert_png(&path);
        shots.push(path);
    }
    assert_eq!(shots.len(), 8, "eight screenshots must be captured");
    for path in &shots {
        eprintln!("cockpit_information screenshot: {}", path.display());
    }
    assert_eq!(populated_server.post_count() + stale_server.post_count() + unavailable_server.post_count(), 0);
}

// ---------------------------------------------------------------------------
// WEB.9 / CW.1 generic declared component views
// ---------------------------------------------------------------------------

const VIEW_MAX_GENERATION: &str = "18446744073709551615";
const VIEW_CONFIG_DIGEST: &str =
    "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a";

/// Run one component-view driver with extra empty-dump resilience.
///
/// `browser_support::run_browser_result_sized*` already retries once; a run
/// that serialized an empty document twice while many component cases shared
/// one test binary is retried again here with a fresh profile. A driver that
/// ran always writes a JSON result, so only a truly empty serialization is
/// retried.
fn run_component_driver(
    server: &FixtureServer,
    hash: &str,
    body: &str,
    label: &str,
    width: u32,
    height: u32,
    theme: &str,
) -> serde_json::Value {
    server.register(
        "/harness.html",
        Response::text(200, "text/html; charset=utf-8", driver_page(body)),
    );
    let (path, launch_width) = if width < 500 {
        server.register(
            "/frame.html",
            Response::text(
                200,
                "text/html; charset=utf-8",
                viewport_frame_page(width, height, hash),
            ),
        );
        ("/frame.html", 500)
    } else {
        ("/harness.html", width)
    };
    let url = if width < 500 {
        format!("{}{path}", server.base_url())
    } else {
        format!("{}{path}{hash}", server.base_url())
    };
    let chrome = resolve_chrome().expect("installed Chrome (CHROME_BIN) is required");
    let mut last = String::from("no attempt ran");
    for attempt in 0..4 {
        let outcome = browser_support::run_browser_sized_theme(
            &chrome,
            &url,
            BUDGET_MS,
            &format!("{label}-retry{attempt}"),
            launch_width,
            height,
            Some(theme),
        );
        if !outcome.status.success() {
            last = format!("status {:?}: {}", outcome.status, outcome.stderr);
            continue;
        }
        match outcome.try_result_json() {
            Ok(result) => {
                assert!(
                    result.get("harnessError").is_none(),
                    "driver {label} threw: {result}"
                );
                return result;
            }
            Err(error) => last = error,
        }
    }
    panic!("component driver {label} produced no result after retries: {last}");
}

fn view_binding(generation: &str) -> serde_json::Value {
    serde_json::json!({
        "binding_id": "fixture.views",
        "api": "plugin",
        "api_version": 1,
        "implementation_version": "fixture-views",
        "generation": generation,
        "config_digest": VIEW_CONFIG_DIGEST,
    })
}

fn view_digest(seed: u32) -> String {
    format!("sha256:{seed:064x}")
}

fn view_descriptor(
    view_id: &str,
    namespace: &str,
    kind: &str,
    schema_version: u64,
    fields: &[&str],
) -> serde_json::Value {
    serde_json::json!({
        "view_id": view_id,
        "schema_namespace": namespace,
        "kind": kind,
        "title": format!("View {view_id}"),
        "schema_version": schema_version,
        "fields": fields,
    })
}

fn view_entry(
    plugin_id: &str,
    binding: &serde_json::Value,
    namespace: &str,
    schema_version: u64,
    sequence: &str,
    payload: serde_json::Value,
    digest: &str,
) -> serde_json::Value {
    serde_json::json!({
        "format": "ReportEnvelope/v1",
        "plugin_id": plugin_id,
        "binding": binding,
        "schema_namespace": namespace,
        "schema_version": schema_version,
        "sequence": sequence,
        "reported_at_unix_ms": "1791820900000",
        "observed_at_unix_ms": "1791820900001",
        "execution_id": null,
        "durability": "durable",
        "payload": payload,
        "payload_digest": digest,
    })
}

/// Persistent evidence outside a removable worktree. The runner passes
/// `RUN_ROOT`; the fallback derives the primary checkout's `target/test-runs`.
fn component_evidence_dir(label: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis())
        .unwrap_or(0);
    let leaf = format!("component-views-{label}-{stamp}-{}", std::process::id());
    if let Ok(root) = std::env::var("RUN_ROOT") {
        if !root.is_empty() {
            let bead = std::env::var("BEAD").unwrap_or_else(|_| "agalma-51dg.2".to_string());
            return Path::new(&root).join(bead).join(leaf);
        }
    }
    let worktree = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("worktree root");
    let primary_target = worktree
        .parent()
        .and_then(Path::parent)
        .expect("primary target root");
    primary_target
        .join("test-runs")
        .join("agalma-51dg.2")
        .join(leaf)
}

fn write_evidence(label: &str, file: &str, contents: &str) -> PathBuf {
    let dir = component_evidence_dir(label);
    fs::create_dir_all(&dir).expect("create component evidence directory");
    let path = dir.join(file);
    fs::write(&path, contents).expect("write component evidence");
    path
}

fn populated_payload(kind: &str) -> serde_json::Value {
    match kind {
        "Status" => serde_json::json!({ "value": "ready" }),
        "Metrics" => serde_json::json!({ "a": "text", "b": false, "c": 0, "d": null }),
        "Table" => serde_json::json!([{ "id": "r1", "v": "one" }]),
        "Timeline" => serde_json::json!([{ "t": "t1", "m": "one" }]),
        "Alert" => serde_json::json!({ "level": "warning", "message": "attention" }),
        "ArtifactLink" => serde_json::json!({
            "label": "pkg",
            "reference": "javascript:alert(1)",
            "digest": "sha256:6666666666666666666666666666666666666666666666666666666666666666",
        }),
        other => panic!("unknown kind {other}"),
    }
}

fn kind_fields(kind: &str) -> Vec<&'static str> {
    match kind {
        "Status" => vec!["value"],
        "Metrics" => vec!["a", "b", "c", "d"],
        "Table" => vec!["id", "v"],
        "Timeline" => vec!["t", "m"],
        "Alert" => vec!["level", "message"],
        "ArtifactLink" => vec!["label", "reference", "digest"],
        other => panic!("unknown kind {other}"),
    }
}

fn populated_summary(kind: &str) -> &'static str {
    match kind {
        "Status" => "ready",
        "Metrics" => "a=text;b=false;c=0;d=null",
        "Table" => "r1,one",
        "Timeline" => "t1|one",
        "Alert" => "warning|attention",
        "ArtifactLink" => {
            "label=pkg;reference=javascript:alert(1);digest=sha256:6666666666666666666666666666666666666666666666666666666666666666"
        }
        other => panic!("unknown kind {other}"),
    }
}

const VIEW_SUMMARY_JS: &str = r#"
    function summary(view) {
      if (!view) return null;
      if (view.kind === 'Status') return view.data?.value ?? null;
      if (view.kind === 'Metrics') return (view.data?.rows ?? []).map((row) => `${row.field}=${row.text}`).join(';');
      if (view.kind === 'Table') return (view.data?.rows ?? []).map((row) => row.map((cell) => cell.text).join(',')).join(';');
      if (view.kind === 'Timeline') return (view.data?.rows ?? []).map((row) => `${row.time}|${row.text}`).join(';');
      if (view.kind === 'Alert') return view.data?.level === undefined ? '' : `${view.data.level}|${view.data.message}`;
      if (view.kind === 'ArtifactLink') return (view.data?.entries ?? []).map((row) => `${row.field}=${row.text}`).join(';');
      return null;
    }
"#;

#[test]
fn cockpit_component_views_kind_state_matrix() {
    const KINDS: [&str; 6] = ["Status", "Metrics", "Table", "Timeline", "Alert", "ArtifactLink"];
    const STATES: [&str; 6] = [
        "populated", "empty", "missing", "malformed", "schema-mismatched", "retained-stale",
    ];
    let plugin_id = "example.unfamiliar-reporter";
    let binding = view_binding("9007199254740993");
    let mut cases = Vec::new();
    for kind in KINDS {
        for state in STATES {
            let namespace = format!("private.vendor.telemetry.{}", kind.to_lowercase());
            let (payload, entry_version, entries, report_state, stale, expected_state, expected_value) =
                match state {
                    "populated" => (
                        populated_payload(kind),
                        1,
                        true,
                        "available",
                        false,
                        "rendered",
                        Some(populated_summary(kind).to_string()),
                    ),
                    "empty" => {
                        let payload = if kind == "Status" {
                            serde_json::json!({})
                        } else {
                            match kind {
                                "Table" | "Timeline" => serde_json::json!([]),
                                _ => serde_json::json!({}),
                            }
                        };
                        let expected_state = if kind == "Status" { "rendered" } else { "empty" };
                        let expected_value = if kind == "Status" { Some("unknown".to_string()) } else { Some(String::new()) };
                        (payload, 1, true, "available", false, expected_state, expected_value)
                    }
                    "missing" => (serde_json::json!({}), 1, false, "missing", false, "missing", None),
                    "malformed" => (
                        serde_json::json!("not-a-structure"),
                        1,
                        true,
                        "available",
                        false,
                        "malformed",
                        None,
                    ),
                    "schema-mismatched" => (
                        populated_payload(kind),
                        2,
                        true,
                        "available",
                        false,
                        "schema-mismatch",
                        None,
                    ),
                    "retained-stale" => (
                        populated_payload(kind),
                        1,
                        true,
                        "available",
                        true,
                        "stale",
                        Some(populated_summary(kind).to_string()),
                    ),
                    other => panic!("unknown state {other}"),
                };
            let fields: Vec<&str> = if state == "empty" && matches!(kind, "Metrics" | "Alert" | "ArtifactLink") {
                Vec::new()
            } else {
                kind_fields(kind)
            };
            let descriptor = view_descriptor(&kind.to_lowercase(), &namespace, kind, 1, &fields);
            let entries_json = if entries {
                vec![view_entry(
                    plugin_id,
                    &binding,
                    &namespace,
                    entry_version,
                    "1",
                    payload,
                    &view_digest(7),
                )]
            } else {
                Vec::new()
            };
            let mut expected = serde_json::json!({ "state": expected_state });
            if let Some(value) = expected_value {
                expected["value"] = serde_json::json!(value);
            }
            cases.push(serde_json::json!({
                "id": format!("kind={kind};state={state}"),
                "kind": kind,
                "pluginId": plugin_id,
                "binding": binding,
                "descriptor": descriptor,
                "entries": entries_json,
                "reportState": report_state,
                "stale": stale,
                "expected": expected,
            }));
        }
    }
    assert_eq!(cases.len(), 36, "six kinds x six data states");
    assert_eq!(
        cases.iter().map(|case| case["id"].as_str().unwrap()).collect::<std::collections::BTreeSet<_>>().len(),
        36,
        "scenario IDs are unique"
    );

    let router: Router = Arc::new(|_, _, _, _| None);
    let server = FixtureServer::start(router).expect("start fixture server");
    server.register("/cases.json", Response::json(200, serde_json::Value::Array(cases).to_string()));
    let body = format!(
        r#"
    const {{ resolveComponentViews }} = await import('/views.js');
    const cases = await (await fetch('/cases.json')).json();
    {summary}
    const failures = [];
    const observed = [];
    for (const c of cases) {{
      const result = resolveComponentViews({{
        pluginId: c.pluginId,
        binding: c.binding,
        descriptors: [c.descriptor],
        entries: c.entries,
        nextCursor: null,
        reportState: c.reportState,
        stale: c.stale,
      }});
      const view = result.views[0] ?? null;
      const got = {{ id: c.id, state: view?.state ?? null, value: summary(view) }};
      observed.push(got);
      if (got.state !== c.expected.state) failures.push({{ id: c.id, field: 'state', expected: c.expected.state, got: got.state }});
      if (Object.prototype.hasOwnProperty.call(c.expected, 'value') && got.value !== c.expected.value) {{
        failures.push({{ id: c.id, field: 'value', expected: c.expected.value, got: got.value }});
      }}
    }}
    return {{ observed, failures }};
    "#,
        summary = VIEW_SUMMARY_JS,
    );
    let result = run_component_driver(&server, "", &body, "component-kind-state", 1280, 2000, "");
    let failures = result["failures"].as_array().unwrap();
    assert!(failures.is_empty(), "kind/state failures: {failures:?}");
    let observed = result["observed"].as_array().unwrap();
    assert_eq!(observed.len(), 36, "all 36 kind/state cases must execute");
    let ids = observed
        .iter()
        .map(|case| case["id"].as_str().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(ids.len(), 36, "observed IDs must be unique");
    for id in &ids {
        eprintln!("component view kind/state: {id}");
    }
    let path = write_evidence(
        "kind-state",
        "kind-state-scenarios.txt",
        &ids.iter().map(|id| format!("{id}\n")).collect::<String>(),
    );
    eprintln!("component view kind/state evidence: {}", path.display());
    assert_eq!(server.post_count(), 0);
}

#[test]
fn cockpit_component_views_field_boundaries() {
    let plugin_id = "example.field-boundary";
    let binding = view_binding("7");
    let mut cases = Vec::new();
    let mut add = |id: &str, field: &str, payload: serde_json::Value, expected: serde_json::Value| {
        cases.push(serde_json::json!({
            "id": id,
            "pluginId": plugin_id,
            "binding": binding,
            "descriptor": view_descriptor("metrics", "private.boundary.metrics", "Metrics", 1, &[field]),
            "entries": [view_entry(
                plugin_id,
                &binding,
                "private.boundary.metrics",
                1,
                "1",
                payload,
                &view_digest(8),
            )],
            "expected": expected,
        }));
    };
    add("string", "value", serde_json::json!({"value":"ready"}), serde_json::json!({"text":"ready","missing":false,"truncated":false}));
    add("false", "value", serde_json::json!({"value":false}), serde_json::json!({"text":"false","missing":false,"truncated":false}));
    add("zero", "value", serde_json::json!({"value":0}), serde_json::json!({"text":"0","missing":false,"truncated":false}));
    add("null", "value", serde_json::json!({"value":null}), serde_json::json!({"text":"null","missing":false,"truncated":false}));
    add("absent", "value", serde_json::json!({"other":"x"}), serde_json::json!({"text":"missing","missing":true,"truncated":false}));
    add("dotted", "a.b", serde_json::json!({"a":{"b":"nested"}}), serde_json::json!({"text":"nested","missing":false,"truncated":false}));
    add("dotted-false", "a.b", serde_json::json!({"a":{"b":false}}), serde_json::json!({"text":"false","missing":false,"truncated":false}));
    add("dotted-zero", "a.b", serde_json::json!({"a":{"b":0}}), serde_json::json!({"text":"0","missing":false,"truncated":false}));
    add("dotted-null", "a.b", serde_json::json!({"a":{"b":null}}), serde_json::json!({"text":"null","missing":false,"truncated":false}));
    add("dotted-absent", "a.b", serde_json::json!({"a":{}}), serde_json::json!({"text":"missing","missing":true,"truncated":false}));
    add("own-property-only", "constructor", serde_json::json!({}), serde_json::json!({"text":"missing","missing":true,"truncated":false}));
    add("empty-string", "value", serde_json::json!({"value":""}), serde_json::json!({"text":"","missing":false,"truncated":false}));
    let exact = "x".repeat(4096);
    add(
        "text-at-bound",
        "value",
        serde_json::json!({"value": exact}),
        serde_json::json!({"textLength":4096,"missing":false,"truncated":false}),
    );
    let over = "x".repeat(4097);
    add(
        "text-over-bound",
        "value",
        serde_json::json!({"value": over}),
        serde_json::json!({"textLength":4096,"missing":false,"truncated":true}),
    );
    add("object-value", "value", serde_json::json!({"value":{"k":1}}), serde_json::json!({"text":"{\"k\":1}","missing":false,"truncated":false}));
    add("array-value", "value", serde_json::json!({"value":["a",1]}), serde_json::json!({"text":"[\"a\",1]","missing":false,"truncated":false}));
    assert_eq!(cases.len(), 16, "sixteen field/value boundaries");

    let router: Router = Arc::new(|_, _, _, _| None);
    let server = FixtureServer::start(router).expect("start fixture server");
    server.register("/cases.json", Response::json(200, serde_json::Value::Array(cases).to_string()));
    let body = r#"
    const { resolveComponentViews } = await import('/views.js');
    const cases = await (await fetch('/cases.json')).json();
    const failures = [];
    const observed = [];
    for (const c of cases) {
      const view = resolveComponentViews({
        pluginId: c.pluginId,
        binding: c.binding,
        descriptors: [c.descriptor],
        entries: c.entries,
        nextCursor: null,
        reportState: 'available',
      }).views[0];
      const row = view?.data?.rows?.[0] ?? null;
      const got = {
        id: c.id,
        state: view?.state ?? null,
        text: row?.text ?? null,
        textLength: row ? row.text.length : null,
        missing: row?.missing ?? null,
        truncated: row?.truncated ?? null,
      };
      observed.push(got);
      const expected = c.expected;
      for (const field of ['state', 'text', 'textLength', 'missing', 'truncated']) {
        if (!Object.prototype.hasOwnProperty.call(expected, field)) continue;
        if (got[field] !== expected[field]) failures.push({ id: c.id, field, expected: expected[field], got: got[field] });
      }
      if (got.state !== 'rendered') failures.push({ id: c.id, field: 'state', expected: 'rendered', got: got.state });
    }
    return { observed, failures };
    "#;
    let result = run_component_driver(&server, "", &body, "component-field-boundaries", 1280, 2000, "");
    let failures = result["failures"].as_array().unwrap();
    assert!(failures.is_empty(), "field boundary failures: {failures:?}");
    let observed = result["observed"].as_array().unwrap();
    assert_eq!(observed.len(), 16);
    let ids = observed
        .iter()
        .map(|case| case["id"].as_str().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(ids.len(), 16);
    for id in &ids {
        eprintln!("component field boundary: {id}");
    }
    let path = write_evidence(
        "field-boundaries",
        "field-boundary-cases.txt",
        &ids.iter().map(|id| format!("{id}\n")).collect::<String>(),
    );
    eprintln!("component field boundary evidence: {}", path.display());
    assert_eq!(server.post_count(), 0);
}

#[test]
fn cockpit_component_views_sequence_version_binding_permutations() {
    let plugin_id = "example.sequence";
    let namespace = "private.sequence.metrics";
    let page_binding = view_binding("7");
    let descriptor = view_descriptor("metrics", namespace, "Metrics", 1, &["value"]);
    let entry = |binding: serde_json::Value, version: u64, sequence: &str, value: &str, digest_seed: u32| {
        view_entry(
            plugin_id,
            &binding,
            namespace,
            version,
            sequence,
            serde_json::json!({ "value": value }),
            &view_digest(digest_seed),
        )
    };
    let mut cases = Vec::new();
    let mut add = |id: &str, entries: Vec<serde_json::Value>, expected: serde_json::Value| {
        cases.push(serde_json::json!({
            "id": id,
            "pluginId": plugin_id,
            "binding": page_binding,
            "descriptor": descriptor,
            "entries": entries,
            "expected": expected,
        }));
    };
    add(
        "greatest-sequence-not-lexical",
        vec![
            entry(page_binding.clone(), 1, "1", "one", 1),
            entry(page_binding.clone(), 1, "10", "ten", 2),
        ],
        serde_json::json!({"state":"rendered","value":"ten","sequence":"10"}),
    );
    add(
        "greatest-u64",
        vec![
            entry(page_binding.clone(), 1, "9", "nine", 3),
            entry(page_binding.clone(), 1, VIEW_MAX_GENERATION, "max", 4),
        ],
        serde_json::json!({"state":"rendered","value":"max","sequence":VIEW_MAX_GENERATION}),
    );
    add(
        "duplicate-latest-identical",
        vec![
            entry(page_binding.clone(), 1, "5", "same", 5),
            entry(page_binding.clone(), 1, "5", "same", 5),
        ],
        serde_json::json!({"state":"rendered","value":"same","sequence":"5"}),
    );
    add(
        "duplicate-latest-conflict",
        vec![
            entry(page_binding.clone(), 1, "5", "first", 6),
            entry(page_binding.clone(), 1, "5", "second", 7),
        ],
        serde_json::json!({"state":"conflict"}),
    );
    add(
        "latest-version-mismatch-no-resurrection",
        vec![
            entry(page_binding.clone(), 1, "4", "old", 8),
            entry(page_binding.clone(), 2, "5", "new", 9),
        ],
        serde_json::json!({"state":"schema-mismatch"}),
    );
    let mut binding = page_binding.clone();
    binding["config_digest"] = serde_json::json!("sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    add(
        "config-digest-conflict",
        vec![entry(binding, 1, "1", "x", 10)],
        serde_json::json!({"state":"conflict"}),
    );
    let mut binding = page_binding.clone();
    binding["binding_id"] = serde_json::json!("fixture.other");
    add("binding-id-conflict", vec![entry(binding, 1, "1", "x", 11)], serde_json::json!({"state":"conflict"}));
    let mut binding = page_binding.clone();
    binding["api"] = serde_json::json!("other-api");
    add("api-conflict", vec![entry(binding, 1, "1", "x", 12)], serde_json::json!({"state":"conflict"}));
    let mut binding = page_binding.clone();
    binding["implementation_version"] = serde_json::json!("fixture-other");
    add("implementation-conflict", vec![entry(binding, 1, "1", "x", 13)], serde_json::json!({"state":"conflict"}));
    let mut binding = page_binding.clone();
    binding["generation"] = serde_json::json!("6");
    add("generation-conflict", vec![entry(binding, 1, "1", "x", 14)], serde_json::json!({"state":"conflict"}));
    add("invalid-sequence", vec![entry(page_binding.clone(), 1, "01", "x", 15)], serde_json::json!({"state":"malformed"}));
    let mut legacy = page_binding.clone();
    legacy["generation"] = serde_json::json!(7);
    add(
        "legacy-safe-numeric-generation",
        vec![entry(legacy, 1, "1", "safe", 16)],
        serde_json::json!({"state":"rendered","value":"safe","sequence":"1"}),
    );
    assert_eq!(cases.len(), 12, "twelve sequence/version/binding permutations");

    let router: Router = Arc::new(|_, _, _, _| None);
    let server = FixtureServer::start(router).expect("start fixture server");
    server.register("/cases.json", Response::json(200, serde_json::Value::Array(cases).to_string()));
    let body = r#"
    const { resolveComponentViews } = await import('/views.js');
    const cases = await (await fetch('/cases.json')).json();
    const failures = [];
    const observed = [];
    for (const c of cases) {
      const view = resolveComponentViews({
        pluginId: c.pluginId,
        binding: c.binding,
        descriptors: [c.descriptor],
        entries: c.entries,
        nextCursor: null,
        reportState: 'available',
      }).views[0];
      const got = {
        id: c.id,
        state: view?.state ?? null,
        value: view?.data?.rows?.[0]?.text ?? null,
        sequence: view?.identity?.sequence ?? null,
      };
      observed.push(got);
      if (got.state !== c.expected.state) failures.push({ id: c.id, field: 'state', expected: c.expected.state, got: got.state });
      if (Object.prototype.hasOwnProperty.call(c.expected, 'value') && got.value !== c.expected.value) {
        failures.push({ id: c.id, field: 'value', expected: c.expected.value, got: got.value });
      }
      if (Object.prototype.hasOwnProperty.call(c.expected, 'sequence') && got.sequence !== c.expected.sequence) {
        failures.push({ id: c.id, field: 'sequence', expected: c.expected.sequence, got: got.sequence });
      }
    }
    return { observed, failures };
    "#;
    let result = run_component_driver(&server, "", &body, "component-sequence-binding", 1280, 2000, "");
    let failures = result["failures"].as_array().unwrap();
    assert!(failures.is_empty(), "sequence/version/binding failures: {failures:?}");
    let observed = result["observed"].as_array().unwrap();
    assert_eq!(observed.len(), 12);
    let ids = observed
        .iter()
        .map(|case| case["id"].as_str().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(ids.len(), 12);
    for id in &ids {
        eprintln!("component sequence permutation: {id}");
    }
    let path = write_evidence(
        "sequence-binding",
        "sequence-version-binding-cases.txt",
        &ids.iter().map(|id| format!("{id}\n")).collect::<String>(),
    );
    eprintln!("component sequence evidence: {}", path.display());
    assert_eq!(server.post_count(), 0);
}

#[test]
fn cockpit_component_views_route_epoch_late_response_traces() {
    let traces = [
        ("a-commit", "trace.a", false),
        ("a-pending-b-late", "trace.b", false),
        ("a-reject-b-success", "trace.b", false),
        ("a-late-after-b-request", "trace.b", false),
        ("a-b-a-old-response", "trace.a", false),
        ("a-first-failure-b", "trace.b", false),
        ("away-a-late", "", false),
        ("b-first-failure-a", "trace.a", false),
    ];
    let router: Router = Arc::new(|_, _, _, _| None);
    let server = FixtureServer::start(router).expect("start fixture server");
    let body = r#"
    const trace = await (await fetch('/trace.json')).json();
    const root = document.createElement('div');
    document.body.appendChild(root);
    const marker = (phase) => { document.getElementById('harness-result').textContent = JSON.stringify({ phase, id: trace.id }); };
    marker('boot');
    const calls = [];
    const detailFor = (id) => ({
      metadata: {
        format: 'ControlPlaneRead/v1',
        collected_at_unix_ms: 1791820900000,
        registry_revision: '1',
        report_watermark: null,
        sources: [
          { source: 'registry', state: 'available', detail: '', observed_at_unix_ms: 1791820900000 },
          { source: 'reports', state: 'empty', detail: '', observed_at_unix_ms: 1791820900000 },
        ],
      },
      binding: {
        binding_id: id, api: 'plugin', api_version: 1, implementation_version: 'trace',
        generation: '1', config_digest: 'sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a',
      },
      descriptors: [], entries: [], next_cursor: null,
    });
    const api = {
      getOverview: () => Promise.reject(new ApiError('unsupported', 'trace overview absent')),
      getComponents: () => Promise.reject(new ApiError('unsupported', 'trace components absent')),
      getFactory: () => Promise.reject(new ApiError('unsupported', 'trace factory absent')),
      getStatus: () => Promise.reject(new ApiError('unsupported', 'trace status absent')),
      getComponentDetail: (id) => new Promise((resolve, reject) => {
        calls.push({ id, resolve, reject, settled: false });
      }),
    };
    const settle = () => new Promise((resolve) => setTimeout(resolve, 0));
    const waitCall = async (id, index = 0) => {
      for (let attempt = 0; attempt < 600; attempt += 1) {
        const matches = calls.filter((call) => call.id === id);
        if (matches.length > index && !matches[index].settled) return matches[index];
        await settle();
      }
      throw new Error(`no pending detail call for ${id} index ${index}; calls=${JSON.stringify(calls.map((call) => call.id))}`);
    };
    const resolveCall = (call, id) => { call.settled = true; call.resolve(detailFor(id)); };
    const rejectCall = (call) => { call.settled = true; call.reject(new ApiError('unavailable', 'trace failure')); };
    const cockpit = new Cockpit({ document, root, api, intervalMs: 60000 });
    cockpit.start();
    await cockpit.refresher.activeCycle;
    const navigate = async (hash) => {
      const changed = new Promise((resolve) => window.addEventListener('hashchange', resolve, { once: true }));
      location.hash = hash;
      await changed;
      await settle();
    };
    const pageId = () => cockpit.state.detail.page?.binding?.binding_id ?? null;
    let lateCommitted = false;
    let lateSubject = null;
    let afterLate = null;

    if (trace.id === 'a-commit') {
      await navigate('#/components/trace.a');
      resolveCall(await waitCall('trace.a'), 'trace.a');
      await cockpit.refresher.activeCycle;
    } else if (trace.id === 'a-pending-b-late') {
      marker('navigate-a');
      await navigate('#/components/trace.a');
      marker('wait-a');
      const a = await waitCall('trace.a');
      marker('navigate-b');
      await navigate('#/components/trace.b');
      marker('resolve-a');
      resolveCall(a, 'trace.a');
      marker('await-cycle-a');
      await cockpit.refresher.activeCycle;
      marker('after-late');
      afterLate = pageId();
      lateSubject = afterLate;
      lateCommitted = afterLate === 'trace.a';
      marker('refresh-b');
      const bCycle = cockpit.refresh();
      marker('wait-b');
      resolveCall(await waitCall('trace.b'), 'trace.b');
      marker('await-cycle-b');
      await bCycle;
      marker('done');
    } else if (trace.id === 'a-reject-b-success') {
      await navigate('#/components/trace.a');
      const a = await waitCall('trace.a');
      await navigate('#/components/trace.b');
      rejectCall(a);
      await cockpit.refresher.activeCycle;
      afterLate = pageId();
      lateCommitted = afterLate === 'trace.a';
      const bCycle = cockpit.refresh();
      resolveCall(await waitCall('trace.b'), 'trace.b');
      await bCycle;
    } else if (trace.id === 'a-late-after-b-request') {
      await navigate('#/components/trace.a');
      const a = await waitCall('trace.a');
      await navigate('#/components/trace.b');
      resolveCall(a, 'trace.a');
      await cockpit.refresher.activeCycle;
      const bCycle = cockpit.refresh();
      const b = await waitCall('trace.b');
      afterLate = pageId();
      resolveCall(b, 'trace.b');
      await bCycle;
    } else if (trace.id === 'a-b-a-old-response') {
      marker('aba-navigate-a');
      await navigate('#/components/trace.a');
      marker('aba-wait-a1');
      const firstA = await waitCall('trace.a', 0);
      marker('aba-navigate-b');
      await navigate('#/components/trace.b');
      marker('aba-navigate-a2');
      await navigate('#/components/trace.a');
      marker('aba-resolve-a1');
      resolveCall(firstA, 'trace.a');
      marker('aba-await-a1');
      await cockpit.refresher.activeCycle;
      afterLate = pageId();
      lateCommitted = afterLate === 'trace.a';
      marker('aba-refresh-a2');
      const secondA = cockpit.refresh();
      marker('aba-wait-a2');
      resolveCall(await waitCall('trace.a', 1), 'trace.a');
      marker('aba-await-a2');
      await secondA;
      marker('aba-done');
    } else if (trace.id === 'a-first-failure-b') {
      await navigate('#/components/trace.a');
      rejectCall(await waitCall('trace.a'));
      await cockpit.refresher.activeCycle;
      await navigate('#/components/trace.b');
      resolveCall(await waitCall('trace.b'), 'trace.b');
      await cockpit.refresher.activeCycle;
    } else if (trace.id === 'away-a-late') {
      await navigate('#/components/trace.a');
      const a = await waitCall('trace.a');
      await navigate('#/overview');
      resolveCall(a, 'trace.a');
      await cockpit.refresher.activeCycle;
      afterLate = pageId();
      lateCommitted = afterLate === 'trace.a';
    } else if (trace.id === 'b-first-failure-a') {
      await navigate('#/components/trace.b');
      rejectCall(await waitCall('trace.b'));
      await cockpit.refresher.activeCycle;
      await navigate('#/components/trace.a');
      resolveCall(await waitCall('trace.a'), 'trace.a');
      await cockpit.refresher.activeCycle;
    }

    const result = {
      id: trace.id,
      final: pageId(),
      finalSubject: cockpit.state.detail.pluginId,
      detailEpoch: cockpit.state.detail.epoch,
      detailError: cockpit.snapshot().sources.detail?.error?.message ?? null,
      lateSubject,
      lateCommitted,
      route: cockpit.state.route.name,
    };
    cockpit.stop();
    return result;
    "#;
    let mut evidence = String::new();
    let mut seen = std::collections::BTreeSet::new();
    for (id, expected_final, expected_late) in traces {
        server.register("/trace.json", Response::json(200, serde_json::json!({"id": id}).to_string()));
        let result = run_component_driver(&server, "", body, &format!("component-route-{id}"), 1280, 2000, "");
        assert_eq!(result["id"].as_str().unwrap(), id);
        let final_subject = result["final"].as_str().unwrap_or("");
        assert_eq!(final_subject, expected_final, "trace {id} final subject: {result}");
        assert_eq!(
            result["lateCommitted"].as_bool(),
            Some(expected_late),
            "trace {id} late commit: {result}"
        );
        if id == "away-a-late" {
            assert_eq!(result["finalSubject"], serde_json::Value::Null, "trace {id}: {result}");
        }
        assert_eq!(server.post_count(), 0, "trace {id} must not POST");
        assert!(seen.insert(id));
        evidence.push_str(&format!("{id}={}\n", result["final"]));
        eprintln!("component route trace: {id} final={final_subject}");
    }
    assert_eq!(seen.len(), 8, "eight route/epoch traces");
    let path = write_evidence("route-traces", "route-epoch-traces.txt", &evidence);
    eprintln!("component route trace evidence: {}", path.display());
}

fn long_component_detail(table_rows: usize, timeline_rows: usize) -> serde_json::Value {
    let binding = view_binding("42");
    let rows: Vec<serde_json::Value> = (0..table_rows)
        .map(|index| {
            serde_json::json!({
                "id": format!("row-{index:03}"),
                "state": if index % 2 == 0 { "ready" } else { "unknown" },
                "detail": format!("observed value {index} with a long tail {}", "x".repeat(24)),
            })
        })
        .collect();
    let events: Vec<serde_json::Value> = (0..timeline_rows)
        .map(|index| {
            serde_json::json!({
                "at": format!("2026-10-19T00:00:{index:02}Z"),
                "message": format!("Observation {index}"),
                "detail": "preserved order",
            })
        })
        .collect();
    serde_json::json!({
        "metadata": {
            "format": "ControlPlaneRead/v1",
            "collected_at_unix_ms": 1791820900000i64,
            "registry_revision": "1",
            "report_watermark": "100",
            "sources": [
                { "source": "registry", "state": "available", "detail": "", "observed_at_unix_ms": 1791820900000i64 },
                { "source": "reports", "state": "available", "detail": "", "observed_at_unix_ms": 1791820900000i64 }
            ]
        },
        "binding": binding,
        "descriptors": [
            view_descriptor("rows", "private.long.rows", "Table", 1, &["id", "state", "detail"]),
            view_descriptor("events", "private.long.events", "Timeline", 1, &["at", "message", "detail"]),
        ],
        "entries": [
            view_entry("example.unfamiliar-reporter", &binding, "private.long.rows", 1, "1", serde_json::Value::Array(rows), &view_digest(21)),
            view_entry("example.unfamiliar-reporter", &binding, "private.long.events", 1, "2", serde_json::json!({ "entries": events }), &view_digest(22)),
        ],
        "next_cursor": null,
    })
}

#[test]
fn cockpit_component_views_layout_theme_stale_and_expansion_matrix() {
    let long = long_component_detail(40, 30);
    let (server, _state) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW)],
        components: vec![ok_json(COMPONENTS)],
        detail: vec![ok_value(&long), Response::unavailable("detail source lost")],
        ..Script::default()
    });
    let body = r#"
    const cockpit = startCockpit({
      document, root: document.getElementById('app'), api: new DashboardApi(), intervalMs: 60000,
    });
    await cockpit.refresher.activeCycle;
    const settle = () => new Promise((resolve) => setTimeout(resolve, 0));
    const expand = document.querySelector('details[data-disclosure-key$=":rows"]');
    const timelineDetails = document.querySelector('details[data-disclosure-key$=":timeline"]');
    const defaultRows = document.querySelectorAll('.component-view[data-kind="Table"] > .table tbody tr').length;
    const defaultTimeline = document.querySelectorAll('.component-view[data-kind="Timeline"] > .timeline > li').length;
    expand.open = true;
    timelineDetails.open = true;
    await settle();
    const summary = expand.querySelector('summary');
    summary.focus();
    window.scrollTo(0, Math.min(240, document.documentElement.scrollHeight));
    const scrollBefore = window.scrollY;
    const focusBefore = document.activeElement?.dataset?.focusKey ?? null;
    await cockpit.refresh();
    const expandAfter = document.querySelector('details[data-disclosure-key$=":rows"]');
    const timelineAfter = document.querySelector('details[data-disclosure-key$=":timeline"]');
    const scrollAfter = window.scrollY;
    const focusAfter = document.activeElement?.dataset?.focusKey ?? null;
    const table = document.querySelector('.component-view[data-kind="Table"] > .table');
    const rect = (node) => {
      const r = node.getBoundingClientRect();
      return { width: r.width, height: r.height };
    };
    const hiddenPages = [...document.querySelectorAll('[data-page]')]
      .filter((node) => node.dataset.page !== 'component')
      .map((node) => ({
        name: node.dataset.page,
        hidden: node.hidden,
        display: getComputedStyle(node).display,
        rect: rect(node),
      }));
    return {
      width: innerWidth,
      theme: matchMedia('(prefers-color-scheme: light)').matches ? 'light' : 'dark',
      expectedTheme: window.__expectedTheme,
      expectedWidth: window.__expectedWidth,
      scrollWidth: document.documentElement.scrollWidth,
      defaultRows,
      defaultTimeline,
      expandedRows: document.querySelectorAll('.component-view[data-kind="Table"] tbody tr').length,
      expandedTimeline: document.querySelectorAll('.component-view[data-kind="Timeline"] .timeline > li').length,
      expandOpenAfter: expandAfter.open,
      timelineOpenAfter: timelineAfter.open,
      scrollBefore,
      scrollAfter,
      focusBefore,
      focusAfter,
      localStale: document.querySelectorAll('[data-stale-source]').length,
      topStale: document.querySelectorAll('#component-detail > .callout--stale').length,
      viewStates: [...document.querySelectorAll('.component-view')].map((node) => ({ kind: node.dataset.kind, state: node.dataset.state })),
      tableClient: table.clientWidth,
      tableScroll: table.scrollWidth,
      hiddenPages,
      bindingCollapsed: document.querySelector('details[data-disclosure-key$=":binding"]').open === false,
      technicalCollapsed: [...document.querySelectorAll('.component-view details[data-disclosure-key$=":technical"]')].every((node) => node.open === false),
      expandKey: expandAfter.dataset.disclosureKey,
      disclosures: [...cockpit.state.disclosures.entries()],
      posts: 0,
    };
    "#;
    let mut executed = Vec::new();
    for width in [390u32, 720, 1280, 1920] {
        for theme in ["light", "dark"] {
            let script = format!(
                "window.__expectedWidth = {width}; window.__expectedTheme = '{theme}';\n{body}"
            );
            let height = if width == 1280 { 900 } else { 1000 };
            let result = run_component_driver(
                &server,
                "#/components/example.unfamiliar-reporter",
                &script,
                &format!("component-layout-{width}-{theme}"),
                width,
                height,
                theme,
            );
            assert_eq!(result["width"].as_f64().unwrap(), f64::from(width));
            assert_eq!(result["theme"].as_str().unwrap(), theme);
            assert_eq!(result["hiddenPages"].as_array().unwrap().len(), 5);
            for page in result["hiddenPages"].as_array().unwrap() {
                assert_eq!(page["hidden"].as_bool(), Some(true), "{width} {theme} {page:?}");
                assert_eq!(page["display"].as_str().unwrap(), "none", "{width} {theme} {page:?}");
                assert_eq!(page["rect"]["width"].as_f64().unwrap(), 0.0, "{width} {theme} {page:?}");
                assert_eq!(page["rect"]["height"].as_f64().unwrap(), 0.0, "{width} {theme} {page:?}");
            }
            assert!(
                result["scrollWidth"].as_f64().unwrap() <= f64::from(width) + 1.0,
                "{width} {theme} page overflow: {result}"
            );
            assert_eq!(result["defaultRows"].as_u64(), Some(24), "{width} {theme}");
            assert_eq!(result["defaultTimeline"].as_u64(), Some(24), "{width} {theme}");
            assert_eq!(result["expandedRows"].as_u64(), Some(40), "{width} {theme}");
            assert_eq!(result["expandedTimeline"].as_u64(), Some(30), "{width} {theme}");
            assert_eq!(result["expandOpenAfter"].as_bool(), Some(true), "{width} {theme}: {result}");
            assert_eq!(result["timelineOpenAfter"].as_bool(), Some(true), "{width} {theme}: {result}");
            assert_eq!(result["focusBefore"].as_str(), result["focusAfter"].as_str(), "{width} {theme} focus");
            assert_eq!(result["localStale"].as_u64(), Some(2), "{width} {theme} stale labels: {result}");
            assert_eq!(result["topStale"].as_u64(), Some(1), "{width} {theme} top stale notice");
            assert_eq!(result["bindingCollapsed"].as_bool(), Some(true), "{width} {theme}");
            assert_eq!(result["technicalCollapsed"].as_bool(), Some(true), "{width} {theme}");
            assert!(
                result["tableScroll"].as_f64().unwrap() >= result["tableClient"].as_f64().unwrap(),
                "{width} {theme} table must remain a bounded scroll box: {result}"
            );
            assert!(
                (result["scrollAfter"].as_f64().unwrap() - result["scrollBefore"].as_f64().unwrap()).abs() < 2.0,
                "{width} {theme} scroll must stay stable: {result}"
            );
            let states: Vec<&str> = result["viewStates"].as_array().unwrap()
                .iter().map(|view| view["state"].as_str().unwrap()).collect();
            assert_eq!(states, vec!["stale", "stale"], "{width} {theme}");
            executed.push(format!("width={width};theme={theme};state=stale-expanded"));
            assert_eq!(server.post_count(), 0);
        }
    }
    assert_eq!(executed.len(), 8, "4 widths x 2 themes must execute");
    assert_eq!(executed.iter().collect::<std::collections::BTreeSet<_>>().len(), 8);
    for id in &executed {
        eprintln!("component layout scenario: {id}");
    }
    let path = write_evidence(
        "layout",
        "layout-scenarios.txt",
        &executed.iter().map(|id| format!("{id}\n")).collect::<String>(),
    );
    eprintln!("component layout evidence: {}", path.display());
}

fn component_screenshot_page() -> String {
    r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<title>Agalma cockpit</title>
<link rel="stylesheet" href="/dashboard.css"></head>
<body><main id="app"><noscript>This local cockpit requires JavaScript.</noscript></main>
<script type="module">
import { startCockpit } from '/app.js';
const params = new URLSearchParams(location.search);
const mode = params.get('mode') ?? 'populated';
const cockpit = startCockpit({ document, root: document.getElementById('app'), intervalMs: 60000 });
await cockpit.refresher.activeCycle;
if (mode === 'stale') await cockpit.refresh();
if (mode === 'expanded') {
  for (const node of document.querySelectorAll('details.component-expand')) node.open = true;
  await new Promise((resolve) => setTimeout(resolve, 0));
}
</script></body></html>
"#
    .to_string()
}

fn component_screenshot_frame_page(width: u32, height: u32, src: &str) -> String {
    let offset = 500u32.saturating_sub(width) / 2;
    format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>viewport</title>
<style>html,body{{margin:0;border:0;background:#000}} iframe{{display:block;border:0;margin-left:{offset}px;width:{width}px;height:{height}px}}</style>
</head><body><iframe src="/component-shot.html{src}"></iframe></body></html>
"#
    )
}

#[test]
fn cockpit_component_views_captures_eight_component_screenshots() {
    let chrome = resolve_chrome().expect("installed Chrome (CHROME_BIN) is required");
    let dir = component_evidence_dir("screenshots");
    fs::create_dir_all(&dir).expect("create component screenshot directory");
    let populated_fixture: serde_json::Value = serde_json::from_str(COMPONENT_VIEWS).unwrap();
    let long_fixture = long_component_detail(40, 30);
    let unknown_fixture = unknown_component_detail("example.not-registered");

    let (populated_server, _) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW)],
        components: vec![ok_json(COMPONENTS)],
        detail: vec![ok_value(&populated_fixture)],
        ..Script::default()
    });
    let (stale_server, _) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW)],
        components: vec![ok_json(COMPONENTS)],
        detail: vec![ok_value(&populated_fixture), Response::unavailable("detail source lost")],
        ..Script::default()
    });
    let (unknown_server, _) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW)],
        components: vec![ok_json(COMPONENTS)],
        detail: vec![ok_value(&unknown_fixture)],
        ..Script::default()
    });
    let (long_server, _) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW)],
        components: vec![ok_json(COMPONENTS)],
        detail: vec![ok_value(&long_fixture)],
        ..Script::default()
    });
    let page = component_screenshot_page();
    for server in [&populated_server, &stale_server, &unknown_server, &long_server] {
        server.register("/component-shot.html", Response::text(200, "text/html; charset=utf-8", page.clone()));
    }
    populated_server.register(
        "/component-shot-frame.html",
        Response::text(
            200,
            "text/html; charset=utf-8",
            component_screenshot_frame_page(
                390,
                844,
                "?mode=populated#/components/example.unfamiliar-reporter",
            ),
        ),
    );

    let mut shots = Vec::new();
    for theme in ["dark", "light"] {
        let name = format!("component-populated-390-{theme}.png");
        let path = dir.join(&name);
        let url = format!("{}/component-shot-frame.html", populated_server.base_url());
        let outcome = browser_support::capture_screenshot(
            &chrome,
            &url,
            &path,
            BUDGET_MS,
            &format!("component-shot-390-{theme}"),
            390,
            844,
            theme,
        );
        assert!(outcome.status.success(), "screenshot {name} failed: {}", outcome.stderr);
        assert_png(&path);
        shots.push(path);
    }
    let direct = [
        (720u32, 900u32, "dark", "populated", &populated_server, "example.unfamiliar-reporter"),
        (1280, 900, "dark", "populated", &populated_server, "example.unfamiliar-reporter"),
        (1920, 1080, "dark", "populated", &populated_server, "example.unfamiliar-reporter"),
        (1280, 900, "dark", "stale", &stale_server, "example.unfamiliar-reporter"),
        (1280, 900, "dark", "unknown", &unknown_server, "example.not-registered"),
        (1280, 900, "dark", "expanded", &long_server, "example.unfamiliar-reporter"),
    ];
    for (width, height, theme, mode, server, plugin_id) in direct {
        let name = format!("component-{mode}-{width}-{theme}.png");
        let path = dir.join(&name);
        let url = format!(
            "{}/component-shot.html?mode={mode}#/components/{plugin_id}",
            server.base_url()
        );
        let outcome = browser_support::capture_screenshot(
            &chrome,
            &url,
            &path,
            BUDGET_MS,
            &format!("component-shot-{mode}-{width}-{theme}"),
            width,
            height,
            theme,
        );
        assert!(outcome.status.success(), "screenshot {name} failed: {}", outcome.stderr);
        assert_png(&path);
        shots.push(path);
    }
    assert_eq!(shots.len(), 8, "eight component screenshots must be captured");
    let listing = shots
        .iter()
        .map(|path| format!("{}\n", path.display()))
        .collect::<String>();
    let listing_path = dir.join("screenshots.txt");
    fs::write(&listing_path, &listing).expect("write screenshot listing");
    for path in &shots {
        eprintln!("component view screenshot: {}", path.display());
    }
    eprintln!("component view screenshot listing: {}", listing_path.display());
    assert_eq!(
        populated_server.post_count() + stale_server.post_count() + unknown_server.post_count() + long_server.post_count(),
        0
    );
}

#[test]
fn cockpit_component_views_unknown_plugin_private_namespace() {
    let (server, _state) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW)],
        components: vec![ok_json(COMPONENTS)],
        detail: vec![ok_json(COMPONENT_VIEWS)],
        ..Script::default()
    });
    let result = run_component_driver(
        &server,
        "#/components/example.unfamiliar-reporter",
        r#"
    const cockpit = startCockpit({ document, root: document.getElementById('app'), api: new DashboardApi(), intervalMs: 60000 });
    await cockpit.refresher.activeCycle;
    const page = document.querySelector('[data-page="component"]');
    const views = [...page.querySelectorAll('.component-view')].map((node) => ({
      id: node.dataset.viewId,
      kind: node.dataset.kind,
      state: node.dataset.state,
    }));
    return {
      route: cockpit.state.route.name,
      routeId: cockpit.state.route.params.id,
      text: page.textContent,
      views,
      notRegistered: page.textContent.includes('not registered in the hub registry'),
      scripts: page.querySelectorAll('script').length,
      images: page.querySelectorAll('img').length,
      handlers: page.querySelectorAll('[onerror]').length,
      artifactLinks: page.querySelectorAll('.component-view[data-kind="ArtifactLink"] a').length,
      bindingCollapsed: page.querySelector('details[data-disclosure-key$=":binding"]').open === false,
    };
    "#,
        "component-unknown-plugin-views",
        1280,
        2000,
        "",
    );
    assert_eq!(result["route"].as_str().unwrap(), "component");
    assert_eq!(result["routeId"].as_str().unwrap(), "example.unfamiliar-reporter");
    assert_eq!(result["notRegistered"].as_bool(), Some(false));
    let views = result["views"].as_array().unwrap();
    assert_eq!(views.len(), 6, "all six declared kinds render without a plugin branch");
    let ids: Vec<&str> = views.iter().map(|view| view["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["status", "metrics", "rows", "events", "alert", "artifact"]);
    assert!(views.iter().all(|view| view["state"] == "rendered"), "views={views:?}");
    let text = result["text"].as_str().unwrap();
    for namespace in [
        "private.vendor.telemetry.status",
        "private.vendor.telemetry.metrics",
        "private.vendor.telemetry.rows",
        "private.vendor.telemetry.events",
        "private.vendor.telemetry.alert",
        "private.vendor.telemetry.artifact",
    ] {
        assert!(text.contains(namespace), "private namespace {namespace} missing: {text}");
    }
    assert_eq!(result["scripts"].as_u64(), Some(0));
    assert_eq!(result["images"].as_u64(), Some(0));
    assert_eq!(result["handlers"].as_u64(), Some(0));
    assert_eq!(result["artifactLinks"].as_u64(), Some(0), "artifact metadata must never become a link");
    assert_eq!(result["bindingCollapsed"].as_bool(), Some(true));
    assert_eq!(server.post_count(), 0, "generic rendering must never POST");
}

#[test]
fn cockpit_component_views_injection_bounded_text_and_exact_identities() {
    let detail: serde_json::Value = serde_json::from_str(COMPONENT_VIEWS).unwrap();
    let (server, _state) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW)],
        components: vec![ok_json(COMPONENTS)],
        detail: vec![ok_value(&detail)],
        ..Script::default()
    });
    let result = run_component_driver(
        &server,
        "#/components/example.unfamiliar-reporter",
        r#"
    const { resolveComponentViews } = await import('/views.js');
    const cockpit = startCockpit({ document, root: document.getElementById('app'), api: new DashboardApi(), intervalMs: 60000 });
    await cockpit.refresher.activeCycle;
    const page = document.querySelector('[data-page="component"]');
    const unsafe = (root) => ({
      hrefs: root.querySelectorAll('[href]').length,
      srcs: root.querySelectorAll('[src]').length,
      scripts: root.querySelectorAll('script').length,
      images: root.querySelectorAll('img').length,
      handlers: root.querySelectorAll('[onerror]').length,
    });
    const artifact = page.querySelector('.component-view[data-kind="ArtifactLink"]');
    const raw = page.querySelector('details[data-disclosure-key$=":binding"]');
    const table = page.querySelector('.component-view[data-kind="Table"] > .table');
    const tableText = table.textContent;
    const artifactText = artifact.textContent;
    const unsafeGlobal = unsafe(document);
    const unsafePage = unsafe(page);
    const unsafeArtifact = unsafe(artifact);
    const exact = {
      generation: page.textContent.includes('18446744073709551615'),
      sequence: raw.textContent.includes('18446744073709551615'),
      falseAndZero: page.textContent.includes('ready: false') && page.textContent.includes('count: 0'),
      nullValue: page.textContent.includes('absent: null'),
      tableNull: tableText.includes('null'),
      artifactInert: artifactText.includes('javascript:alert(\'artifact\')')
        && artifactText.includes('sha256:6666666666666666666666666666666666666666666666666666666666666666')
        && artifactText.includes('Download unavailable'),
    };
    // Unsafe numeric generations are not compared as if they were integral.
    const unsafeBinding = {
      binding_id: 'fixture.unsafe', api: 'plugin', api_version: 1,
      implementation_version: 'fixture-views', generation: 9007199254740992,
      config_digest: 'sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a',
    };
    const unsafeView = resolveComponentViews({
      pluginId: 'example.unsafe',
      binding: unsafeBinding,
      descriptors: [{ view_id: 'v', schema_namespace: 'private.unsafe', kind: 'Status', title: 'v', schema_version: 1, fields: ['value'] }],
      entries: [{
        format: 'ReportEnvelope/v1', plugin_id: 'example.unsafe', binding: unsafeBinding,
        schema_namespace: 'private.unsafe', schema_version: 1, sequence: '1',
        reported_at_unix_ms: '1', observed_at_unix_ms: '1', execution_id: null,
        durability: 'durable', payload: { value: 'ready' },
        payload_digest: 'sha256:8888888888888888888888888888888888888888888888888888888888888888',
      }],
      nextCursor: null,
      reportState: 'available',
    }).views[0];
    return {
      text: page.textContent,
      exact,
      unsafeGlobal,
      unsafePage,
      unsafeArtifact,
      unsafeState: unsafeView.state,
      bounded: document.querySelectorAll('pre').length > 0,
      posts: 0,
    };
    "#,
        "component-injection-exact",
        1280,
        2000,
        "",
    );
    let text = result["text"].as_str().unwrap();
    assert!(text.contains("<script>alert('status')</script>"), "hostile status text must stay text: {text}");
    assert!(text.contains("<script>alert('row')</script>"), "hostile row text must stay text: {text}");
    assert!(text.contains("<img src=x onerror=alert(1)>"), "hostile label must stay text: {text}");
    assert!(text.contains("javascript:alert('artifact')"), "dangerous URL must stay inert text: {text}");
    assert_eq!(result["unsafeGlobal"]["scripts"].as_u64(), Some(1), "only the driver module may exist");
    assert_eq!(result["unsafeGlobal"]["images"].as_u64(), Some(0));
    assert_eq!(result["unsafeGlobal"]["handlers"].as_u64(), Some(0));
    assert_eq!(result["unsafePage"]["scripts"].as_u64(), Some(0));
    assert_eq!(result["unsafePage"]["images"].as_u64(), Some(0));
    assert_eq!(result["unsafePage"]["handlers"].as_u64(), Some(0));
    assert_eq!(result["unsafeArtifact"]["hrefs"].as_u64(), Some(0));
    assert_eq!(result["unsafeArtifact"]["srcs"].as_u64(), Some(0));
    for key in ["generation", "sequence", "falseAndZero", "nullValue", "tableNull", "artifactInert"] {
        assert_eq!(result["exact"][key].as_bool(), Some(true), "exact identity {key}: {result}");
    }
    assert_eq!(result["unsafeState"].as_str(), Some("conflict"), "unsafe numeric generation must not be rounded: {result}");
    assert_eq!(result["bounded"].as_bool(), Some(true));
    assert_eq!(server.post_count(), 0);
    assert!(server.requests().iter().all(|request| request.path.starts_with('/')));
}

// ---------------------------------------------------------------------------
// Status MVP browser acceptance (agalma-51dg.12)
// ---------------------------------------------------------------------------

fn status_metadata(observed_at: i64) -> serde_json::Value {
    serde_json::json!({
        "format": "ControlPlaneRead/v1",
        "collected_at_unix_ms": observed_at,
        "registry_revision": "9007199254740993",
        "report_watermark": null,
        "sources": [
            { "source": "registry", "state": "available", "detail": "", "observed_at_unix_ms": observed_at },
            { "source": "reports", "state": "empty", "detail": "", "observed_at_unix_ms": observed_at }
        ]
    })
}

fn status_entry(
    plugin_id: &str,
    name: &str,
    generation: &str,
    status: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "plugin_id": plugin_id,
        "name": name,
        "binding": {
            "binding_id": plugin_id,
            "api": "plugin",
            "api_version": 1,
            "implementation_version": "status-fixture",
            "generation": generation,
            "config_digest": sha256_placeholder()
        },
        "generation": generation,
        "health": "ready",
        "freshness": "fresh",
        "completeness": "Complete",
        "available": true,
        "views": [],
        "status": status
    })
}

fn status_components_page(entries: Vec<serde_json::Value>) -> String {
    serde_json::json!({
        "metadata": status_metadata(1791820800001),
        "entries": entries,
        "next_cursor": null
    })
    .to_string()
}

fn status_detail(
    plugin_id: &str,
    generation: &str,
    status: serde_json::Value,
    descriptor: bool,
    reports_state: &str,
) -> serde_json::Value {
    let descriptors = if descriptor {
        serde_json::json!([{
            "view_id": "status",
            "schema_namespace": format!("{plugin_id}.status"),
            "kind": "Status",
            "title": "Declared status",
            "schema_version": 1,
            "fields": ["ready"]
        }])
    } else {
        serde_json::json!([])
    };
    serde_json::json!({
        "metadata": {
            "format": "ControlPlaneRead/v1",
            "collected_at_unix_ms": 1791820800002i64,
            "registry_revision": "9007199254740993",
            "report_watermark": null,
            "sources": [
                { "source": "registry", "state": "available", "detail": "", "observed_at_unix_ms": 1791820800002i64 },
                { "source": "reports", "state": reports_state, "detail": "status fixture", "observed_at_unix_ms": 1791820800002i64 }
            ]
        },
        "binding": {
            "binding_id": plugin_id,
            "api": "plugin",
            "api_version": 1,
            "implementation_version": "status-fixture",
            "generation": generation,
            "config_digest": sha256_placeholder()
        },
        "descriptors": descriptors,
        "entries": [],
        "next_cursor": null,
        "status": status
    })
}

fn healthy_status() -> serde_json::Value {
    serde_json::json!({
        "origin": "discovered",
        "generation": "9007199254740993",
        "desired": "running",
        "lifecycle": "ready",
        "lifecycle_source": "live-registry",
        "lifecycle_observed_at_unix_ms": "1791820800000",
        "health": "ready",
        "health_observed_at_unix_ms": "1791820800001",
        "health_detail": "listener bound",
        "last_error": null,
        "reporting": "fresh",
        "incomplete": false
    })
}

#[test]
fn cockpit_status_mvp_list_renders_evidence_not_legacy_health() {
    let long_id = format!("example.{}", "long-identity-".repeat(12));
    let builtin = status_entry(
        "component:harness",
        "Embedded Harness",
        "0",
        serde_json::json!({
            "origin": "built-in",
            "generation": null,
            "desired": "stopped",
            "lifecycle": null,
            "lifecycle_source": "none",
            "lifecycle_observed_at_unix_ms": null,
            "health": null,
            "health_observed_at_unix_ms": null,
            "health_detail": null,
            "last_error": null,
            "reporting": "unavailable",
            "incomplete": false
        }),
    );
    let failed = status_entry(
        "example.failed",
        "Failed Component",
        "2",
        serde_json::json!({
            "origin": "discovered",
            "generation": "2",
            "desired": "stopped",
            "lifecycle": "failed",
            "lifecycle_source": "live-registry",
            "lifecycle_observed_at_unix_ms": "1791820790000",
            "health": "ready",
            "health_observed_at_unix_ms": "1791820780000",
            "health_detail": "last heartbeat",
            "last_error": "process exited with status 1",
            "reporting": "stale",
            "incomplete": true
        }),
    );
    let page = status_components_page(vec![
        status_entry(&long_id, "Long Reporter", "9007199254740993", healthy_status()),
        failed,
        builtin,
    ]);
    let (server, _) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW)],
        components: vec![ok_json(&page)],
        ..Script::default()
    });
    let result = run_driver_at(
        &server,
        "#/components",
        r#"
    const cockpit = new Cockpit({ document, root: document.getElementById('app'), api: new DashboardApi(), intervalMs: 60000 });
    await cockpit.refresh();
    const rows = [...document.querySelectorAll('#components tbody tr')].map((row) => ({
      cells: [...row.querySelectorAll('td')].map((cell) => cell.textContent),
      link: row.querySelector('a')?.getAttribute('href') ?? null,
    }));
    return { rows, page: document.getElementById('components').textContent };
    "#,
        "status-list",
    );
    assert!(result.get("harnessError").is_none(), "{result}");
    let rows = result["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 3, "{result}");

    let long = rows[0]["cells"].as_array().unwrap();
    assert!(long[0].as_str().unwrap().contains("Long Reporter"));
    assert_eq!(long[1], "Discovered");
    assert_eq!(long[2], "9007199254740993");
    assert_eq!(long[3], "ready");
    assert_eq!(long[4], "ready");
    assert_eq!(long[5], "fresh");
    assert!(long[6].as_str().unwrap().contains("health"), "{long:?}");
    assert_eq!(
        rows[0]["link"].as_str().unwrap(),
        format!("#/components/{}", long_id)
    );

    let failed = rows[1]["cells"].as_array().unwrap();
    assert_eq!(failed[3], "failed");
    assert!(
        failed[4].as_str().unwrap().contains("stale observation"),
        "a stale Ready heartbeat is not an unqualified healthy badge: {failed:?}"
    );
    assert!(failed[5].as_str().unwrap().contains("Stale"), "{failed:?}");
    assert!(failed[5].as_str().unwrap().contains("incomplete"), "{failed:?}");
    assert!(
        failed[7].as_str().unwrap().contains("process exited with status 1"),
        "{failed:?}"
    );

    let builtin = rows[2]["cells"].as_array().unwrap();
    assert_eq!(builtin[1], "Built-in");
    assert_eq!(builtin[2], "none (built-in)", "no synthetic installed generation");
    assert!(builtin[3].as_str().unwrap().contains("Embedded component; lifecycle not observed"));
    assert_eq!(builtin[4], "Unknown", "legacy Ready must not become health");
    assert_eq!(builtin[5], "Not reporting");
    assert_eq!(builtin[6], "Unknown");
    assert_eq!(server.post_count(), 0, "status reads never mutate");
}

#[test]
fn cockpit_status_mvp_detail_separates_lifecycle_health_and_missing_reports() {
    let status = serde_json::json!({
        "origin": "discovered",
        "generation": "42",
        "desired": "running",
        "lifecycle": "failed",
        "lifecycle_source": "live-registry",
        "lifecycle_observed_at_unix_ms": "1791820790000",
        "health": "ready",
        "health_observed_at_unix_ms": "1791820780000",
        "health_detail": "last heartbeat before exit",
        "last_error": "process exited with status 1",
        "reporting": "stale",
        "incomplete": true
    });
    let detail = status_detail("example.status.failed", "42", status, true, "missing");
    let (server, _) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW)],
        components: vec![ok_json(&status_components_page(vec![status_entry(
            "example.status.failed",
            "Failed Status",
            "42",
            serde_json::json!(null),
        )]))],
        detail: vec![ok_value(&detail)],
        ..Script::default()
    });
    let result = run_driver_at(
        &server,
        "#/components/example.status.failed",
        r#"
    const cockpit = new Cockpit({ document, root: document.getElementById('app'), api: new DashboardApi(), intervalMs: 60000 });
    await cockpit.refresh();
    const summary = document.querySelector('[data-status-source]');
    return {
      source: summary?.dataset.statusSource ?? null,
      overall: summary?.dataset.overall ?? null,
      text: summary?.textContent ?? '',
      page: document.getElementById('component-detail').textContent,
      disclosures: document.querySelectorAll('details[data-disclosure-key]').length,
    };
    "#,
        "status-detail",
    );
    assert!(result.get("harnessError").is_none(), "{result}");
    assert_eq!(result["source"], "detail");
    assert_eq!(result["overall"], "attention");
    let text = result["text"].as_str().unwrap();
    assert!(text.contains("Identity"), "{text}");
    assert!(text.contains("example.status.failed"), "{text}");
    assert!(text.contains("Observed lifecycle"), "{text}");
    assert!(text.contains("failed"), "{text}");
    assert!(text.contains("Reported health"), "{text}");
    assert!(
        text.contains("ready (stale observation)"),
        "health is kept but qualified: {text}"
    );
    assert!(text.contains("Not reporting") || text.contains("Stale"), "{text}");
    assert!(text.contains("process exited with status 1"), "{text}");
    assert!(
        text.contains("not healthy even if an earlier heartbeat reported Ready"),
        "{text}"
    );
    // Missing reports still retain the status summary and declared views.
    let page = result["page"].as_str().unwrap();
    assert!(page.contains("Declared status"), "{page}");
    assert!(page.contains("Binding"), "binding diagnostics remain: {page}");
    assert!(
        result["disclosures"].as_u64().unwrap() >= 1,
        "binding metadata stays collapsed in diagnostics"
    );
    assert_eq!(server.post_count(), 0);
}

#[test]
fn cockpit_status_mvp_cold_failure_falls_back_only_to_exact_list_match() {
    let status = serde_json::json!({
        "origin": "discovered",
        "generation": "7",
        "desired": "running",
        "lifecycle": "ready",
        "lifecycle_source": "live-registry",
        "lifecycle_observed_at_unix_ms": "1791820800000",
        "health": "degraded",
        "health_observed_at_unix_ms": "1791820800001",
        "health_detail": "binding degraded",
        "last_error": null,
        "reporting": "fresh",
        "incomplete": false
    });
    let components = status_components_page(vec![status_entry(
        "example.list-only",
        "List Only",
        "7",
        status,
    )]);
    let (server, _) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW)],
        components: vec![ok_json(&components)],
        detail: vec![Response::unavailable("detail source lost")],
        ..Script::default()
    });
    let matched = run_driver_at(
        &server,
        "#/components/example.list-only",
        r#"
    const cockpit = new Cockpit({ document, root: document.getElementById('app'), api: new DashboardApi(), intervalMs: 60000 });
    await cockpit.refresh();
    await cockpit.refresh();
    const summary = document.querySelector('[data-status-source]');
    return {
      source: summary?.dataset.statusSource ?? null,
      text: summary?.textContent ?? '',
      detail: document.getElementById('component-detail').textContent,
    };
    "#,
        "status-cold-match",
    );
    assert!(matched.get("harnessError").is_none(), "{matched}");
    assert_eq!(matched["source"], "list-fallback");
    let text = matched["text"].as_str().unwrap();
    assert!(text.contains("previous Components-page observation"), "{text}");
    assert!(text.contains("example.list-only"), "{text}");
    assert!(text.contains("degraded"), "{text}");
    assert!(text.contains("unavailable"), "the actual read error is shown: {text}");

    let unknown = run_driver_at(
        &server,
        "#/components/example.not-on-page",
        r#"
    const cockpit = new Cockpit({ document, root: document.getElementById('app'), api: new DashboardApi(), intervalMs: 60000 });
    await cockpit.refresh();
    await cockpit.refresh();
    const summary = document.querySelector('[data-status-source]');
    return {
      source: summary?.dataset.statusSource ?? null,
      text: summary?.textContent ?? '',
      page: document.getElementById('component-detail').textContent,
    };
    "#,
        "status-cold-unknown",
    );
    assert!(unknown.get("harnessError").is_none(), "{unknown}");
    assert_eq!(unknown["source"], "unknown");
    let text = unknown["text"].as_str().unwrap();
    assert!(text.contains("Status is unknown"), "{text}");
    assert!(
        !text.contains("degraded") && !text.contains("List Only"),
        "another component's status must never be borrowed: {text}"
    );
    assert!(
        unknown["page"].as_str().unwrap().contains("← Components"),
        "navigation stays usable without any status source"
    );
}

#[test]
fn cockpit_status_mvp_retained_detail_is_labelled_stale() {
    let status = healthy_status();
    let detail = status_detail("example.retained", "1", status, true, "available");
    let (server, _) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW)],
        components: vec![ok_json(&status_components_page(vec![status_entry(
            "example.retained",
            "Retained",
            "1",
            serde_json::json!(null),
        )]))],
        detail: vec![ok_value(&detail), Response::unavailable("detail source lost")],
        ..Script::default()
    });
    let result = run_driver_at(
        &server,
        "#/components/example.retained",
        r#"
    const cockpit = new Cockpit({ document, root: document.getElementById('app'), api: new DashboardApi(), intervalMs: 60000 });
    await cockpit.refresh();
    const first = cockpit.state.detail.page?.status?.health ?? null;
    await cockpit.refresh();
    const summary = document.querySelector('[data-status-source]');
    return {
      first,
      source: summary?.dataset.statusSource ?? null,
      page: document.getElementById('component-detail').textContent,
      text: summary?.textContent ?? '',
      subject: cockpit.state.detail.page?.binding?.binding_id ?? null,
      generation: cockpit.state.detail.page?.status?.generation ?? null,
    };
    "#,
        "status-retained",
    );
    assert!(result.get("harnessError").is_none(), "{result}");
    assert_eq!(result["first"], "ready");
    assert_eq!(result["subject"], "example.retained");
    assert_eq!(result["generation"], "9007199254740993");
    assert_eq!(result["source"], "retained-detail");
    let text = result["text"].as_str().unwrap();
    assert!(text.contains("last successful detail read"), "{text}");
    assert!(text.contains("ready"), "{text}");
    let page = result["page"].as_str().unwrap();
    assert!(page.contains("Showing stale data"), "stale notice is visible: {page}");
}

#[test]
fn cockpit_status_mvp_keyboard_navigation_and_long_identity() {
    let long_id = format!("example.{}", "a-very-long-component-identity-".repeat(8));
    let (server, _) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW)],
        components: vec![ok_json(&status_components_page(vec![status_entry(
            &long_id,
            "Long Name Reporter",
            "9007199254740993",
            healthy_status(),
        )]))],
        detail: vec![ok_value(&status_detail(
            &long_id,
            "9007199254740993",
            healthy_status(),
            true,
            "available",
        ))],
        ..Script::default()
    });
    let result = run_driver_at(
        &server,
        "#/components",
        &format!(r#"
    const cockpit = new Cockpit({{ document, root: document.getElementById('app'), api: new DashboardApi(), intervalMs: 60000 }});
    cockpit.start();
    await cockpit.refresher.activeCycle;
    const link = document.querySelector('#components tbody a');
    link.focus();
    const linkFocused = document.activeElement === link;
    const href = link.getAttribute('href');
    const changed = new Promise((resolve) => window.addEventListener('hashchange', resolve, {{ once: true }}));
    location.hash = href;
    await changed;
    await cockpit.refresher.activeCycle;
    cockpit.stop();
    const status = document.querySelector('[data-status-source]');
    const summary = document.querySelector('#component-detail details[data-disclosure-key] summary');
    summary?.focus();
    const summaryFocused = document.activeElement === summary;
    const summaryKey = summary?.dataset.focusKey ?? null;
    const details = document.querySelector('#component-detail details[data-disclosure-key]');
    const initiallyOpen = details?.open ?? null;
    summary?.click();
    const opened = details?.open ?? null;
    return {{
      longId: {long_id:?},
      linkFocused,
      href,
      summaryFocused,
      summaryKey,
      initiallyOpen,
      opened,
      pageText: document.getElementById('component-detail').textContent,
      bodyOverflow: document.documentElement.scrollWidth - window.innerWidth,
      statusText: status?.textContent ?? '',
    }};
    "#),
        "status-keyboard",
    );
    assert!(result.get("harnessError").is_none(), "{result}");
    assert_eq!(result["linkFocused"], true);
    assert_eq!(
        result["href"].as_str().unwrap(),
        format!("#/components/{long_id}")
    );
    assert_eq!(result["summaryFocused"], true);
    assert!(result["summaryKey"].as_str().unwrap().contains(&long_id));
    assert_eq!(result["initiallyOpen"], false);
    assert_eq!(result["opened"], true);
    assert!(
        result["pageText"].as_str().unwrap().contains(&long_id),
        "long identity remains readable in the page"
    );
    assert!(
        result["bodyOverflow"].as_i64().unwrap() <= 1,
        "long identity does not force page-wide horizontal overflow"
    );
}

fn status_evidence_dir(label: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis())
        .unwrap_or(0);
    let leaf = format!("status-mvp-{label}-{stamp}-{}", std::process::id());
    if let Ok(root) = std::env::var("RUN_ROOT") {
        if !root.is_empty() {
            let bead = std::env::var("BEAD").unwrap_or_else(|_| "agalma-51dg.12".to_string());
            return Path::new(&root).join(bead).join(leaf);
        }
    }
    let worktree = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("worktree root");
    let primary_target = worktree
        .parent()
        .and_then(Path::parent)
        .expect("primary target root");
    primary_target
        .join("test-runs")
        .join("agalma-51dg.12")
        .join(leaf)
}

#[test]
fn cockpit_status_mvp_captures_four_status_screenshots() {
    let chrome = resolve_chrome().expect("installed Chrome (CHROME_BIN) is required");
    let dir = status_evidence_dir("screenshots");
    fs::create_dir_all(&dir).expect("create status screenshot directory");

    let list_page = status_components_page(vec![
        status_entry(
            "example.unfamiliar-reporter",
            "Unfamiliar Reporter",
            "9007199254740993",
            healthy_status(),
        ),
        status_entry(
            "example.failed",
            "Failed Component",
            "2",
            serde_json::json!({
                "origin": "discovered",
                "generation": "2",
                "desired": "stopped",
                "lifecycle": "failed",
                "lifecycle_source": "live-registry",
                "lifecycle_observed_at_unix_ms": "1791820790000",
                "health": "ready",
                "health_observed_at_unix_ms": "1791820780000",
                "health_detail": "last heartbeat",
                "last_error": "process exited with status 1",
                "reporting": "stale",
                "incomplete": true
            }),
        ),
        status_entry(
            "component:harness",
            "Embedded Harness",
            "0",
            serde_json::json!({
                "origin": "built-in",
                "generation": null,
                "desired": "stopped",
                "lifecycle": null,
                "lifecycle_source": "none",
                "lifecycle_observed_at_unix_ms": null,
                "health": null,
                "health_observed_at_unix_ms": null,
                "health_detail": null,
                "last_error": null,
                "reporting": "unavailable",
                "incomplete": false
            }),
        ),
    ]);
    let missing_status = serde_json::json!({
        "origin": "discovered",
        "generation": "3",
        "desired": "running",
        "lifecycle": "ready",
        "lifecycle_source": "live-registry",
        "lifecycle_observed_at_unix_ms": "1791820800000",
        "health": null,
        "health_observed_at_unix_ms": null,
        "health_detail": null,
        "last_error": null,
        "reporting": "unavailable",
        "incomplete": false
    });
    let failed_status = serde_json::json!({
        "origin": "discovered",
        "generation": "2",
        "desired": "stopped",
        "lifecycle": "failed",
        "lifecycle_source": "live-registry",
        "lifecycle_observed_at_unix_ms": "1791820790000",
        "health": "ready",
        "health_observed_at_unix_ms": "1791820780000",
        "health_detail": "last heartbeat before exit",
        "last_error": "process exited with status 1",
        "reporting": "stale",
        "incomplete": true
    });

    let (list_server, _) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW)],
        components: vec![ok_json(&list_page)],
        ..Script::default()
    });
    let (missing_server, _) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW)],
        components: vec![ok_json(&list_page)],
        detail: vec![ok_value(&status_detail(
            "example.missing-reports",
            "3",
            missing_status,
            true,
            "missing",
        ))],
        ..Script::default()
    });
    let (failed_server, _) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW)],
        components: vec![ok_json(&list_page)],
        detail: vec![ok_value(&status_detail(
            "example.failed",
            "2",
            failed_status,
            true,
            "available",
        ))],
        ..Script::default()
    });
    let page = component_screenshot_page();
    for server in [&list_server, &missing_server, &failed_server] {
        server.register(
            "/component-shot.html",
            Response::text(200, "text/html; charset=utf-8", page.clone()),
        );
    }
    missing_server.register(
        "/component-shot-frame.html",
        Response::text(
            200,
            "text/html; charset=utf-8",
            component_screenshot_frame_page(
                390,
                844,
                "?mode=populated#/components/example.missing-reports",
            ),
        ),
    );

    let mut shots = Vec::new();
    for (name, server, hash, width, height) in [
        (
            "status-list-desktop-1280-dark.png",
            &list_server,
            "#/components",
            1280u32,
            900u32,
        ),
        (
            "status-detail-missing-reports-desktop-1280-dark.png",
            &missing_server,
            "#/components/example.missing-reports",
            1280,
            900,
        ),
        (
            "status-detail-failed-stale-desktop-1280-dark.png",
            &failed_server,
            "#/components/example.failed",
            1280,
            900,
        ),
    ] {
        let path = dir.join(name);
        let url = format!("{}/component-shot.html{hash}", server.base_url());
        let outcome = browser_support::capture_screenshot(
            &chrome,
            &url,
            &path,
            BUDGET_MS,
            name,
            width,
            height,
            "dark",
        );
        assert!(outcome.status.success(), "screenshot {name} failed: {}", outcome.stderr);
        assert_png(&path);
        shots.push(path);
    }
    let path = dir.join("status-detail-missing-reports-mobile-390-dark.png");
    let outcome = browser_support::capture_screenshot(
        &chrome,
        &format!("{}/component-shot-frame.html", missing_server.base_url()),
        &path,
        BUDGET_MS,
        "status-mobile-detail",
        390,
        844,
        "dark",
    );
    assert!(outcome.status.success(), "mobile screenshot failed: {}", outcome.stderr);
    assert_png(&path);
    shots.push(path);

    assert_eq!(shots.len(), 4, "four required status screenshots");
    let listing = shots
        .iter()
        .map(|path| format!("{}\n", path.display()))
        .collect::<String>();
    let listing_path = dir.join("screenshots.txt");
    fs::write(&listing_path, &listing).expect("write screenshot listing");
    for shot in &shots {
        eprintln!("status MVP screenshot: {}", shot.display());
    }
    eprintln!("status MVP screenshot listing: {}", listing_path.display());
    assert_eq!(list_server.post_count() + missing_server.post_count() + failed_server.post_count(), 0);
}

#[test]
fn cockpit_status_mvp_navigation_failure_sequences() {
    let subjects = ["seq.a", "seq.b", "seq.c"];
    let components = status_components_page(
        subjects
            .iter()
            .map(|id| {
                status_entry(
                    id,
                    id,
                    "1",
                    serde_json::json!({
                        "origin": "discovered",
                        "generation": "1",
                        "desired": "running",
                        "lifecycle": "ready",
                        "lifecycle_source": "live-registry",
                        "lifecycle_observed_at_unix_ms": "1791820800000",
                        "health": "ready",
                        "health_observed_at_unix_ms": "1791820800001",
                        "health_detail": "ok",
                        "last_error": null,
                        "reporting": "fresh",
                        "incomplete": false
                    }),
                )
            })
            .collect(),
    );
    let router: Router = Arc::new(|_, _, _, _| None);
    let server = FixtureServer::start(router).expect("start sequence server");
    let body = r#"
    const subjects = ['seq.a', 'seq.b', 'seq.c'];
    const componentsPage = __COMPONENTS__;
    const detailFor = (id) => ({
      metadata: {
        format: 'ControlPlaneRead/v1',
        collected_at_unix_ms: 1791820800002,
        registry_revision: '1',
        report_watermark: null,
        sources: [
          { source: 'registry', state: 'available', detail: '', observed_at_unix_ms: 1791820800002 },
          { source: 'reports', state: 'empty', detail: '', observed_at_unix_ms: 1791820800002 },
        ],
      },
      binding: {
        binding_id: id, api: 'plugin', api_version: 1, implementation_version: 'seq',
        generation: '1', config_digest: 'sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a',
      },
      descriptors: [],
      entries: [],
      next_cursor: null,
      status: {
        origin: 'discovered', generation: '1', desired: 'running', lifecycle: 'ready',
        lifecycle_source: 'live-registry', lifecycle_observed_at_unix_ms: '1791820800000',
        health: 'ready', health_observed_at_unix_ms: '1791820800001', health_detail: 'ok',
        last_error: null, reporting: 'fresh', incomplete: false,
      },
    });
    let failDetail = false;
    const api = {
      getOverview: () => Promise.reject(new ApiError('unsupported', 'sequence overview absent')),
      getComponents: () => Promise.resolve(componentsPage),
      getFactory: () => Promise.reject(new ApiError('unsupported', 'sequence factory absent')),
      getStatus: () => Promise.reject(new ApiError('unsupported', 'sequence status absent')),
      getComponentDetail: (id) => failDetail
        ? Promise.reject(new ApiError('unavailable', 'injected detail failure'))
        : Promise.resolve(detailFor(id)),
    };
    let seed = 0x51d6_0012;
    const rand = () => {
      seed ^= seed << 13; seed ^= seed >>> 7; seed ^= seed << 17;
      return seed >>> 0;
    };
    const failures = [];
    const trace = [];
    const root = document.createElement('div');
    document.body.appendChild(root);
    location.hash = '#/components/seq.a';
    const cockpit = new Cockpit({ document, root, api, intervalMs: 60000 });
    cockpit.start();
    await cockpit.refresh();
    const settle = () => new Promise((resolve) => setTimeout(resolve, 0));
    const navigate = async (hash) => {
      if (location.hash === hash) {
        await cockpit.refresh();
        return;
      }
      location.hash = hash;
      for (let attempt = 0; attempt < 200; attempt += 1) {
        await settle();
        if (`#/${cockpit.state.route.name === 'component' ? `components/${cockpit.state.route.params.id}` : cockpit.state.route.name}` === hash) break;
      }
      await cockpit.refresh();
    };
    for (let seq = 0; seq < 64; seq += 1) {
      const start = `#/components/${subjects[seq % subjects.length]}`;
      if (location.hash !== start) await navigate(start);
      cockpit.state.detail.page = null;
      cockpit.state.detail.pluginId = null;
      cockpit.state.detail.cursor = null;
      cockpit.state.detail.epoch += 1;
      await cockpit.refresh();
      for (let step = 0; step < 12; step += 1) {
        const op = rand() % 7;
        if (op === 0 || op === 1 || op === 6) {
          await navigate(`#/components/${subjects[rand() % subjects.length]}`);
        } else if (op === 2) {
          failDetail = false;
          await cockpit.refresh();
        } else if (op === 3) {
          failDetail = true;
          await cockpit.refresh();
        } else if (op === 4) {
          await navigate('#/components');
        } else {
          failDetail = rand() % 2 === 0;
          await cockpit.refresh();
        }
        const route = cockpit.state.route;
        const page = cockpit.state.detail.page;
        const label = `seq${seq}:step${step}:${route.name}:${route.params?.id ?? ''}`;
        trace.push(label);
        if (page && route.name !== 'component') {
          failures.push(`${label}: retained detail after leaving the route`);
        }
        if (page && route.name === 'component' && page.binding?.binding_id !== route.params.id) {
          failures.push(`${label}: cross-subject detail ${page.binding?.binding_id}`);
        }
        if (route.name === 'component') {
          const summary = document.querySelector('[data-status-source]');
          const text = summary?.textContent ?? '';
          if (!summary) failures.push(`${label}: no status summary`);
          if (!text.includes(route.params.id)) failures.push(`${label}: current identity missing`);
          for (const other of subjects) {
            if (other !== route.params.id && text.includes(other)) {
              failures.push(`${label}: borrowed ${other}`);
            }
          }
          if (page && page.status?.generation !== '1') {
            failures.push(`${label}: wrong generation ${page.status?.generation}`);
          }
          if (summary?.dataset.statusSource === 'list-fallback'
              && !text.includes('previous Components-page observation')) {
            failures.push(`${label}: unlabelled list fallback`);
          }
        }
      }
    }
    cockpit.stop();
    return {
      sequences: 64,
      steps: trace.length,
      failureCount: failures.length,
      failures: failures.slice(0, 8),
    };
    "#
    .replace("__COMPONENTS__", &components);
    let result = run_driver_at_size_budget(
        &server,
        "#/components/seq.a",
        &body,
        "status-sequences",
        1280,
        2000,
        120_000,
    );
    assert!(result.get("harnessError").is_none(), "{result}");
    assert_eq!(result["sequences"], 64, "{result}");
    assert_eq!(result["steps"], 64 * 12, "at least 12 steps per sequence");
    assert_eq!(result["failureCount"], 0, "{}", result["failures"]);
    assert_eq!(server.post_count(), 0, "navigation and refresh never mutate");
}

#[test]
fn cockpit_status_mvp_api_validator_rejects_unknown_status_vocabularies() {
    let (server, _) = scripted_server(Script {
        overview: vec![ok_json(OVERVIEW)],
        ..Script::default()
    });
    let result = run_driver(
        &server,
        r#"
    const status = {
      origin: 'discovered', generation: '9007199254740993', desired: 'running',
      lifecycle: 'ready', lifecycle_source: 'live-registry',
      lifecycle_observed_at_unix_ms: '1791820800000', health: 'ready',
      health_observed_at_unix_ms: '1791820800001', health_detail: 'ok', last_error: null,
      reporting: 'fresh', incomplete: false,
    };
    const rejected = (mutate) => {
      const candidate = structuredClone(status);
      mutate(candidate);
      try { validateComponentStatus(candidate); return null; }
      catch (error) { return error.kind; }
    };
    return {
      valid: validateComponentStatus(status),
      missing: validateComponentStatus(undefined),
      origin: rejected((value) => { value.origin = 'builtin'; }),
      lifecycle: rejected((value) => { value.lifecycle = 'healthy'; }),
      reporting: rejected((value) => { value.reporting = 'ok'; }),
      generation: rejected((value) => { value.generation = 1.5; }),
      timestamp: rejected((value) => { value.health_observed_at_unix_ms = '-1'; }),
      incomplete: rejected((value) => { value.incomplete = 'yes'; }),
    };
    "#,
        "status-validator",
    );
    assert!(result.get("harnessError").is_none(), "{result}");
    assert_eq!(result["valid"]["generation"], "9007199254740993");
    assert_eq!(result["missing"], serde_json::Value::Null);
    for key in ["origin", "lifecycle", "reporting", "generation", "timestamp", "incomplete"] {
        assert_eq!(result[key], "malformed", "status field {key} must be refused");
    }
}
