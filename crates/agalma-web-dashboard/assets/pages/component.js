// WEB.6 Component detail page (#/components/<plugin_id>).
//
// Reads: the route-active `read_component` source. The page shows the binding
// pin, the declared descriptors and the bounded, inert report envelopes with
// opaque-cursor paging. While the server route is absent (wave-1 interlock) the
// loader's typed error is rendered explicitly, never as a blank or fabricated
// page. A valid-but-unknown plugin id renders the honest not-registered state.

import {
  panel, table, pager, emptyState, staleNotice, kv, textNode, el, boundedText,
} from '../ui.js';

export const title = 'Component';

function renderBinding(doc, root, binding) {
  root.appendChild(kv(doc, 'Binding', binding.binding_id ?? 'unknown'));
  root.appendChild(kv(doc, 'API', `${binding.api ?? 'unknown'} v${binding.api_version ?? 'unknown'}`));
  root.appendChild(kv(doc, 'Implementation', binding.implementation_version ?? 'unknown'));
  root.appendChild(kv(doc, 'Generation (pin)', binding.generation ?? 'unknown'));
  root.appendChild(kv(doc, 'Config digest', binding.config_digest ?? 'unknown'));
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
  page.appendChild(back);

  const route = cockpit.state.route;
  if (route.name !== 'component') {
    page.appendChild(emptyState(doc, 'No component selected.'));
    return outer;
  }
  const pluginId = route.params.id;
  page.appendChild(kv(doc, 'Plugin', pluginId));

  const source = snapshot.sources.detail ?? {};
  const data = source.data;
  if (source.error) {
    if (data) {
      page.appendChild(staleNotice(doc, source.lastSuccessAt));
    } else {
      page.appendChild(textNode(doc, 'p', `detail read unavailable: ${source.error.kind}: ${source.error.message}`, 'callout--reload'));
      return outer;
    }
  }
  if (!data) {
    page.appendChild(emptyState(doc, 'Component detail unknown.'));
    return outer;
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

  if (data.binding) renderBinding(doc, page, data.binding);
  renderDescriptors(doc, page, data.descriptors);
  renderEnvelopes(doc, page, data.entries);
  page.appendChild(pager(doc, data.next_cursor, 'next-detail', () => cockpit.nextDetailPage()));
  return outer;
}
