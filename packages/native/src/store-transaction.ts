export interface NativeStorePort {
  callStore<T>(operation: string, input?: unknown): Promise<T>;
  transaction<T>(fn: (view: NativeStorePort) => Promise<T>): Promise<T>;
}

interface TransactionBinding {
  callStore(operation: string, input: string): Promise<string>;
  commit(): Promise<void>;
  rollback(): Promise<void>;
}

type Lift = <T>(run: () => Promise<T>) => Promise<T>;

class TransactionView implements NativeStorePort {
  private active = true;
  constructor(
    private readonly raw: TransactionBinding,
    private readonly lift: Lift,
  ) {}

  expire(): void {
    this.active = false;
  }

  async callStore<T>(operation: string, input: unknown = {}): Promise<T> {
    this.check();
    return JSON.parse(
      await this.lift(() =>
        this.raw.callStore(operation, JSON.stringify(input)),
      ),
    ) as T;
  }

  async transaction<T>(fn: (view: NativeStorePort) => Promise<T>): Promise<T> {
    this.check();
    const nested = new TransactionView(this.raw, this.lift);
    try {
      return await fn(nested);
    } finally {
      nested.expire();
    }
  }

  private check(): void {
    if (!this.active)
      throw Object.assign(new Error("The transaction view is expired"), {
        code: "transaction_scope_expired",
      });
  }
}

export async function withNativeTransaction<T>(
  raw: TransactionBinding,
  fn: (view: NativeStorePort) => Promise<T>,
  lift: Lift,
): Promise<T> {
  const view = new TransactionView(raw, lift);
  let committing = false;
  try {
    const result = await fn(view);
    view.expire();
    committing = true;
    await lift(() => raw.commit());
    return result;
  } catch (error) {
    view.expire();
    if (!committing) await lift(() => raw.rollback());
    throw error;
  }
}
