"""Shared code of the sandbox helpers (stdlib only).

Installed at /usr/local/lib/sandbox/sandboxlib.py. The helpers run with
`python3 -I` (isolated mode), so PYTHONPATH and user site-packages under
/home/agent can never replace this module or the standard library.
"""

import json
import os
import re
import signal
import sys
import time

ENV_FILE = "/etc/sandbox/environment"
OPS_DIR = "/tmp/sandbox-ops"
MARKER_VAR = "SANDBOX_OP_MARKER"
_MARKER_RE = re.compile(r"^[A-Za-z0-9._-]{1,128}$")


def fail(code, message, exit_code=2):
    """Print one JSON error line on stderr and exit."""
    sys.stderr.write(json.dumps({"error": code, "message": message}) + "\n")
    sys.exit(exit_code)


def check_marker(marker):
    if not _MARKER_RE.match(marker) or marker in (".", ".."):
        fail("invalid_marker", "marker must match [A-Za-z0-9._-]{1,128}")
    return marker


def load_environment(path=ENV_FILE):
    env = {}
    with open(path, encoding="utf-8") as fh:
        for line in fh:
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            key, _, value = line.partition("=")
            env[key] = value
    return env


def status_path(marker):
    return os.path.join(OPS_DIR, marker + ".json")


def write_status(marker, record):
    """Replace the op's status file atomically (same directory, rename)."""
    os.makedirs(OPS_DIR, mode=0o700, exist_ok=True)
    final = status_path(marker)
    tmp = "%s.%d.tmp" % (final, os.getpid())
    with open(tmp, "w", encoding="utf-8") as fh:
        json.dump(record, fh, sort_keys=True)
    os.rename(tmp, final)


def read_status(marker):
    try:
        with open(status_path(marker), encoding="utf-8") as fh:
            return json.load(fh)
    except (OSError, ValueError):
        return None


def _is_zombie(pid):
    try:
        with open("/proc/%d/stat" % pid, "rb") as fh:
            data = fh.read()
    except OSError:
        return True
    # The state follows the parenthesised command name, which may hold spaces.
    return data[data.rfind(b")") + 2 : data.rfind(b")") + 3] == b"Z"


def matching_pids(marker, pgid=None):
    """Live processes that carry MARKER_VAR=marker or belong to pgid.

    The environment match catches processes that left the group (setsid);
    the group match catches processes that cleared their environment.
    """
    needle = ("%s=%s" % (MARKER_VAR, marker)).encode()
    me = os.getpid()
    found = []
    for name in os.listdir("/proc"):
        if not name.isdigit():
            continue
        pid = int(name)
        if pid == me or _is_zombie(pid):
            continue
        hit = False
        if pgid is not None:
            try:
                hit = os.getpgid(pid) == pgid
            except OSError:
                continue
        if not hit:
            try:
                with open("/proc/%d/environ" % pid, "rb") as fh:
                    hit = needle in fh.read().split(b"\0")
            except OSError:
                continue
        if hit:
            found.append(pid)
    return found


def _signal_all(pids, pgid, sig):
    if pgid is not None:
        try:
            os.killpg(pgid, sig)
        except OSError:
            pass
    for pid in pids:
        try:
            os.kill(pid, sig)
        except OSError:
            pass


def kill_tree(marker, pgid=None, grace_s=2.0, kill_timeout_s=5.0):
    """SIGTERM every matching process, SIGKILL what is left after grace_s.

    Returns (signalled, remaining): how many processes were found at the
    start and how many still match when it gives up.
    """
    pids = matching_pids(marker, pgid)
    signalled = len(pids)
    if not pids:
        return 0, 0
    _signal_all(pids, pgid, signal.SIGTERM)
    end = time.monotonic() + grace_s
    while time.monotonic() < end:
        pids = matching_pids(marker, pgid)
        if not pids:
            return signalled, 0
        time.sleep(0.05)
    end = time.monotonic() + kill_timeout_s
    while pids and time.monotonic() < end:
        _signal_all(pids, pgid, signal.SIGKILL)
        time.sleep(0.05)
        pids = matching_pids(marker, pgid)
    return signalled, len(pids)
