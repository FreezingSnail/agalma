// WEB.6 System page: dashboard self-health and the read-source states.
//
// Reads: `/api/v1/status` (route-active) plus the overview metadata sources and
// the local refresh states. While the status route is absent (wave-1 interlock)
// the loader's typed error is rendered explicitly; private launch inputs never
// appear because the status payload never carries them.

import {
  panel, kv, emptyState, pill, textNode, el, formatTimestamp,
} from '../ui.js';

export const title = 'System';

function renderStatus(doc, snapshot) {
  const root = panel(doc, 'Dashboard health', 'status');
  const source = snapshot.sources.status ?? {};
  const data = source.data;
  if (source.error) {
    if (!data) {
      root.appendChild(textNode(doc, 'p', `status read unavailable: ${source.error.kind}: ${source.error.message}`, 'callout--reload'));
      return root;
    }
  }
  if (!data) {
    root.appendChild(emptyState(doc, 'Dashboard status unknown.'));
    return root;
  }
  root.appendChild(kv(doc, 'Version', data.version ?? 'unknown'));
  const health = kv(doc, 'Health', data.health_state ?? 'unknown');
  health.appendChild(doc.createTextNode(' '));
  health.appendChild(pill(doc, data.health_state ?? 'unknown'));
  root.appendChild(health);
  root.appendChild(kv(doc, 'Listener', data.listener?.state ?? 'unknown'));
  root.appendChild(kv(doc, 'Listener URL', data.listener?.url ?? 'unknown'));
  root.appendChild(kv(doc, 'Binding', data.binding?.state ?? 'unknown'));
  root.appendChild(kv(doc, 'Detail', data.detail ?? 'unknown'));
  root.appendChild(kv(doc, 'Observed', formatTimestamp(data.observed_at_unix_ms)));
  if (source.error && data) {
    root.appendChild(textNode(doc, 'p', `Last status refresh failed: ${source.error.kind}: ${source.error.message}`, 'callout--stale'));
  }
  return root;
}

function renderSources(doc, cockpit, snapshot) {
  const root = panel(doc, 'Read sources', 'sources');
  const overviewSources = snapshot.sources.overview?.data?.metadata?.sources ?? null;
  if (overviewSources) {
    for (const source of overviewSources) {
      root.appendChild(kv(doc, source.source, source.state ?? 'unknown'));
    }
  }
  for (const [name, source] of Object.entries(snapshot.sources)) {
    const status = source.error
      ? `${source.error.kind}${source.data ? ' (stale)' : ''}`
      : source.status;
    const row = kv(doc, `refresh:${name}`, status);
    if (source.lastSuccessAt !== null) {
      row.appendChild(doc.createTextNode(` last success ${formatTimestamp(source.lastSuccessAt)}`));
    }
    root.appendChild(row);
  }
  root.appendChild(kv(doc, 'Interval', `${snapshot.intervalMs ?? cockpit.intervalMs} ms`));
  return root;
}

/** Render the System page. */
export function render(cockpit) {
  const doc = cockpit.doc;
  const snapshot = cockpit.snapshot();
  const outer = el(doc, 'div', 'page');
  outer.dataset.page = 'system';
  outer.appendChild(renderStatus(doc, snapshot));
  outer.appendChild(renderSources(doc, cockpit, snapshot));
  return outer;
}
