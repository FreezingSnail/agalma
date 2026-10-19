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
  timestampModel, exactTimeText,
} from './ui.js';
import {
  sourceFreshnessModel, runtimeModel, repositoryIdentity, summaryCards, currentWorkModel,
  costModel, componentPageModel, componentSeverity, deriveAttention, coverageGaps,
  timelineModel, formatHash as formatComponentHash,
} from './pages/overview.js';

export { boundedText, formatTimestamp, FIELD_TEXT_LIMIT, LIST_ROW_LIMIT };
export {
  sourceFreshnessModel, runtimeModel, repositoryIdentity, summaryCards, currentWorkModel,
  costModel, componentPageModel, componentSeverity, deriveAttention, coverageGaps,
  timelineModel, formatComponentHash,
};

export const COMPONENT_PAGE_LIMIT = 50;
export const DETAIL_PAGE_LIMIT = 50;

/** One bounded text line for header/status text. */
function textLine(doc, value, className) {
  const node = el(doc, 'p', className);
  const { text, truncated } = boundedText(value);
  node.appendChild(doc.createTextNode(text));
  if (truncated) node.appendChild(doc.createTextNode(' …[truncated]'));
  return node;
}

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
    // One clock source for relative/age presentation. `renderNow` is captured
    // once per render so every age calculation in that frame agrees.
    this.now = options.now ?? (() => Date.now());
    this.renderNow = this.now();
    this.state = {
      route: parseHash(this.#initialHash()),
      components: { cursor: null, page: null, reloadNote: null, rows: new Map() },
      // The detail page is pinned to one route subject and epoch. `page` is
      // committed only by a response for the still-current pin, so a late
      // response after navigation can never become another target's data.
      detail: { pluginId: null, cursor: null, page: null, epoch: 0 },
      status: null,
      lastRenderError: null,
      renderCount: 0,
      disclosures: new Map(),
      identityKey: null,
      renderNow: this.renderNow,
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
      now: this.now,
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

  /**
   * Pin the detail route subject/epoch.
   *
   * A subject change (or leaving the detail route) resets the opaque cursor and
   * retained page and advances the epoch. Responses captured under an older
   * epoch are never committed, even when they are for the same visible target.
   */
  #pinDetail(route) {
    if (route?.name !== 'component' || !route.params?.id) {
      this.state.detail.pluginId = null;
      this.state.detail.cursor = null;
      this.state.detail.page = null;
      this.state.detail.epoch += 1;
      return;
    }
    const pluginId = route.params.id;
    if (pluginId !== this.state.detail.pluginId) {
      // A new target resets the opaque cursor; the previous page is dropped.
      this.state.detail.pluginId = pluginId;
      this.state.detail.cursor = null;
      this.state.detail.page = null;
    }
    this.state.detail.epoch += 1;
  }

  #loadDetail(signal) {
    const route = this.state.route;
    if (route?.name !== 'component' || !route.params?.id) return null;
    const pluginId = route.params.id;
    if (pluginId !== this.state.detail.pluginId) this.#pinDetail(route);
    const epoch = this.state.detail.epoch;
    const cursor = this.state.detail.cursor;
    return this.api
      .getComponentDetail(pluginId, { limit: DETAIL_PAGE_LIMIT, cursor, signal })
      .then((page) => {
        const current = this.state.route;
        if (
          current?.name !== 'component'
          || current.params?.id !== pluginId
          || this.state.detail.pluginId !== pluginId
          || this.state.detail.epoch !== epoch
        ) {
          // Never commit a response for another target or an older route epoch.
          return null;
        }
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
    // The shell is the `.app` grid: a reserved desktop navigation column plus
    // one content host. Keeping every content child inside the host is what
    // keeps the fixed sidebar from covering runtime, notices and pages.
    this.root.classList.add('app');
    this.nav = el(doc, 'nav', 'nav');
    this.nav.appendChild(el(doc, 'h1', 'brand', 'Agalma cockpit'));
    this.navLinks = el(doc, 'div', 'nav-links');
    this.nav.appendChild(this.navLinks);
    this.root.appendChild(this.nav);

    this.content = el(doc, 'div', 'content');
    this.header = el(doc, 'header', 'cockpit-header');
    this.header.id = 'cockpit-header';
    this.content.appendChild(this.header);
    this.routeNotice = el(doc, 'p', 'empty');
    this.routeNotice.id = 'route-notice';
    this.routeNotice.hidden = true;
    this.content.appendChild(this.routeNotice);
    this.pageHost = doc.createElement('div');
    this.content.appendChild(this.pageHost);
    this.errorSection = panel(doc, 'Error', 'errors');
    this.errorSection.hidden = true;
    this.content.appendChild(this.errorSection);
    this.root.appendChild(this.content);
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
    // Pin only a real route transition; `#loadDetail` also pins when it first
    // observes an uninitialized subject.
    if (changed) this.#pinDetail(route);
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
    // A background refresh rebuilds the shell. Capture the focused control's
    // bounded identity and every disclosure's open state first, then restore
    // them afterwards so polling never steals focus or collapses a disclosure.
    this.renderNow = this.now();
    const focus = this.#captureFocus();
    this.#captureDisclosures();
    try {
      this.#syncIdentity(snapshot);
      // The typed runtime observation drives control availability. Update it
      // without re-entering render (the control panel's own onChange notifies
      // only for explicit user interactions).
      this.controls.setRuntime(this.#factorySource(snapshot).data ?? null, false);
      this.#renderHeader(snapshot);
      this.#renderNav();
      this.#renderRouteNotice();
      snapshot.renderNow = this.renderNow;
      this.#renderPages();
      this.#renderError();
      this.state.renderCount += 1;
    } catch (error) {
      this.renderError(error);
    } finally {
      this.#restoreFocus(focus);
    }
  }

  /** Persist one native disclosure's open state under a semantic key. */
  bindDisclosure(node, key, defaultOpen = false) {
    if (!node) return;
    if (!this.state.disclosures.has(key)) this.state.disclosures.set(key, defaultOpen);
    node.open = this.state.disclosures.get(key) === true;
    node.addEventListener('toggle', () => {
      this.state.disclosures.set(key, node.open === true);
    });
  }

  #captureDisclosures() {
    for (const node of this.root.querySelectorAll('details[data-disclosure-key]')) {
      this.state.disclosures.set(node.dataset.disclosureKey, node.open === true);
    }
  }

  #syncIdentity(snapshot) {
    const identity = repositoryIdentity(snapshot);
    if (!identity.known) return;
    if (this.state.identityKey !== null && this.state.identityKey !== identity.key) {
      // Contextual disclosures belong to one repository/state identity; never
      // carry plugin-derived expansion to a different runtime.
      this.state.disclosures.clear();
    }
    this.state.identityKey = identity.key;
  }

  #captureFocus() {
    const active = this.doc.activeElement;
    if (!active || active === this.doc.body || active === this.root) return null;
    const focusKey = active.dataset?.focusKey;
    if (focusKey) return { focusKey };
    if (active.id) return { id: active.id };
    const action = active.dataset?.action;
    if (action) return { action };
    return null;
  }

  #restoreFocus(token) {
    if (!token) return;
    let node = null;
    if (token.focusKey) {
      node = [...this.root.querySelectorAll('[data-focus-key]')]
        .find((candidate) => candidate.dataset.focusKey === token.focusKey) ?? null;
    } else if (token.id) {
      node = this.doc.getElementById(token.id);
    } else if (token.action) {
      node = this.root.querySelector(`[data-action="${token.action}"]`);
    }
    if (!node || node === this.doc.activeElement || typeof node.focus !== 'function') return;
    try {
      node.focus({ preventScroll: true });
    } catch {
      // An unavailable focus target is not a render failure.
    }
  }

  #renderHeader(snapshot) {
    const host = this.header;
    while (host.firstChild) host.removeChild(host.firstChild);
    const doc = this.doc;
    const identity = repositoryIdentity(snapshot);
    const active = this.state.route?.name === 'component' ? 'component' : (this.state.route?.name ?? 'overview');
    const routeTitle = ROUTES[active]?.title ?? 'Overview';

    const identityBlock = el(doc, 'div', 'header-identity');
    identityBlock.appendChild(el(doc, 'p', 'header-eyebrow', 'Repository'));
    identityBlock.appendChild(el(doc, 'p', 'header-repository', identity.repository_id ?? 'Repository identity unknown'));
    if (identity.state_id !== null) identityBlock.appendChild(el(doc, 'p', 'header-state note', `State ${identity.state_id}`));
    identityBlock.appendChild(el(doc, 'p', 'header-page-title', routeTitle));
    host.appendChild(identityBlock);

    const runtime = runtimeModel(snapshot);
    const runtimeBlock = el(doc, 'div', 'header-runtime');
    runtimeBlock.appendChild(el(doc, 'p', 'header-eyebrow', 'Runtime'));
    const runtimeLine = el(doc, 'p', 'header-runtime-line');
    runtimeLine.appendChild(pill(doc, runtime.pill));
    runtimeLine.appendChild(doc.createTextNode(' '));
    runtimeLine.appendChild(el(doc, 'span', 'header-runtime-state', runtime.state));
    runtimeBlock.appendChild(runtimeLine);
    if (runtime.reason) runtimeBlock.appendChild(textLine(doc, runtime.reason, 'note'));
    host.appendChild(runtimeBlock);

    const refreshBlock = el(doc, 'div', 'header-refresh');
    refreshBlock.id = 'refresh-status';
    refreshBlock.appendChild(el(doc, 'p', 'header-eyebrow', 'Refresh state'));
    const list = el(doc, 'ul', 'source-list');
    for (const item of sourceFreshnessModel(snapshot, this.renderNow)) {
      const row = el(doc, 'li', `source-state source-state--${item.state}`);
      row.dataset.source = item.name;
      row.appendChild(el(doc, 'span', 'source-name', item.title));
      row.appendChild(el(doc, 'span', 'source-label', item.label));
      if (item.exact) {
        const model = timestampModel(item.source?.lastSuccessAt, this.renderNow);
        const time = el(doc, 'span', 'source-time');
        time.appendChild(el(doc, 'span', 'source-relative', model.relative));
        time.appendChild(doc.createTextNode(' '));
        time.appendChild(exactTimeText(doc, model));
        row.appendChild(time);
      }
      list.appendChild(row);
    }
    refreshBlock.appendChild(list);
    const systemLink = el(doc, 'a', 'inline-action', 'System diagnostics');
    systemLink.setAttribute('href', '#/system');
    systemLink.dataset.focusKey = 'header:system';
    refreshBlock.appendChild(systemLink);
    host.appendChild(refreshBlock);
  }

  #renderNav() {
    const nav = this.navLinks;
    while (nav.firstChild) nav.removeChild(nav.firstChild);
    const active = this.state.route?.name === 'component' ? 'components' : this.state.route?.name;
    for (const name of NAV_ROUTES) {
      const spec = ROUTES[name];
      if (!spec) continue;
      const link = el(this.doc, 'a', null, spec.title);
      link.setAttribute('href', formatHash({ name, params: {} }));
      link.dataset.focusKey = `nav:${name}`;
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
  // Page exit stops timers and aborts the active read. Persisted pageshow and
  // becoming visible again restart one refresh loop; `start()` is idempotent,
  // so duplicate events never create overlapping cycles or timers.
  const view = doc.defaultView ?? globalThis;
  view.addEventListener('pagehide', () => cockpit.stop());
  view.addEventListener('pageshow', () => cockpit.start());
  doc.addEventListener('visibilitychange', () => {
    if (doc.visibilityState === 'hidden') {
      cockpit.stop();
    } else {
      cockpit.start();
    }
  });
  cockpit.start();
  return cockpit;
}
