// WEB.6 Projects page: opaque repository/state identity, the five factory
// runtime sources, the stop observation and the observed component count.
//
// Reads: factory + components. Identity strings are opaque and exact. An absent
// factory runtime renders explicit unknowns, never a fabricated identity.

import { panel, kv, emptyState, el } from '../ui.js';

export const title = 'Projects';

export function render(cockpit) {
  const doc = cockpit.doc;
  const snapshot = cockpit.snapshot();
  const outer = el(doc, 'div', 'page');
  outer.dataset.page = 'projects';
  const page = panel(doc, 'Project / repository', 'projects');
  outer.appendChild(page);

  const factory = snapshot.sources.factory ?? { data: null };
  if (!factory.data) {
    page.appendChild(kv(doc, 'Repository', 'unknown'));
    page.appendChild(kv(doc, 'State', 'unknown'));
    page.appendChild(kv(doc, 'Runtime instance', 'unknown'));
    page.appendChild(emptyState(doc, 'Factory runtime unavailable.'));
  } else {
    page.appendChild(kv(doc, 'Repository', factory.data.repository_id ?? 'unknown'));
    page.appendChild(kv(doc, 'State', factory.data.state_id ?? 'unknown'));
    page.appendChild(kv(doc, 'Runtime instance', factory.data.runtime_instance_id ?? 'unknown'));
    page.appendChild(kv(doc, 'Stop', factory.data.stop ? (factory.data.stop.stopped ? 'stopped' : 'not stopped') : 'unknown'));
    for (const source of factory.data.sources ?? []) {
      page.appendChild(kv(doc, `source:${source.source}`, source.state ?? 'unknown'));
    }
  }

  const components = snapshot.sources.components ?? {};
  const data = components.data ?? cockpit.state.components.page;
  if (!data) {
    page.appendChild(kv(doc, 'Components', 'unknown'));
  } else {
    const suffix = data.next_cursor === null || data.next_cursor === undefined ? '' : '+';
    page.appendChild(kv(doc, 'Components', `${data.entries.length}${suffix} observed in this page`));
  }
  return outer;
}
