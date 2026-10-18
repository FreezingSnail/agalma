// WEB.6 cockpit shell renderer.
//
// One page renders factory/worker status, eligible queue summaries, executions,
// component health and the existing economics/timeline. Navigation is ordinary
// component paging with an explicit Next control and an opaque server cursor;
// there is no per-task dispatch button. Plugin-derived text is written with
// `textContent`/`createTextNode` only, bounded to 4 KiB with an explicit
// truncation marker. Lists are bounded to 100 rows per page with an explicit
// additional-content indicator.
//
// Unknown is never rendered as healthy, zero, empty or stopped: absent runtime,
// health, cost, stop and source states all carry their own explicit label. A
// failed source keeps its last successful data visible as stale and shows the
// last success time and the current error. A renderer exception becomes a
// visible bounded error instead of a blank page.

import { ApiError, ApiErrorKind, DashboardApi } from './api.js';
import { Refresher, DEFAULT_INTERVAL_MS } from './refresh.js';
import { ControlPanel, RESUME_CONFIRMATION } from './controls.js';

export const FIELD_TEXT_LIMIT = 4096;
export const LIST_ROW_LIMIT = 100;
export const COMPONENT_PAGE_LIMIT = 50;
const SAFE_DATE_MAX = 8640000000000000;

/** Bound plugin text to 4 KiB, reporting whether it was truncated. */
export function boundedText(value, limit = FIELD_TEXT_LIMIT) {
  const text = value === null || value === undefined ? '' : String(value);
  if (text.length <= limit) return { text, truncated: false };
  return { text: text.slice(0, limit), truncated: true };
}

/** Format a timestamp only inside the safe `Date` range; otherwise unknown. */
export function formatTimestamp(value) {
  if (!Number.isInteger(value) || value < 0 || value > SAFE_DATE_MAX) return 'unknown';
  try {
    return new Date(value).toISOString();
  } catch {
    return 'unknown';
  }
}

function el(doc, tag, className, text) {
  const node = doc.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function textNode(doc, tag, value, className) {
  const { text, truncated } = boundedText(value);
  const node = el(doc, tag, className);
  node.appendChild(doc.createTextNode(text));
  if (truncated) {
    node.appendChild(doc.createTextNode(' …[truncated]'));
  }
  return node;
}

function section(doc, title, id) {
  const root = el(doc, 'section', 'panel');
  root.id = id;
  root.appendChild(el(doc, 'h2', null, title));
  return root;
}

function kv(doc, label, value, className = 'value') {
  const row = el(doc, 'div', 'kv');
  row.appendChild(el(doc, 'span', 'key', label));
  row.appendChild(doc.createTextNode(': '));
  row.appendChild(el(doc, 'span', className, value));
  return row;
}

const SOURCE_LABELS = {
  available: 'available',
  empty: 'empty (no entries)',
  missing: 'missing',
  unreadable: 'unreadable',
  unknown: 'unknown',
};

function sourceLabel(state) {
  return SOURCE_LABELS[state] ?? `unknown source state (${String(state)})`;
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
    this.state = {
      components: { cursor: null, page: null, reloadNote: null, rows: new Map() },
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
    this.#registerSources();
    this.#buildStaticShell();
  }

  #registerSources() {
    this.refresher.register('overview', (signal) => this.api.getOverview({ signal }));
    this.refresher.register('components', (signal) => this.#loadComponents(signal));
    if (this.factoryEnabled) {
      this.refresher.register('factory', (signal) => this.api.getFactory({ signal }));
    }
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

  #buildStaticShell() {
    const doc = this.doc;
    while (this.root.firstChild) this.root.removeChild(this.root.firstChild);
    this.root.appendChild(el(doc, 'h1', null, 'Agalma cockpit'));
    this.runtimeSection = section(doc, 'Factory / worker status', 'runtime');
    this.queueSection = section(doc, 'Eligible queue (automatic operation)', 'queue');
    this.execSection = section(doc, 'Executions', 'executions');
    this.componentsSection = section(doc, 'Components', 'components');
    this.economicsSection = section(doc, 'Economics', 'economics');
    this.timelineSection = section(doc, 'Timeline', 'timeline');
    this.sourcesSection = section(doc, 'Read sources', 'sources');
    this.controlsSection = section(doc, 'Controls', 'controls');
    this.errorSection = section(doc, 'Error', 'errors');
    this.errorSection.hidden = true;
    for (const part of [
      this.runtimeSection, this.controlsSection, this.queueSection, this.execSection,
      this.componentsSection, this.economicsSection, this.timelineSection, this.sourcesSection,
      this.errorSection,
    ]) {
      this.root.appendChild(part);
    }
  }

  start() {
    return this.refresher.start();
  }

  stop() {
    this.refresher.stop();
  }

  refresh() {
    return this.refresher.refreshOnce();
  }

  /** Advance the component page using the opaque server cursor. */
  nextComponentPage() {
    const page = this.state.components.page;
    if (!page || page.next_cursor === null) return Promise.resolve();
    this.state.components.cursor = page.next_cursor;
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
      this.#renderRuntime(snapshot);
      this.#renderControls();
      this.#renderQueue(snapshot);
      this.#renderExecutions(snapshot);
      this.#renderComponents(snapshot);
      this.#renderEconomics(snapshot);
      this.#renderTimeline(snapshot);
      this.#renderSources(snapshot);
      this.state.renderCount += 1;
    } catch (error) {
      this.renderError(error);
    }
  }

  #factorySource(snapshot) {
    return snapshot.sources.factory ?? { data: null, error: null, status: 'absent', stale: false, lastSuccessAt: null };
  }

  #runtimeLabel(snapshot) {
    const factory = this.#factorySource(snapshot);
    if (factory.error) {
      return factory.error.kind === ApiErrorKind.UNSUPPORTED
        ? 'Unavailable (endpoint absent)'
        : 'Unavailable';
    }
    if (!factory.data) return 'Unknown';
    return factory.data.status;
  }

  #renderRuntime(snapshot) {
    const section = this.runtimeSection;
    while (section.firstChild) section.removeChild(section.firstChild);
    section.appendChild(el(this.doc, 'h2', null, 'Factory / worker status'));
    const label = this.#runtimeLabel(snapshot);
    section.appendChild(kv(this.doc, 'Runtime', label));
    const factory = this.#factorySource(snapshot);
    if (factory.data) {
      const stop = factory.data.stop;
      section.appendChild(kv(this.doc, 'Stop', stop ? (stop.stopped ? 'stopped' : 'not stopped') : 'unknown'));
      for (const source of factory.data.sources) {
        section.appendChild(kv(this.doc, `source:${source.source}`, sourceLabel(source.state)));
      }
    } else {
      section.appendChild(kv(this.doc, 'Stop', 'unknown'));
      section.appendChild(kv(this.doc, 'Note', 'factory runtime endpoint is not available in this slice'));
    }
  }

  #renderControls() {
    const section = this.controlsSection;
    while (section.firstChild) section.removeChild(section.firstChild);
    section.appendChild(el(this.doc, 'h2', null, 'Controls'));
    const state = this.controls.state();
    const note = state.known
      ? (state.readOnly ? 'read-only: no control permission advertised' : 'typed permissions advertised')
      : 'controls disabled: runtime unknown or offline';
    section.appendChild(kv(this.doc, 'Permission', state.readOnly ? 'read-only' : (state.known ? 'available' : 'disabled')));
    section.appendChild(this.doc.createTextNode(note));
    const buttons = el(this.doc, 'div', 'controls');
    this.pauseButton = el(this.doc, 'button', null, 'Pause');
    this.pauseButton.type = 'button';
    this.pauseButton.disabled = !state.enabled.pause;
    this.pauseButton.dataset.action = 'pause';
    if (state.enabled.pause) {
      this.pauseButton.addEventListener('click', () => {
        this.controls.pause();
      });
    }
    this.resumeButton = el(this.doc, 'button', null, 'Resume');
    this.resumeButton.type = 'button';
    this.resumeButton.disabled = !state.enabled.resume;
    this.resumeButton.dataset.action = 'resume';
    if (state.enabled.resume) {
      this.resumeButton.addEventListener('click', () => {
        const phrase = this.confirmPhrase();
        if (phrase !== RESUME_CONFIRMATION) return;
        this.controls.resume(phrase);
      });
    }
    buttons.appendChild(this.pauseButton);
    buttons.appendChild(this.resumeButton);
    section.appendChild(buttons);
    if (state.pending) section.appendChild(kv(this.doc, 'Pending', String(state.pendingAction)));
    if (state.lastResult) {
      section.appendChild(kv(this.doc, 'Result', state.lastResult.kind === 'receipt' ? (state.lastResult.outcome ?? 'receipt') : state.lastResult.kind));
      if (state.lastResult.kind === 'unconfirmed') {
        section.appendChild(kv(this.doc, 'EffectUnconfirmed', state.lastResult.reason ?? 'effect unknown'));
      }
      if (state.lastResult.actionId) section.appendChild(kv(this.doc, 'Action ID', state.lastResult.actionId));
    }
  }

  #renderQueue(snapshot) {
    const section = this.queueSection;
    while (section.firstChild) section.removeChild(section.firstChild);
    section.appendChild(el(this.doc, 'h2', null, 'Eligible queue (automatic operation)'));
    section.appendChild(this.doc.createTextNode('Work is consumed automatically; there is no per-task dispatch button.'));
    const factory = this.#factorySource(snapshot);
    if (!factory.data) {
      section.appendChild(el(this.doc, 'p', null, 'Eligible work is unknown (factory runtime unavailable).'));
      return;
    }
    const eligible = factory.data.eligible;
    const entries = eligible.entries.slice(0, LIST_ROW_LIMIT);
    if (entries.length === 0) {
      section.appendChild(el(this.doc, 'p', null, 'No eligible work in this observation.'));
    } else {
      const table = el(this.doc, 'table');
      const head = el(this.doc, 'tr');
      for (const heading of ['Task', 'Title', 'Priority', 'Family']) head.appendChild(el(this.doc, 'th', null, heading));
      table.appendChild(head);
      for (const entry of entries) {
        const row = el(this.doc, 'tr');
        row.appendChild(textNode(this.doc, 'td', entry.task_id));
        row.appendChild(textNode(this.doc, 'td', entry.title));
        row.appendChild(textNode(this.doc, 'td', entry.priority));
        row.appendChild(textNode(this.doc, 'td', entry.family));
        table.appendChild(row);
      }
      section.appendChild(table);
    }
    const shown = entries.length;
    const total = eligible.total ?? 'unknown';
    section.appendChild(el(this.doc, 'p', 'note', `Showing ${shown} of ${total}.`));
    this.#pager(section, eligible.next_cursor, 'next-queue', () => this.refresh());
  }

  #renderExecutions(snapshot) {
    const section = this.execSection;
    while (section.firstChild) section.removeChild(section.firstChild);
    section.appendChild(el(this.doc, 'h2', null, 'Executions'));
    const overview = snapshot.sources.overview?.data;
    const executions = overview?.executions ?? [];
    if (executions.length === 0) {
      section.appendChild(el(this.doc, 'p', null, overview ? 'No executions recorded.' : 'Executions unknown.'));
    } else {
      const table = el(this.doc, 'table');
      const head = el(this.doc, 'tr');
      for (const heading of ['Execution', 'Task', 'Generation', 'Phase', 'State', 'Updated']) {
        head.appendChild(el(this.doc, 'th', null, heading));
      }
      table.appendChild(head);
      for (const execution of executions.slice(0, LIST_ROW_LIMIT)) {
        const row = el(this.doc, 'tr');
        row.appendChild(textNode(this.doc, 'td', execution.execution_id));
        row.appendChild(textNode(this.doc, 'td', execution.task_id));
        row.appendChild(textNode(this.doc, 'td', execution.generation));
        row.appendChild(textNode(this.doc, 'td', execution.phase));
        row.appendChild(textNode(this.doc, 'td', execution.state));
        row.appendChild(textNode(this.doc, 'td', formatTimestamp(execution.updated_at_unix_ms)));
        table.appendChild(row);
      }
      section.appendChild(table);
      if (executions.length > LIST_ROW_LIMIT) {
        section.appendChild(el(this.doc, 'p', 'truncation', `…and ${executions.length - LIST_ROW_LIMIT} more executions.`));
      }
    }
  }

  #renderComponents(snapshot) {
    const section = this.componentsSection;
    while (section.firstChild) section.removeChild(section.firstChild);
    section.appendChild(el(this.doc, 'h2', null, 'Components'));
    const source = snapshot.sources.components ?? {};
    const page = source.data ?? this.state.components.page;
    if (source.error && page) {
      section.appendChild(el(this.doc, 'p', 'stale', `Showing stale component data from ${formatTimestamp(source.lastSuccessAt)} because the last refresh failed.`));
    }
    if (this.state.components.reloadNote) {
      section.appendChild(el(this.doc, 'p', 'reload', this.state.components.reloadNote));
    }
    if (!page) {
      section.appendChild(el(this.doc, 'p', null, 'Components unknown.'));
      return;
    }
    if (page.entries.length === 0) {
      section.appendChild(el(this.doc, 'p', null, 'No components in this page.'));
    } else {
      const table = el(this.doc, 'table');
      const head = el(this.doc, 'tr');
      for (const heading of ['Plugin', 'Name', 'Health', 'Freshness', 'Available', 'Generation']) {
        head.appendChild(el(this.doc, 'th', null, heading));
      }
      table.appendChild(head);
      for (const entry of page.entries.slice(0, LIST_ROW_LIMIT)) {
        const row = el(this.doc, 'tr');
        row.appendChild(textNode(this.doc, 'td', entry.plugin_id));
        row.appendChild(textNode(this.doc, 'td', entry.name));
        row.appendChild(textNode(this.doc, 'td', entry.health));
        row.appendChild(textNode(this.doc, 'td', entry.freshness ?? 'unknown'));
        row.appendChild(textNode(this.doc, 'td', entry.available ? 'true' : 'false'));
        row.appendChild(textNode(this.doc, 'td', entry.generation));
        table.appendChild(row);
      }
      section.appendChild(table);
    }
    this.#pager(section, page.next_cursor, 'next-components', () => this.nextComponentPage());
  }

  #pager(parent, nextCursor, id, onClick) {
    const button = el(this.doc, 'button', 'pager', 'Next');
    button.id = id;
    button.disabled = nextCursor === null || nextCursor === undefined;
    button.dataset.nextCursor = nextCursor ?? '';
    button.addEventListener('click', onClick);
    parent.appendChild(button);
    if (nextCursor === null || nextCursor === undefined) {
      parent.appendChild(el(this.doc, 'span', 'note', ' (last page)'));
    } else {
      parent.appendChild(el(this.doc, 'span', 'note', ' (more available)'));
    }
  }

  #renderEconomics(snapshot) {
    const section = this.economicsSection;
    while (section.firstChild) section.removeChild(section.firstChild);
    section.appendChild(el(this.doc, 'h2', null, 'Economics'));
    const economics = snapshot.sources.overview?.data?.economics;
    if (!economics) {
      section.appendChild(el(this.doc, 'p', null, 'Economics unknown.'));
      return;
    }
    section.appendChild(kv(this.doc, 'Completeness', economics.completeness ?? 'unknown'));
    section.appendChild(kv(this.doc, 'Usage', economics.usage === null || economics.usage === undefined ? 'unknown' : `${economics.usage} ${economics.usage_unit}`));
    section.appendChild(kv(this.doc, 'Cost', economics.cost === null || economics.cost === undefined ? 'unknown' : `${economics.cost} ${economics.cost_unit}`));
    section.appendChild(kv(this.doc, 'Allowance', economics.allowance === null || economics.allowance === undefined ? 'unknown' : `${economics.allowance} ${economics.cost_unit}`));
  }

  #renderTimeline(snapshot) {
    const section = this.timelineSection;
    while (section.firstChild) section.removeChild(section.firstChild);
    section.appendChild(el(this.doc, 'h2', null, 'Timeline'));
    const timeline = snapshot.sources.overview?.data?.timeline;
    if (!timeline) {
      section.appendChild(el(this.doc, 'p', null, 'Timeline unknown.'));
      return;
    }
    if (timeline.length === 0) {
      section.appendChild(el(this.doc, 'p', null, 'No timeline entries.'));
      return;
    }
    const list = el(this.doc, 'ul');
    for (const entry of timeline.slice(0, LIST_ROW_LIMIT)) {
      const item = el(this.doc, 'li');
      item.appendChild(textNode(this.doc, 'span', `${formatTimestamp(entry.at_unix_ms)} ${entry.kind}: `, 'time'));
      item.appendChild(textNode(this.doc, 'span', entry.title));
      item.appendChild(this.doc.createTextNode(' — '));
      item.appendChild(textNode(this.doc, 'span', entry.detail));
      list.appendChild(item);
    }
    section.appendChild(list);
  }

  #renderSources(snapshot) {
    const section = this.sourcesSection;
    while (section.firstChild) section.removeChild(section.firstChild);
    section.appendChild(el(this.doc, 'h2', null, 'Read sources'));
    const overviewSources = snapshot.sources.overview?.data?.metadata?.sources ?? null;
    if (overviewSources) {
      for (const source of overviewSources) {
        section.appendChild(kv(this.doc, source.source, sourceLabel(source.state)));
      }
    }
    for (const [name, source] of Object.entries(snapshot.sources)) {
      const status = source.error
        ? `${source.error.kind}${source.data ? ' (stale)' : ''}`
        : source.status;
      const row = kv(this.doc, `refresh:${name}`, status);
      if (source.lastSuccessAt !== null) {
        row.appendChild(this.doc.createTextNode(` last success ${formatTimestamp(source.lastSuccessAt)}`));
      }
      section.appendChild(row);
    }
    section.appendChild(kv(this.doc, 'Interval', `${snapshot.intervalMs ?? this.intervalMs} ms`));
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
