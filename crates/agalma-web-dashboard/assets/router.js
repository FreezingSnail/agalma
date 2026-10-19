// WEB.6 hash router for the multi-page cockpit.
//
// Hash routing is required: the preview target and the Rust transport serve one
// static shell, so a deep link must not need a server rewrite (the History API
// is rejected). An unknown or empty hash resolves to `#/overview` with
// `valid: false` so the shell can show a visible notice. Navigation is a hash
// change only: it never submits a control POST.

import { render as renderOverview } from './pages/overview.js';
import { render as renderComponents } from './pages/components.js';
import { render as renderComponent } from './pages/component.js';
import { render as renderTasks } from './pages/tasks.js';
import { render as renderProjects } from './pages/projects.js';
import { render as renderSystem } from './pages/system.js';

/** Ordered route table. `render(cockpit)` returns the page element. */
export const ROUTES = {
  overview: { title: 'Overview', render: renderOverview },
  components: { title: 'Components', render: renderComponents },
  component: { title: 'Component', render: renderComponent },
  tasks: { title: 'Tasks', render: renderTasks },
  projects: { title: 'Projects', render: renderProjects },
  system: { title: 'System', render: renderSystem },
};

/** Routes offered in the navigation landmark (the detail route is reached from
 * the components table, not the nav). */
export const NAV_ROUTES = ['overview', 'components', 'tasks', 'projects', 'system'];

const SIMPLE = new Set(['overview', 'components', 'tasks', 'projects', 'system']);

/**
 * Parse a raw hash into `{name, params, valid}`.
 *
 * `#/components` is the list; `#/components/<plugin_id>` is the detail route.
 */
export function parseHash(raw) {
  const hash = raw === null || raw === undefined ? '' : String(raw);
  const body = hash.startsWith('#') ? hash.slice(1) : hash;
  const path = body.startsWith('/') ? body.slice(1) : body;
  if (path === '') return { name: 'overview', params: {}, valid: false };
  const segments = path.split('/');
  const [head, ...rest] = segments;
  if (head === 'components') {
    if (rest.length === 0) return { name: 'components', params: {}, valid: true };
    if (rest.length === 1 && rest[0] !== '') {
      try {
        const id = decodeURIComponent(rest[0]);
        if (id === '') return { name: 'overview', params: {}, valid: false };
        return { name: 'component', params: { id }, valid: true };
      } catch {
        return { name: 'overview', params: {}, valid: false };
      }
    }
    return { name: 'overview', params: {}, valid: false };
  }
  if (SIMPLE.has(head) && rest.length === 0) return { name: head, params: {}, valid: true };
  return { name: 'overview', params: {}, valid: false };
}

/** Format `{name, params}` back into a canonical hash. */
export function formatHash(route) {
  const name = route?.name ?? 'overview';
  const params = route?.params ?? {};
  if (name === 'component') {
    const id = params.id === null || params.id === undefined ? '' : String(params.id);
    if (id === '') return '#/overview';
    return `#/components/${encodeURIComponent(id)}`;
  }
  if (SIMPLE.has(name)) return `#/${name}`;
  return '#/overview';
}

/**
 * Begin dispatching hash changes. Dispatches the current route immediately and
 * returns a `stop` function that removes the listener.
 */
export function startRouter({ onRoute } = {}) {
  const view = globalThis;
  const handler = () => {
    const route = parseHash(view.location?.hash ?? '');
    if (typeof onRoute === 'function') onRoute(route);
  };
  view.addEventListener('hashchange', handler);
  handler();
  return () => view.removeEventListener('hashchange', handler);
}
