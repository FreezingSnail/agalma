// WEB.9 generic component detail page (#/components/<plugin_id>).
//
// The page is the consumer of the accepted canonical renderer contract:
// declared `ViewKind` descriptors are rendered in descriptor order from the
// current `ReadComponent` report page. Envelope selection, binding matching,
// schema checks and kind interpretation live in `views.js`; this module only
// builds bounded inert DOM. Registration remains a registry fact. A component
// with no reports still shows its binding and declared views.

import {
  panel, table, pager, emptyState, staleNotice, kv, textNode, el, pill,
  boundedText, disclosure, formatTimestamp, componentStatusModel, timestampModel,
} from '../ui.js';
import {
  resolveComponentViews, exactU64, VIEW_DEFAULT_ROWS,
} from '../views.js';

export const title = 'Component';

function renderBinding(doc, root, binding) {
  root.appendChild(kv(doc, 'Binding', binding?.binding_id ?? 'unknown'));
  root.appendChild(kv(doc, 'API', `${binding?.api ?? 'unknown'} v${binding?.api_version ?? 'unknown'}`));
  root.appendChild(kv(doc, 'Implementation', binding?.implementation_version ?? 'unknown'));
  root.appendChild(kv(doc, 'Generation (pin)', exactU64(binding?.generation) ?? 'unknown'));
  root.appendChild(kv(doc, 'Config digest', binding?.config_digest ?? 'unknown'));
}

function renderDescriptors(doc, root, descriptors) {
  root.appendChild(el(doc, 'h3', null, 'Declared views'));
  if (!descriptors || descriptors.length === 0) {
    root.appendChild(emptyState(doc, 'No declared views.'));
    return;
  }
  const rows = descriptors.map((view) => [
    view.view_id,
    view.kind,
    view.title,
    view.schema_namespace,
    String(view.schema_version),
    (view.fields ?? []).join(', '),
  ]);
  root.appendChild(table(doc, ['View', 'Kind', 'Title', 'Namespace', 'Version', 'Fields'], rows));
}

function renderEnvelopes(doc, root, entries) {
  root.appendChild(el(doc, 'h3', null, 'Reports'));
  if (!entries || entries.length === 0) {
    root.appendChild(emptyState(doc, 'No report envelopes.'));
    return;
  }
  const rows = entries.map((entry) => [
    entry.schema_namespace,
    String(entry.schema_version),
    String(entry.sequence),
    String(entry.observed_at_unix_ms),
    entry.execution_id ?? 'none',
    entry.payload_digest,
  ]);
  root.appendChild(table(doc, ['Namespace', 'Schema', 'Sequence', 'Observed', 'Execution', 'Digest'], rows));
  for (const entry of entries) {
    const block = el(doc, 'div', 'panel');
    block.appendChild(kv(doc, 'Envelope', entry.schema_namespace));
    const payload = boundedText(JSON.stringify(entry.payload ?? null));
    block.appendChild(textNode(doc, 'pre', payload.text));
    if (payload.truncated) block.appendChild(textNode(doc, 'p', '…[truncated]', 'empty'));
    root.appendChild(block);
  }
}

function subjectKey(pluginId, binding) {
  const generation = exactU64(binding?.generation) ?? 'unknown';
  return `component:${pluginId}@${generation}`;
}

function sourceState(doc, label, source) {
  const row = el(doc, 'div', 'component-source-state');
  row.appendChild(el(doc, 'span', 'key', label));
  row.appendChild(doc.createTextNode(': '));
  row.appendChild(pill(doc, source?.state ?? 'unknown'));
  if (source?.observed_at_unix_ms !== null && source?.observed_at_unix_ms !== undefined) {
    row.appendChild(doc.createTextNode(' '));
    row.appendChild(textNode(doc, 'span', formatTimestamp(source.observed_at_unix_ms), 'note mono'));
  }
  if (typeof source?.detail === 'string' && source.detail !== '') {
    row.appendChild(doc.createTextNode(' — '));
    row.appendChild(textNode(doc, 'span', source.detail, 'note'));
  }
  return row;
}

function statusRow(doc, label, value) {
  const row = el(doc, 'div', 'kv component-status-row');
  row.appendChild(textNode(doc, 'span', label, 'key'));
  row.appendChild(doc.createTextNode(': '));
  if (value !== null && typeof value === 'object' && typeof value.nodeType === 'number') {
    row.appendChild(value);
  } else {
    row.appendChild(textNode(doc, 'span', value, 'value'));
  }
  return row;
}

function observationText(model, now) {
  if (!model.lastObservation) return 'time unknown';
  const time = timestampModel(model.lastObservation.at, now);
  return `${time.relative} (${time.exact ?? 'time unknown'}; ${model.lastObservation.source})`;
}

/**
 * The shared generic status summary. It renders the evidence-bearing projection
 * only; legacy inferred health/availability fields are never consulted.
 */
function renderStatusSummary(doc, root, model, options) {
  const {
    now, context, detailError = null, listSourceAt = null, pluginId = null,
  } = options;
  const section = el(doc, 'section', 'component-status-summary');
  section.dataset.statusSource = context;
  section.appendChild(el(doc, 'h3', null, 'Observed status'));

  if (context === 'list-fallback') {
    section.appendChild(textNode(
      doc,
      'p',
      `Detail read failed. Showing the previous Components-page observation for this exact plugin ID (list source age: ${formatTimestamp(listSourceAt)}). This is not a current detail read.`,
      'callout--stale',
    ));
  } else if (context === 'unknown') {
    section.appendChild(textNode(
      doc,
      'p',
      'Detail read failed and no matching component is on the loaded Components page. Status is unknown.',
      'callout--stale',
    ));
  } else if (context === 'retained-detail') {
    section.appendChild(textNode(
      doc,
      'p',
      'Showing the last successful detail read. Values and observation times are retained; current status is unknown until a read succeeds.',
      'callout--stale',
    ));
  } else if (!model.known) {
    section.appendChild(textNode(
      doc,
      'p',
      'This response has no evidence-bearing status object. Status is unknown; legacy inferred health is not used.',
      'note',
    ));
  }

  if (model.lifecycle === 'failed' || model.lifecycle === 'retired') {
    section.dataset.overall = 'attention';
    section.appendChild(textNode(
      doc,
      'p',
      `Lifecycle is ${model.lifecycle}. This component is not healthy even if an earlier heartbeat reported Ready.`,
      'callout--reload',
    ));
  }
  if (model.stale) {
    section.appendChild(textNode(
      doc,
      'p',
      'Report observation is stale. The original values and times are shown; they are not a current health claim.',
      'callout--stale',
    ));
  }

  const healthText = model.health === null
    ? 'Unknown'
    : model.stale
      ? `${model.health} (stale observation)`
      : model.incomplete
        ? `${model.health} (incomplete report)`
        : model.reporting === 'unavailable'
          ? `${model.health} (report unavailable)`
          : model.health;
  const rows = el(doc, 'div', 'component-status-rows');
  if (pluginId !== null) rows.appendChild(statusRow(doc, 'Identity', pluginId));
  rows.appendChild(statusRow(doc, 'Origin', model.originLabel));
  rows.appendChild(statusRow(doc, 'Generation', model.generationLabel));
  rows.appendChild(statusRow(doc, 'Desired', model.desiredLabel));
  rows.appendChild(statusRow(doc, 'Observed lifecycle', model.lifecycleLabel));
  rows.appendChild(statusRow(
    doc,
    'Lifecycle observed',
    model.lifecycleAt === null ? 'time unknown' : formatTimestamp(model.lifecycleAt),
  ));
  rows.appendChild(statusRow(doc, 'Reported health', healthText));
  rows.appendChild(statusRow(
    doc,
    'Health observed',
    model.healthAt === null ? 'time unknown' : formatTimestamp(model.healthAt),
  ));
  rows.appendChild(statusRow(doc, 'Reporting', model.incomplete
    ? `${model.reportingLabel} (incomplete)`
    : model.reportingLabel));
  rows.appendChild(statusRow(doc, 'Last known observation', observationText(model, now)));
  if (model.healthDetail) rows.appendChild(statusRow(doc, 'Health detail', model.healthDetail));
  rows.appendChild(statusRow(doc, 'Issue', model.lastError ?? model.issue ?? '—'));
  if (detailError) {
    rows.appendChild(statusRow(
      doc,
      'Detail error',
      `${detailError.kind ?? 'error'}: ${detailError.message ?? String(detailError)}`,
    ));
  }
  section.appendChild(rows);
  root.appendChild(section);
}

/** The exact plugin-ID match on the already loaded Components page, if any. */
function loadedListEntry(cockpit, pluginId) {
  const entries = cockpit.state.components?.page?.entries;
  if (!Array.isArray(entries)) return null;
  return entries.find((entry) => entry.plugin_id === pluginId) ?? null;
}

function renderStatus(doc, section, view) {
  const line = el(doc, 'p', 'component-status');
  if (view.data.known && view.data.tone) {
    line.appendChild(pill(doc, view.data.value));
  } else {
    line.appendChild(textNode(
      doc,
      'span',
      view.data.value,
      `component-status-value${view.data.known ? '' : ' component-status-value--unknown'}`,
    ));
    if (!view.data.known) line.appendChild(textNode(doc, 'span', ' unknown or absent', 'note'));
  }
  if (view.data.truncated) line.appendChild(textNode(doc, 'span', ' …[truncated]', 'note'));
  section.appendChild(line);
}

function renderMetrics(doc, section, view) {
  const list = el(doc, 'div', 'component-metrics');
  for (const row of view.data.rows) {
    list.appendChild(kv(
      doc,
      row.label,
      row.text,
      row.missing ? 'value value--missing' : 'value',
    ));
  }
  section.appendChild(list);
  if (view.data.truncated > 0) {
    section.appendChild(textNode(doc, 'p', `…and ${view.data.truncated} more metrics.`, 'truncation'));
  }
}

function cellNode(doc, cell) {
  return textNode(
    doc,
    'span',
    cell.text,
    cell.missing ? 'value value--missing' : null,
  );
}

function buildTable(doc, rows, headings) {
  const root = el(doc, 'table', 'table');
  const head = el(doc, 'thead');
  const headRow = el(doc, 'tr');
  for (const heading of headings) headRow.appendChild(el(doc, 'th', null, heading));
  head.appendChild(headRow);
  root.appendChild(head);
  const body = el(doc, 'tbody');
  for (const row of rows) {
    const tr = el(doc, 'tr');
    for (const cell of row) {
      const td = el(doc, 'td');
      td.appendChild(cellNode(doc, cell));
      tr.appendChild(td);
    }
    body.appendChild(tr);
  }
  root.appendChild(body);
  return root;
}

function renderTable(doc, cockpit, section, view, key) {
  const visible = view.data.rows.slice(0, VIEW_DEFAULT_ROWS);
  const remaining = view.data.rows.slice(VIEW_DEFAULT_ROWS);
  section.appendChild(buildTable(doc, visible, view.data.headers));
  if (view.data.total > VIEW_DEFAULT_ROWS) {
    section.appendChild(textNode(doc, 'p', `…and ${view.data.total - VIEW_DEFAULT_ROWS} more rows.`, 'truncation'));
    const details = disclosure(doc, `${key}:rows`, `Show all ${view.data.total} rows`);
    cockpit.bindDisclosure(details, `${key}:rows`);
    const body = el(doc, 'div', 'component-expansion');
    if (remaining.length > 0) body.appendChild(buildTable(doc, remaining, view.data.headers));
    if (view.data.truncated > 0) {
      body.appendChild(textNode(
        doc,
        'p',
        `…and ${view.data.truncated} more rows beyond the ${view.data.total} rows on this page.`,
        'truncation',
      ));
    }
    details.appendChild(body);
    section.appendChild(details);
  } else if (view.data.truncated > 0) {
    section.appendChild(textNode(doc, 'p', `…and ${view.data.truncated} more rows.`, 'truncation'));
  }
}

function timelineList(doc, rows) {
  const list = el(doc, 'ul', 'timeline');
  for (const row of rows) {
    const item = el(doc, 'li');
    item.appendChild(textNode(doc, 'span', row.time, 'time'));
    item.appendChild(textNode(doc, 'span', row.text));
    if (row.truncated) item.appendChild(textNode(doc, 'span', ' …[truncated]', 'note'));
    list.appendChild(item);
  }
  return list;
}

function renderTimeline(doc, cockpit, section, view, key) {
  const visible = view.data.rows.slice(0, VIEW_DEFAULT_ROWS);
  const remaining = view.data.rows.slice(VIEW_DEFAULT_ROWS);
  section.appendChild(timelineList(doc, visible));
  if (view.data.total > VIEW_DEFAULT_ROWS) {
    section.appendChild(textNode(doc, 'p', `…and ${view.data.total - VIEW_DEFAULT_ROWS} more entries.`, 'truncation'));
    const details = disclosure(doc, `${key}:timeline`, `Show all ${view.data.total} entries`);
    cockpit.bindDisclosure(details, `${key}:timeline`);
    const body = el(doc, 'div', 'component-expansion');
    if (remaining.length > 0) body.appendChild(timelineList(doc, remaining));
    if (view.data.truncated > 0) {
      body.appendChild(textNode(
        doc,
        'p',
        `…and ${view.data.truncated} more entries beyond the ${view.data.total} entries on this page.`,
        'truncation',
      ));
    }
    details.appendChild(body);
    section.appendChild(details);
  } else if (view.data.truncated > 0) {
    section.appendChild(textNode(doc, 'p', `…and ${view.data.truncated} more entries.`, 'truncation'));
  }
}

function renderAlert(doc, section, view) {
  const alert = el(
    doc,
    'div',
    `component-alert${view.data.levelKnown ? '' : ' component-alert--unclassified'}`,
  );
  const level = el(doc, 'span', 'component-alert-level');
  level.appendChild(pill(doc, view.data.level));
  if (!view.data.levelKnown) level.appendChild(textNode(doc, 'span', ' unclassified severity', 'note'));
  alert.appendChild(level);
  alert.appendChild(textNode(doc, 'p', view.data.message, 'component-alert-message'));
  if (view.data.truncated) alert.appendChild(textNode(doc, 'p', '…[truncated]', 'truncation'));
  section.appendChild(alert);
}

function renderArtifact(doc, section, view) {
  const list = el(doc, 'div', 'component-artifact');
  for (const row of view.data.entries) {
    list.appendChild(kv(
      doc,
      row.label,
      row.text,
      row.missing ? 'value value--missing' : 'value',
    ));
  }
  section.appendChild(list);
  if (view.data.truncated > 0) {
    section.appendChild(textNode(doc, 'p', `…and ${view.data.truncated} more metadata fields.`, 'truncation'));
  }
  section.appendChild(textNode(
    doc,
    'p',
    'Download unavailable. Artifact metadata is display-only in this build.',
    'note component-artifact-download',
  ));
}

function renderViewData(doc, cockpit, section, view, key) {
  switch (view.kind) {
    case 'Status': renderStatus(doc, section, view); break;
    case 'Metrics': renderMetrics(doc, section, view); break;
    case 'Table': renderTable(doc, cockpit, section, view, key); break;
    case 'Timeline': renderTimeline(doc, cockpit, section, view, key); break;
    case 'Alert': renderAlert(doc, section, view); break;
    case 'ArtifactLink': renderArtifact(doc, section, view); break;
    default: break;
  }
}

function renderViewTechnical(doc, cockpit, section, view, key) {
  if (!view.entry) return;
  const details = disclosure(doc, `${key}:technical`, 'Technical report details');
  cockpit.bindDisclosure(details, `${key}:technical`);
  details.appendChild(kv(doc, 'Namespace', view.schema_namespace));
  details.appendChild(kv(doc, 'Schema', String(view.schema_version)));
  if (view.identity?.sequence !== null) details.appendChild(kv(doc, 'Sequence', view.identity.sequence));
  if (view.identity?.payload_digest) details.appendChild(kv(doc, 'Payload digest', view.identity.payload_digest));
  if (view.identity?.observed_at_unix_ms !== null) {
    details.appendChild(kv(doc, 'Observed', String(view.identity.observed_at_unix_ms)));
  }
  if (view.identity?.execution_id) details.appendChild(kv(doc, 'Execution', view.identity.execution_id));
  const payload = boundedText(JSON.stringify(view.entry.payload ?? null));
  details.appendChild(textNode(doc, 'pre', payload.text));
  if (payload.truncated) details.appendChild(textNode(doc, 'p', '…[truncated]', 'empty'));
  section.appendChild(details);
}

function renderView(doc, cockpit, page, view, stale, lastSuccessAt, subject) {
  const section = el(doc, 'section', 'component-view');
  section.dataset.viewId = view.view_id;
  section.dataset.kind = view.kind;
  section.dataset.state = view.state;
  section.appendChild(el(doc, 'h3', null, view.title));

  if (view.state === 'rendered' || view.state === 'stale') {
    if (stale || view.state === 'stale') {
      const notice = textNode(
        doc,
        'p',
        `Showing stale data from ${formatTimestamp(lastSuccessAt)} because the last refresh failed.`,
        'callout--stale',
      );
      notice.dataset.staleSource = view.schema_namespace;
      section.appendChild(notice);
    }
    renderViewData(doc, cockpit, section, view, `${subject}:${view.view_id}`);
  } else if (view.state === 'empty') {
    section.appendChild(emptyState(doc, 'No data in this report.'));
  } else if (view.state === 'awaiting') {
    section.appendChild(emptyState(doc, 'Awaiting data for this view on this page.'));
  } else if (view.state === 'missing') {
    section.appendChild(emptyState(doc, 'Report data is missing for this view.'));
  } else if (view.state === 'unknown') {
    section.appendChild(emptyState(doc, 'Report data is unknown for this view.'));
  } else if (view.state === 'schema-mismatch') {
    section.appendChild(emptyState(doc, view.note ?? 'Unsupported report schema version.'));
  } else if (view.state === 'conflict') {
    section.appendChild(emptyState(doc, view.note ?? 'Conflicting report identity or binding.'));
  } else if (view.state === 'malformed') {
    section.appendChild(emptyState(doc, view.note ?? 'The report payload has an unsupported structure.'));
  } else {
    section.appendChild(emptyState(doc, view.note ?? 'This view is unsupported.'));
  }

  if (view.state === 'rendered' || view.state === 'stale') {
    renderViewTechnical(doc, cockpit, section, view, `${subject}:${view.view_id}`);
  }
  page.appendChild(section);
}

/** Render the Component detail page. */
export function render(cockpit) {
  const doc = cockpit.doc;
  const snapshot = cockpit.snapshot();
  const outer = el(doc, 'div', 'page');
  outer.dataset.page = 'component';
  const page = panel(doc, 'Component detail', 'component-detail');
  outer.appendChild(page);

  const back = el(doc, 'a', null, '← Components');
  back.setAttribute('href', '#/components');
  back.dataset.focusKey = 'component:back';
  page.appendChild(back);

  const route = cockpit.state.route;
  if (route.name !== 'component') {
    page.appendChild(emptyState(doc, 'No component selected.'));
    return outer;
  }
  const pluginId = route.params.id;
  page.appendChild(kv(doc, 'Plugin', pluginId));

  const source = snapshot.sources.detail ?? {};
  const now = snapshot.renderNow ?? cockpit.renderNow ?? Date.now();
  // The page reads the route-pinned page retained by the loader, never the
  // refresher's generic last-success value; a late response for another target
  // is therefore unable to become this page's data.
  const data = cockpit.state.detail?.page ?? null;
  if (source.error && !data) {
    // Cold detail failure: keep the route identity and fall back only to an
    // exact plugin-ID match on the already loaded Components page. Never search
    // other pages, borrow another component, or invent a generation.
    const fallback = loadedListEntry(cockpit, pluginId);
    if (fallback) {
      renderStatusSummary(doc, page, componentStatusModel(fallback.status), {
        now,
        context: 'list-fallback',
        detailError: source.error,
        listSourceAt: snapshot.sources.components?.lastSuccessAt ?? null,
        pluginId,
      });
    } else {
      renderStatusSummary(doc, page, componentStatusModel(null), {
        now,
        context: 'unknown',
        detailError: source.error,
        pluginId,
      });
    }
    return outer;
  }
  if (!data) {
    page.appendChild(emptyState(doc, 'Component detail unknown.'));
    return outer;
  }
  if (source.error) {
    page.appendChild(staleNotice(doc, source.lastSuccessAt));
  }

  // Registration is a registry fact only. Report-source availability does not
  // decide whether the component exists: a builtin with no reports still has a
  // real binding and descriptors, and an unreadable/unknown registry is not the
  // same as an absent component. Only an explicit registry `missing` state is
  // rendered as not registered.
  const sources = data.metadata?.sources ?? [];
  const registry = sources.find((entry) => entry.source === 'registry');
  if (registry && registry.state === 'missing') {
    page.appendChild(emptyState(doc, 'This component is not registered in the hub registry.'));
    return outer;
  }
  const reports = sources.find((entry) => entry.source === 'reports') ?? { state: 'unknown' };
  const subject = subjectKey(pluginId, data.binding);

  const hero = el(doc, 'div', 'component-hero');
  const states = el(doc, 'div', 'component-source-states');
  states.appendChild(sourceState(doc, 'Registry', registry ?? { state: 'unknown' }));
  states.appendChild(sourceState(doc, 'Reports', reports));
  hero.appendChild(states);
  hero.appendChild(textNode(
    doc,
    'p',
    `Declared observations for ${pluginId}. Binding generation ${exactU64(data.binding?.generation) ?? 'unknown'}.`,
    'component-scope',
  ));
  page.appendChild(hero);

  renderStatusSummary(doc, page, componentStatusModel(data.status), {
    now,
    context: source.error ? 'retained-detail' : 'detail',
    detailError: source.error ?? null,
    pluginId,
  });

  const model = resolveComponentViews({
    pluginId,
    binding: data.binding,
    descriptors: data.descriptors ?? [],
    entries: data.entries ?? [],
    nextCursor: data.next_cursor ?? null,
    reportState: reports.state,
    stale: source.stale === true,
  });

  const views = el(doc, 'section', 'component-views');
  if (model.views.length === 0) {
    const empty = emptyState(doc, 'No declared views.');
    empty.dataset.emptyViews = 'true';
    views.appendChild(empty);
  } else {
    for (const view of model.views) {
      renderView(doc, cockpit, views, view, source.stale === true, source.lastSuccessAt, subject);
    }
  }
  page.appendChild(views);

  page.appendChild(pager(doc, data.next_cursor, 'next-detail', () => cockpit.nextDetailPage()));

  const technical = disclosure(doc, `${subject}:binding`, 'Details: binding and schema metadata');
  cockpit.bindDisclosure(technical, `${subject}:binding`);
  renderBinding(doc, technical, data.binding);
  renderDescriptors(doc, technical, data.descriptors ?? []);
  renderEnvelopes(doc, technical, data.entries ?? []);
  page.appendChild(technical);

  return outer;
}
