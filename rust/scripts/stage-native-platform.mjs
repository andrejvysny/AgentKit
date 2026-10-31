import { cpSync, existsSync, mkdirSync, realpathSync } from "node:fs";
import { dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

/** Stage only the host's real addon; never fabricate or cross-label binaries. */
export function stageNativePlatform(repoRoot, staging) {
  const key = `${process.platform}-${process.arch}`;
  let target;
  if (key === "darwin-arm64") target = "darwin-arm64";
  else if (key === "win32-x64") target = "win32-x64-msvc";
  else if (
    key === "linux-x64" &&
    process.report?.getReport().header.glibcVersionRuntime
  )
    target = "linux-x64-gnu";
  else throw new Error(`unsupported native staging host: ${key}`);
  const binaryName = `agentkit-native.${target}.node`;
  const binary = join(repoRoot, "packages", "native", "generated", binaryName);
  if (!existsSync(binary)) throw new Error(`native addon missing: ${binary}`);
  const destination = join(staging, "packages", `native-${target}`);
  if (existsSync(destination))
    throw new Error(`native platform staging already exists: ${destination}`);
  mkdirSync(join(staging, "packages"), { recursive: true });
  cpSync(join(repoRoot, "packages", `native-${target}`), destination, {
    recursive: true,
  });
  cpSync(binary, join(destination, binaryName));
  return { target, binaryName, destination };
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(resolve(process.argv[1])).href
) {
  const repoRoot = realpathSync(
    resolve(dirname(fileURLToPath(import.meta.url)), "../.."),
  );
  if (!process.argv[2])
    throw new Error(
      "usage: node stage-native-platform.mjs <existing-outside-repository-directory>",
    );
  const destination = realpathSync(resolve(process.argv[2]));
  const inside = relative(repoRoot, destination);
  if (
    inside === "" ||
    (inside !== ".." && !inside.startsWith(`..${sep}`) && !isAbsolute(inside))
  )
    throw new Error("native platform staging must be outside the repository");
  const staged = stageNativePlatform(repoRoot, destination);
  process.stdout.write(`${staged.destination}\n`);
}
