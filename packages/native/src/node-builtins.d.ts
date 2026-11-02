// The published wrapper keeps ZERO static Node dependencies (the root
// export must stay importable for bundlers); Node builtins are reached
// only lazily, after the native binding has begun to load. This ambient
// shape keeps the zero-types build tsconfig honest without pulling
// @types/node into the public dependency story.
declare module "node:module" {
  export function createRequire(
    filename: string,
  ): (id: string) => unknown;
}
