//! WEB.6 cockpit page acceptance (`agalma-rwn.6`).
//!
//! Drives the **compiled** dashboard assets through the installed Chrome in
//! headless mode against a real loopback fixture server that serves independent
//! `fixtures/cockpit` JSON. Six critical fixture browser regressions:
//!
//! 1. one page renders factory/worker status, eligible queue, executions,
//!    component health and economics/timeline from independent unfamiliar
//!    component fixtures, with exact identity/money strings (values > 2^53 and
//!    exact decimals) and bounded safe text;
//! 2. injected `<script>`/`<img>` text stays inert text;
//! 3. a failing source keeps its last successful data visible as stale and a
//!    revision conflict reloads the collection once;
//! 4. component paging uses the opaque server cursor within the 100-row bound;
//! 5. controls are rendered permission-gated, resume requires the literal
//!    phrase, a lost response queries the retained action id and shows
//!    `EffectUnconfirmed` without replay, and a view issues zero POSTs;
//! 6. page exit stops the refresh timer and aborts the active read.
//!
//! The production first-launch smoke is the separate seventh acceptance item in
//! `first_launch.rs`. These cases run through the real refresh, control and
//! render modules in one driver page; the runner retains raw logs under
//! `target/test-runs`.

mod browser_support;

use std::sync::{Arc, Mutex};

use browser_support::{driver_page, resolve_chrome, FixtureServer, Response, Router};

const BUDGET_MS: u64 = 8000;

const OVERVIEW: &str = include_str!("../../../fixtures/cockpit/overview.json");
const COMPONENTS: &str = include_str!("../../../fixtures/cockpit/components-page.json");
const INJECTION: &str = include_str!("../../../fixtures/cockpit/injection-and-boundaries.json");
const FACTORY_RUNTIME: &str = include_str!("../../../fixtures/cockpit/factory-runtime.json");
const FACTORY_READONLY: &str = include_str!("../../../fixtures/cockpit/factory-readonly.json");
const CONTROL_RECEIPT: &str = include_str!("../../../fixtures/cockpit/control-receipt.json");
const CONTROL_UNCONFIRMED: &str = include_str!("../../../fixtures/cockpit/control-unconfirmed.json");

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
                script.posted_bodies.clear();
                None
            }
            ("GET", "/api/v1/overview") => Some(script.next_overview()),
            ("GET", "/api/v1/components") => Some(script.next_components()),
            ("GET", "/api/v1/factory") => Some(script.next_factory()),
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
    server.register(
        "/harness.html",
        Response::text(200, "text/html; charset=utf-8", driver_page(body)),
    );
    let chrome = resolve_chrome().expect("installed Chrome (CHROME_BIN) is required");
    // An empty serialized result means the module never executed (Chrome dumped
    // before the module ran), so the fixture endpoints saw no request and a
    // fresh-profile retry is side-effect free. A driver that ran always writes
    // a JSON result or a harness error, which is never retried.
    let (outcome, result) = browser_support::run_browser_result(
        &chrome,
        &format!("{}/harness.html", server.base_url()),
        BUDGET_MS,
        label,
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
