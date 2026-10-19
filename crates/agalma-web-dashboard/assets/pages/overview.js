// UI.3 informative Overview and shared presentation models.
//
// Reads: overview + factory + the current component page. The models below are
// pure functions over the refresh snapshot so the browser acceptance driver can
// assert the data rules independently of DOM construction. Unknown is never
// rendered as healthy, zero, empty, idle or stopped.

import { RESUME_CONFIRMATION } from '../controls.js';
import { ApiErrorKind } from '../api.js';
import { formatHash } from '../router.js';
import {
  panel, kv, emptyState, textNode, el, pill, table, timestampModel,
  exactTimeText, disclosure, anchor, summaryCard, boundedText, LIST_ROW_LIMIT,
  FIELD_TEXT_LIMIT, staleNotice,
} from '../ui.js';

export const title = 'Overview';

const CORE_SOURCES = ['overview', 'factory', 'components'];
const SOURCE_TITLES = { overview: 'Overview', factory: 'Factory', components: 'Components' };
const SOURCE_LABELS = {
  available: 'available',
  empty: 'empty (no entries)',
  missing: 'missing',
  unreadable: 'unreadable',
  unknown: 'unknown',
};
const RUNTIME_PILL = {
  Starting: 'degraded',
  Recovering: 'degraded',
  Idle: 'idle',
  Running: 'running',
  Paused: 'paused',
  Failed: 'failed',
  Unavailable: 'unavailable',
};
const SEVERITY_RANK = { error: 0, warning: 1, unknown: 2 };
const ATTENTION_LIMIT = 10;

function lower(value) {
  return value === null || value === undefined ? '' : String(value).toLowerCase();
}

function sourceLabel(state) {
  return SOURCE_LABELS[state] ?? `unknown source state (${String(state)})`;
}

function exactText(value) {
  if (value === null || value === undefined) return 'unknown';
  return String(value);
}

/**
 * A visible retained-data notice for one source. Only rendered when the last
 * read failed but a previous successful value is still displayed.
 */
function sourceStaleNotice(doc, snapshot, name) {
  const source = snapshot?.sources?.[name];
  if (!source?.error || source.data === null || source.data === undefined) return null;
  const node = staleNotice(doc, source.lastSuccessAt);
  node.dataset.staleSource = name;
  node.setAttribute('aria-label', `${SOURCE_TITLES[name] ?? name} data is stale`);
  return node;
}

/** Append a retained-data notice only when the source is stale. */
function appendSourceStale(doc, root, snapshot, name) {
  const node = sourceStaleNotice(doc, snapshot, name);
  if (node) root.appendChild(node);
}

function isObject(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

/** One source freshness observation for the shared header and attention rules. */
export function sourceFreshnessModel(snapshot, now) {
  const sources = snapshot?.sources ?? {};
  return CORE_SOURCES.map((name) => {
    const source = sources[name];
    const title = SOURCE_TITLES[name];
    if (!source || source.status === 'idle' || (!source.data && !source.error && source.lastSuccessAt === null)) {
      return { name, title, state: 'not_loaded', label: 'Not loaded', exact: null, relative: null, source: source ?? null };
    }
    const time = timestampModel(source.lastSuccessAt, now);
    if (source.error) {
      if (source.data !== null && source.data !== undefined) {
        return {
          name,
          title,
          state: 'stale',
          label: 'Refresh failed; showing retained data',
          exact: time.exact,
          relative: time.relative,
          source,
        };
      }
      return {
        name,
        title,
        state: 'unavailable',
        label: source.error.kind === ApiErrorKind.UNSUPPORTED ? 'Unavailable (endpoint absent)' : 'Unavailable',
        exact: null,
        relative: null,
        source,
      };
    }
    if (source.data !== null && source.data !== undefined) {
      return { name, title, state: 'available', label: 'Updated', exact: time.exact, relative: time.relative, source };
    }
    return { name, title, state: 'unknown', label: 'Unknown', exact: null, relative: null, source };
  });
}

/** Repository/state identity from the authoritative factory source. */
export function repositoryIdentity(snapshot) {
  const factory = snapshot?.sources?.factory?.data;
  if (!isObject(factory)) {
    return { known: false, repository_id: null, state_id: null, key: null };
  }
  const repositoryId = typeof factory.repository_id === 'string' ? factory.repository_id : null;
  const stateId = typeof factory.state_id === 'string' ? factory.state_id : null;
  const known = repositoryId !== null || stateId !== null;
  return {
    known,
    repository_id: repositoryId,
    state_id: stateId,
    key: known ? `${repositoryId ?? 'unknown'}\u0000${stateId ?? 'unknown'}` : null,
  };
}

/** Typed runtime model: unavailable, not loaded, or an observed status. */
export function runtimeModel(snapshot) {
  const factory = snapshot?.sources?.factory;
  if (factory?.error && (factory.data === null || factory.data === undefined)) {
    const absent = factory.error.kind === ApiErrorKind.UNSUPPORTED;
    return {
      known: false,
      state: absent ? 'Unavailable (endpoint absent)' : 'Unavailable',
      pill: 'unavailable',
      reason: null,
    };
  }
  if (!factory || (factory.data === null || factory.data === undefined)) {
    return { known: false, state: 'Not loaded', pill: 'unknown', reason: null };
  }
  const status = typeof factory.data.status === 'string' ? factory.data.status : 'unknown';
  return {
    known: true,
    state: status,
    pill: RUNTIME_PILL[status] ?? 'unknown',
    reason: factory.data.reason ?? null,
  };
}

function stopModel(factoryData) {
  if (!isObject(factoryData)) return { known: false, stopped: null, reason: null };
  const stop = factoryData.stop;
  if (!isObject(stop) || typeof stop.stopped !== 'boolean') {
    return { known: false, stopped: null, reason: null };
  }
  return { known: true, stopped: stop.stopped, reason: stop.reason ?? null };
}

/** The four Overview summary cards. */
export function summaryCards(snapshot) {
  const factorySource = snapshot?.sources?.factory;
  const factoryData = factorySource?.data;
  const runtime = runtimeModel(snapshot);
  const stop = stopModel(factoryData);
  let factoryValue = runtime.state;
  let factoryDetail;
  let factoryTone = runtime.pill;
  if (!runtime.known) {
    factoryValue = runtime.state;
    factoryDetail = factorySource?.error ? 'Factory runtime read is unavailable.' : 'Factory runtime has not loaded.';
  } else {
    factoryDetail = factoryData?.reason ? String(factoryData.reason) : 'No reason reported.';
    if (stop.known) {
      factoryDetail = `${factoryDetail} ${stop.stopped ? 'Stopped.' : 'No stop observed.'}`;
    } else {
      factoryDetail = `${factoryDetail} Stop state unknown.`;
    }
  }

  const eligible = factoryData?.eligible;
  const totalKnown = isObject(eligible)
    && eligible.total !== null
    && eligible.total !== undefined
    && String(eligible.total) !== '';
  const eligibleTotal = totalKnown ? exactText(eligible.total) : 'unknown';
  const loaded = isObject(eligible) && Array.isArray(eligible.entries) ? eligible.entries.length : null;
  const eligibleDetail = totalKnown
    ? `${loaded ?? 0} entries loaded. Total ${eligibleTotal}.`
    : 'Total unknown. Loaded entries are not a total.';

  const components = componentPageModel(snapshot);
  const componentValue = components.known ? `${components.observed} observed in page` : 'unknown';
  const componentDetail = components.known
    ? `${components.ready} ready, ${components.attention} attention, ${components.unknown} unknown in this page.`
    : 'Component page not loaded.';

  const cost = costModel(snapshot);
  const costValue = cost.known ? `${cost.cost} ${cost.unit ?? 'unknown'}` : 'unknown';
  const costDetail = cost.known
    ? `Completeness ${cost.completeness}.`
    : `Cost unknown. Completeness ${cost.completeness}.`;

  return [
    { id: 'summary-factory', label: 'Factory state', value: factoryValue, detail: factoryDetail, tone: factoryTone, source: 'factory' },
    { id: 'summary-eligible', label: 'Eligible total', value: eligibleTotal, detail: eligibleDetail, tone: totalKnown ? null : 'unknown', source: 'factory' },
    { id: 'summary-components', label: 'Observed components', value: componentValue, detail: componentDetail, tone: components.attention > 0 ? 'warning' : (components.known ? null : 'unknown'), source: 'components' },
    { id: 'summary-cost', label: 'Observed cost', value: costValue, detail: costDetail, tone: cost.known ? null : 'unknown', source: 'overview' },
  ];
}

/** The current authoritative execution and an exact-identity optional attempt. */
export function currentWorkModel(snapshot) {
  const factorySource = snapshot?.sources?.factory;
  if (factorySource?.error && (factorySource.data === null || factorySource.data === undefined)) {
    return { kind: 'unknown', label: 'Current execution unavailable' };
  }
  if (!factorySource || factorySource.data === null || factorySource.data === undefined) {
    return { kind: 'unknown', label: 'Current execution not loaded' };
  }
  const current = factorySource.data.current_execution;
  if (current === null || current === undefined) {
    return { kind: 'none', label: 'No current execution reported' };
  }
  if (!isObject(current)) return { kind: 'unknown', label: 'Current execution unknown' };
  const executionId = typeof current.execution_id === 'string' ? current.execution_id : null;
  const taskId = typeof current.task_id === 'string' ? current.task_id : null;
  const generation = current.generation === null || current.generation === undefined ? null : String(current.generation);
  let attempt = null;
  const overview = snapshot?.sources?.overview?.data;
  if (executionId !== null && generation !== null && Array.isArray(overview?.executions)) {
    const match = overview.executions.find((candidate) => (
      isObject(candidate)
      && candidate.execution_id === executionId
      && String(candidate.generation) === generation
    ));
    if (match) attempt = match.attempt ?? null;
  }
  return {
    kind: 'execution',
    execution_id: executionId,
    task_id: taskId,
    generation,
    phase: current.phase ?? null,
    state: current.state ?? null,
    updated_at_unix_ms: Number.isInteger(current.updated_at_unix_ms) ? current.updated_at_unix_ms : null,
    attempt,
  };
}

/** Observed economics; amounts remain exact strings. */
export function costModel(snapshot) {
  const overview = snapshot?.sources?.overview;
  const data = overview?.data;
  const economics = data?.economics;
  if (!isObject(economics)) {
    return { known: false, cost: null, unit: null, completeness: 'unknown', usage: null, usage_unit: null, allowance: null, allowance_unit: null, observed_at_unix_ms: null };
  }
  const cost = economics.cost;
  const known = cost !== null && cost !== undefined;
  return {
    known,
    cost: known ? String(cost) : null,
    unit: economics.cost_unit ?? null,
    completeness: economics.completeness ?? 'unknown',
    usage: economics.usage === null || economics.usage === undefined ? null : String(economics.usage),
    usage_unit: economics.usage_unit ?? null,
    allowance: economics.allowance === null || economics.allowance === undefined ? null : String(economics.allowance),
    allowance_unit: economics.cost_unit ?? null,
    observed_at_unix_ms: Number.isInteger(economics.observed_at_unix_ms) ? economics.observed_at_unix_ms : null,
    partial: lower(economics.completeness) === 'partial',
    complete: lower(economics.completeness) === 'complete',
  };
}

/**
 * Disjoint ready/attention/unknown classification of the currently fetched
 * component page. The three counts always sum to the page length.
 */
export function componentPageModel(snapshot) {
  const source = snapshot?.sources?.components;
  const page = source?.data;
  const entries = Array.isArray(page?.entries) ? page.entries : null;
  if (!entries) {
    return { known: false, observed: 0, ready: 0, attention: 0, unknown: 0, continuation: false, entries: [] };
  }
  let ready = 0;
  let attention = 0;
  let unknown = 0;
  for (const entry of entries) {
    const severity = componentSeverity(entry);
    if (severity === null) ready += 1;
    else if (severity === 'error' || severity === 'warning') attention += 1;
    else unknown += 1;
  }
  return {
    known: true,
    observed: entries.length,
    ready,
    attention,
    unknown,
    continuation: page.next_cursor !== null && page.next_cursor !== undefined,
    entries,
  };
}

/** Explicit error/warning/unknown classification for one observed component. */
export function componentSeverity(entry) {
  if (!isObject(entry)) return 'unknown';
  const health = lower(entry.health);
  const freshness = lower(entry.freshness);
  const completeness = lower(entry.completeness);
  if (entry.available !== true || health === 'failed' || health === 'unavailable') return 'error';
  if (health === 'degraded' || freshness === 'stale' || completeness === 'partial') return 'warning';
  if (health !== 'ready' || freshness !== 'fresh' || completeness !== 'complete') return 'unknown';
  return null;
}

/**
 * Deterministic attention list: source/component failures, explicit stop and
 * observed unhealthy components. Stable deduplication by key, severity first,
 * then identity; at most ten entries plus an exact additional count.
 */
export function deriveAttention(snapshot) {
  const entries = [];
  const push = (key, severity, title, detail, href = null, scope = null) => {
    entries.push({ key, severity, title, detail, href, scope });
  };
  const sources = snapshot?.sources ?? {};

  for (const name of CORE_SOURCES) {
    const source = sources[name];
    const sourceTitle = SOURCE_TITLES[name];
    if (!source || source.status === 'idle' || (!source.data && !source.error && source.lastSuccessAt === null)) {
      push(`source:${name}`, 'unknown', `${sourceTitle} source not loaded`, 'No successful observation is available yet.', null, null);
      continue;
    }
    if (source.error) {
      if (source.data !== null && source.data !== undefined) {
        const time = timestampModel(source.lastSuccessAt, snapshot?.renderNow);
        push(
          `source:${name}`,
          'warning',
          `${sourceTitle} data is stale`,
          `Refresh failed. Last successful read ${time.exact ?? 'unknown'}. ${source.error.message}`,
        );
      } else {
        push(
          `source:${name}`,
          'error',
          `${sourceTitle} source unavailable`,
          `${source.error.kind}: ${source.error.message}`,
        );
      }
    }
  }

  const factory = sources.factory?.data;
  if (isObject(factory)) {
    const status = lower(factory.status);
    const reason = factory.reason ? ` Reason: ${factory.reason}` : '';
    if (status === 'failed' || status === 'unavailable') {
      push('factory:status', 'error', `Factory is ${factory.status}`, `Factory state is ${factory.status}.${reason}`);
    } else if (status === 'starting' || status === 'recovering') {
      push('factory:status', 'warning', `Factory is ${factory.status}`, `Startup or recovery is in progress.${reason}`);
    } else if (status === 'paused') {
      push('factory:status', 'warning', 'Factory is paused', `Factory state is Paused.${reason}`);
    }
    const stop = stopModel(factory);
    if (!stop.known) {
      push('factory:stop', 'unknown', 'Stop state unknown', 'The factory runtime did not report a stop observation.');
    } else if (stop.stopped) {
      push('factory:stop', 'warning', 'Factory is stopped', stop.reason ? `Explicit stop observed. Reason: ${stop.reason}` : 'Explicit stop observed.');
    }
  } else {
    const overviewFactory = sources.overview?.data?.factory;
    if (isObject(overviewFactory) && overviewFactory.stopped === true) {
      push(
        'factory:stop',
        'warning',
        'Factory is stopped',
        `The overview read observed a stop${overviewFactory.reason ? `: ${overviewFactory.reason}` : '.'} Factory runtime detail is unavailable.`,
      );
    }
  }

  const economics = sources.overview?.data?.economics;
  if (isObject(economics)) {
    const completeness = lower(economics.completeness);
    if (completeness === 'partial') {
      push('overview:economics', 'warning', 'Economics observation is partial', 'Observed cost/usage are not a complete accounting.');
    } else if (completeness !== 'complete') {
      push('overview:economics', 'unknown', 'Economics completeness unknown', `Completeness is ${exactText(economics.completeness)}.`);
    }
  } else if (sources.overview?.data !== null && sources.overview?.data !== undefined) {
    push('overview:economics', 'unknown', 'Economics observation missing', 'Overview has no economics data.');
  }

  for (const upstream of sources.overview?.data?.metadata?.sources ?? []) {
    if (!isObject(upstream)) continue;
    const state = lower(upstream.state);
    if (state === 'missing' || state === 'unreadable' || state === 'unknown') {
      const detail = typeof upstream.detail === 'string' && upstream.detail !== '' ? upstream.detail : `Source state is ${upstream.state}.`;
      push(`overview:source:${exactText(upstream.source)}`, 'unknown', `Read source ${exactText(upstream.source)} is ${exactText(upstream.state)}`, detail);
    }
  }

  const componentPage = componentPageModel(snapshot);
  if (componentPage.known) {
    for (const entry of componentPage.entries) {
      const severity = componentSeverity(entry);
      if (severity === null) continue;
      const pluginId = typeof entry.plugin_id === 'string' ? entry.plugin_id : 'unknown';
      const generation = entry.generation === null || entry.generation === undefined ? 'unknown' : String(entry.generation);
      const name = typeof entry.name === 'string' && entry.name !== '' ? entry.name : pluginId;
      const detail = `Observed in the current component page only: available=${entry.available === true}, health=${exactText(entry.health)}, freshness=${exactText(entry.freshness)}, completeness=${exactText(entry.completeness)}.`;
      push(
        `component:${pluginId}@${generation}`,
        severity,
        `${name} needs attention`,
        detail,
        formatHash({ name: 'component', params: { id: pluginId } }),
        'component page',
      );
    }
    if (componentPage.observed === 0) {
      push('components:page', 'unknown', 'No components in the observed page', 'This page contains no component entries.');
    }
  }

  const deduped = new Map();
  for (const entry of entries) {
    const existing = deduped.get(entry.key);
    if (!existing || SEVERITY_RANK[entry.severity] < SEVERITY_RANK[existing.severity]) {
      deduped.set(entry.key, entry);
    }
  }
  const ordered = [...deduped.values()].sort((left, right) => {
    const bySeverity = SEVERITY_RANK[left.severity] - SEVERITY_RANK[right.severity];
    if (bySeverity !== 0) return bySeverity;
    return left.key < right.key ? -1 : (left.key > right.key ? 1 : 0);
  });
  const visible = ordered.slice(0, ATTENTION_LIMIT);
  return {
    items: visible,
    hiddenCount: Math.max(0, ordered.length - ATTENTION_LIMIT),
    total: ordered.length,
  };
}

/** Coverage gaps stated separately from the observed attention entries. */
export function coverageGaps(snapshot) {
  const gaps = [];
  for (const item of sourceFreshnessModel(snapshot, snapshot?.renderNow ?? 0)) {
    if (item.state === 'not_loaded') gaps.push(`${item.title} source not loaded.`);
    else if (item.state === 'unavailable') gaps.push(`${item.title} source unavailable.`);
    else if (item.state === 'stale') gaps.push(`${item.title} data is stale (last success ${item.exact ?? 'unknown'}).`);
  }
  return gaps;
}

/**
 * Newest-first bounded activity model. Valid times sort descending with stable
 * source order for ties; invalid times stay last. At most 100 entries.
 */
export function timelineModel(entries, now) {
  const list = Array.isArray(entries) ? entries : [];
  const indexed = list.map((entry, index) => {
    const value = isObject(entry) ? entry.at_unix_ms : null;
    const time = timestampModel(value, now);
    return { entry, index, time };
  });
  indexed.sort((left, right) => {
    if (left.time.valid && right.time.valid) {
      if (left.time.exact === right.time.exact) return left.index - right.index;
      return left.entry.at_unix_ms > right.entry.at_unix_ms ? -1 : 1;
    }
    if (left.time.valid) return -1;
    if (right.time.valid) return 1;
    return left.index - right.index;
  });
  const bounded = indexed.slice(0, 100);
  return {
    items: bounded.map(({ entry, index, time }) => ({
      key: `timeline:${index}`,
      kind: entry?.kind ?? 'unknown',
      title: entry?.title,
      detail: entry?.detail,
      relative: time.relative,
      exact: time.exact,
      valid: time.valid,
      future: time.future,
      index,
    })),
    hiddenCount: Math.max(0, indexed.length - 100),
    total: indexed.length,
  };
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

function renderSummary(doc, snapshot) {
  const grid = el(doc, 'section', 'summary-grid');
  grid.id = 'overview-summary';
  grid.setAttribute('aria-label', 'Overview summary');
  for (const card of summaryCards(snapshot)) {
    grid.appendChild(summaryCard(
      doc,
      card.id,
      card.label,
      card.value,
      card.detail,
      card.tone,
      sourceStaleNotice(doc, snapshot, card.source),
    ));
  }
  return grid;
}

function renderCurrentWork(doc, cockpit, snapshot) {
  const root = panel(doc, 'Current work', 'current-work');
  root.classList.add('panel--dominant');
  appendSourceStale(doc, root, snapshot, 'factory');
  const model = currentWorkModel(snapshot);
  if (model.kind === 'unknown') {
    root.appendChild(emptyState(doc, `${model.label}.`));
    return root;
  }
  if (model.kind === 'none') {
    root.appendChild(textNode(doc, 'p', 'No current execution reported.', 'current-none'));
    root.appendChild(textNode(doc, 'p', 'Factory reports no active execution.', 'note'));
    return root;
  }
  root.appendChild(kv(doc, 'Task', model.task_id ?? 'unknown'));
  root.appendChild(kv(doc, 'Phase', model.phase ?? 'unknown'));
  const stateRow = kv(doc, 'State', model.state ?? 'unknown');
  stateRow.appendChild(doc.createTextNode(' '));
  stateRow.appendChild(pill(doc, model.state ?? 'unknown'));
  root.appendChild(stateRow);
  if (model.attempt !== null) root.appendChild(kv(doc, 'Attempt', model.attempt));
  const updated = timestampModel(model.updated_at_unix_ms, cockpit.renderNow);
  const updatedRow = kv(doc, 'Updated', updated.relative);
  updatedRow.appendChild(doc.createTextNode(' '));
  updatedRow.appendChild(exactTimeText(doc, updated));
  root.appendChild(updatedRow);
  const technical = disclosure(doc, 'overview:current-work', 'Technical identity');
  cockpit.bindDisclosure(technical, 'overview:current-work');
  technical.appendChild(kv(doc, 'Execution ID', model.execution_id ?? 'unknown', 'value mono'));
  technical.appendChild(kv(doc, 'Task ID', model.task_id ?? 'unknown', 'value mono'));
  technical.appendChild(kv(doc, 'Generation', model.generation ?? 'unknown', 'value mono'));
  root.appendChild(technical);
  return root;
}

function renderAttention(doc, cockpit, snapshot) {
  const root = panel(doc, 'Needs attention', 'attention');
  const attention = deriveAttention(snapshot);
  if (attention.items.length === 0) {
    root.appendChild(textNode(doc, 'p', 'No issues in available observations.', 'attention-empty'));
  } else {
    const list = el(doc, 'ul', 'attention-list');
    for (const item of attention.items) {
      const entry = el(doc, 'li', `attention-item attention-item--${item.severity}`);
      entry.dataset.attentionKey = item.key;
      entry.appendChild(el(doc, 'span', 'attention-severity', item.severity));
      const body = el(doc, 'div', 'attention-body');
      body.appendChild(textNode(doc, 'p', item.title, 'attention-title'));
      body.appendChild(textNode(doc, 'p', item.detail, 'attention-detail'));
      if (item.href) {
        const link = anchor(doc, item.href, 'Open component detail', 'inline-action');
        link.dataset.focusKey = `attention:${item.key}`;
        body.appendChild(link);
      }
      entry.appendChild(body);
      list.appendChild(entry);
    }
    root.appendChild(list);
  }
  if (attention.hiddenCount > 0) {
    root.appendChild(textNode(doc, 'p', `…and ${attention.hiddenCount} additional attention entries.`, 'note'));
  }
  const gaps = coverageGaps(snapshot);
  if (gaps.length > 0) {
    const details = disclosure(doc, 'overview:coverage', 'Coverage gaps');
    cockpit.bindDisclosure(details, 'overview:coverage');
    for (const gap of gaps) details.appendChild(textNode(doc, 'p', gap, 'note'));
    root.appendChild(details);
  }
  return root;
}

function renderEligible(doc, snapshot) {
  const root = panel(doc, 'Eligible work', 'eligible');
  appendSourceStale(doc, root, snapshot, 'factory');
  const eligible = snapshot?.sources?.factory?.data?.eligible;
  if (!isObject(eligible)) {
    root.appendChild(emptyState(doc, 'Eligible work unknown (factory runtime unavailable).'));
    return root;
  }
  const entries = Array.isArray(eligible.entries) ? eligible.entries.slice(0, 5) : [];
  if (entries.length === 0) {
    root.appendChild(emptyState(doc, 'No eligible work reported in this observation.'));
  } else {
    const rows = entries.map((entry) => [
      entry.task_id ?? 'unknown',
      entry.title ?? 'unknown',
      entry.priority ?? 'unknown',
      entry.family ?? 'unknown',
    ]);
    root.appendChild(table(doc, ['Task', 'Title', 'Priority', 'Family'], rows));
  }
  const totalKnown = eligible.total !== null && eligible.total !== undefined && String(eligible.total) !== '';
  const summary = totalKnown
    ? `Showing ${entries.length} of ${String(eligible.total)} eligible tasks in source order. This is not a dispatch order.`
    : `Showing ${entries.length} loaded entries in source order; total unknown. This is not a dispatch order.`;
  root.appendChild(textNode(doc, 'p', summary, 'note'));
  const tasksLink = anchor(doc, '#/tasks', 'Open Tasks queue', 'inline-action');
  tasksLink.dataset.focusKey = 'overview:tasks';
  root.appendChild(tasksLink);
  return root;
}

function renderEconomics(doc, cockpit, snapshot) {
  const root = panel(doc, 'Economics details', 'economics');
  appendSourceStale(doc, root, snapshot, 'overview');
  const cost = costModel(snapshot);
  if (!cost.known && cost.completeness === 'unknown' && cost.usage === null && cost.allowance === null) {
    root.appendChild(emptyState(doc, 'Economics unknown.'));
    return root;
  }
  root.appendChild(kv(doc, 'Completeness', cost.completeness));
  root.appendChild(kv(doc, 'Usage', cost.usage === null ? 'unknown' : `${cost.usage} ${cost.usage_unit ?? 'unknown'}`));
  root.appendChild(kv(doc, 'Cost', cost.known ? `${cost.cost} ${cost.unit ?? 'unknown'}` : 'unknown'));
  root.appendChild(kv(doc, 'Allowance', cost.allowance === null ? 'unknown' : `${cost.allowance} ${cost.allowance_unit ?? 'unknown'}`));
  if (cost.observed_at_unix_ms !== null) {
    const observed = timestampModel(cost.observed_at_unix_ms, cockpit.renderNow);
    const row = kv(doc, 'Observed', observed.relative);
    row.appendChild(doc.createTextNode(' '));
    row.appendChild(exactTimeText(doc, observed));
    root.appendChild(row);
  } else {
    root.appendChild(kv(doc, 'Observed', 'unknown'));
  }
  return root;
}

function activityItem(doc, item) {
  const entry = el(doc, 'li', 'activity-item');
  const time = el(doc, 'div', 'activity-time');
  const relative = el(doc, 'span', 'activity-relative', item.relative);
  if (item.exact) relative.setAttribute('title', item.exact);
  time.appendChild(relative);
  time.appendChild(exactTimeText(doc, { exact: item.exact }));
  entry.appendChild(time);
  const body = el(doc, 'div', 'activity-body');
  body.appendChild(textNode(doc, 'span', `${item.kind}: `, 'activity-kind mono'));
  body.appendChild(textNode(doc, 'span', item.title));
  if (item.detail !== null && item.detail !== undefined) {
    body.appendChild(doc.createTextNode(' — '));
    body.appendChild(textNode(doc, 'span', item.detail, 'activity-detail'));
  }
  entry.appendChild(body);
  return entry;
}

function renderTimeline(doc, cockpit, snapshot) {
  const root = panel(doc, 'Recent activity', 'timeline');
  const overview = snapshot?.sources?.overview;
  if (overview?.error && (overview.data === null || overview.data === undefined)) {
    root.appendChild(emptyState(doc, 'Recent activity unknown (overview source unavailable).'));
    return root;
  }
  appendSourceStale(doc, root, snapshot, 'overview');
  const timeline = overview?.data?.timeline;
  if (!Array.isArray(timeline)) {
    root.appendChild(emptyState(doc, 'Recent activity unknown.'));
    return root;
  }
  const model = timelineModel(timeline, cockpit.renderNow);
  if (model.items.length === 0) {
    root.appendChild(emptyState(doc, 'No recent activity reported.'));
    return root;
  }
  const visible = model.items.slice(0, 10);
  const more = model.items.slice(10);
  const list = el(doc, 'ol', 'activity-list');
  for (const item of visible) list.appendChild(activityItem(doc, item));
  root.appendChild(list);
  if (more.length > 0 || model.hiddenCount > 0) {
    const details = disclosure(doc, 'overview:timeline', `Show ${Math.min(model.items.length - 10, 90)} more activity entries (up to 100)`);
    cockpit.bindDisclosure(details, 'overview:timeline');
    if (more.length > 0) {
      const extended = el(doc, 'ol', 'activity-list');
      for (const item of more) extended.appendChild(activityItem(doc, item));
      details.appendChild(extended);
    }
    if (model.hiddenCount > 0) {
      details.appendChild(textNode(doc, 'p', `…and ${model.hiddenCount} more entries beyond the 100-entry bound.`, 'note'));
    }
    root.appendChild(details);
  }
  return root;
}

function renderRuntimeDetails(doc, snapshot) {
  const root = panel(doc, 'Factory / worker status', 'runtime');
  root.classList.add('panel--compact');
  appendSourceStale(doc, root, snapshot, 'factory');
  const factory = snapshot.sources.factory ?? { data: null, error: null };
  const runtime = runtimeModel(snapshot);
  root.appendChild(kv(doc, 'Runtime', runtime.state));
  if (isObject(factory.data)) {
    const stop = stopModel(factory.data);
    root.appendChild(kv(doc, 'Stop', stop.known ? (stop.stopped ? 'stopped' : 'not stopped') : 'unknown'));
    const current = currentWorkModel(snapshot);
    root.appendChild(kv(
      doc,
      'Current execution',
      current.kind === 'execution'
        ? `${current.execution_id ?? 'unknown'} (${current.state ?? 'unknown'})`
        : current.label,
    ));
    if (factory.data.reason) root.appendChild(kv(doc, 'Reason', factory.data.reason));
    for (const source of factory.data.sources ?? []) {
      root.appendChild(kv(doc, `source:${source.source}`, sourceLabel(source.state)));
    }
  } else {
    root.appendChild(kv(doc, 'Stop', 'unknown'));
    root.appendChild(kv(doc, 'Current execution', 'unknown'));
    root.appendChild(kv(
      doc,
      'Note',
      factory.error?.kind === ApiErrorKind.UNSUPPORTED
        ? 'factory runtime endpoint is not available in this slice'
        : 'factory runtime unavailable',
    ));
  }
  return root;
}

function renderControls(doc, cockpit) {
  const root = panel(doc, 'Controls', 'controls');
  const state = cockpit.controls.state();
  const note = state.known
    ? (state.readOnly ? 'Read-only: no control permission advertised.' : 'Controls available.')
    : 'controls disabled: runtime unknown or offline';
  root.appendChild(kv(doc, 'Permission', state.readOnly ? 'read-only' : (state.known ? 'available' : 'disabled')));
  root.appendChild(textNode(doc, 'p', note, 'note'));
  const buttons = el(doc, 'div', 'controls');
  const pauseButton = el(doc, 'button', null, 'Pause');
  pauseButton.type = 'button';
  pauseButton.disabled = !state.enabled.pause;
  pauseButton.dataset.action = 'pause';
  pauseButton.dataset.focusKey = 'control:pause';
  if (state.enabled.pause) pauseButton.addEventListener('click', () => cockpit.controls.pause());
  const resumeButton = el(doc, 'button', null, 'Resume');
  resumeButton.type = 'button';
  resumeButton.disabled = !state.enabled.resume;
  resumeButton.dataset.action = 'resume';
  resumeButton.dataset.focusKey = 'control:resume';
  if (state.enabled.resume) {
    resumeButton.addEventListener('click', () => {
      const phrase = cockpit.confirmPhrase();
      if (phrase !== RESUME_CONFIRMATION) return;
      cockpit.controls.resume(phrase);
    });
  }
  buttons.appendChild(pauseButton);
  buttons.appendChild(resumeButton);
  root.appendChild(buttons);
  if (state.pending) root.appendChild(kv(doc, 'Pending', String(state.pendingAction)));
  if (state.lastResult) {
    root.appendChild(kv(doc, 'Result', state.lastResult.kind === 'receipt' ? (state.lastResult.outcome ?? 'receipt') : state.lastResult.kind));
    if (state.lastResult.kind === 'unconfirmed') {
      root.appendChild(kv(doc, 'EffectUnconfirmed', state.lastResult.reason ?? 'effect unknown'));
    }
    if (state.lastResult.actionId) root.appendChild(kv(doc, 'Action ID', state.lastResult.actionId));
  }
  return root;
}

function renderDiagnostics(doc, cockpit, snapshot) {
  const details = disclosure(doc, 'overview:diagnostics', 'Technical diagnostics');
  cockpit.bindDisclosure(details, 'overview:diagnostics');
  const overview = snapshot.sources.overview?.data;
  const sources = overview?.metadata?.sources;
  const sourcePanel = el(doc, 'section', 'panel panel--diagnostic');
  sourcePanel.appendChild(el(doc, 'h3', null, 'Overview read sources'));
  if (Array.isArray(sources)) {
    for (const source of sources) {
      sourcePanel.appendChild(kv(doc, source.source, sourceLabel(source.state)));
    }
  } else {
    sourcePanel.appendChild(emptyState(doc, 'Read sources unknown.'));
  }
  details.appendChild(sourcePanel);

  const executions = panel(doc, 'Execution history (never the current work)', 'executions');
  const rows = Array.isArray(overview?.executions) ? overview.executions : [];
  if (rows.length === 0) {
    executions.appendChild(emptyState(doc, overview ? 'No executions recorded.' : 'Executions unknown.'));
  } else {
    const data = rows.slice(0, LIST_ROW_LIMIT).map((execution) => [
      execution.execution_id,
      execution.task_id,
      execution.generation,
      execution.phase,
      execution.state,
      execution.attempt ?? 'unknown',
      execution.revision ?? 'unknown',
      timestampModel(execution.updated_at_unix_ms, cockpit.renderNow).exact ?? 'unknown',
    ]);
    executions.appendChild(table(
      doc,
      ['Execution', 'Task', 'Generation', 'Phase', 'State', 'Attempt', 'Revision', 'Updated'],
      data,
    ));
  }
  details.appendChild(executions);

  const technical = el(doc, 'section', 'panel panel--diagnostic');
  technical.appendChild(el(doc, 'h3', null, 'Refresh state'));
  for (const item of sourceFreshnessModel(snapshot, cockpit.renderNow)) {
    const line = `${item.title}: ${item.label}${item.exact ? ` (${item.exact})` : ''}`;
    technical.appendChild(textNode(doc, 'p', line, 'note'));
  }
  technical.appendChild(anchor(doc, '#/system', 'Open System diagnostics', 'inline-action'));
  details.appendChild(technical);
  return details;
}

/** Render the Overview page. */
export function render(cockpit) {
  const doc = cockpit.doc;
  const snapshot = cockpit.snapshot();
  snapshot.renderNow = cockpit.renderNow;
  const page = el(doc, 'div', 'page page--overview');
  page.dataset.page = 'overview';
  page.appendChild(renderSummary(doc, snapshot));
  // One Overview controls region, before the content grid so primary actions
  // stay reachable on short/mobile viewports without scrolling a long page.
  page.appendChild(renderControls(doc, cockpit));

  const overviewGrid = el(doc, 'div', 'overview-grid');
  const main = el(doc, 'div', 'overview-main');
  main.appendChild(renderCurrentWork(doc, cockpit, snapshot));
  main.appendChild(renderEligible(doc, snapshot));
  main.appendChild(renderEconomics(doc, cockpit, snapshot));
  const side = el(doc, 'aside', 'overview-side');
  side.appendChild(renderAttention(doc, cockpit, snapshot));
  side.appendChild(renderRuntimeDetails(doc, snapshot));
  overviewGrid.appendChild(main);
  overviewGrid.appendChild(side);
  page.appendChild(overviewGrid);

  page.appendChild(renderTimeline(doc, cockpit, snapshot));
  page.appendChild(renderDiagnostics(doc, cockpit, snapshot));
  return page;
}

export { FIELD_TEXT_LIMIT, boundedText, formatHash };
