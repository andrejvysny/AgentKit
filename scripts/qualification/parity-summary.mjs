// Summary parsing for the differential parity lanes.
//
// `bun test` prints its authoritative counts only in the trailing summary
// block, while progress output can contain other "<n> pass" shaped lines
// (per-file headers, test log noise). Parsing the FIRST such line reports
// fabricated numbers whenever a lane is killed mid-run, so:
//   - a lane that printed the final summary is counted from the LAST
//     line-anchored match of each kind (the trailing summary block);
//   - a lane killed before its summary counts only bun's per-test
//     `(fail)`-style markers, and `complete:false`/`partial:true` say the
//     numbers are a lower bound that proves nothing on their own.

const SUMMARY_KINDS = ["pass", "fail", "skip", "todo"];

function lastSummaryCount(output, kind) {
  const matches = [
    ...output.matchAll(new RegExp(`^[ \\t]*(\\d+) ${kind}\\b[^\\d]*$`, "gm")),
  ];
  return matches.length ? Number(matches.at(-1)[1]) : null;
}

function markerCount(output, kind) {
  return [...output.matchAll(new RegExp(`^\\(${kind}\\)`, "gm"))].length;
}

/**
 * Parse one `bun test` lane's counts.
 *
 * @param {string} output combined stdout+stderr of the lane
 * @returns {{pass:number, fail:number, skip:number, todo:number,
 *   complete:boolean, partial:boolean}}
 */
export function parseParitySummary(output) {
  const text = output ?? "";
  const complete = /Ran \d+ tests? across/.test(text);
  const counts = {};
  for (const kind of SUMMARY_KINDS) {
    if (complete) {
      counts[kind] = lastSummaryCount(text, kind) ?? markerCount(text, kind);
    } else {
      // Truncated output: summary-shaped lines are unreliable here.
      counts[kind] = markerCount(text, kind);
    }
  }
  return { ...counts, complete, partial: !complete };
}
