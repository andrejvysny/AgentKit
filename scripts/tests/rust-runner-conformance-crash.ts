import { readFile } from "node:fs/promises";
import { join } from "node:path";
import {
  environment,
  snapshot,
  state,
  submit,
  terminal,
  until,
  worker,
} from "./rust-runner-conformance-driver.js";
import type { Json } from "./rust-runner-conformance-model.js";

async function child(database: string, ready: string): Promise<never> {
  const env = await environment("sqlite", database);
  await submit(env);
  await env.runner.startWorker(worker(env, state(), "crash_recovery"), {
    concurrency: 1,
  });
  await until(
    () => env.runner.retryDeadline("run") === 1200,
    "child retry intent",
  );
  await Bun.write(
    ready,
    JSON.stringify({ retryDue: env.runner.retryDeadline("run") }),
  );
  return new Promise<never>(() => {
    setInterval(() => {}, 1000);
  });
}

export async function runCrashScenario(directory: string): Promise<Json[]> {
  const database = join(directory, "typescript-crash.db");
  const ready = join(directory, "typescript-ready.json");
  const proc = Bun.spawn(
    [process.execPath, import.meta.path, "child", database, ready],
    { stdout: "pipe", stderr: "pipe" },
  );
  const stderr = new Response(proc.stderr).text();
  try {
    await until(
      async () => (await Bun.file(ready).exists()) || proc.exitCode !== null,
      "child process ready",
    );
    if (proc.exitCode !== null)
      throw new Error(`Crash worker exited ${proc.exitCode}: ${await stderr}`);
    const intent = JSON.parse(await readFile(ready, "utf8")) as {
      retryDue: number;
    };
    if (intent.retryDue !== 1200)
      throw new Error("Child missed retry boundary");
    proc.kill("SIGKILL");
    await proc.exited;
    await stderr;
  } finally {
    if (proc.exitCode === null) {
      proc.kill("SIGKILL");
      await proc.exited;
    }
  }
  const witness = state();
  const transcript: Json[] = [];
  const first = await environment("sqlite", database, 2000);
  transcript.push(await snapshot(first, witness, "recovered"));
  first.close();
  const reopened = await environment("sqlite", database, 2100);
  try {
    transcript.push(await snapshot(reopened, witness, "reopened_again"));
    await reopened.runner.recoverWithReport();
    transcript.push(await snapshot(reopened, witness, "recovered_again"));
    reopened.time.at = 2200;
    await reopened.runner.resume("run");
    const handle = await reopened.runner.startWorker(
      worker(reopened, witness, "crash_recovery"),
      { concurrency: 1 },
    );
    await terminal(reopened);
    await handle.stop();
    transcript.push(await snapshot(reopened, witness, "settled"));
    return transcript;
  } finally {
    reopened.close();
  }
}

if (import.meta.main && process.argv[2] === "child") {
  const database = process.argv[3];
  const ready = process.argv[4];
  if (!database || !ready) throw new Error("Missing crash worker paths");
  await child(database, ready);
}
