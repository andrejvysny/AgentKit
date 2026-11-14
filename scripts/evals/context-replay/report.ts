import {
  bill,
  type CacheConfig,
  type CapturedSession,
  type Classes,
  type Rates,
  replay,
} from "./replay";

/** Frozen illustrative rates in abstract units per million tokens; no currency implied. */
export const ILLUSTRATIVE_PRICES: {
  version: string;
  unit: string;
  rates: Rates;
} = {
  version: "illustrative-v1",
  unit: "simulated units per million tokens; not provider prices",
  rates: {
    input_uncached: 2,
    cache_read: 0.2,
    cache_write_short: 2.5,
    cache_write_long: 4,
    output: 8,
    reasoning: 8,
  },
};
const classKeys: (keyof Classes)[] = [
  "tools",
  "system",
  "project",
  "memory",
  "history",
  "turn_context",
  "unattributed",
  "output",
];

export function report(sessions: CapturedSession[], config: CacheConfig) {
  return {
    version: 1,
    provenance:
      "scripted host-assembled request values; cache and bill simulated; not live pricing",
    prices: ILLUSTRATIVE_PRICES,
    cache: config,
    sessions: sessions.map((capture) => {
      const session = replay(capture, config);
      const requests = session.requests.map((request) => ({
        ...request,
        bill: scale(bill(request.usage, ILLUSTRATIVE_PRICES.rates)),
      }));
      const class_totals = Object.fromEntries(
        classKeys.map((key) => [
          key,
          requests.some((request) => request.classes[key] === null)
            ? null
            : requests.reduce(
                (sum, request) => sum + (request.classes[key] ?? 0),
                0,
              ),
        ]),
      ) as Classes;
      return {
        ...session,
        requests,
        class_totals,
        bill: scale(bill(session.totals, ILLUSTRATIVE_PRICES.rates)),
      };
    }),
  };
}
function scale(cost: ReturnType<typeof bill>) {
  return {
    ...cost,
    lower: cost.lower / 1_000_000,
    upper: cost.upper === null ? null : cost.upper / 1_000_000,
  };
}
