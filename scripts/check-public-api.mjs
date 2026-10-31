#!/usr/bin/env node
import { Extractor, ExtractorConfig } from "@microsoft/api-extractor";
import {
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  writeFileSync,
} from "node:fs";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../", import.meta.url));
const reports = join(root, "docs/rust/api");
const update = process.argv.includes("--update");
const selected = process.argv
  .find((arg) => arg.startsWith("--entry="))
  ?.slice(8);
const readJson = (path) => JSON.parse(readFileSync(path, "utf8"));

function entries() {
  return readdirSync(join(root, "packages"))
    .sort()
    .flatMap((directory) => {
      const folder = join(root, "packages", directory);
      const manifest = readJson(join(folder, "package.json"));
      if (manifest.private) return [];
      const exports = manifest.publishConfig?.exports ?? manifest.exports;
      return Object.entries(exports).map(([subpath, target]) => ({
        folder,
        package: manifest.name,
        subpath,
        name: `${directory}${subpath === "." ? "" : `--${subpath.slice(2).replaceAll("/", "--")}`}`,
        entry: resolve(folder, target.types),
      }));
    });
}

function compiler(entry) {
  return {
    overrideTsconfig: {
      compilerOptions: {
        target: "ES2022",
        module: "ESNext",
        moduleResolution: "Bundler",
        lib: ["ES2022", "DOM", "DOM.Iterable"],
        strict: true,
        skipLibCheck: true,
        types: ["bun"],
        paths: Object.fromEntries(
          readdirSync(join(root, "packages"))
            .filter((name) => name !== "agentkit")
            .map((name) => [
              `@agentkit/${name}`,
              [join(root, "packages", name, "dist/index.d.ts")],
            ]),
        ),
      },
      files: [entry.entry],
    },
  };
}

function configuration(entry) {
  return ExtractorConfig.prepare({
    configObject: {
      projectFolder: entry.folder,
      mainEntryPointFilePath: entry.entry,
      bundledPackages: ["@agentkit/*"],
      compiler: compiler(entry),
      apiReport: {
        enabled: true,
        includeForgottenExports: true,
        reportFileName: `${entry.name}.api.md`,
        reportFolder: reports,
        reportTempFolder: join(root, "node_modules/.cache/agentkit-api"),
      },
      docModel: { enabled: false },
      dtsRollup: { enabled: false },
      tsdocMetadata: { enabled: false },
      newlineKind: "lf",
      messages: {
        extractorMessageReporting: {
          default: { logLevel: "warning" },
          "ae-missing-release-tag": { logLevel: "none" },
          "ae-forgotten-export": { logLevel: "none" },
          "ae-undocumented": { logLevel: "none" },
          "ae-unresolved-link": { logLevel: "none" },
        },
        tsdocMessageReporting: { default: { logLevel: "none" } },
      },
    },
    configObjectFullPath: join(root, "scripts/api-extractor.json"),
    packageJsonFullPath: join(entry.folder, "package.json"),
  });
}

mkdirSync(reports, { recursive: true });
const inventory = entries();
const manifestPath = join(reports, "manifest.json");
const surfaces = inventory.map(({ package: name, subpath, name: report }) => ({
  name,
  subpath,
  report,
}));
if (update && !selected) {
  const manifest = {
    frozenCommit: "5a4bb61a479cc8ca3dfb0e24a8a0e1e0b37a9c61",
    surfaces,
  };
  writeFileSync(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`);
} else if (
  JSON.stringify(readJson(manifestPath).surfaces) !== JSON.stringify(surfaces)
) {
  throw new Error(
    "Public export inventory changed; review the frozen API before updating",
  );
}
const targets = inventory.filter(
  (entry) => !selected || selected === entry.name,
);
if (!targets.length) throw new Error(`Unknown public API entry: ${selected}`);
let failed = false;
for (const entry of targets) {
  if (!existsSync(entry.entry))
    throw new Error(`Build first; missing ${entry.entry}`);
  const result = Extractor.invoke(configuration(entry), {
    localBuild: update,
    showVerboseMessages: false,
    messageCallback(message) {
      if (message.messageId === "console-preamble") message.handled = true;
    },
  });
  failed ||= !result.succeeded;
  console.log(`${entry.name}: ${result.succeeded ? "passed" : "FAILED"}`);
}
process.exitCode = failed ? 1 : 0;
