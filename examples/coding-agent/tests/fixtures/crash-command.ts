/**
 * The command the crash child runs, shared with the test that re-issues it.
 *
 * It must be the SAME string in both places: an uncertain mutating call is
 * recognised by tool name plus the hash of its arguments, so a command that
 * differs by one character is, to the runtime, a different operation — and the
 * test would prove nothing.
 */
export const CRASH_COMMAND = `printf 'ran\\n' >> runs.log && sleep 3`;
