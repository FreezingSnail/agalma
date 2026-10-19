// WEB.6 Tasks page: the eligible queue and the executions table.
//
// Reads: factory + overview. Work is consumed automatically; there is no
// per-task dispatch button, so this page issues no POST. Exact generations and
// revisions are rendered verbatim.

import {
  panel, table, pager, emptyState, textNode, el, formatTimestamp, LIST_ROW_LIMIT,
} from '../ui.js';

export const title = 'Tasks';

function renderQueue(doc, cockpit, snapshot) {
  const root = panel(doc, 'Eligible queue (automatic operation)', 'queue');
  root.appendChild(doc.createTextNode('Work is consumed automatically; there is no per-task dispatch button.'));
  const factory = snapshot.sources.factory ?? { data: null };
  if (!factory.data) {
    root.appendChild(emptyState(doc, 'Eligible work is unknown (factory runtime unavailable).'));
    return root;
  }
  const eligible = factory.data.eligible ?? { entries: [], total: 'unknown', next_cursor: null, revision: 'unknown' };
  const entries = eligible.entries.slice(0, LIST_ROW_LIMIT);
  if (entries.length === 0) {
    root.appendChild(emptyState(doc, 'No eligible work in this observation.'));
  } else {
    const rows = entries.map((entry) => [
      entry.task_id,
      entry.title,
      entry.priority,
      entry.family,
    ]);
    root.appendChild(table(doc, ['Task', 'Title', 'Priority', 'Family'], rows));
  }
  const total = eligible.total ?? 'unknown';
  root.appendChild(textNode(doc, 'p', `Showing ${entries.length} of ${total}. Revision ${eligible.revision ?? 'unknown'}.`, 'note'));
  root.appendChild(pager(doc, eligible.next_cursor, 'next-queue', () => cockpit.refresh()));
  return root;
}

function renderExecutions(doc, snapshot) {
  const root = panel(doc, 'Executions', 'executions');
  const overview = snapshot.sources.overview?.data;
  const executions = overview?.executions ?? [];
  if (executions.length === 0) {
    root.appendChild(emptyState(doc, overview ? 'No executions recorded.' : 'Executions unknown.'));
    return root;
  }
  const rows = executions.slice(0, LIST_ROW_LIMIT).map((execution) => [
    execution.execution_id,
    execution.task_id,
    execution.generation,
    execution.phase,
    execution.state,
    execution.attempt ?? 'unknown',
    execution.revision ?? 'unknown',
    formatTimestamp(execution.updated_at_unix_ms),
  ]);
  root.appendChild(table(
    doc,
    ['Execution', 'Task', 'Generation', 'Phase', 'State', 'Attempt', 'Revision', 'Updated'],
    rows,
  ));
  return root;
}

/** Render the Tasks page. */
export function render(cockpit) {
  const doc = cockpit.doc;
  const snapshot = cockpit.snapshot();
  const outer = el(doc, 'div', 'page');
  outer.dataset.page = 'tasks';
  outer.appendChild(renderQueue(doc, cockpit, snapshot));
  outer.appendChild(renderExecutions(doc, snapshot));
  return outer;
}
