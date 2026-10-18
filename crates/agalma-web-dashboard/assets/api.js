// WEB.6 browser API client for the local cockpit.
//
// The single page talks to the same-origin read adapter only:
//
//   GET  /api/v1/overview
//   GET  /api/v1/components?limit&cursor
//   GET  /api/v1/factory
//   GET  /api/v1/factory/actions/{action_id}
//   POST /api/v1/factory/pause
//   POST /api/v1/factory/resume
//
// This module validates the response family/version/source metadata before any
// value reaches the renderer. Identities, generations, revisions and money are
// exact decimal strings and are never coerced through `Number`. An unsupported
// or malformed response yields an explicit error, never a successful empty
// result. No hub grant, token or private binding is ever imported here.

export const OVERVIEW_FORMAT = 'ControlPlaneRead/v1';
export const FACTORY_RUNTIME_FORMAT = 'FactoryRuntime/v1';
export const FACTORY_CONTROL_FORMAT = 'FactoryControl/v1';

export const READ_PAGE_MAX_ENTRIES = 100;
export const FACTORY_PAGE_MAX_ENTRIES = 100;
export const ACTION_ID_MAX = 128;
export const TEXT_MAX_BYTES = 4096;

/** A typed API failure. `kind` is one of the values below. */
export class ApiError extends Error {
  constructor(kind, message, detail) {
    super(message);
    this.name = 'ApiError';
    this.kind = kind;
    this.detail = detail ?? null;
  }
}

export const ApiErrorKind = Object.freeze({
  UNSUPPORTED: 'unsupported', // wrong family/version or absent optional endpoint
  MALFORMED: 'malformed', // shape/bounds violation
  UNAVAILABLE: 'unavailable', // 503 / binding not ready
  NOT_FOUND: 'not_found', // 404 from lookup
  CONFLICT: 'conflict', // 409 cursor/revision conflict
  HTTP: 'http', // other non-2xx
  TIMEOUT: 'timeout', // request deadline exceeded
  ABORTED: 'aborted', // caller aborted (page exit / newer cycle)
});

const EXACT_U64 = /^(0|[1-9][0-9]{0,19})$/;
const AMOUNT = /^-?(0|[1-9][0-9]*)(\.[0-9]+)?$/;
const ACTION_ID = /^[A-Za-z0-9._-]{1,128}$/;

/** Whether `value` is an exact unsigned 64-bit decimal string. */
export function isExactU64(value) {
  return typeof value === 'string' && EXACT_U64.test(value) && BigInt(value) <= 18446744073709551615n;
}

/** Whether `value` is an exact decimal amount string. */
export function isAmount(value) {
  return typeof value === 'string' && AMOUNT.test(value);
}

function fail(message, detail) {
  throw new ApiError(ApiErrorKind.MALFORMED, message, detail);
}

function must(condition, message, detail) {
  if (!condition) fail(message, detail);
}

function isObject(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function sourceList(metadata) {
  must(isObject(metadata), 'response metadata is not an object');
  must(metadata.format === OVERVIEW_FORMAT, `unsupported read family ${metadata.format}`);
  must(Array.isArray(metadata.sources), 'metadata.sources is not an array');
  for (const source of metadata.sources) {
    must(isObject(source) && typeof source.source === 'string', 'malformed source record');
    must(typeof source.state === 'string', 'malformed source state');
    must(source.detail === null || typeof source.detail === 'string', 'malformed source detail');
    must(
      source.observed_at_unix_ms === null || Number.isInteger(source.observed_at_unix_ms),
      'malformed source timestamp',
    );
  }
  return metadata.sources;
}

/** Validate and normalize an overview response; throws on malformed input. */
export function validateOverview(value) {
  must(isObject(value), 'overview is not an object');
  const sources = sourceList(value.metadata);
  must(isObject(value.factory), 'overview.factory is not an object');
  must(typeof value.factory.stopped === 'boolean', 'factory.stopped is not a boolean');
  must(Array.isArray(value.executions), 'overview.executions is not an array');
  must(value.executions.length <= READ_PAGE_MAX_ENTRIES, 'overview executions exceed 100');
  for (const execution of value.executions) {
    must(isObject(execution), 'execution is not an object');
    must(typeof execution.execution_id === 'string', 'execution_id is not a string');
    must(typeof execution.task_id === 'string', 'task_id is not a string');
    must(isExactU64(execution.generation), 'execution.generation is not an exact u64 string');
    must(typeof execution.phase === 'string' && typeof execution.state === 'string', 'malformed execution state');
    must(
      Number.isInteger(execution.updated_at_unix_ms),
      'execution.updated_at_unix_ms is not an integer',
    );
  }
  must(Array.isArray(value.timeline), 'overview.timeline is not an array');
  must(value.timeline.length <= READ_PAGE_MAX_ENTRIES, 'overview timeline exceeds 100');
  const economics = value.economics;
  must(isObject(economics), 'overview.economics is not an object');
  for (const key of ['usage', 'cost', 'allowance']) {
    const money = economics[key];
    must(money === null || isAmount(money), `economics.${key} is not null or an exact amount string`);
  }
  return {
    metadata: value.metadata,
    sources,
    factory: value.factory,
    executions: value.executions,
    economics,
    timeline: value.timeline,
  };
}

/** Validate and normalize a component page; throws on malformed input. */
export function validateComponents(value) {
  must(isObject(value), 'component page is not an object');
  const sources = sourceList(value.metadata);
  must(Array.isArray(value.entries), 'component entries is not an array');
  must(value.entries.length <= READ_PAGE_MAX_ENTRIES, 'component page exceeds 100 entries');
  must(value.next_cursor === null || typeof value.next_cursor === 'string', 'component next_cursor is malformed');
  for (const entry of value.entries) {
    must(isObject(entry), 'component entry is not an object');
    must(typeof entry.plugin_id === 'string' && typeof entry.name === 'string', 'component identity malformed');
    must(isExactU64(entry.generation), 'component.generation is not an exact u64 string');
    must(typeof entry.health === 'string', 'component.health is not a string');
    must(typeof entry.available === 'boolean', 'component.available is not a boolean');
    must(Array.isArray(entry.views), 'component.views is not an array');
    const binding = entry.binding;
    must(isObject(binding), 'component.binding is not an object');
    // The binding pin is descriptive identity metadata; the authoritative exact
    // generation is `entry.generation`, validated above as a u64 string. The pin
    // generation is accepted as a number or string and is never coerced for
    // identity or money, so it is not used as a numeric key here.
    must(
      typeof binding.generation === 'number' || isExactU64(binding.generation),
      'binding.generation is neither a number nor an exact u64 string',
    );
  }
  return { metadata: value.metadata, sources, entries: value.entries, next_cursor: value.next_cursor };
}

const FACTORY_STATUS = new Set([
  'Starting', 'Recovering', 'Idle', 'Running', 'Paused', 'Failed', 'Unavailable',
]);
const FACTORY_SOURCE_NAMES = ['runtime', 'worker', 'queue', 'execution', 'stop'];

/** Validate a FactoryRuntime/v1 snapshot; throws on malformed input. */
export function validateFactory(value) {
  must(isObject(value), 'factory runtime is not an object');
  must(value.format === FACTORY_RUNTIME_FORMAT, `unsupported factory runtime family ${value.format}`);
  must(FACTORY_STATUS.has(value.status), `unknown factory status ${value.status}`);
  must(Array.isArray(value.sources) && value.sources.length === 5, 'factory.sources must be exactly five');
  const names = new Set();
  for (const source of value.sources) {
    must(isObject(source) && FACTORY_SOURCE_NAMES.includes(source.source), 'unknown factory source name');
    must(!names.has(source.source), 'duplicate factory source name');
    names.add(source.source);
    must(typeof source.state === 'string', 'factory source state is not a string');
  }
  const permissions = value.permissions;
  must(isObject(permissions), 'factory.permissions is not an object');
  must(typeof permissions.pause === 'boolean' && typeof permissions.resume === 'boolean', 'factory permissions must be booleans');
  const eligible = value.eligible;
  must(isObject(eligible) && Array.isArray(eligible.entries), 'factory.eligible.entries is not an array');
  must(
    eligible.entries.length <= FACTORY_PAGE_MAX_ENTRIES,
    'factory eligible page exceeds 100 entries',
  );
  return value;
}

/** Validate a FactoryControl/v1 response (receipt, unconfirmed or not_found). */
export function validateControl(value) {
  must(isObject(value), 'control response is not an object');
  must(value.format === FACTORY_CONTROL_FORMAT, `unsupported control family ${value.format}`);
  must(typeof value.result === 'string', 'control result tag is not a string');
  if (value.result === 'receipt') {
    must(isObject(value.receipt), 'receipt body is missing');
    must(isExactU64(value.receipt.recorded_at_unix_ms), 'receipt timestamp is not exact');
    must(value.receipt.outcome === 'applied' || value.receipt.outcome === 'denied', 'receipt outcome is unknown');
  } else if (value.result === 'unconfirmed') {
    must(value.code === 'EffectUnconfirmed', 'unconfirmed response has the wrong code');
    must(typeof value.action_id === 'string', 'unconfirmed action_id is missing');
    must(typeof value.reason === 'string', 'unconfirmed reason is missing');
  } else if (value.result === 'not_found') {
    must(typeof value.action_id === 'string', 'not_found action_id is missing');
  } else {
    fail(`unknown control result tag ${value.result}`);
  }
  return value;
}

function decodeBodyKind(status) {
  if (status === 400) return ApiErrorKind.MALFORMED;
  if (status === 404) return ApiErrorKind.NOT_FOUND;
  if (status === 409) return ApiErrorKind.CONFLICT;
  if (status === 503) return ApiErrorKind.UNAVAILABLE;
  if (status === 504) return ApiErrorKind.TIMEOUT;
  return ApiErrorKind.HTTP;
}

/** Same-origin typed client. `fetchImpl` is injectable for deterministic tests. */
export class DashboardApi {
  constructor(options = {}) {
    this.baseUrl = options.baseUrl ?? '';
    this.fetchImpl = options.fetchImpl ?? ((...args) => globalThis.fetch(...args));
    this.requestTimeoutMs = options.requestTimeoutMs ?? 5000;
    this.clock = options.clock ?? (() => Date.now());
  }

  async #request(method, path, { body, signal } = {}) {
    const controller = new AbortController();
    const onAbort = () => controller.abort();
    if (signal) {
      if (signal.aborted) controller.abort();
      else signal.addEventListener('abort', onAbort, { once: true });
    }
    let deadlineReached = false;
    const deadline = setTimeout(() => {
      deadlineReached = true;
      controller.abort();
    }, this.requestTimeoutMs);
    let response;
    try {
      response = await this.fetchImpl(`${this.baseUrl}${path}`, {
        method,
        headers: body === undefined ? undefined : { 'content-type': 'application/json' },
        body: body === undefined ? undefined : JSON.stringify(body),
        signal: controller.signal,
        credentials: 'same-origin',
        cache: 'no-store',
      });
    } catch (error) {
      if (deadlineReached) throw new ApiError(ApiErrorKind.TIMEOUT, `request deadline exceeded for ${path}`);
      if (error?.name === 'AbortError') throw new ApiError(ApiErrorKind.ABORTED, `request aborted for ${path}`);
      throw new ApiError(ApiErrorKind.UNAVAILABLE, `request failed for ${path}`, String(error));
    } finally {
      clearTimeout(deadline);
      if (signal) signal.removeEventListener('abort', onAbort);
    }
    if (!response.ok) {
      let detail = null;
      try {
        detail = await response.json();
      } catch {
        detail = null;
      }
      throw new ApiError(decodeBodyKind(response.status), `HTTP ${response.status} for ${path}`, detail);
    }
    try {
      return await response.json();
    } catch {
      throw new ApiError(ApiErrorKind.MALFORMED, `response for ${path} is not JSON`);
    }
  }

  getOverview(options) {
    return this.#request('GET', '/api/v1/overview', options).then(validateOverview);
  }

  getComponents({ limit, cursor, signal } = {}) {
    const query = new URLSearchParams();
    if (typeof limit === 'number') query.set('limit', String(limit));
    if (typeof cursor === 'string') query.set('cursor', cursor);
    const suffix = query.toString() === '' ? '' : `?${query.toString()}`;
    return this.#request('GET', `/api/v1/components${suffix}`, { signal }).then(validateComponents);
  }

  getFactory({ signal } = {}) {
    return this.#request('GET', '/api/v1/factory', { signal }).then(
      (value) => {
        if (isObject(value) && typeof value.error === 'string') {
          // Explicit server-side unavailability for the optional endpoint.
          throw new ApiError(ApiErrorKind.UNSUPPORTED, `factory runtime endpoint unavailable: ${value.error}`);
        }
        return validateFactory(value);
      },
      (error) => {
        // The factory runtime route is optional in M1. An absent 404 is an
        // unsupported endpoint, not a malformed successful page.
        if (error instanceof ApiError && error.kind === ApiErrorKind.NOT_FOUND) {
          throw new ApiError(ApiErrorKind.UNSUPPORTED, 'factory runtime endpoint absent');
        }
        throw error;
      },
    );
  }

  lookupAction(actionId, { signal } = {}) {
    if (!ACTION_ID.test(actionId)) {
      return Promise.reject(new ApiError(ApiErrorKind.MALFORMED, 'invalid action id'));
    }
    return this.#request('GET', `/api/v1/factory/actions/${encodeURIComponent(actionId)}`, { signal }).then(
      validateControl,
    );
  }

  // `confirmation` is preserved exactly for input identity. Pause sends null.
  pause(actionId, { confirmation = null, signal } = {}) {
    return this.#control('pause', actionId, confirmation, { signal });
  }

  resume(actionId, { confirmation, signal } = {}) {
    return this.#control('resume', actionId, confirmation, { signal });
  }

  #control(operation, actionId, confirmation, { signal } = {}) {
    if (!ACTION_ID.test(actionId)) {
      return Promise.reject(new ApiError(ApiErrorKind.MALFORMED, 'invalid action id'));
    }
    const body = {
      format: FACTORY_CONTROL_FORMAT,
      operation,
      action_id: actionId,
      confirmation: confirmation === undefined ? null : confirmation,
    };
    return this.#request('POST', `/api/v1/factory/${operation}`, { body, signal }).then(validateControl);
  }
}

/** Generate one opaque action id for a single explicit click. */
export function newActionId(source) {
  const bytes = new Uint8Array(12);
  if (source !== undefined && typeof source.getRandomValues === 'function') {
    source.getRandomValues(bytes);
  } else if (typeof globalThis.crypto?.getRandomValues === 'function') {
    globalThis.crypto.getRandomValues(bytes);
  } else {
    for (let index = 0; index < bytes.length; index += 1) {
      bytes[index] = Math.floor(Math.random() * 256);
    }
  }
  let hex = '';
  for (const byte of bytes) hex += byte.toString(16).padStart(2, '0');
  return `ui-${hex}`;
}
