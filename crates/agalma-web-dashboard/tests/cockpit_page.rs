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
//! 6. an unknown plugin id renders the explicit not-registered state;
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

const BUDGET_MS: u64 = 15000;

const OVERVIEW: &str = include_str!("../../../fixtures/cockpit/overview.json");
const COMPONENTS: &str = include_str!("../../../fixtures/cockpit/components-page.json");
const INJECTION: &str = include_str!("../../../fixtures/cockpit/injection-and-boundaries.json");
const FACTORY_RUNTIME: &str = include_str!("../../../fixtures/cockpit/factory-runtime.json");
const FACTORY_READONLY: &str = include_str!("../../../fixtures/cockpit/factory-readonly.json");
const CONTROL_RECEIPT: &str = include_str!("../../../fixtures/cockpit/control-receipt.json");
const CONTROL_UNCONFIRMED: &str = include_str!("../../../fixtures/cockpit/control-unconfirmed.json");
const COMPONENT_DETAIL: &str = include_str!("../../../fixtures/cockpit/component-detail.json");
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
    const links = [...document.querySelectorAll('a[data-focus-key^="component:"]')];
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
    assert_eq!(result["count"].as_u64().unwrap(), 6);
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
