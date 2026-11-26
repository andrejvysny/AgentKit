#!/usr/bin/python3 -I
"""Generate the image's third-party notice from what is installed.

Runs inside the image at build time (as root, stdlib only) and prints the
notice on stdout. Sections: Debian packages (dpkg), Python distributions
under /usr/local (importlib.metadata), Node.js, and the crates and
libraries statically linked into agentkit-sandboxd (from the committed
list given as the first argument; `#` lines are comments). Licence names
come from the DEP-5 `License:` fields of each package's copyright file
where present; the full texts stay in the image at the paths listed.

Usage: generate-notice.py SANDBOXD_CRATES_LIST
"""

import os
import subprocess
import sys
from importlib import metadata

COPYRIGHT = "/usr/share/doc/%s/copyright"
NODE_LICENSE = "/usr/local/share/doc/nodejs/LICENSE"


def dep5_licenses(path):
    names = set()
    try:
        with open(path, encoding="utf-8", errors="replace") as fh:
            for line in fh:
                if line.startswith("License:"):
                    name = line[len("License:"):].strip()
                    if name:
                        names.add(name)
    except OSError:
        return None
    return sorted(names)


def debian_section():
    out = subprocess.run(
        ["dpkg-query", "-W", "-f", "${Package}\t${Version}\t${Architecture}\n"],
        check=True, capture_output=True, text=True,
    ).stdout
    rows = sorted(line.split("\t") for line in out.splitlines() if line)
    lines = ["Debian packages (%d), from dpkg; licence texts in /usr/share/doc/<package>/copyright" % len(rows), ""]
    for name, version, arch in rows:
        licenses = dep5_licenses(COPYRIGHT % name)
        if licenses is None:
            text = "no copyright file"
        elif licenses:
            text = "; ".join(licenses)
        else:
            text = "see copyright file"
        lines.append("%s %s (%s): %s" % (name, version, arch, text))
    return lines


def python_license(dist):
    meta = dist.metadata
    expr = meta.get("License-Expression")
    if expr:
        return expr.strip()
    classifiers = sorted(c.split(" :: ")[-1] for c in meta.get_all("Classifier") or []
                         if c.startswith("License ::"))
    if classifiers:
        return "; ".join(classifiers)
    lic = (meta.get("License") or "").strip().splitlines()
    return lic[0][:120] if lic else "see distribution metadata"


def python_section():
    dists = []
    for dist in metadata.distributions():
        path = str(getattr(dist, "_path", ""))
        if path.startswith("/usr/local/"):
            dists.append(dist)
    dists.sort(key=lambda d: d.metadata["Name"].lower())
    lines = ["Python distributions (%d), installed from PyPI into /usr/local; "
             "licence texts in each *.dist-info directory" % len(dists), ""]
    for dist in dists:
        lines.append("%s %s: %s" % (dist.metadata["Name"], dist.version, python_license(dist)))
    return lines


def node_section():
    version = subprocess.run(["/usr/local/bin/node", "--version"], check=True,
                             capture_output=True, text=True).stdout.strip()
    npm = subprocess.run(["/usr/local/bin/node", "-p", "require('/usr/local/lib/node_modules/npm/package.json').version"],
                         check=True, capture_output=True, text=True).stdout.strip()
    return [
        "Node.js",
        "",
        "node %s (official nodejs.org binary): MIT and bundled third-party licences, full text in %s"
        % (version, NODE_LICENSE),
        "npm %s (bundled with Node.js): Artistic-2.0, full text in "
        "/usr/local/lib/node_modules/npm/LICENSE" % npm,
    ]


def sandboxd_section(path):
    with open(path, encoding="utf-8") as fh:
        rows = [line.rstrip("\n") for line in fh if line.strip() and not line.startswith("#")]
    return ["agentkit-sandboxd (/usr/local/bin/agentkit-sandboxd, a static Rust binary): "
            "the crates and libraries linked into it (%d)" % len(rows), ""] + rows


def main():
    if len(sys.argv) != 2:
        sys.stderr.write("usage: generate-notice.py SANDBOXD_CRATES_LIST\n")
        return 2
    sections = [
        ["AgentKit agent sandbox image: third-party notice",
         "",
         "Generated at build time from the packages installed in this image.",
         "The sandbox helpers in /usr/local/bin/sandbox-* and /usr/local/lib/sandbox,",
         "and agentkit-sandboxd, are part of AgentKit (MIT)."],
        debian_section(),
        python_section(),
        node_section(),
        sandboxd_section(sys.argv[1]),
    ]
    sys.stdout.write("\n\n".join("\n".join(s) for s in sections) + "\n")
    if not os.path.exists(NODE_LICENSE):
        sys.stderr.write("missing %s\n" % NODE_LICENSE)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
