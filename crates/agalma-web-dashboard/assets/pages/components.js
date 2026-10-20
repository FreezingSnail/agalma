// WEB.6/UI.3 Components page: the evidence-bearing status table, name links and
// the opaque-cursor pager.
//
// Reads: components. Every identity/generation/timestamp comes from the
// validated status projection; legacy inferred health is never used. A missing
// status object renders explicit unknown values. Generation-less built-ins do
// not display the legacy synthetic generation zero. Component names link to the
// existing encoded detail route, including unicode, slash, percent, space and
// unknown plugin IDs. A revision conflict resets the collection once and shows
// the reload notice; a failed refresh keeps the last successful page visible as
// stale.

import {
  panel, table, pager, emptyState, staleNotice, pill, textNode, el, boundedText,
  componentStatusModel, timeNode,
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

function nameCell(doc, entry) {
  const wrapper = el(doc, 'div', 'component-name');
  wrapper.appendChild(nameLink(doc, entry));
  wrapper.appendChild(textNode(
    doc,
    'span',
    typeof entry.plugin_id === 'string' ? entry.plugin_id : '',
    'note mono component-id',
  ));
  return wrapper;
}

/** Reported health, with stale/unavailable observations never shown as a
 * current unqualified healthy badge. */
function healthCell(doc, model) {
  if (model.health === null) {
    return textNode(doc, 'span', 'Unknown', 'note');
  }
  if (model.stale || model.reporting === 'unavailable' || model.incomplete) {
    const qualifier = model.stale
      ? 'stale observation'
      : model.incomplete ? 'incomplete report' : 'report unavailable';
    return textNode(doc, 'span', `${model.health} (${qualifier})`, 'value value--missing');
  }
  return pill(doc, model.health);
}

function reportingCell(doc, model) {
  if (model.incomplete) {
    return textNode(doc, 'span', `${model.reportingLabel} (incomplete)`, 'note');
  }
  if (model.reporting === 'fresh') return pill(doc, 'fresh');
  if (model.reporting === 'stale') return pill(doc, 'stale');
  if (model.reporting === 'unavailable') {
    return textNode(doc, 'span', 'Not reporting', 'note');
  }
  return textNode(doc, 'span', 'Unknown', 'note');
}

function observationCell(doc, model, now) {
  if (!model.lastObservation) return textNode(doc, 'span', 'Unknown', 'note');
  const { node } = timeNode(doc, model.lastObservation.at, now, 'mono');
  const cell = el(doc, 'span', 'component-observation');
  cell.appendChild(node);
  cell.appendChild(doc.createTextNode(` (${model.lastObservation.source})`));
  return cell;
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
    const headings = [
      'Name', 'Origin', 'Generation', 'Lifecycle', 'Reported health',
      'Reporting', 'Last observation', 'Issue',
    ];
    const now = snapshot.renderNow ?? cockpit.renderNow;
    const rows = data.entries.map((entry) => {
      const model = componentStatusModel(entry.status);
      const issue = model.issue ?? '—';
      return [
        nameCell(doc, entry),
        model.originLabel,
        model.generationLabel,
        model.lifecycleLabel,
        healthCell(doc, model),
        reportingCell(doc, model),
        observationCell(doc, model, now),
        issue,
      ];
    });
    page.appendChild(table(doc, headings, rows));
  }
  page.appendChild(pager(doc, data.next_cursor, 'next-components', () => cockpit.nextComponentPage()));
  return outer;
}
