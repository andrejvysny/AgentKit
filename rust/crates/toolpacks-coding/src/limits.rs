//! Every bound the coding tool pack enforces, named. Mirrors
//! `packages/toolpacks-coding/src/limits.ts`.

/// Every bound the coding tool pack enforces, named. Override via options.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodingLimits {
    /// read_file: lines returned per call.
    pub read_max_lines: usize,
    /// read_file: bytes of numbered output returned per call.
    pub read_max_bytes: usize,
    /// read_file: files larger than this are refused (not read at all).
    pub read_max_file_bytes: u64,
    /// list_dir: entries returned.
    pub list_max_entries: usize,
    /// find_files: results when the model gives no limit.
    pub find_default_results: usize,
    /// find_files: hard cap on results.
    pub find_max_results: usize,
    /// code_search / find_files / repo_map: directory entries scanned.
    pub search_max_files: usize,
    /// code_search: files larger than this are skipped.
    pub search_max_file_bytes: u64,
    /// code_search: total bytes of file content scanned.
    pub search_max_total_bytes: u64,
    /// code_search: bytes of output.
    pub search_max_output_bytes: usize,
    /// code_search / find_files / repo_map: wall-clock budget.
    pub search_time_budget_ms: u64,
    /// code_search: matches when the model gives no limit.
    pub search_default_matches: usize,
    /// code_search: hard cap on matches.
    pub search_max_matches: usize,
    /// code_search: hard cap on context lines around a match.
    pub search_max_context: usize,
    /// code_search: characters kept of each output line.
    pub search_max_line_chars: usize,
    /// code_search / find_files: longest pattern accepted.
    pub search_max_pattern_chars: usize,
    /// An ignore file larger than this is not parsed.
    pub ignore_file_max_bytes: u64,
    /// repo_map: tree depth when the model gives none.
    pub repo_map_default_depth: usize,
    /// repo_map: hard cap on tree depth.
    pub repo_map_max_depth: usize,
    /// repo_map: tree entries listed.
    pub repo_map_max_entries: usize,
    /// repo_map: bytes shown of each manifest file.
    pub repo_map_manifest_excerpt_bytes: usize,
    /// edit_file: files larger than this are refused.
    pub edit_max_file_bytes: u64,
    /// edit_file: bytes of unified diff returned.
    pub edit_max_diff_bytes: usize,
    /// write_file: content larger than this is refused.
    pub write_max_bytes: usize,
    /// git_inspect: bytes of output returned.
    pub git_max_output_bytes: usize,
    /// git_inspect: `maxCount` ceiling for `log`.
    pub git_max_count: u32,
    /// git_inspect: process timeout.
    pub git_timeout_ms: u64,
    /// run_command / run_tests: timeout when none is given.
    pub command_default_timeout_sec: u64,
    /// run_command / run_tests: timeout ceiling.
    pub command_max_timeout_sec: u64,
    /// run_command / run_tests: tail lines returned.
    pub command_tail_max_lines: usize,
    /// run_command / run_tests: tail bytes returned.
    pub command_tail_max_bytes: usize,
    /// run_command / run_tests: bytes of the full log kept in scratch.
    pub command_max_log_bytes: u64,
    /// run_command / run_tests: wait after SIGTERM before SIGKILL, and after a
    /// kill before giving up on pipes. Capped at 10000.
    pub command_kill_grace_ms: u64,
    /// Per-tool `timeoutMs` advertised to the run loop for cheap tools.
    pub tool_timeout_ms: u64,
}

impl Default for CodingLimits {
    fn default() -> Self {
        Self {
            read_max_lines: 2000,
            read_max_bytes: 50 * 1024,
            read_max_file_bytes: 10 * 1024 * 1024,
            list_max_entries: 500,
            find_default_results: 200,
            find_max_results: 1000,
            search_max_files: 10_000,
            search_max_file_bytes: 1024 * 1024,
            search_max_total_bytes: 128 * 1024 * 1024,
            search_max_output_bytes: 48 * 1024,
            search_time_budget_ms: 20_000,
            search_default_matches: 100,
            search_max_matches: 1000,
            search_max_context: 20,
            search_max_line_chars: 1024,
            search_max_pattern_chars: 4096,
            ignore_file_max_bytes: 256 * 1024,
            repo_map_default_depth: 3,
            repo_map_max_depth: 6,
            repo_map_max_entries: 400,
            repo_map_manifest_excerpt_bytes: 2048,
            edit_max_file_bytes: 5 * 1024 * 1024,
            edit_max_diff_bytes: 32 * 1024,
            write_max_bytes: 5 * 1024 * 1024,
            git_max_output_bytes: 50 * 1024,
            git_max_count: 50,
            git_timeout_ms: 20_000,
            command_default_timeout_sec: 120,
            command_max_timeout_sec: 600,
            command_tail_max_lines: 2000,
            command_tail_max_bytes: 50 * 1024,
            command_max_log_bytes: 64 * 1024 * 1024,
            command_kill_grace_ms: 2000,
            tool_timeout_ms: 30_000,
        }
    }
}

/// Longest timeout any `*_ms` / `*_sec` bound may take: a day. Anything
/// longer is a typo that would effectively disable the bound.
const MAX_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1000;

impl CodingLimits {
    /// Reject bounds that would make the command runner misbehave: a zero
    /// tail cap, a zero/absurd timeout, or a kill grace above the runner's
    /// hard cap (which would otherwise be silently clamped). A grace of 0 is
    /// valid: SIGKILL immediately after SIGTERM.
    pub fn validate(&self) -> Result<(), String> {
        if self.command_tail_max_bytes < 1 {
            return Err("coding limit \"commandTailMaxBytes\" must be at least 1".to_owned());
        }
        let ms_bounds: [(&str, u64); 3] = [
            ("searchTimeBudgetMs", self.search_time_budget_ms),
            ("gitTimeoutMs", self.git_timeout_ms),
            ("toolTimeoutMs", self.tool_timeout_ms),
        ];
        for (name, value) in ms_bounds {
            if value == 0 || value > MAX_TIMEOUT_MS {
                return Err(format!(
                    "coding limit \"{name}\" must be between 1 and {MAX_TIMEOUT_MS} ms"
                ));
            }
        }
        let sec_bounds: [(&str, u64); 2] = [
            ("commandDefaultTimeoutSec", self.command_default_timeout_sec),
            ("commandMaxTimeoutSec", self.command_max_timeout_sec),
        ];
        for (name, value) in sec_bounds {
            if value == 0 || value > MAX_TIMEOUT_MS / 1000 {
                return Err(format!(
                    "coding limit \"{name}\" must be between 1 and {} s",
                    MAX_TIMEOUT_MS / 1000
                ));
            }
        }
        let cap = crate::process::KILL_GRACE_CAP_MS;
        if self.command_kill_grace_ms > cap {
            return Err(format!(
                "coding limit \"commandKillGraceMs\" must not exceed {cap} ms"
            ));
        }
        Ok(())
    }
}

/// Overridable subset of [`CodingLimits`]; mirrors `Partial<CodingLimits>`.
#[derive(Clone, Debug, Default)]
pub struct CodingLimitsOverride {
    pub read_max_lines: Option<usize>,
    pub read_max_bytes: Option<usize>,
    pub read_max_file_bytes: Option<u64>,
    pub list_max_entries: Option<usize>,
    pub find_default_results: Option<usize>,
    pub find_max_results: Option<usize>,
    pub search_max_files: Option<usize>,
    pub search_max_file_bytes: Option<u64>,
    pub search_max_total_bytes: Option<u64>,
    pub search_max_output_bytes: Option<usize>,
    pub search_time_budget_ms: Option<u64>,
    pub search_default_matches: Option<usize>,
    pub search_max_matches: Option<usize>,
    pub search_max_context: Option<usize>,
    pub search_max_line_chars: Option<usize>,
    pub search_max_pattern_chars: Option<usize>,
    pub ignore_file_max_bytes: Option<u64>,
    pub repo_map_default_depth: Option<usize>,
    pub repo_map_max_depth: Option<usize>,
    pub repo_map_max_entries: Option<usize>,
    pub repo_map_manifest_excerpt_bytes: Option<usize>,
    pub edit_max_file_bytes: Option<u64>,
    pub edit_max_diff_bytes: Option<usize>,
    pub write_max_bytes: Option<usize>,
    pub git_max_output_bytes: Option<usize>,
    pub git_max_count: Option<u32>,
    pub git_timeout_ms: Option<u64>,
    pub command_default_timeout_sec: Option<u64>,
    pub command_max_timeout_sec: Option<u64>,
    pub command_tail_max_lines: Option<usize>,
    pub command_tail_max_bytes: Option<usize>,
    pub command_max_log_bytes: Option<u64>,
    pub command_kill_grace_ms: Option<u64>,
    pub tool_timeout_ms: Option<u64>,
}

impl CodingLimitsOverride {
    /// Merge over the defaults, mirroring `resolveLimits` in `pack.ts`.
    pub fn resolve(&self) -> CodingLimits {
        let base = CodingLimits::default();
        CodingLimits {
            read_max_lines: self.read_max_lines.unwrap_or(base.read_max_lines),
            read_max_bytes: self.read_max_bytes.unwrap_or(base.read_max_bytes),
            read_max_file_bytes: self.read_max_file_bytes.unwrap_or(base.read_max_file_bytes),
            list_max_entries: self.list_max_entries.unwrap_or(base.list_max_entries),
            find_default_results: self
                .find_default_results
                .unwrap_or(base.find_default_results),
            find_max_results: self.find_max_results.unwrap_or(base.find_max_results),
            search_max_files: self.search_max_files.unwrap_or(base.search_max_files),
            search_max_file_bytes: self
                .search_max_file_bytes
                .unwrap_or(base.search_max_file_bytes),
            search_max_total_bytes: self
                .search_max_total_bytes
                .unwrap_or(base.search_max_total_bytes),
            search_max_output_bytes: self
                .search_max_output_bytes
                .unwrap_or(base.search_max_output_bytes),
            search_time_budget_ms: self
                .search_time_budget_ms
                .unwrap_or(base.search_time_budget_ms),
            search_default_matches: self
                .search_default_matches
                .unwrap_or(base.search_default_matches),
            search_max_matches: self.search_max_matches.unwrap_or(base.search_max_matches),
            search_max_context: self.search_max_context.unwrap_or(base.search_max_context),
            search_max_line_chars: self
                .search_max_line_chars
                .unwrap_or(base.search_max_line_chars),
            search_max_pattern_chars: self
                .search_max_pattern_chars
                .unwrap_or(base.search_max_pattern_chars),
            ignore_file_max_bytes: self
                .ignore_file_max_bytes
                .unwrap_or(base.ignore_file_max_bytes),
            repo_map_default_depth: self
                .repo_map_default_depth
                .unwrap_or(base.repo_map_default_depth),
            repo_map_max_depth: self.repo_map_max_depth.unwrap_or(base.repo_map_max_depth),
            repo_map_max_entries: self
                .repo_map_max_entries
                .unwrap_or(base.repo_map_max_entries),
            repo_map_manifest_excerpt_bytes: self
                .repo_map_manifest_excerpt_bytes
                .unwrap_or(base.repo_map_manifest_excerpt_bytes),
            edit_max_file_bytes: self.edit_max_file_bytes.unwrap_or(base.edit_max_file_bytes),
            edit_max_diff_bytes: self.edit_max_diff_bytes.unwrap_or(base.edit_max_diff_bytes),
            write_max_bytes: self.write_max_bytes.unwrap_or(base.write_max_bytes),
            git_max_output_bytes: self
                .git_max_output_bytes
                .unwrap_or(base.git_max_output_bytes),
            git_max_count: self.git_max_count.unwrap_or(base.git_max_count),
            git_timeout_ms: self.git_timeout_ms.unwrap_or(base.git_timeout_ms),
            command_default_timeout_sec: self
                .command_default_timeout_sec
                .unwrap_or(base.command_default_timeout_sec),
            command_max_timeout_sec: self
                .command_max_timeout_sec
                .unwrap_or(base.command_max_timeout_sec),
            command_tail_max_lines: self
                .command_tail_max_lines
                .unwrap_or(base.command_tail_max_lines),
            command_tail_max_bytes: self
                .command_tail_max_bytes
                .unwrap_or(base.command_tail_max_bytes),
            command_max_log_bytes: self
                .command_max_log_bytes
                .unwrap_or(base.command_max_log_bytes),
            command_kill_grace_ms: self
                .command_kill_grace_ms
                .unwrap_or(base.command_kill_grace_ms),
            tool_timeout_ms: self.tool_timeout_ms.unwrap_or(base.tool_timeout_ms),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_validate() {
        assert_eq!(CodingLimits::default().validate(), Ok(()));
    }

    #[test]
    fn a_zero_kill_grace_is_valid() {
        let limits = CodingLimits {
            command_kill_grace_ms: 0,
            ..CodingLimits::default()
        };
        assert_eq!(limits.validate(), Ok(()));
    }

    #[test]
    fn tail_max_bytes_zero_is_rejected() {
        let limits = CodingLimits {
            command_tail_max_bytes: 0,
            ..CodingLimits::default()
        };
        assert!(limits.validate().is_err());
    }

    #[test]
    fn zero_oversized_and_over_cap_timeouts_are_rejected() {
        for limits in [
            CodingLimits {
                tool_timeout_ms: 0,
                ..CodingLimits::default()
            },
            CodingLimits {
                git_timeout_ms: MAX_TIMEOUT_MS + 1,
                ..CodingLimits::default()
            },
            CodingLimits {
                command_kill_grace_ms: crate::process::KILL_GRACE_CAP_MS + 1,
                ..CodingLimits::default()
            },
            CodingLimits {
                command_max_timeout_sec: 24 * 60 * 60 + 1,
                ..CodingLimits::default()
            },
        ] {
            assert!(limits.validate().is_err(), "{limits:?}");
        }
    }
}
