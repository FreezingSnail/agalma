// WEB.9 generic declared-component view renderer.
//
// This module owns the browser-side interpretation of the canonical
// `ViewKind` building blocks. It consumes only the current `ReadComponent`
// projection: descriptor identity/order, bounded report envelopes, and the
// active binding pin. It has no plugin/component-ID table, no private payload
// types and no executable markup or remote fetch.
//
// Deterministic rules:
// - a view reads only its own declared namespace;
// - the page binding pin must match every envelope for that namespace exactly
//   (legacy numeric generations only when safely integral);
// - the greatest sequence wins before the descriptor schema version is checked,
//   so a superseded older-version row can never be resurrected;
// - field lookup is own-property only, supports dotted paths, and keeps null,
//   false, zero and absent distinct;
// - text is bounded to 4 KiB and rows to 100, with table/timeline presentation
//   defaulting to 24 rows plus an explicit expansion marker;
// - ArtifactLink is inert metadata: it never produces an `href`/download/URL.

export const VIEW_TEXT_LIMIT = 4096;
export const VIEW_ROW_LIMIT = 100;
export const VIEW_DEFAULT_ROWS = 24;

export const SUPPORTED_VIEW_KINDS = Object.freeze([
  'Status', 'Metrics', 'Table', 'Timeline', 'Alert', 'ArtifactLink',
]);

const U64_MAX = 18446744073709551615n;
const EXACT_U64 = /^(0|[1-9][0-9]{0,19})$/;
const RECOGNIZED_TONES = new Set([
  'ready', 'degraded', 'unavailable', 'unknown', 'stale',
  'running', 'idle', 'paused', 'failed',
  'ok', 'healthy', 'info', 'warn', 'warning', 'error', 'critical',
]);

/** Bound one text value to the shared 4 KiB presentation limit. */
export function bounded(value, limit = VIEW_TEXT_LIMIT) {
  const text = value === null || value === undefined ? '' : String(value);
  if (text.length <= limit) return { text, truncated: false };
  return { text: text.slice(0, limit), truncated: true };
}

/**
 * Canonical exact-u64 text, or null. Decimal strings are accepted exactly;
 * legacy numeric values only when safely integral (never rounded).
 */
export function exactU64(value) {
  if (typeof value === 'string') {
    if (!EXACT_U64.test(value)) return null;
    try {
      return BigInt(value) <= U64_MAX ? value : null;
    } catch {
      return null;
    }
  }
  if (typeof value === 'number') {
    if (!Number.isSafeInteger(value) || value < 0) return null;
    return String(value);
  }
  return null;
}

/** A safe browser schema version, or null when it cannot be compared exactly. */
export function safeSchemaVersion(value) {
  return Number.isSafeInteger(value) && value >= 0 ? value : null;
}

function hasOwn(value, key) {
  return value !== null
    && (typeof value === 'object' || typeof value === 'function')
    && Object.prototype.hasOwnProperty.call(value, key);
}

/** Look up one top-level or dotted field using own properties only. */
export function readField(payload, field) {
  if (typeof field !== 'string') return { found: false };
  let current = payload;
  for (const part of field.split('.')) {
    if (!hasOwn(current, part)) return { found: false };
    current = current[part];
  }
  return { found: true, value: current };
}

/** Exact text for one JSON value. Null/false/zero stay distinct. */
export function valueText(value) {
  if (value === null) return { text: 'null', truncated: false };
  if (value === undefined) return { text: 'missing', truncated: false };
  if (typeof value === 'string') return bounded(value);
  if (typeof value === 'boolean') return { text: value ? 'true' : 'false', truncated: false };
  if (typeof value === 'number') return bounded(String(value));
  try {
    return bounded(JSON.stringify(value));
  } catch {
    return bounded(String(value));
  }
}

function isObject(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function isScalar(value) {
  return value === null || ['string', 'number', 'boolean'].includes(typeof value);
}

/** Recognized display tone, or null for an unclassified value. */
export function displayTone(value) {
  if (value === null || value === undefined) return null;
  const text = String(value).trim().toLowerCase();
  return RECOGNIZED_TONES.has(text) ? text : null;
}

function metricRows(fields, payload) {
  if (!isObject(payload)) return null;
  const rows = [];
  if (fields.length === 0) {
    for (const key of Object.keys(payload)) {
      const value = valueText(payload[key]);
      rows.push({ field: key, label: key, text: value.text, truncated: value.truncated, missing: false });
    }
  } else {
    for (const field of fields) {
      const hit = readField(payload, field);
      if (!hit.found) {
        rows.push({ field, label: field, text: 'missing', truncated: false, missing: true });
      } else {
        const value = valueText(hit.value);
        rows.push({ field, label: field, text: value.text, truncated: value.truncated, missing: false });
      }
    }
  }
  return {
    rows: rows.slice(0, VIEW_ROW_LIMIT),
    truncated: Math.max(0, rows.length - VIEW_ROW_LIMIT),
  };
}

function statusModel(fields, payload) {
  if (!isObject(payload)) return { state: 'malformed' };
  if (fields.length === 0) {
    return { state: 'rendered', value: 'unknown', known: false, tone: null, field: null, truncated: false };
  }
  for (const field of fields) {
    const hit = readField(payload, field);
    if (!hit.found) continue;
    if (hit.value === null) {
      return { state: 'rendered', value: 'unknown', known: false, tone: null, field, truncated: false };
    }
    if (!isScalar(hit.value)) continue;
    const value = valueText(hit.value);
    return {
      state: 'rendered',
      value: value.text,
      known: true,
      tone: displayTone(hit.value),
      field,
      truncated: value.truncated,
    };
  }
  if (fields.length === 0 && payload !== null && typeof payload === 'object') {
    const value = valueText(payload);
    return { state: 'rendered', value: value.text, known: true, tone: null, field: null, truncated: value.truncated };
  }
  return { state: 'rendered', value: 'unknown', known: false, tone: null, field: null, truncated: false };
}

function tableModel(fields, payload) {
  if (!Array.isArray(payload)) return { state: 'malformed' };
  if (payload.length === 0) return { state: 'empty', headers: fields.length === 0 ? ['value'] : fields, rows: [], total: 0 };
  const headers = fields.length === 0 ? ['value'] : fields;
  const rows = payload.slice(0, VIEW_ROW_LIMIT).map((item) => {
    if (fields.length === 0) {
      const value = valueText(item);
      return [{ text: value.text, truncated: value.truncated, missing: false }];
    }
    return fields.map((field) => {
      const hit = readField(item, field);
      if (!hit.found) return { text: 'missing', truncated: false, missing: true };
      const value = valueText(hit.value);
      return { text: value.text, truncated: value.truncated, missing: false };
    });
  });
  return {
    state: 'rendered',
    headers,
    rows,
    total: payload.length,
    truncated: Math.max(0, payload.length - VIEW_ROW_LIMIT),
  };
}

function timelineModel(fields, payload) {
  let items;
  if (Array.isArray(payload)) items = payload;
  else if (isObject(payload) && Array.isArray(payload.entries)) items = payload.entries;
  else return { state: 'malformed' };
  if (items.length === 0) return { state: 'empty', rows: [], total: 0 };
  const rows = items.slice(0, VIEW_ROW_LIMIT).map((item) => {
    let time = { text: 'missing', truncated: false };
    if (fields.length > 0) {
      const hit = readField(item, fields[0]);
      if (hit.found) time = valueText(hit.value);
    }
    let text;
    if (fields.length <= 1) {
      text = valueText(item);
    } else {
      const parts = [];
      let truncated = false;
      for (const field of fields.slice(1)) {
        const hit = readField(item, field);
        if (!hit.found) {
          parts.push('missing');
          continue;
        }
        const value = valueText(hit.value);
        parts.push(value.text);
        truncated = truncated || value.truncated;
      }
      text = { text: parts.join(' '), truncated };
    }
    return {
      time: time.text,
      timeTruncated: time.truncated,
      text: text.text,
      truncated: text.truncated,
    };
  });
  return {
    state: 'rendered',
    rows,
    total: items.length,
    truncated: Math.max(0, items.length - VIEW_ROW_LIMIT),
  };
}

function alertModel(fields, payload) {
  if (!isObject(payload)) return { state: 'malformed' };
  let levelHit = readField(payload, 'level');
  let levelField = 'level';
  if (!levelHit.found) {
    levelHit = readField(payload, 'severity');
    levelField = 'severity';
  }
  if (!levelHit.found && fields.includes('level')) {
    levelHit = readField(payload, 'level');
  }
  const levelValue = levelHit.found ? valueText(levelHit.value).text : 'unclassified';
  const tone = levelHit.found ? displayTone(levelHit.value) : null;
  const parts = [];
  let truncated = false;
  for (const field of fields) {
    if (field === levelField || field === 'level' || field === 'severity') continue;
    const hit = readField(payload, field);
    if (!hit.found) {
      parts.push('missing');
      continue;
    }
    const value = valueText(hit.value);
    parts.push(value.text);
    truncated = truncated || value.truncated;
  }
  if (parts.length === 0) {
    for (const field of ['message', 'detail', 'summary']) {
      const hit = readField(payload, field);
      if (!hit.found) continue;
      const value = valueText(hit.value);
      parts.push(value.text);
      truncated = truncated || value.truncated;
      break;
    }
  }
  if (parts.length === 0 && Object.keys(payload).length > 0) {
    const value = valueText(payload);
    parts.push(value.text);
    truncated = truncated || value.truncated;
  }
  if (parts.length === 0 && levelValue === 'unclassified') return { state: 'empty' };
  return {
    state: 'rendered',
    level: levelValue,
    levelKnown: tone !== null,
    tone,
    message: parts.join(' '),
    truncated,
  };
}

function artifactModel(fields, payload) {
  if (!isObject(payload)) return { state: 'malformed' };
  const entries = [];
  const names = fields.length === 0 ? Object.keys(payload) : fields;
  for (const field of names) {
    const hit = readField(payload, field);
    if (!hit.found) {
      entries.push({ field, label: field, text: 'missing', truncated: false, missing: true });
      continue;
    }
    const value = valueText(hit.value);
    entries.push({ field, label: field, text: value.text, truncated: value.truncated, missing: false });
  }
  if (entries.length === 0 && Object.keys(payload).length === 0) return { state: 'empty' };
  return {
    state: 'rendered',
    entries: entries.slice(0, VIEW_ROW_LIMIT),
    truncated: Math.max(0, entries.length - VIEW_ROW_LIMIT),
    download: 'unavailable',
  };
}

function renderKindModel(kind, fields, payload) {
  switch (kind) {
    case 'Status': return statusModel(fields, payload);
    case 'Metrics': {
      const metrics = metricRows(fields, payload);
      if (metrics === null) return { state: 'malformed' };
      if (metrics.rows.length === 0) return { state: 'empty', rows: [], truncated: 0 };
      return { state: 'rendered', ...metrics };
    }
    case 'Table': return tableModel(fields, payload);
    case 'Timeline': return timelineModel(fields, payload);
    case 'Alert': return alertModel(fields, payload);
    case 'ArtifactLink': return artifactModel(fields, payload);
    default: return { state: 'unsupported' };
  }
}

function canonicalBinding(binding) {
  if (!isObject(binding)) return null;
  const generation = exactU64(binding.generation);
  if (generation === null) return null;
  if (typeof binding.binding_id !== 'string') return null;
  if (typeof binding.api !== 'string') return null;
  if (!Number.isSafeInteger(binding.api_version) || binding.api_version < 0) return null;
  if (typeof binding.implementation_version !== 'string') return null;
  if (typeof binding.config_digest !== 'string') return null;
  return {
    binding_id: binding.binding_id,
    api: binding.api,
    api_version: binding.api_version,
    implementation_version: binding.implementation_version,
    generation,
    config_digest: binding.config_digest,
  };
}

/** Full exact binding-pin equality, including the canonical generation text. */
export function bindingsMatch(left, right) {
  const a = canonicalBinding(left);
  const b = canonicalBinding(right);
  if (!a || !b) return false;
  return a.binding_id === b.binding_id
    && a.api === b.api
    && a.api_version === b.api_version
    && a.implementation_version === b.implementation_version
    && a.generation === b.generation
    && a.config_digest === b.config_digest;
}

function selectNamespace(entries, pluginId, pageBinding) {
  let conflict = false;
  let malformed = false;
  let maximum = null;
  let latest = [];
  for (const entry of entries) {
    if (!isObject(entry)) {
      malformed = true;
      continue;
    }
    if (entry.plugin_id !== pluginId) conflict = true;
    if (!bindingsMatch(entry.binding, pageBinding)) conflict = true;
    const sequence = exactU64(entry.sequence);
    if (sequence === null) {
      malformed = true;
      continue;
    }
    const numeric = BigInt(sequence);
    if (maximum === null || numeric > maximum) {
      maximum = numeric;
      latest = [entry];
    } else if (numeric === maximum) {
      latest.push(entry);
    }
  }
  if (conflict) return { state: 'conflict', note: 'Conflicting publisher or binding identity for this namespace.' };
  if (malformed) return { state: 'malformed', note: 'Malformed report envelope identity for this namespace.' };
  if (latest.length === 0) return { state: 'absent' };
  const identity = (entry) => [
    entry.schema_namespace,
    safeSchemaVersion(entry.schema_version),
    entry.payload_digest,
    canonicalBinding(entry.binding)?.generation ?? null,
  ].join('\u0000');
  const first = identity(latest[0]);
  if (latest.some((entry) => identity(entry) !== first)) {
    return { state: 'conflict', note: 'Multiple latest reports disagree on schema or payload identity.' };
  }
  if (safeSchemaVersion(latest[0].schema_version) === null) {
    return { state: 'malformed', note: 'The report schema version is outside the exact browser range.' };
  }
  return { state: 'selected', entry: latest[0] };
}

function awaitingNote(reportState, nextCursor) {
  let note;
  if (reportState === 'missing') note = 'The report projection is missing for this component.';
  else if (reportState === 'empty') note = 'No reports have been published for this component.';
  else if (reportState === 'unreadable' || reportState === 'unknown') note = 'Report data is unknown for this component.';
  else note = 'Awaiting data for this view on this page.';
  if (nextCursor !== null && nextCursor !== undefined) {
    note += ' Only the current report page is loaded; more pages may exist.';
  }
  return note;
}

/**
 * Resolve the current page into declared view models.
 *
 * The result never contains another route subject or an unmatched binding.
 * `reportState` is the `reports` metadata source state; `stale` is the browser
 * source-retention flag (last good data displayed after a failed refresh).
 */
export function resolveComponentViews(options = {}) {
  const pluginId = typeof options.pluginId === 'string' ? options.pluginId : '';
  const binding = options.binding;
  const pageBinding = canonicalBinding(binding);
  const descriptors = Array.isArray(options.descriptors) ? options.descriptors : [];
  const entries = Array.isArray(options.entries) ? options.entries : [];
  const nextCursor = options.nextCursor ?? null;
  const reportState = typeof options.reportState === 'string' ? options.reportState : 'unknown';
  const stale = options.stale === true;

  const grouped = new Map();
  for (const entry of entries) {
    const namespace = isObject(entry) ? entry.schema_namespace : null;
    if (typeof namespace !== 'string') continue;
    if (!grouped.has(namespace)) grouped.set(namespace, []);
    grouped.get(namespace).push(entry);
  }

  const views = descriptors.map((descriptor) => {
    const namespace = isObject(descriptor) ? descriptor.schema_namespace : null;
    const title = isObject(descriptor) && typeof descriptor.title === 'string' ? descriptor.title : '(untitled view)';
    const kind = isObject(descriptor) && typeof descriptor.kind === 'string' ? descriptor.kind : '';
    const viewId = isObject(descriptor) && typeof descriptor.view_id === 'string' ? descriptor.view_id : '';
    const expectedVersion = isObject(descriptor) ? safeSchemaVersion(descriptor.schema_version) : null;
    const fields = isObject(descriptor) && Array.isArray(descriptor.fields)
      ? descriptor.fields.filter((field) => typeof field === 'string')
      : [];
    const base = {
      view_id: viewId,
      title,
      kind,
      schema_namespace: typeof namespace === 'string' ? namespace : '',
      schema_version: expectedVersion,
      fields,
      data: null,
      entry: null,
      identity: null,
      note: null,
    };
    if (!SUPPORTED_VIEW_KINDS.includes(kind)) {
      return { ...base, state: 'unsupported', note: `Unsupported view kind: ${kind || 'unknown'}.` };
    }
    if (expectedVersion === null) {
      return { ...base, state: 'unsupported', note: 'The declared schema version is outside the exact browser range.' };
    }
    if (!pageBinding) {
      return { ...base, state: 'conflict', note: 'The active binding pin is malformed or missing.' };
    }
    const selection = typeof namespace === 'string' && grouped.has(namespace)
      ? selectNamespace(grouped.get(namespace), pluginId, pageBinding)
      : { state: 'absent' };
    if (selection.state === 'conflict') return { ...base, state: 'conflict', note: selection.note };
    if (selection.state === 'malformed') return { ...base, state: 'malformed', note: selection.note };
    if (selection.state === 'absent') {
      const state = reportState === 'missing'
        ? 'missing'
        : (['unreadable', 'unknown'].includes(reportState) ? 'unknown' : (reportState === 'empty' ? 'empty' : 'awaiting'));
      return { ...base, state, note: awaitingNote(reportState, nextCursor) };
    }

    const entry = selection.entry;
    if (entry.schema_version !== expectedVersion) {
      return {
        ...base,
        state: 'schema-mismatch',
        entry,
        note: `View expects schema v${expectedVersion} but the latest report carries v${entry.schema_version}.`,
      };
    }
    const model = renderKindModel(kind, fields, entry.payload);
    if (model.state === 'malformed') {
      return { ...base, state: 'malformed', entry, note: 'The report payload has an unsupported structure; raw data is retained in Details.' };
    }
    if (model.state === 'unsupported') {
      return { ...base, state: 'unsupported', entry, note: 'The report building block is unsupported.' };
    }
    const identity = {
      schema_namespace: entry.schema_namespace,
      schema_version: entry.schema_version,
      sequence: exactU64(entry.sequence),
      payload_digest: typeof entry.payload_digest === 'string' ? entry.payload_digest : '',
      reported_at_unix_ms: exactU64(entry.reported_at_unix_ms),
      observed_at_unix_ms: exactU64(entry.observed_at_unix_ms),
      execution_id: entry.execution_id ?? null,
      durability: typeof entry.durability === 'string' ? entry.durability : '',
    };
    return {
      ...base,
      state: model.state === 'empty' ? 'empty' : (stale ? 'stale' : 'rendered'),
      data: model,
      entry,
      identity,
      note: null,
    };
  });

  return { views, reportState, stale };
}
