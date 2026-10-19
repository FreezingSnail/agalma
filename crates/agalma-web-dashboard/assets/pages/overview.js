// WEB.6 Overview page: runtime label, stop latch, current execution, the five
// runtime source states, the (unchanged) control panel, economics and timeline.
//
// Reads: overview + factory. Drop-in equivalent of the pre-navigation cockpit
// body, split across the page modules. Unknown is never rendered as healthy,
// zero, empty or stopped.

import { RESUME_CONFIRMATION } from '../controls.js';
import { ApiErrorKind } from '../api.js';
import {
  panel, kv, emptyState, textNode, el, formatTimestamp,
} from '../ui.js';

export const title = 'Overview';

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

function runtimeLabel(factory) {
  if (factory.error) {
    return factory.error.kind === ApiErrorKind.UNSUPPORTED
      ? 'Unavailable (endpoint absent)'
      : 'Unavailable';
  }
  if (!factory.data) return 'Unknown';
  return factory.data.status;
}

function renderRuntime(doc, snapshot) {
  const root = panel(doc, 'Factory / worker status', 'runtime');
  const factory = snapshot.sources.factory ?? { data: null, error: null };
  root.appendChild(kv(doc, 'Runtime', runtimeLabel(factory)));
  if (factory.data) {
    const stop = factory.data.stop;
    root.appendChild(kv(doc, 'Stop', stop ? (stop.stopped ? 'stopped' : 'not stopped') : 'unknown'));
    const current = factory.data.current_execution;
    root.appendChild(kv(
      doc,
      'Current execution',
      current ? `${current.execution_id ?? 'unknown'} (${current.state ?? 'unknown'})` : (current === null ? 'none' : 'unknown'),
    ));
    for (const source of factory.data.sources ?? []) {
      root.appendChild(kv(doc, `source:${source.source}`, sourceLabel(source.state)));
    }
  } else {
    root.appendChild(kv(doc, 'Stop', 'unknown'));
    root.appendChild(kv(doc, 'Current execution', 'unknown'));
    root.appendChild(kv(doc, 'Note', 'factory runtime endpoint is not available in this slice'));
  }
  return root;
}

function renderControls(doc, cockpit) {
  const root = panel(doc, 'Controls', 'controls');
  const state = cockpit.controls.state();
  const note = state.known
    ? (state.readOnly ? 'read-only: no control permission advertised' : 'typed permissions advertised')
    : 'controls disabled: runtime unknown or offline';
  root.appendChild(kv(doc, 'Permission', state.readOnly ? 'read-only' : (state.known ? 'available' : 'disabled')));
  root.appendChild(doc.createTextNode(note));
  const buttons = el(doc, 'div', 'controls');
  const pauseButton = el(doc, 'button', null, 'Pause');
  pauseButton.type = 'button';
  pauseButton.disabled = !state.enabled.pause;
  pauseButton.dataset.action = 'pause';
  if (state.enabled.pause) pauseButton.addEventListener('click', () => cockpit.controls.pause());
  const resumeButton = el(doc, 'button', null, 'Resume');
  resumeButton.type = 'button';
  resumeButton.disabled = !state.enabled.resume;
  resumeButton.dataset.action = 'resume';
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

function renderEconomics(doc, snapshot) {
  const root = panel(doc, 'Economics', 'economics');
  const economics = snapshot.sources.overview?.data?.economics;
  if (!economics) {
    root.appendChild(emptyState(doc, 'Economics unknown.'));
    return root;
  }
  root.appendChild(kv(doc, 'Completeness', economics.completeness ?? 'unknown'));
  root.appendChild(kv(doc, 'Usage', economics.usage === null || economics.usage === undefined ? 'unknown' : `${economics.usage} ${economics.usage_unit}`));
  root.appendChild(kv(doc, 'Cost', economics.cost === null || economics.cost === undefined ? 'unknown' : `${economics.cost} ${economics.cost_unit}`));
  root.appendChild(kv(doc, 'Allowance', economics.allowance === null || economics.allowance === undefined ? 'unknown' : `${economics.allowance} ${economics.cost_unit}`));
  return root;
}

function renderTimeline(doc, snapshot) {
  const root = panel(doc, 'Timeline', 'timeline');
  const timeline = snapshot.sources.overview?.data?.timeline;
  if (!timeline) {
    root.appendChild(emptyState(doc, 'Timeline unknown.'));
    return root;
  }
  if (timeline.length === 0) {
    root.appendChild(emptyState(doc, 'No timeline entries.'));
    return root;
  }
  for (const entry of timeline.slice(0, 100)) {
    const item = el(doc, 'div', 'time');
    item.appendChild(textNode(doc, 'span', `${formatTimestamp(entry.at_unix_ms)} ${entry.kind}: `, 'time'));
    item.appendChild(textNode(doc, 'span', entry.title));
    item.appendChild(doc.createTextNode(' — '));
    item.appendChild(textNode(doc, 'span', entry.detail));
    root.appendChild(item);
  }
  if (timeline.length > 100) {
    root.appendChild(emptyState(doc, `…and ${timeline.length - 100} more entries.`));
  }
  return root;
}

/** Render the Overview page. */
export function render(cockpit) {
  const doc = cockpit.doc;
  const snapshot = cockpit.snapshot();
  const page = el(doc, 'div', 'page');
  page.dataset.page = 'overview';
  page.appendChild(renderRuntime(doc, snapshot));
  page.appendChild(renderControls(doc, cockpit));
  page.appendChild(renderEconomics(doc, snapshot));
  page.appendChild(renderTimeline(doc, snapshot));
  return page;
}
