// WEB.6 refresh scheduler.
//
// One cycle runs at a time. Each cycle reads every registered source in a
// serial order and records each source's result independently, so one failing
// endpoint never erases another source's last successful data. A refresh view
// keeps the previous data visible as stale and exposes the last success time and
// the current error.
//
// Cycles are abortable under a five-second request deadline. A stopped or
// superseded cycle can never publish its result: `cycleSeq` is captured at the
// start and checked after every await, so a late aborted reply cannot overwrite
// a newer cycle. On page exit timers stop and reads abort. This module never
// submits a control.

export const DEFAULT_INTERVAL_MS = 1000;
export const MIN_INTERVAL_MS = 1000;
export const MAX_INTERVAL_MS = 60000;

function clampInterval(value) {
  if (!Number.isFinite(value)) return DEFAULT_INTERVAL_MS;
  return Math.min(MAX_INTERVAL_MS, Math.max(MIN_INTERVAL_MS, Math.trunc(value)));
}

/** Per-source refresh state: `data` is the last successful value, or null. */
export function emptySourceState() {
  return { data: null, lastSuccessAt: null, lastAttemptAt: null, error: null, status: 'idle' };
}

/**
 * Serial refresh scheduler.
 *
 * `sources` is an ordered map of name -> async loader `(signal) => value`. The
 * loader's resolved value is the validated source data.
 */
export class Refresher {
  constructor(options = {}) {
    this.api = options.api;
    this.intervalMs = clampInterval(options.intervalMs ?? DEFAULT_INTERVAL_MS);
    this.onUpdate = options.onUpdate ?? (() => {});
    this.onError = options.onError ?? (() => {});
    this.setTimer = options.setTimer ?? ((fn, ms) => globalThis.setTimeout(fn, ms));
    this.clearTimer = options.clearTimer ?? ((id) => globalThis.clearTimeout(id));
    this.now = options.now ?? (() => Date.now());
    this.sources = new Map();
    this.state = new Map();
    this.running = false;
    this.cycleSeq = 0;
    this.activeCycle = null;
    this.pendingTimer = null;
    this.controller = null;
    this.lastCycleAt = null;
    this.cycleCount = 0;
  }

  /** Register (or replace) one named source loader. */
  register(name, loader) {
    this.sources.set(name, loader);
    if (!this.state.has(name)) this.state.set(name, emptySourceState());
    return this;
  }

  /** Configure the interval, clamped to the accepted 1-60s range. */
  setIntervalMs(value) {
    this.intervalMs = clampInterval(value);
    return this;
  }

  /** Begin refreshing; starts a cycle immediately and reschedules after each. */
  start() {
    if (this.running) return this.activeCycle ?? Promise.resolve();
    this.running = true;
    return this.#runCycle();
  }

  /** Stop timers and abort the active read. Any in-flight cycle is abandoned. */
  stop() {
    this.running = false;
    this.cycleSeq += 1; // supersede any in-flight cycle
    if (this.pendingTimer !== null) {
      this.clearTimer(this.pendingTimer);
      this.pendingTimer = null;
    }
    if (this.controller) {
      this.controller.abort();
      this.controller = null;
    }
  }

  /** Whether a cycle is currently reading. */
  get busy() {
    return this.activeCycle !== null;
  }

  /**
   * Run exactly one cycle now, resolving when it completes. Concurrent callers
   * share the same cycle, so reads never overlap.
   */
  refreshOnce() {
    if (this.activeCycle) return this.activeCycle;
    return this.#runCycle();
  }

  #runCycle() {
    const seq = (this.cycleSeq += 1);
    const controller = new AbortController();
    this.controller = controller;
    this.lastCycleAt = this.now();
    const cycle = this.#performCycle(seq, controller.signal)
      .catch((error) => {
        if (this.onError) this.onError(error);
      })
      .finally(() => {
        if (this.activeCycle === cycle) this.activeCycle = null;
        if (this.controller === controller) this.controller = null;
        if (this.running && this.cycleSeq === seq) this.#schedule();
      });
    this.activeCycle = cycle;
    return cycle;
  }

  async #performCycle(seq, signal) {
    for (const [name, loader] of this.sources) {
      if (this.cycleSeq !== seq) return; // superseded: publish nothing
      const source = this.state.get(name);
      source.lastAttemptAt = this.now();
      try {
        const value = await loader(signal);
        if (this.cycleSeq !== seq) return; // a newer cycle owns the state
        source.data = value;
        source.lastSuccessAt = this.now();
        source.error = null;
        source.status = 'available';
      } catch (error) {
        if (this.cycleSeq !== seq) return;
        if (error && error.kind === 'aborted') {
          source.status = 'aborted';
          continue;
        }
        source.status = 'failed';
        source.error = { kind: error?.kind ?? 'error', message: String(error?.message ?? error) };
      }
    }
    if (this.cycleSeq !== seq) return;
    this.cycleCount += 1;
    this.onUpdate(this.snapshot());
  }

  #schedule() {
    this.pendingTimer = this.setTimer(() => {
      this.pendingTimer = null;
      if (!this.running) return;
      this.#runCycle();
    }, this.intervalMs);
  }

  /** A serializable snapshot of every source's last result and staleness. */
  snapshot() {
    const sources = {};
    for (const [name, source] of this.state) {
      sources[name] = {
        data: source.data,
        lastSuccessAt: source.lastSuccessAt,
        lastAttemptAt: source.lastAttemptAt,
        error: source.error,
        status: source.status,
        // Stale means a previous success is still displayed after a failure.
        stale: source.error !== null && source.data !== null,
      };
    }
    return {
      sources,
      intervalMs: this.intervalMs,
      lastCycleAt: this.lastCycleAt,
      cycleCount: this.cycleCount,
      busy: this.busy,
    };
  }
}
