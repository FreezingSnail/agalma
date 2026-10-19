// WEB.6/UI.3 Components page: the hub registry table, name links and the
// opaque-cursor pager.
//
// Reads: components. Exact plugin generations (u64 decimal strings) are rendered
// verbatim and never coerced through `Number`. Component names link to the
// existing encoded detail route, including unicode, slash, percent, space and
// unknown plugin IDs. A revision conflict resets the collection once and shows
// the reload notice; a failed refresh keeps the last successful page visible as
// stale.

import {
  panel, table, pager, emptyState, staleNotice, pill, textNode, el, boundedText,
} from '../ui.js';
import { formatHash } from '../router.js';

export const title = 'Components';

function nameLink(doc, entry) {
  const pluginId = typeof entry.plugin_id === 'string' ? entry.plugin_id : '';
  const rawName = typeof entry.name === 'string' && entry.name !== '' ? entry.name : '(unnamed component)';
  const { text, truncated } = boundedText(rawName);
  const link = el(doc, 'a', 'component-link');
  link.setAttribute('href', formatHash({ name: 'component', params: { id: pluginId } }));
  link.dataset.focusKey = `component:${pluginId}`;
  link.appendChild(doc.createTextNode(text));
  if (truncated) link.appendChild(doc.createTextNode(' …[truncated]'));
  return link;
}

/** Render the Components page. */
export function render(cockpit) {
  const doc = cockpit.doc;
  const snapshot = cockpit.snapshot();
  const outer = el(doc, 'div', 'page');
  outer.dataset.page = 'components';
  const page = panel(doc, 'Components', 'components');
  outer.appendChild(page);

  const source = snapshot.sources.components ?? {};
  const data = source.data ?? cockpit.state.components.page;

  if (source.error && data) {
    page.appendChild(staleNotice(doc, source.lastSuccessAt));
  }
  if (cockpit.state.components.reloadNote) {
    page.appendChild(textNode(doc, 'p', cockpit.state.components.reloadNote, 'callout--reload'));
  }
  if (!data) {
    page.appendChild(emptyState(doc, 'Components unknown.'));
    return outer;
  }
  if (data.entries.length === 0) {
    page.appendChild(emptyState(doc, 'No components in this page.'));
  } else {
    const headings = ['Plugin', 'Name', 'Generation', 'Health', 'Freshness', 'Completeness', 'Available', 'Views'];
    const rows = data.entries.map((entry) => [
      entry.plugin_id,
      nameLink(doc, entry),
      entry.generation,
      pill(doc, entry.health),
      entry.freshness ?? 'unknown',
      entry.completeness ?? 'unknown',
      entry.available ? 'true' : 'false',
      String(entry.views.length),
    ]);
    page.appendChild(table(doc, headings, rows));
  }
  page.appendChild(pager(doc, data.next_cursor, 'next-components', () => cockpit.nextComponentPage()));
  return outer;
}
