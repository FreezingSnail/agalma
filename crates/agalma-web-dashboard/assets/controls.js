// WEB.6 explicit control interactions.
//
// Controls are exposed only when the typed factory runtime advertises the
// matching permission. A read-only, unknown or offline runtime yields disabled
// controls. Resume requires the existing literal confirmation phrase. One
// explicit click generates exactly one action id; duplicate clicks are disabled
// while the click is pending. If the response is lost the retained id is
// queried once against the durable receipt and the result is shown as
// `EffectUnconfirmed`; the browser never replays the mutation automatically.
// Loading, refreshing, navigation and reload never submit a control.

import { ApiError, ApiErrorKind, newActionId } from './api.js';

/** The existing literal Resume confirmation phrase. */
export const RESUME_CONFIRMATION = 'resume';

function disabled(reason) {
  return new ApiError(ApiErrorKind.MALFORMED, `control disabled: ${reason}`);
}

export class ControlPanel {
  constructor(options = {}) {
    this.api = options.api;
    this.newId = options.newId ?? (() => newActionId(options.randomSource));
    this.onChange = options.onChange ?? (() => {});
    this.runtime = null;
    this.pending = false;
    this.pendingAction = null;
    this.retainedActionId = null;
    this.lastResult = null;
  }

  /**
   * Apply the latest typed factory runtime observation. Passing `null` (or an
   * Unavailable/unknown runtime) disables both controls.
   */
  setRuntime(runtime, notify = true) {
    this.runtime = runtime ?? null;
    if (notify) this.onChange(this.state());
  }

  #permissions() {
    const runtime = this.runtime;
    if (!runtime || runtime.status === 'Unavailable') return null;
    if (!runtime.permissions) return null;
    return { pause: runtime.permissions.pause === true, resume: runtime.permissions.resume === true };
  }

  /** Whether pause/resume are currently offered, and why not when they are not. */
  state() {
    const permissions = this.#permissions();
    const known = permissions !== null;
    return {
      known,
      readOnly: known && !permissions.pause && !permissions.resume,
      permissions: permissions ?? { pause: false, resume: false },
      enabled: {
        pause: known && permissions.pause && !this.pending,
        resume: known && permissions.resume && !this.pending,
      },
      pending: this.pending,
      pendingAction: this.pendingAction,
      retainedActionId: this.retainedActionId,
      lastResult: this.lastResult,
    };
  }

  async pause() {
    return this.#submit('pause', null);
  }

  async resume(confirmation) {
    if (confirmation !== RESUME_CONFIRMATION) {
      throw disabled('resume requires the literal confirmation phrase');
    }
    return this.#submit('resume', confirmation);
  }

  async #submit(operation, confirmation) {
    const state = this.state();
    if (!state.enabled[operation]) {
      throw disabled(`${operation} is not enabled`);
    }
    // One id per explicit click, retained across a lost response.
    const actionId = this.newId();
    this.retainedActionId = actionId;
    this.pending = true;
    this.pendingAction = operation;
    this.onChange(this.state());
    try {
      const response = operation === 'pause'
        ? await this.api.pause(actionId, { confirmation })
        : await this.api.resume(actionId, { confirmation });
      this.lastResult = this.#interpret(operation, actionId, response);
      return this.lastResult;
    } catch (error) {
      const uncertain = this.#isLostResponse(error);
      if (!uncertain) {
        this.lastResult = { kind: 'error', operation, actionId, error: { kind: error.kind, message: error.message } };
        return this.lastResult;
      }
      // Lost response: query the retained id once; never replay the mutation.
      this.lastResult = await this.#reconcile(operation, actionId);
      return this.lastResult;
    } finally {
      this.pending = false;
      this.pendingAction = null;
      this.onChange(this.state());
    }
  }

  #isLostResponse(error) {
    if (!(error instanceof ApiError)) return false;
    return error.kind === ApiErrorKind.TIMEOUT
      || error.kind === ApiErrorKind.ABORTED
      || error.kind === ApiErrorKind.UNAVAILABLE
      || error.kind === ApiErrorKind.HTTP;
  }

  async #reconcile(operation, actionId) {
    try {
      const response = await this.api.lookupAction(actionId);
      return this.#interpret(operation, actionId, response);
    } catch (error) {
      if (error instanceof ApiError && error.kind === ApiErrorKind.NOT_FOUND) {
        return { kind: 'not_found', operation, actionId };
      }
      // The effect remains unknown: show EffectUnconfirmed, never replay.
      return { kind: 'unconfirmed', operation, actionId, reason: error?.message ?? 'effect unconfirmed' };
    }
  }

  #interpret(operation, actionId, response) {
    if (response.result === 'receipt') {
      return {
        kind: 'receipt',
        operation,
        actionId,
        outcome: response.receipt.outcome,
        reason: response.receipt.reason ?? null,
        observedStop: response.observed_stop ?? null,
      };
    }
    if (response.result === 'unconfirmed') {
      return { kind: 'unconfirmed', operation, actionId, reason: response.reason };
    }
    return { kind: 'not_found', operation, actionId };
  }

  /** Explicit receipt lookup for one retained action id (observational). */
  async lookup(actionId = this.retainedActionId) {
    if (!actionId) throw disabled('no retained action id');
    return this.api.lookupAction(actionId);
  }
}
