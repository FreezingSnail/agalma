// WEB.6 cockpit shell for the multi-page cockpit (UI v2).
//
// The shell keeps the retained DOM contract (`#app` root, `h1` brand "Agalma
// cockpit") and the retained module exports the browser acceptance driver
// imports. It renders the navigation from `ROUTES`, one page player per module,
// the runtime pill and the global error section, and moves the control panel to
// the Overview page unchanged in behavior.
//
// Read registration: overview, factory and components stay always-on; the
// component-detail read is route-active (its loader fetches only while its
// route is active and otherwise returns the last successful page). Navigation,
// hash change, reload and refresh never submit a control POST. Plugin-derived
// text is written with `textContent`/`createTextNode` only, bounded to 4 KiB
// with an explicit truncation marker; lists are bounded to 100 rows per page.
// Unknown is never rendered as healthy, zero, empty or stopped.

import { ApiError, ApiErrorKind, DashboardApi } from './api.js';
import { Refresher, DEFAULT_INTERVAL_MS } from './refresh.js';
import { ControlPanel, RESUME_CONFIRMATION } from './controls.js';
import { ROUTES, NAV_ROUTES, parseHash, formatHash, startRouter } from './router.js';
import {
  boundedText, formatTimestamp, FIELD_TEXT_LIMIT, LIST_ROW_LIMIT, el, panel, pill,
} from './ui.js';

export { boundedText, formatTimestamp, FIELD_TEXT_LIMIT, LIST_ROW_LIMIT };

export const COMPONENT_PAGE_LIMIT = 50;
export const DETAIL_PAGE_LIMIT = 50;

/** Map a typed factory runtime status onto a pill state (unknown stays unknown). */
const RUNTIME_PILL = {
  Starting: 'degraded',
  Recovering: 'degraded',
  Idle: 'idle',
  Running: 'running',
  Paused: 'paused',
  Failed: 'failed',
  Unavailable: 'unavailable',
};

/**
 * Default operator confirmation-phrase provider for a Resume click.
 *
 * Prefers a `prompt` typed phrase; falls back to a boolean `confirm` that
 * resolves to the literal phrase when accepted. Returns `null` when no operator
 * surface is available, so the caller treats the click as unconfirmed and never
 * submits. This is injectable through the `Cockpit` options for tests.
 */
export function defaultConfirmPhrase() {
  try {
    if (typeof globalThis.prompt === 'function') {
      return globalThis.prompt.call(globalThis, `Type "${RESUME_CONFIRMATION}" to confirm resuming the factory:`);
    }
    if (typeof globalThis.confirm === 'function') {
      return globalThis.confirm.call(globalThis, 'Resume the factory?') ? RESUME_CONFIRMATION : null;
    }
  } catch {
    return null;
  }
  return null;
}

export class Cockpit {
  constructor(options = {}) {
    this.doc = options.document ?? globalThis.document;
    this.root = options.root ?? this.doc.getElementById('app');
    this.api = options.api ?? new DashboardApi();
    this.factoryEnabled = options.factoryEnabled ?? true;
    this.intervalMs = options.intervalMs ?? DEFAULT_INTERVAL_MS;
    this.randomSource = options.randomSource;
    // Injectable operator phrase provider; production defaults to the browser
    // prompt/confirm surface and returns null when none is available.
    this.confirmPhrase = options.confirmPhrase ?? defaultConfirmPhrase;
    this.state = {
      route: parseHash(this.#initialHash()),
      components: { cursor: null, page: null, reloadNote: null, rows: new Map() },
      detail: { pluginId: null, cursor: null, page: null },
      status: null,
      lastRenderError: null,
      renderCount: 0,
    };
    this.controls = new ControlPanel({
      api: this.api,
      randomSource: this.randomSource,
      onChange: () => this.render(),
    });
    this.refresher = new Refresher({
      api: this.api,
      intervalMs: this.intervalMs,
      onUpdate: (snapshot) => this.render(snapshot),
      onError: (error) => this.renderError(error),
      now: options.now,
      setTimer: options.setTimer,
      clearTimer: options.clearTimer,
    });
    this.routerStop = null;
    this.#registerSources();
    this.#buildStaticShell();
    this.render();
  }

  #initialHash() {
    const view = this.doc.defaultView ?? globalThis;
    return view.location?.hash ?? '';
  }

  #registerSources() {
    this.refresher.register('overview', (signal) => this.api.getOverview({ signal }));
    this.refresher.register('components', (signal) => this.#loadComponents(signal));
    if (this.factoryEnabled) {
      this.refresher.register('factory', (signal) => this.api.getFactory({ signal }));
    }
    // Route-active reads: the loaders fetch only while their route is active
    // and otherwise short-circuit to the last successful value, so the refresh
    // module itself needs no change.
    this.refresher.register('detail', (signal) => this.#loadDetail(signal));
    this.refresher.register('status', (signal) => this.#loadStatus(signal));
  }

  async #loadComponents(signal) {
    let conflicted = false;
    try {
      const page = await this.api.getComponents({ limit: COMPONENT_PAGE_LIMIT, cursor: this.state.components.cursor, signal });
      this.state.components.page = page;
      this.#keyComponentRows(page.entries);
      // A normal successful load clears a previous conflict notice; the
      // conflict-triggered reload below keeps its notice for this cycle.
      if (!conflicted) this.state.components.reloadNote = null;
      return page;
    } catch (error) {
      if (error instanceof ApiError && error.kind === ApiErrorKind.CONFLICT && this.state.components.cursor !== null) {
        // Revision conflict: reset the collection exactly once, visibly.
        conflicted = true;
        this.state.components.cursor = null;
        this.state.components.reloadNote = 'Collection reloaded after a revision conflict.';
        const page = await this.api.getComponents({ limit: COMPONENT_PAGE_LIMIT, cursor: null, signal });
        this.state.components.page = page;
        this.#keyComponentRows(page.entries);
        return page;
      }
      throw error;
    }
  }

  #keyComponentRows(entries) {
    const next = new Map();
    for (const entry of entries) {
      // Component identity + generation is the row key: a generation
      // replacement drops the previous generation's row state.
      const key = `${entry.plugin_id}@${entry.generation}`;
      next.set(key, this.state.components.rows.get(key) ?? {});
    }
    this.state.components.rows = next;
  }

  #loadDetail(signal) {
    const route = this.state.route;
    if (route?.name !== 'component' || !route.params?.id) return this.state.detail.page;
    const pluginId = route.params.id;
    if (pluginId !== this.state.detail.pluginId) {
      // A new target resets the opaque cursor; the previous page is dropped.
      this.state.detail.pluginId = pluginId;
      this.state.detail.cursor = null;
      this.state.detail.page = null;
    }
    return this.api
      .getComponentDetail(pluginId, { limit: DETAIL_PAGE_LIMIT, cursor: this.state.detail.cursor, signal })
      .then((page) => {
        this.state.detail.page = page;
        return page;
      });
  }

  #loadStatus(signal) {
    if (this.state.route?.name !== 'system') return this.state.status;
    return this.api.getStatus({ signal }).then((value) => {
      this.state.status = value;
      return value;
    });
  }

  #buildStaticShell() {
    const doc = this.doc;
    while (this.root.firstChild) this.root.removeChild(this.root.firstChild);
    this.root.appendChild(el(doc, 'h1', null, 'Agalma cockpit'));
    this.nav = el(doc, 'nav', 'nav');
    this.root.appendChild(this.nav);
    this.runtimePillHost = el(doc, 'div', 'kv');
    this.root.appendChild(this.runtimePillHost);
    this.routeNotice = el(doc, 'p', 'empty');
    this.routeNotice.hidden = true;
    this.root.appendChild(this.routeNotice);
    this.pageHost = doc.createElement('div');
    this.root.appendChild(this.pageHost);
    this.errorSection = panel(doc, 'Error', 'errors');
    this.errorSection.hidden = true;
    this.root.appendChild(this.errorSection);
  }

  start() {
    if (!this.routerStop) {
      this.routerStop = startRouter({ onRoute: (route) => this.#onRoute(route) });
    }
    return this.refresher.start();
  }

  #onRoute(route) {
    const previous = this.state.route;
    const changed = !previous
      || previous.name !== route.name
      || previous.params?.id !== route.params?.id;
    this.state.route = route;
    this.render();
    // Only a real route change triggers a read; the initial dispatch is
    // covered by `start()`'s first refresh cycle.
    if (changed && previous) this.refresh();
  }

  stop() {
    if (this.routerStop) {
      this.routerStop();
      this.routerStop = null;
    }
    this.refresher.stop();
  }

  refresh() {
    return this.refresher.refreshOnce();
  }

  /** Advance the component page using the opaque server cursor. */
  nextComponentPage() {
    const page = this.state.components.page;
    if (!page || page.next_cursor === null || page.next_cursor === undefined) return Promise.resolve();
    this.state.components.cursor = page.next_cursor;
    return this.refresh();
  }

  /** Advance the component-detail page using the opaque server cursor. */
  nextDetailPage() {
    const page = this.state.detail.page;
    if (!page || page.next_cursor === null || page.next_cursor === undefined) return Promise.resolve();
    this.state.detail.cursor = page.next_cursor;
    return this.refresh();
  }

  snapshot() {
    return this.refresher.snapshot();
  }

  renderError(error) {
    this.state.lastRenderError = { kind: error?.kind ?? 'error', message: String(error?.message ?? error) };
    this.#renderError();
  }

  #renderError() {
    if (!this.errorSection) return;
    const error = this.state.lastRenderError;
    while (this.errorSection.firstChild) this.errorSection.removeChild(this.errorSection.firstChild);
    this.errorSection.appendChild(this.doc.createElement('h2')).textContent = 'Error';
    if (!error) {
      this.errorSection.hidden = true;
      return;
    }
    this.errorSection.hidden = false;
    const { text } = boundedText(`${error.kind}: ${error.message}`);
    this.errorSection.appendChild(this.doc.createTextNode(text));
  }

  /** Render one refresh snapshot. Never throws into the timer loop. */
  render(snapshot = this.refresher.snapshot()) {
    try {
      // The typed runtime observation drives control availability. Update it
      // without re-entering render (the control panel's own onChange notifies
      // only for explicit user interactions).
      this.controls.setRuntime(this.#factorySource(snapshot).data ?? null, false);
      this.#renderRuntimePill(snapshot);
      this.#renderNav();
      this.#renderRouteNotice();
      this.#renderPages();
      this.#renderError();
      this.state.renderCount += 1;
    } catch (error) {
      this.renderError(error);
    }
  }

  #renderRuntimePill(snapshot) {
    const host = this.runtimePillHost;
    while (host.firstChild) host.removeChild(host.firstChild);
    const factory = this.#factorySource(snapshot);
    let state = 'unknown';
    if (factory.error) state = 'unavailable';
    else if (factory.data) state = RUNTIME_PILL[factory.data.status] ?? 'unknown';
    host.appendChild(el(this.doc, 'span', 'key', 'Runtime'));
    host.appendChild(this.doc.createTextNode(': '));
    host.appendChild(pill(this.doc, state));
  }

  #renderNav() {
    const nav = this.nav;
    while (nav.firstChild) nav.removeChild(nav.firstChild);
    const active = this.state.route?.name === 'component' ? 'components' : this.state.route?.name;
    for (const name of NAV_ROUTES) {
      const spec = ROUTES[name];
      if (!spec) continue;
      const link = el(this.doc, 'a', null, spec.title);
      link.setAttribute('href', formatHash({ name, params: {} }));
      if (name === active) link.setAttribute('aria-current', 'page');
      nav.appendChild(link);
    }
  }

  #renderRouteNotice() {
    const valid = this.state.route?.valid !== false;
    this.routeNotice.hidden = valid;
    this.routeNotice.textContent = valid ? '' : 'Unknown route; showing Overview.';
  }

  #renderPages() {
    const active = this.state.route?.name ?? 'overview';
    while (this.pageHost.firstChild) this.pageHost.removeChild(this.pageHost.firstChild);
    for (const [name, spec] of Object.entries(ROUTES)) {
      const page = spec.render(this);
      page.hidden = name !== active;
      this.pageHost.appendChild(page);
    }
  }

  #factorySource(snapshot) {
    return snapshot.sources.factory ?? { data: null, error: null, status: 'absent', stale: false, lastSuccessAt: null };
  }
}

/** Build and mount the cockpit. Returns the live instance. */
export function startCockpit(options = {}) {
  const doc = options.document ?? globalThis.document;
  const cockpit = new Cockpit({
    ...options,
    document: doc,
    api: options.api ?? new DashboardApi({ baseUrl: options.baseUrl ?? '', fetchImpl: options.fetchImpl }),
  });
  // Page exit stops timers and aborts the active read. These listeners stay
  // attached so a driver page can dispatch `pagehide` on the view directly.
  const view = doc.defaultView ?? globalThis;
  view.addEventListener('pagehide', () => cockpit.stop());
  doc.addEventListener('visibilitychange', () => {
    if (doc.visibilityState === 'hidden') cockpit.stop();
  });
  cockpit.start();
  return cockpit;
}
