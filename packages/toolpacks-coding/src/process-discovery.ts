import {
  identityKey,
  readProcessTable,
  startedStrictlyBefore,
  type ProcessDiscovery,
  type ProcessIdentity,
  type ProcessTable,
} from "./process-identity.js";
import { scanMarkedIdentities, type MarkedDiscovery } from "./process-tree.js";

export interface ExecutionCensus {
  execId: string;
  rootIdentity?: ProcessIdentity;
  rootPid?: number;
  known?: readonly ProcessIdentity[];
}

export interface DiscoverySources {
  marked(): Promise<MarkedDiscovery>;
  table(): Promise<ProcessTable>;
}

/** Group membership is accepted only while an owned identity anchors that group. */
export async function discoverExecution(
  input: ExecutionCensus,
  sources: DiscoverySources = {
    marked: scanMarkedIdentities,
    table: readProcessTable,
  },
): Promise<ProcessDiscovery> {
  const [markedResult, tableResult] = await Promise.allSettled([
    sources.marked(),
    sources.table(),
  ]);
  const marked =
    markedResult.status === "fulfilled"
      ? markedResult.value
      : { processes: [], complete: false };
  const table =
    tableResult.status === "fulfilled"
      ? tableResult.value
      : { processes: [], complete: false };
  const identities = new Map<string, ProcessIdentity>(
    (input.known ?? []).map((identity) => [identityKey(identity), identity]),
  );
  for (const identity of marked.processes.filter(
    (row) => row.execId === input.execId,
  ))
    identities.set(identityKey(identity), {
      pid: identity.pid,
      startIdentity: identity.startIdentity,
    });
  if (input.rootIdentity)
    identities.set(identityKey(input.rootIdentity), input.rootIdentity);
  const pgid = input.rootIdentity?.pid ?? input.rootPid;
  const grouped =
    pgid === undefined
      ? []
      : table.processes.filter(
          (row) => row.pgid === pgid && row.state !== "zombie",
        );
  const anchored = grouped.some((row) => identities.has(identityKey(row)));
  if (anchored)
    for (const row of grouped)
      identities.set(identityKey(row), {
        pid: row.pid,
        startIdentity: row.startIdentity,
      });
  return {
    identities: [...identities.values()],
    complete:
      marked.complete &&
      table.complete &&
      (grouped.length === 0 || anchored) &&
      unreadableExcluded(
        marked.unreadable ?? [],
        identities,
        input.rootIdentity,
      ),
  };
}

/**
 * The marker is inherited through the environment, so a process that began
 * before this execution's root cannot carry it. Anything newer stays unproven.
 */
function unreadableExcluded(
  unreadable: readonly ProcessIdentity[],
  identities: ReadonlyMap<string, ProcessIdentity>,
  root: ProcessIdentity | undefined,
): boolean {
  return unreadable.every(
    (identity) =>
      identities.has(identityKey(identity)) ||
      (root !== undefined &&
        startedStrictlyBefore(identity.startIdentity, root.startIdentity)),
  );
}
