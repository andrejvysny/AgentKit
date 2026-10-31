export function selectedBackend(): "ts" | "native" {
  const value = (
    globalThis as { process?: { env?: Record<string, string | undefined> } }
  ).process?.env?.AGENTKIT_BACKEND;
  if (value === undefined || value === "ts") return "ts";
  if (value === "native") return "native";
  throw Object.assign(new Error(`Invalid AGENTKIT_BACKEND: ${value}`), {
    code: "invalid_input",
  });
}
