import { executableName, shellWords } from "./command-words.js";

const INPUT_FLAGS = new Set([
  "--preload",
  "--require",
  "--import",
  "--loader",
  "-r",
]);
const BOOLEAN_FLAGS = new Set(["--no-warnings", "--enable-source-maps"]);

/** Supported startup flags precede the positional file; unknown arity cannot be guessed. */
function interpreterFile(words: string[]): string | undefined {
  const name = executableName(words[0] ?? "");
  if (!/^(node(js)?|bun|python[0-9.]*|ruby|perl|lua|sh|bash|zsh)$/.test(name))
    return undefined;
  for (let index = 1; index < words.length; index += 1) {
    const word = words[index]!;
    if (word === "--") {
      const script = words[index + 1];
      if (script === undefined)
        throw new Error("direct interpreter entrypoint is unresolved");
      return script;
    }
    const equal = word.indexOf("=");
    const flag = equal === -1 ? word : word.slice(0, equal);
    if (INPUT_FLAGS.has(flag)) {
      if (equal === -1) index += 1;
      continue;
    }
    if (
      word.startsWith("-r") &&
      word.length > 2 &&
      /^(node(js)?|bun)$/.test(name)
    )
      continue;
    if (BOOLEAN_FLAGS.has(word)) continue;
    if (word.startsWith("-"))
      throw new Error("unsupported direct interpreter options");
    if (name === "bun" && (word === "run" || word === "build"))
      throw new Error("nested Bun entrypoint resolution is unsupported");
    return name === "bun" && word === "test" ? undefined : word;
  }
  throw new Error("direct interpreter entrypoint is unresolved");
}

/** Only literal direct inputs are covered; shell-generated paths cannot be bound. */
export function directScriptFiles(command: string): string[] {
  if (/[$`;&|<>]/.test(command))
    throw new Error(
      "nonliteral package script cannot receive standing approval",
    );
  const words = shellWords(command);
  const files = new Set(
    words.filter(
      (word) =>
        !word.startsWith("-") &&
        /^[\w./ -]+\.(?:[cm]?[jt]s|py|sh|rb|pl)$/.test(word),
    ),
  );
  while (
    words[0] !== undefined &&
    (/^[A-Za-z_][A-Za-z0-9_]*=/.test(words[0]) ||
      /^(env|command|exec)$/.test(words[0]))
  )
    words.shift();
  const entry = words[0];
  if (
    ["npx", "bunx"].includes(executableName(entry ?? "")) &&
    words[1] !== "tsc"
  )
    throw new Error("nested package runner resolution is unsupported");
  if (entry?.startsWith("-") || /^(cd|pushd|popd)$/.test(entry ?? ""))
    throw new Error("unsupported package script command resolution");
  if (entry?.includes("/")) files.add(entry);
  const script = interpreterFile(words);
  if (script !== undefined) files.add(script);
  return [...files];
}

/** Resolve only supported literal wrapper prefixes, never search ordinary arguments. */
export function leadingExecutable(tokens: string[]): string | undefined {
  for (let index = 0; index < tokens.length; index += 1) {
    const word = tokens[index]!;
    if (/^[A-Za-z_][A-Za-z0-9_]*=/.test(word)) continue;
    const name = executableName(word);
    if (name === "env" || name === "command") {
      if (tokens[index + 1] === "--") index += 1;
      continue;
    }
    if (name === "npx" || name === "bunx") {
      const selected = tokens[index + 1];
      return selected === undefined || selected.startsWith("-")
        ? undefined
        : executableName(selected);
    }
    return word.startsWith("-") ? undefined : name;
  }
  return undefined;
}
