import type { SessionService } from "../generated/index.js";

export interface NativeCallOptions {
  signal?: AbortSignal;
}
export interface NativeModelInput {
  model: string;
  providerId?: string;
  reasoningEffort?: "minimal" | "low" | "medium" | "high" | null;
}
export interface NativeContextOptions {
  policy?: "final_answers" | "transcript";
  windowTokens?: number;
  reserveTokens?: number;
  keepRecentTokens?: number;
  toolResultElision?: { keepRecentTurns?: number; maxChars?: number } | false;
}
export interface NativeConfiguration {
  skills?: { id: string; revision: string }[];
  context?: NativeContextOptions;
  workspaceTrusted?: boolean;
}
export interface StandingApprovalGrant {
  grantId: string;
  digest: string;
  label: string;
  toolName: string;
  sourceRequestId: string;
  answerId: string;
  grantedAt: string;
  revokedAt?: string;
}

interface ManagementBinding {
  setExecutionMode(id: string, mode: string): Promise<string>;
  listApprovalGrants(id: string): Promise<string>;
  revokeApprovalGrant(id: string, grant: string): Promise<string>;
  compact(id: string): Promise<void>;
  fork(id: string, message: string): Promise<string>;
  setModel(id: string, input: string): Promise<string>;
  cancelQueuedInput(id: string, input: string): Promise<boolean>;
  clearQueue(id: string): Promise<number>;
  configure(id: string, input: string): Promise<string>;
}

/** Aborting stops waiting, never cancels an admitted durable operation. */
export function waitFor<T>(
  run: () => Promise<T>,
  options?: NativeCallOptions,
): Promise<T> {
  const signal = options?.signal;
  if (signal?.aborted) return Promise.reject(signal.reason);
  if (signal === undefined) return run();
  return new Promise<T>((resolve, reject) => {
    const abort = () => {
      cleanup();
      reject(signal.reason);
    };
    const cleanup = () => signal.removeEventListener("abort", abort);
    signal.addEventListener("abort", abort, { once: true });
    try {
      run().then(
        (value) => {
          cleanup();
          resolve(value);
        },
        (error: unknown) => {
          cleanup();
          reject(error);
        },
      );
    } catch (error) {
      cleanup();
      reject(error);
    }
  });
}

export class NativeSessionManagement {
  protected readonly raw: SessionService & ManagementBinding;
  private readonly lift: <T>(run: () => Promise<T>) => Promise<T>;
  constructor(
    raw: SessionService,
    lift: <T>(run: () => Promise<T>) => Promise<T>,
  ) {
    this.raw = raw as SessionService & ManagementBinding;
    this.lift = lift;
  }
  private async json<T>(
    run: () => Promise<string>,
    options?: NativeCallOptions,
  ): Promise<T> {
    return JSON.parse(await waitFor(() => this.lift(run), options)) as T;
  }
  setExecutionMode(
    id: string,
    mode: string,
    options?: NativeCallOptions,
  ): Promise<Record<string, unknown>> {
    return this.json(() => this.raw.setExecutionMode(id, mode), options);
  }
  listApprovalGrants(
    id: string,
    options?: NativeCallOptions,
  ): Promise<StandingApprovalGrant[]> {
    return this.json(() => this.raw.listApprovalGrants(id), options);
  }
  revokeApprovalGrant(
    id: string,
    grant: string,
    options?: NativeCallOptions,
  ): Promise<StandingApprovalGrant> {
    return this.json(() => this.raw.revokeApprovalGrant(id, grant), options);
  }
  compact(id: string, options?: NativeCallOptions): Promise<void> {
    return waitFor(() => this.lift(() => this.raw.compact(id)), options);
  }
  fork(
    id: string,
    message: string,
    options?: NativeCallOptions,
  ): Promise<Record<string, unknown>> {
    return this.json(() => this.raw.fork(id, message), options);
  }
  setModel(
    id: string,
    input: NativeModelInput,
    options?: NativeCallOptions,
  ): Promise<Record<string, unknown>> {
    return this.json(
      () => this.raw.setModel(id, JSON.stringify(input)),
      options,
    );
  }
  cancelQueuedInput(
    id: string,
    input: string,
    options?: NativeCallOptions,
  ): Promise<boolean> {
    return waitFor(
      () => this.lift(() => this.raw.cancelQueuedInput(id, input)),
      options,
    );
  }
  clearQueue(id: string, options?: NativeCallOptions): Promise<number> {
    return waitFor(() => this.lift(() => this.raw.clearQueue(id)), options);
  }
  configure(
    id: string,
    patch: NativeConfiguration,
    options?: NativeCallOptions,
  ): Promise<Record<string, unknown>> {
    return this.json(
      () => this.raw.configure(id, JSON.stringify(patch)),
      options,
    );
  }
}
