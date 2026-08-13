#!/usr/bin/env python3
"""Gate check: docs/ cites upstream as `<file>:<line>` or `<file>:<start>-<end>`
(e.g. `vendor/frankenphp/frankenphp.c:1471-1619`, or bare `phpthread.go:212-228`
once the file has been established in context). This verifies every such
citation resolves to a real file and an in-bounds line range, and that a
pinned set of "anchor" citations still contain the upstream token they were
cited for -- so a vendor bump that shifts lines without going out of bounds,
or a careless edit, fails the gate instead of waiting for a reviewer to catch
it by hand.

Run standalone: python3 scripts/check_doc_citations.py
"""
import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
VENDOR_ROOT = REPO_ROOT / "vendor" / "frankenphp"

# `<path>:<line>` or `<path>:<start>-<end>`, where <path> looks like a filename
# (has an extension) -- e.g. `frankenphp.c:1489`,
# `vendor/frankenphp/frankenphp.c:1471-1619`, `docs/PORTING-NOTES.md:41-43`.
# Deliberately does not match a bare `:1591` back-reference to a file named
# earlier in the same sentence -- there is no filename to resolve there.
CITATION_RE = re.compile(
    r"(?P<file>[A-Za-z0-9_][A-Za-z0-9_./-]*\.[A-Za-z0-9]+):(?P<start>\d+)(?:-(?P<end>\d+))?"
)

# (doc, citation, token): claims worth pinning to specific upstream text, re-
# checked against docs/ content each time either side changes -- a citation
# can stay in-bounds after a vendor bump while the code at that range changes
# entirely, which the bounds check alone would not catch.
ANCHORS = [
    ("ARCHITECTURE.md", "vendor/frankenphp/frankenphp.c:1471-1619", "php_thread"),
    ("ARCHITECTURE.md", "vendor/frankenphp/phpmainthread.go:248-257", "state.Rebooting"),
    ("ARCHITECTURE.md", "vendor/frankenphp/internal/state/state.go:13-37", "YieldingForReboot"),
    ("ARCHITECTURE.md", "vendor/frankenphp/frankenphp_arginfo.h:58", "fastcgi_finish_request"),
    ("ARCHITECTURE.md", "vendor/frankenphp/context.go:135-147", "sends the response to the client"),
    ("ARCHITECTURE.md", "vendor/frankenphp/testdata/finish-request.php:5-15", "frankenphp_finish_request"),
    ("ARCHITECTURE.md", "vendor/frankenphp/threadworker.go:298-327", "go_frankenphp_finish_worker_request"),
    ("ARCHITECTURE.md", "vendor/frankenphp/options.go:170", "0 = unlimited"),
    ("PORTING-NOTES.md", "vendor/frankenphp/frankenphp.go:430", "go_ub_write"),
    ("PORTING-NOTES.md", "vendor/frankenphp/threadregular.go:91-116", "select {"),
]


class Citation:
    def __init__(self, file, start, end, doc, doc_line):
        self.file = file
        self.start = start
        self.end = end if end is not None else start
        self.doc = doc
        self.doc_line = doc_line

    def __str__(self):
        if self.end != self.start:
            return f"{self.file}:{self.start}-{self.end}"
        return f"{self.file}:{self.start}"


def resolve_file(file_str):
    """Resolve a citation's file part to a Path. Returns (Path, None) or (None, reason)."""
    direct = REPO_ROOT / file_str
    if direct.is_file():
        return direct, None
    vendored = VENDOR_ROOT / file_str
    if vendored.is_file():
        return vendored, None
    if "/" not in file_str:
        matches = sorted(VENDOR_ROOT.rglob(file_str))
        if len(matches) == 1:
            return matches[0], None
        if len(matches) > 1:
            rel = [str(m.relative_to(REPO_ROOT)) for m in matches]
            return None, f"basename '{file_str}' is ambiguous under vendor/frankenphp/: {rel}"
    return (
        None,
        f"no file found for '{file_str}' (checked repo root, vendor/frankenphp/, "
        f"and vendor/frankenphp/**/{file_str})",
    )


def extract_citations(doc_path):
    citations = []
    text = doc_path.read_text()
    for lineno, line in enumerate(text.splitlines(), start=1):
        for m in CITATION_RE.finditer(line):
            end = int(m.group("end")) if m.group("end") else None
            citations.append(Citation(m.group("file"), int(m.group("start")), end, doc_path.name, lineno))
    return citations


def check_bounds(citation, errors):
    resolved, reason = resolve_file(citation.file)
    if resolved is None:
        errors.append(f"{citation.doc}:{citation.doc_line}: citation '{citation}' -- {reason}")
        return None
    n_lines = sum(1 for _ in resolved.open("r", errors="replace"))
    rel = resolved.relative_to(REPO_ROOT)
    if not (1 <= citation.start <= n_lines):
        errors.append(
            f"{citation.doc}:{citation.doc_line}: citation '{citation}' -- "
            f"start line {citation.start} out of bounds for {rel} ({n_lines} lines)"
        )
        return None
    if not (citation.start <= citation.end <= n_lines):
        errors.append(
            f"{citation.doc}:{citation.doc_line}: citation '{citation}' -- "
            f"end line {citation.end} out of bounds for {rel} ({n_lines} lines)"
        )
        return None
    return resolved


def check_anchors(errors):
    for doc, citation_str, token in ANCHORS:
        m = CITATION_RE.fullmatch(citation_str)
        if not m:
            errors.append(f"ANCHORS entry for {doc} has an unparseable citation: {citation_str!r}")
            continue
        start = int(m.group("start"))
        end = int(m.group("end")) if m.group("end") else start
        citation = Citation(m.group("file"), start, end, doc, "ANCHORS")
        resolved = check_bounds(citation, errors)
        if resolved is None:
            continue
        lines = resolved.read_text(errors="replace").splitlines()
        excerpt = "\n".join(lines[start - 1 : end])
        if token not in excerpt:
            rel = resolved.relative_to(REPO_ROOT)
            errors.append(
                f"ANCHORS: {doc} pins {citation_str} to contain {token!r}, but "
                f"{rel}:{start}-{end} no longer does (vendor bump or an edit invalidated this claim)"
            )


def main():
    errors = []
    checked = 0
    docs_dir = REPO_ROOT / "docs"
    doc_paths = sorted(docs_dir.glob("*.md"))
    if not doc_paths:
        print(f"no docs found under {docs_dir}", file=sys.stderr)
        return 1

    for doc_path in doc_paths:
        for citation in extract_citations(doc_path):
            checked += 1
            check_bounds(citation, errors)

    check_anchors(errors)

    if errors:
        print(f"checked {checked} citations across {len(doc_paths)} doc(s); {len(errors)} problem(s):\n")
        for e in errors:
            print(f"  - {e}")
        return 1

    print(f"checked {checked} citations across {len(doc_paths)} doc(s) and {len(ANCHORS)} anchor(s): all resolve")
    return 0


if __name__ == "__main__":
    sys.exit(main())
