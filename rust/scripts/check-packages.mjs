#!/usr/bin/env node
// Validate actual archive contents without publishing unpublished workspace crates.
import { spawnSync } from "node:child_process";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

const rustRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const offline = process.argv.includes("--offline") ? ["--offline"] : [];
const toolchain = process.env.AGENTKIT_RUST_TOOLCHAIN ?? "1.97.0";
const temporary = mkdtempSync(join(tmpdir(), "agentkit-package-consumer-"));
// The target dir lives INSIDE the per-run temporary directory on purpose:
// test binaries bake CARGO_MANIFEST_DIR at compile time, cargo's fingerprint
// does not track that macro's value, and `rmSync` wipes the extracted sources
// below. A persistent shared target dir therefore reuses binaries whose
// baked paths point at an already-deleted earlier run's tmp directory, and
// the packaged-contracts fixture reads fail with NotFound on every rerun.
const buildDirectory = join(temporary, "consumer-build");

function run(command, arguments_, options = {}) {
  const { capture = false, ...spawnOptions } = options;
  const result = spawnSync(command, arguments_, {
    cwd: rustRoot,
    encoding: "utf8",
    maxBuffer: 16 * 1024 * 1024,
    stdio: capture ? "pipe" : "inherit",
    ...spawnOptions,
  });
  if (result.error) throw result.error;
  if (result.status !== 0) {
    if (capture)
      process.stderr.write(`${result.stdout ?? ""}${result.stderr ?? ""}`);
    throw new Error(`${command} failed with status ${result.status}`);
  }
  return result.stdout ?? "";
}

function cargo(arguments_, options) {
  return run("cargo", [`+${toolchain}`, ...arguments_, ...offline], options);
}

function patches(packages, directory) {
  return `[patch.crates-io]\n${packages
    .map(
      (package_) =>
        `${package_.name} = { path = ${JSON.stringify(directory(package_))} }`,
    )
    .join("\n")}\n`;
}

function packageArchives(packages) {
  const configuration = join(temporary, "source-patches.toml");
  writeFileSync(
    configuration,
    patches(packages, (package_) => dirname(package_.manifest_path)),
  );
  const packageOptions = packages.flatMap((package_) => ["-p", package_.name]);
  cargo(
    [
      "package",
      "--no-verify",
      "--allow-dirty",
      "--target-dir",
      join(temporary, "archive-build"),
      "--config",
      configuration,
      ...packageOptions,
    ],
    { capture: true },
  );
  process.stdout.write(`Packaged ${packages.length} crate archives.\n`);
  const destination = join(temporary, "packages");
  mkdirSync(destination);
  for (const package_ of packages) {
    const archive = join(
      temporary,
      "archive-build",
      "package",
      `${package_.name}-${package_.version}.crate`,
    );
    run("tar", ["-xzf", archive, "-C", destination]);
  }
  return (package_) =>
    join(destination, `${package_.name}-${package_.version}`);
}

function checkConsumer(packages, extracted) {
  const consumer = join(temporary, "consumer");
  mkdirSync(join(consumer, "src"), { recursive: true });
  const dependencies = packages
    .map((package_) => {
      const features = Object.keys(package_.features).filter(
        (feature) => feature !== "default",
      );
      return `${package_.name} = { path = ${JSON.stringify(extracted(package_))}, version = ${JSON.stringify(package_.version)}, features = ${JSON.stringify(features)} }`;
    })
    .join("\n");
  const manifest =
    `[package]\nname = "agentkit-package-consumer"\nversion = "0.0.0"\nedition = "2024"\n` +
    `[workspace]\nresolver = "3"\n[dependencies]\n${dependencies}\n${patches(packages, extracted)}`;
  writeFileSync(join(consumer, "Cargo.toml"), manifest);
  writeFileSync(
    join(consumer, "src", "lib.rs"),
    packages
      .map((package_) => `pub use ${package_.name.replaceAll("-", "_")};`)
      .join("\n"),
  );
  cargo([
    "check",
    "--manifest-path",
    join(consumer, "Cargo.toml"),
    "--target-dir",
    buildDirectory,
  ]);
}

function checkPackagedContracts(packages, extracted) {
  const contracts = packages.find(
    (package_) => package_.name === "agentkit-contracts",
  );
  if (!contracts) throw new Error("agentkit-contracts package is missing");
  const directory = extracted(contracts);
  if (!existsSync(join(directory, "src", "schema_bundle.json")))
    throw new Error("Embedded schemas are missing");
  if (!existsSync(join(directory, "tests", "fixtures")))
    throw new Error("Packaged contract fixtures are missing");
  cargo([
    "test",
    "--manifest-path",
    join(directory, "Cargo.toml"),
    "--target-dir",
    buildDirectory,
  ]);
}

try {
  const metadata = JSON.parse(
    cargo(["metadata", "--no-deps", "--format-version", "1"], {
      capture: true,
    }),
  );
  const packages = metadata.packages.filter((package_) =>
    dirname(package_.manifest_path).startsWith(join(rustRoot, "crates") + sep),
  );
  if (packages.length === 0) throw new Error("No library packages found");
  for (const package_ of packages) {
    const files = cargo(
      ["package", "--list", "--allow-dirty", "-p", package_.name],
      { capture: true },
    );
    if (
      files
        .split("\n")
        .some((file) =>
          /(^|\/)(\.env(?:\..*)?|private)(\/|$)|\.(sqlite(?:-.*)?|db(?:-.*)?|log|jsonl)$/.test(
            file,
          ),
        )
    ) {
      throw new Error(
        `Private data or runtime artifacts included in ${package_.name}`,
      );
    }
  }
  const extracted = packageArchives(packages);
  checkConsumer(packages, extracted);
  checkPackagedContracts(packages, extracted);
  process.stdout.write(
    `Validated ${packages.length} crate archives and an isolated all-feature path consumer.\n` +
      "No publication or crates.io registry-resolution verification was performed.\n",
  );
} finally {
  rmSync(temporary, { recursive: true, force: true });
}
