// WEB.6 shared browser DOM builders for the multi-page cockpit shell.
//
// Every builder takes the `document` so the renderers stay testable without a
// live window. Plugin-derived text is written with `textContent`/
// `createTextNode` only, bounded to [`FIELD_TEXT_LIMIT`] with an explicit
// truncation marker; tables bound their rows to [`LIST_ROW_LIMIT`] with an
// explicit additional-content marker. No builder emits an inline style
// attribute: all presentation lives in `dashboard.css` and the class list the
// design freezes.

export const FIELD_TEXT_LIMIT = 4096;
export const LIST_ROW_LIMIT = 100;
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

/** Create one element with an optional class and exact text. */
export function el(doc, tag, className, text) {
  const node = doc.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

/** A bounded text node: 4 KiB of exact text plus a truncation marker. */
export function textNode(doc, tag, value, className) {
  const { text, truncated } = boundedText(value);
  const node = el(doc, tag, className);
  node.appendChild(doc.createTextNode(text));
  if (truncated) node.appendChild(doc.createTextNode(' …[truncated]'));
  return node;
}

const PILL_STATES = new Set([
  'ready', 'degraded', 'unavailable', 'unknown', 'stale',
  'running', 'idle', 'paused', 'failed',
]);

/** One status/health pill. An unrecognized state renders as `unknown`, never
 * as a healthy value. */
export function pill(doc, state) {
  const raw = state === null || state === undefined ? 'unknown' : String(state);
  const known = PILL_STATES.has(raw.toLowerCase());
  const node = el(doc, 'span', `pill pill--${known ? raw.toLowerCase() : 'unknown'}`);
  const { text, truncated } = boundedText(raw);
  node.appendChild(doc.createTextNode(text));
  if (truncated) node.appendChild(doc.createTextNode(' …[truncated]'));
  return node;
}

/**
 * A titled panel. `id` (optional) is the retained DOM anchor used by the
 * cockpit acceptance driver.
 */
export function panel(doc, title, id) {
  const root = el(doc, 'section', 'panel');
  if (id) root.id = id;
  root.appendChild(el(doc, 'h2', null, title));
  return root;
}

/** One key/value row: a `.key` label then a bounded value. */
export function kv(doc, label, value, className = 'value') {
  const row = el(doc, 'div', 'kv');
  row.appendChild(textNode(doc, 'span', label, 'key'));
  row.appendChild(doc.createTextNode(': '));
  row.appendChild(textNode(doc, 'span', value, className));
  return row;
}

/**
 * A dense table. `rows` is an array of cell arrays; a cell is exact text or an
 * already-built node (for example a `pill`). Rows beyond [`LIST_ROW_LIMIT`] are
 * dropped and reported with an explicit footer marker.
 */
export function table(doc, headings, rows) {
  const root = el(doc, 'table', 'table');
  const head = el(doc, 'thead');
  const headRow = el(doc, 'tr');
  for (const heading of headings) headRow.appendChild(el(doc, 'th', null, heading));
  head.appendChild(headRow);
  root.appendChild(head);
  const body = el(doc, 'tbody');
  const bounded = rows.slice(0, LIST_ROW_LIMIT);
  for (const row of bounded) {
    const tr = el(doc, 'tr');
    for (const cell of row) {
      const td = el(doc, 'td');
      if (cell !== null && typeof cell === 'object' && typeof cell.nodeType === 'number') {
        td.appendChild(cell);
      } else {
        td.appendChild(textNode(doc, 'span', cell));
      }
      tr.appendChild(td);
    }
    body.appendChild(tr);
  }
  root.appendChild(body);
  if (rows.length > LIST_ROW_LIMIT) {
    const foot = el(doc, 'tfoot');
    const marker = el(doc, 'tr');
    const cell = el(doc, 'td');
    cell.colSpan = headings.length;
    cell.appendChild(doc.createTextNode(`…and ${rows.length - LIST_ROW_LIMIT} more rows.`));
    marker.appendChild(cell);
    foot.appendChild(marker);
    root.appendChild(foot);
  }
  return root;
}

/** A timeline stream of observations (`{at_unix_ms,kind,title,detail}`). */
export function timeline(doc, entries) {
  const root = el(doc, 'ul', 'timeline');
  const bounded = entries.slice(0, LIST_ROW_LIMIT);
  for (const entry of bounded) {
    const item = el(doc, 'li');
    item.appendChild(textNode(doc, 'span', `${formatTimestamp(entry?.at_unix_ms)} ${entry?.kind ?? 'unknown'}: `, 'time'));
    item.appendChild(textNode(doc, 'span', entry?.title));
    item.appendChild(doc.createTextNode(' — '));
    item.appendChild(textNode(doc, 'span', entry?.detail));
    root.appendChild(item);
  }
  if (entries.length > LIST_ROW_LIMIT) {
    root.appendChild(textNode(doc, 'li', `…and ${entries.length - LIST_ROW_LIMIT} more entries.`, 'empty'));
  }
  return root;
}

/**
 * A `Next` pager button. `id` is the retained anchor; `nextCursor` is the
 * opaque server cursor, carried unchanged in a data attribute and disabled
 * explicitly on the final page.
 */
export function pager(doc, nextCursor, id, onClick) {
  const button = el(doc, 'button', 'pager', 'Next');
  if (id) button.id = id;
  button.type = 'button';
  button.disabled = nextCursor === null || nextCursor === undefined;
  button.dataset.nextCursor = nextCursor ?? '';
  if (typeof onClick === 'function') button.addEventListener('click', onClick);
  return button;
}

/** An explicit empty/absent state, never a blank region. */
export function emptyState(doc, text) {
  return textNode(doc, 'p', text, 'empty');
}

/** A stale-data callout naming the last successful observation. */
export function staleNotice(doc, lastSuccessAt) {
  return textNode(doc, 'p', `Showing stale data from ${formatTimestamp(lastSuccessAt)} because the last refresh failed.`, 'callout--stale');
}
