import { basename } from "node:path";

/** Literal words only: this does not evaluate expansions, substitutions or code. */
export function shellWords(raw: string): string[] {
  const words: string[] = [];
  let word = "";
  let quote = "";
  for (let i = 0; i < raw.length; i += 1) {
    const char = raw[i]!;
    if (char === "\\" && quote !== "'" && i + 1 < raw.length) {
      word += raw[++i];
    } else if (char === quote) {
      quote = "";
    } else if ((char === "'" || char === '"') && quote === "") {
      quote = char;
    } else if (quote === "" && /[\s;&|()<>`$]/.test(char)) {
      if (word !== "") words.push(word);
      word = "";
    } else {
      word += char;
    }
  }
  if (word !== "") words.push(word);
  return words;
}

export function executableName(word: string): string {
  return basename(word).toLowerCase();
}
