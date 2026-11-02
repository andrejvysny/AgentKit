import { cpSync, existsSync, mkdirSync, realpathSync } from "node:fs";
import { dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import {
  hostNativeTarget,
  nativeTargetByTriple,
} from "./native-platform-targets.mjs";

/**
 * Stage a compiled addon into a copy of its platform package directory.
 * Only real, already-built addons are staged; never fabricated or
 * cross-labelled binaries. With no explicit triple, the HOST's own target
 * is staged (the fail-closed dev/gate path); release jobs pass the exact
 * triple they built so a mislabelled artifact is impossible by
 * construction.
 */
export function stageNativePlatform(repoRoot, staging, triple) {
  const target = triple ? nativeTargetByTriple(triple) : hostNativeTarget();
  const binaryName = `agentkit-native.${target.key}.node`;
  const binary = join(repoRoot, "packages", "native", "generated", binaryName);
  if (!existsSync(binary)) throw new Error(`native addon missing: ${binary}`);
  const destination = join(staging, "packages", target.package);
  if (existsSync(destination))
    throw new Error(`native platform staging already exists: ${destination}`);
  mkdirSync(join(staging, "packages"), { recursive: true });
  cpSync(join(repoRoot, "packages", target.package), destination, {
    recursive: true,
  });
  cpSync(binary, join(destination, binaryName));
  return {
    target: target.key,
    key: target.key,
    triple: target.triple,
    package: target.package,
    binaryName,
    destination,
  };
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(resolve(process.argv[1])).href
) {
  const repoRoot = realpathSync(
    resolve(dirname(fileURLToPath(import.meta.url)), "../.."),
  );
  const positional = process.argv.slice(2).filter((a) => !a.startsWith("--"));
  const tripleFlagIndex = process.argv.indexOf("--target");
  const triple =
    tripleFlagIndex >= 0 ? process.argv[tripleFlagIndex + 1] : undefined;
  if (!positional[0])
    throw new Error(
      "usage: node stage-native-platform.mjs <existing-outside-repository-directory> [--target <rust-target-triple>]",
    );
  const destination = realpathSync(resolve(positional[0]));
  const inside = relative(repoRoot, destination);
  if (
    inside === "" ||
    (inside !== ".." && !inside.startsWith(`..${sep}`) && !isAbsolute(inside))
  )
    throw new Error("native platform staging must be outside the repository");
  const staged = stageNativePlatform(repoRoot, destination, triple);
  process.stdout.write(`${staged.destination}\n`);
}
