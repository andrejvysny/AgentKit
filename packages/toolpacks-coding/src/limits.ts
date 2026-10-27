/** Every bound the coding tool pack enforces, named. Override via options. */
export interface CodingLimits {
  /** read_file: lines returned per call. */
  readMaxLines: number;
  /** read_file: bytes of numbered output returned per call. */
  readMaxBytes: number;
  /** read_file: files larger than this are refused (not read at all). */
  readMaxFileBytes: number;
  /** list_dir: entries returned. */
  listMaxEntries: number;
  /** find_files: results when the model gives no limit. */
  findDefaultResults: number;
  /** find_files: hard cap on results. */
  findMaxResults: number;
  /** code_search / find_files / repo_map: directory entries scanned. */
  searchMaxFiles: number;
  /** code_search: files larger than this are skipped. */
  searchMaxFileBytes: number;
  /** code_search: total bytes of file content scanned. */
  searchMaxTotalBytes: number;
  /** code_search: bytes of output. */
  searchMaxOutputBytes: number;
  /** code_search / find_files / repo_map: wall-clock budget. */
  searchTimeBudgetMs: number;
  /** code_search: matches when the model gives no limit. */
  searchDefaultMatches: number;
  /** code_search: hard cap on matches. */
  searchMaxMatches: number;
  /** code_search: hard cap on context lines around a match. */
  searchMaxContext: number;
  /** code_search: characters kept of each output line. */
  searchMaxLineChars: number;
  /** code_search / find_files: longest pattern accepted. */
  searchMaxPatternChars: number;
  /** An ignore file larger than this is not parsed. */
  ignoreFileMaxBytes: number;
  /** repo_map: tree depth when the model gives none. */
  repoMapDefaultDepth: number;
  /** repo_map: hard cap on tree depth. */
  repoMapMaxDepth: number;
  /** repo_map: tree entries listed. */
  repoMapMaxEntries: number;
  /** repo_map: bytes shown of each manifest file. */
  repoMapManifestExcerptBytes: number;
  /** edit_file: files larger than this are refused. */
  editMaxFileBytes: number;
  /** edit_file: bytes of unified diff returned. */
  editMaxDiffBytes: number;
  /** write_file: content larger than this is refused. */
  writeMaxBytes: number;
  /** git_inspect: bytes of output returned. */
  gitMaxOutputBytes: number;
  /** git_inspect: `maxCount` ceiling for `log`. */
  gitMaxCount: number;
  /** git_inspect: process timeout. */
  gitTimeoutMs: number;
  /** run_command / run_tests: timeout when none is given. */
  commandDefaultTimeoutSec: number;
  /** run_command / run_tests: timeout ceiling. */
  commandMaxTimeoutSec: number;
  /** run_command / run_tests: tail lines returned. */
  commandTailMaxLines: number;
  /** run_command / run_tests: tail bytes returned. */
  commandTailMaxBytes: number;
  /** run_command / run_tests: bytes of the full log kept in scratch. */
  commandMaxLogBytes: number;
  /** run_command / run_tests: wait after a kill before giving up on pipes. */
  commandKillGraceMs: number;
  /** Per-tool `timeoutMs` advertised to the run loop for cheap tools. */
  toolTimeoutMs: number;
}

export const DEFAULT_CODING_LIMITS: Readonly<CodingLimits> = Object.freeze({
  readMaxLines: 2000,
  readMaxBytes: 50 * 1024,
  readMaxFileBytes: 10 * 1024 * 1024,
  listMaxEntries: 500,
  findDefaultResults: 200,
  findMaxResults: 1000,
  searchMaxFiles: 10_000,
  searchMaxFileBytes: 1024 * 1024,
  searchMaxTotalBytes: 128 * 1024 * 1024,
  searchMaxOutputBytes: 48 * 1024,
  searchTimeBudgetMs: 20_000,
  searchDefaultMatches: 100,
  searchMaxMatches: 1000,
  searchMaxContext: 20,
  searchMaxLineChars: 1024,
  searchMaxPatternChars: 4096,
  ignoreFileMaxBytes: 256 * 1024,
  repoMapDefaultDepth: 3,
  repoMapMaxDepth: 6,
  repoMapMaxEntries: 400,
  repoMapManifestExcerptBytes: 2048,
  editMaxFileBytes: 5 * 1024 * 1024,
  editMaxDiffBytes: 32 * 1024,
  writeMaxBytes: 5 * 1024 * 1024,
  gitMaxOutputBytes: 50 * 1024,
  gitMaxCount: 50,
  gitTimeoutMs: 20_000,
  commandDefaultTimeoutSec: 120,
  commandMaxTimeoutSec: 600,
  commandTailMaxLines: 2000,
  commandTailMaxBytes: 50 * 1024,
  commandMaxLogBytes: 64 * 1024 * 1024,
  commandKillGraceMs: 2000,
  toolTimeoutMs: 30_000,
});
