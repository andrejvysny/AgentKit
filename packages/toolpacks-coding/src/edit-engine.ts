/**
 * Exact-then-normalized multi-edit matching on LF-normalised text.
 *
 * Adapted from Pi (https://github.com/earendil-works/pi), v1.4.0, commit
 * 4a080252f06caf63a525d7873c5d94d3c23887e0, paths
 * `packages/coding-agent/src/core/tools/edit-diff.ts` and `edit.ts`.
 * Copyright (c) 2025 Mario Zechner. MIT License.
 * Changes: errors are `CodingError` with codes; the `diff` package display
 * helpers are dropped (see ./diff.ts); BOM split inlined; no TUI preview.
 */
import { CodingError } from "./errors.js";

export interface Edit {
  oldText: string;
  newText: string;
}

export type Eol = "\r\n" | "\n" | "\r";

/** Most frequent terminator; a tie prefers LF, then CRLF. */
export function dominantEol(content: string): Eol {
  let crlf = 0;
  let lf = 0;
  let cr = 0;
  for (let i = 0; i < content.length; i++) {
    const c = content.charCodeAt(i);
    if (c === 13) {
      if (content.charCodeAt(i + 1) === 10) {
        crlf++;
        i++;
      } else cr++;
    } else if (c === 10) lf++;
  }
  const top = Math.max(crlf, lf, cr);
  if (lf === top) return "\n";
  return crlf === top ? "\r\n" : "\r";
}

export function normalizeToLF(text: string): string {
  return text.replace(/\r\n/g, "\n").replace(/\r/g, "\n");
}

/**
 * LF view of `text` plus, per view character, its offset in `text` (and one
 * extra entry for the end), so matches can be spliced back into the original.
 */
function lfView(text: string): { view: string; starts: number[] } {
  const starts: number[] = [];
  const parts: string[] = [];
  for (let i = 0; i < text.length; i++) {
    const c = text.charCodeAt(i);
    starts.push(i);
    if (c === 13) {
      parts.push("\n");
      if (text.charCodeAt(i + 1) === 10) i++;
    } else parts.push(text[i] as string);
  }
  starts.push(text.length);
  return { view: parts.join(""), starts };
}

const EOL_AT_END = /(?:\r\n|\r|\n)$/;

/**
 * Inserted text takes the file's dominant terminator; a final newline that
 * replaces a terminator keeps that terminator, so unchanged lines stay exact.
 */
function withEol(replacement: string, replaced: string, eol: Eol): string {
  const out = replacement.replace(/\n/g, eol);
  const tail = EOL_AT_END.exec(replaced)?.[0];
  if (tail === undefined || !replacement.endsWith("\n")) return out;
  return out.slice(0, out.length - eol.length) + tail;
}

export function splitBom(content: string): { bom: string; text: string } {
  return content.startsWith("﻿")
    ? { bom: "﻿", text: content.slice(1) }
    : { bom: "", text: content };
}

/**
 * Normalise text for the fallback match: NFKC, trailing whitespace per line,
 * smart quotes, Unicode dashes and special spaces folded to ASCII.
 */
export function normalizeForFuzzyMatch(text: string): string {
  return text
    .normalize("NFKC")
    .split("\n")
    .map((line) => line.trimEnd())
    .join("\n")
    .replace(/[‘’‚‛]/g, "'")
    .replace(/[“”„‟]/g, '"')
    .replace(/[‐‑‒–—―−]/g, "-")
    .replace(/[  -   　]/g, " ");
}

function splitLinesWithEndings(content: string): string[] {
  return content.match(/[^\n]*\n|[^\n]+/g) ?? [];
}

function splitOriginalLines(content: string): string[] {
  return content.match(/[^\r\n]*(?:\r\n|\r|\n)|[^\r\n]+/g) ?? [];
}

interface LineSpan {
  start: number;
  end: number;
}

interface MatchedEdit {
  editIndex: number;
  matchIndex: number;
  matchLength: number;
  newText: string;
}

type Replacement = Pick<MatchedEdit, "matchIndex" | "matchLength" | "newText">;

function lineSpans(content: string): LineSpan[] {
  let offset = 0;
  return splitLinesWithEndings(content).map((line) => {
    const span = { start: offset, end: offset + line.length };
    offset = span.end;
    return span;
  });
}

function replacementLineRange(lines: LineSpan[], replacement: Replacement) {
  const from = replacement.matchIndex;
  const to = replacement.matchIndex + replacement.matchLength;
  let startLine = -1;
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i] as LineSpan;
    if (from >= line.start && from < line.end) {
      startLine = i;
      break;
    }
  }
  if (startLine === -1)
    throw new CodingError(
      "edit_internal",
      "replacement range is outside the base content",
    );
  let endLine = startLine;
  while (endLine < lines.length && (lines[endLine] as LineSpan).end < to)
    endLine++;
  if (endLine >= lines.length)
    throw new CodingError(
      "edit_internal",
      "replacement range is outside the base content",
    );
  return { startLine, endLine: endLine + 1 };
}

function applyReplacements(
  content: string,
  replacements: Replacement[],
  offset = 0,
): string {
  let result = content;
  for (let i = replacements.length - 1; i >= 0; i--) {
    const r = replacements[i] as Replacement;
    const at = r.matchIndex - offset;
    result =
      result.substring(0, at) +
      r.newText +
      result.substring(at + r.matchLength);
  }
  return result;
}

/**
 * Apply replacements matched against a normalised `baseContent` to the
 * original, copying every untouched line block back byte-for-byte.
 */
function applyPreservingUnchangedLines(
  original: string,
  base: string,
  replacements: Replacement[],
  eol: Eol,
): string {
  const originalLines = splitOriginalLines(original);
  const baseLines = lineSpans(base);
  if (originalLines.length !== baseLines.length)
    throw new CodingError(
      "edit_normalization_unsafe",
      "normalized matching changed the line count; use exact text",
    );
  const groups: Array<{
    startLine: number;
    endLine: number;
    replacements: Replacement[];
  }> = [];
  const sorted = [...replacements].sort((a, b) => a.matchIndex - b.matchIndex);
  for (const replacement of sorted) {
    const range = replacementLineRange(baseLines, replacement);
    const current = groups[groups.length - 1];
    if (current && range.startLine < current.endLine) {
      current.endLine = Math.max(current.endLine, range.endLine);
      current.replacements.push(replacement);
      continue;
    }
    groups.push({ ...range, replacements: [replacement] });
  }
  let cursor = 0;
  let result = "";
  for (const group of groups) {
    result += originalLines.slice(cursor, group.startLine).join("");
    const startOffset = (baseLines[group.startLine] as LineSpan).start;
    const endOffset = (baseLines[group.endLine - 1] as LineSpan).end;
    result += withEol(
      applyReplacements(
        base.slice(startOffset, endOffset),
        group.replacements,
        startOffset,
      ),
      originalLines.slice(group.startLine, group.endLine).join(""),
      eol,
    );
    cursor = group.endLine;
  }
  return result + originalLines.slice(cursor).join("");
}

interface FuzzyMatch {
  found: boolean;
  index: number;
  matchLength: number;
  usedFuzzyMatch: boolean;
}

function findText(content: string, oldText: string): FuzzyMatch {
  const exact = content.indexOf(oldText);
  if (exact !== -1)
    return {
      found: true,
      index: exact,
      matchLength: oldText.length,
      usedFuzzyMatch: false,
    };
  const fuzzyOld = normalizeForFuzzyMatch(oldText);
  const at = normalizeForFuzzyMatch(content).indexOf(fuzzyOld);
  if (at === -1)
    return { found: false, index: -1, matchLength: 0, usedFuzzyMatch: false };
  return {
    found: true,
    index: at,
    matchLength: fuzzyOld.length,
    usedFuzzyMatch: true,
  };
}

function countOccurrences(content: string, oldText: string): number {
  return (
    normalizeForFuzzyMatch(content).split(normalizeForFuzzyMatch(oldText))
      .length - 1
  );
}

function spliceOriginal(
  original: string,
  starts: number[],
  matched: Replacement[],
  eol: Eol,
): string {
  let result = "";
  let cursor = 0;
  for (const r of matched) {
    const from = starts[r.matchIndex] as number;
    const to = starts[r.matchIndex + r.matchLength] as number;
    result +=
      original.slice(cursor, from) +
      withEol(r.newText, original.slice(from, to), eol);
    cursor = to;
  }
  return result + original.slice(cursor);
}

export interface AppliedEdits {
  /** The original, LF-normalised (for diffs). */
  baseContent: string;
  /** The edited text in LF form (for diffs). */
  newContent: string;
  /** The edited text with every untouched line terminator byte-identical. */
  output: string;
}

/**
 * Apply every edit against the same ORIGINAL content: each `oldText` must
 * match exactly once (exact first, normalized fallback) and edits must not
 * overlap. Matching runs on an LF view; replacements are spliced into the
 * original so its line terminators survive.
 */
export function applyEdits(
  original: string,
  edits: Edit[],
  path: string,
): AppliedEdits {
  const { view: normalizedContent, starts } = lfView(original);
  const eol = dominantEol(original);
  const total = edits.length;
  const label = (i: number) => (total === 1 ? "the text" : `edits[${i}]`);
  const normalized = edits.map((edit) => ({
    oldText: normalizeToLF(edit.oldText),
    newText: normalizeToLF(edit.newText),
  }));
  normalized.forEach((edit, i) => {
    if (edit.oldText.length === 0)
      throw new CodingError(
        "edit_empty_old_text",
        `${total === 1 ? "oldText" : `edits[${i}].oldText`} must not be empty in ${path}`,
      );
  });
  const usedFuzzy = normalized.some(
    (edit) => findText(normalizedContent, edit.oldText).usedFuzzyMatch,
  );
  const base = usedFuzzy
    ? normalizeForFuzzyMatch(normalizedContent)
    : normalizedContent;
  const matched: MatchedEdit[] = [];
  normalized.forEach((edit, i) => {
    const match = findText(base, edit.oldText);
    if (!match.found)
      throw new CodingError(
        "edit_no_match",
        `Could not find ${label(i)} in ${path}. oldText must match exactly, including whitespace and newlines.`,
      );
    const occurrences = countOccurrences(base, edit.oldText);
    if (occurrences > 1)
      throw new CodingError(
        "edit_ambiguous",
        `Found ${occurrences} occurrences of ${label(i)} in ${path}. oldText must be unique; add surrounding context.`,
      );
    matched.push({
      editIndex: i,
      matchIndex: match.index,
      matchLength: match.matchLength,
      newText: edit.newText,
    });
  });
  matched.sort((a, b) => a.matchIndex - b.matchIndex);
  for (let i = 1; i < matched.length; i++) {
    const previous = matched[i - 1] as MatchedEdit;
    const current = matched[i] as MatchedEdit;
    if (previous.matchIndex + previous.matchLength > current.matchIndex)
      throw new CodingError(
        "edit_overlap",
        `edits[${previous.editIndex}] and edits[${current.editIndex}] overlap in ${path}. Merge them or target disjoint regions.`,
      );
  }
  const output = usedFuzzy
    ? applyPreservingUnchangedLines(original, base, matched, eol)
    : spliceOriginal(original, starts, matched, eol);
  if (output === original)
    throw new CodingError(
      "edit_no_change",
      `No changes made to ${path}: the replacement produced identical content.`,
    );
  return {
    baseContent: normalizedContent,
    newContent: normalizeToLF(output),
    output,
  };
}
