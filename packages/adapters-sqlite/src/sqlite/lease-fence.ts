import { LeaseLostError, type Clock, type RunWriteFence } from "@agentkit/host";
import type { SqliteConnection } from "./connection.js";

/** The caller must hold the write transaction covering its following mutation. */
export function assertRunLease(
  conn: SqliteConnection,
  clock: Clock,
  fence: RunWriteFence,
): void {
  const lease = conn.get(
    "SELECT lease_token, fencing_token, expires_at FROM leases WHERE task_id = $taskId",
    { $taskId: fence.taskId },
  ) as {
    lease_token: string;
    fencing_token: number;
    expires_at: string;
  } | null;
  if (
    !lease ||
    lease.lease_token !== fence.leaseToken ||
    new Date(lease.expires_at).getTime() <= clock.now().getTime()
  ) {
    throw new LeaseLostError(
      `Lease token ${fence.leaseToken} is not current for task ${fence.taskId}.`,
      {
        ...fence,
        ...(lease
          ? {
              currentFencingToken: lease.fencing_token,
              expiresAt: lease.expires_at,
            }
          : {}),
      },
    );
  }
}
