/** Seeded randomness shared by the conformance drivers. */

/**
 * mulberry32 — 32-bit state, one multiply-xorshift round, uniform enough for
 * schedule selection and short enough to inline. Written out here rather than
 * taken from a package because this workspace adds no dependency for eleven
 * lines, and a PRNG a test depends on for reproducibility should be a thing you
 * can read.
 */
export function mulberry32(seed: number): () => number {
  let state = seed >>> 0;
  return () => {
    state = (state + 0x6d2b79f5) >>> 0;
    let t = state;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

/** The draws a schedule needs, over one {@link mulberry32} stream. */
export interface Rng {
  /** Uniform in [0, 1). */
  next(): number;
  /** Uniform integer in [0, maxExclusive). */
  int(maxExclusive: number): number;
  /** True with probability `p`. */
  chance(p: number): boolean;
  /** One of `items`; throws on an empty list rather than returning undefined. */
  pick<T>(items: readonly T[]): T;
}

export function createRng(seed: number): Rng {
  const next = mulberry32(seed);
  return {
    next,
    int: (maxExclusive: number) => Math.floor(next() * maxExclusive),
    chance: (p: number) => next() < p,
    pick: <T>(items: readonly T[]): T => {
      if (items.length === 0) throw new Error("Rng.pick on an empty list");
      return items[Math.floor(next() * items.length)]!;
    },
  };
}
