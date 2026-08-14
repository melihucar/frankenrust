#!/usr/bin/env python3
"""Gate check: docs/ cites upstream as `<file>:<line>` or `<file>:<start>-<end>`
(e.g. `vendor/frankenphp/frankenphp.c:1471-1619`, or bare `phpthread.go:212-228`
once the file has been established in context -- house style, see
docs/ARCHITECTURE.md:43). A leading `./` (e.g. `./README.md:39-45`) is a
different marker: it means "the file at this exact repo-relative path", used
when a bare basename would also match something under vendor/frankenphp/.

This verifies every such citation resolves to exactly one real file and an
in-bounds line range, and that a pinned set of "anchor" citations still
contain the upstream token they were cited for -- so a vendor bump that shifts
lines without going out of bounds, or a careless edit, fails the gate instead
of waiting for a reviewer to catch it by hand.

Resolution never guesses between candidates: a citation that names more than
one real file is a hard error naming every candidate, full stop. There is no
precedence between repo-root, vendor-root, and vendor-subdirectory matches --
that is the entire point, not an oversight. Ambiguity is fixed by editing the
doc to cite a fuller path (or prefixing `./` for a repo-relative file), never
by adding a table to this checker that picks one silently.

Run standalone:  python3 scripts/check_doc_citations.py
Run self-tests:  python3 scripts/check_doc_citations.py --selftest
"""

import re
import sys
import tempfile
from pathlib import Path, PurePosixPath

REPO_ROOT = Path(__file__).resolve().parent.parent
VENDOR_ROOT = REPO_ROOT / "vendor" / "frankenphp"

# `<path>:<line>` or `<path>:<start>-<end>`, where <path> looks like a filename
# (has an extension) and may carry a leading `./` marker. Matches negative and
# zero line numbers too -- they are invalid citations, but the checker needs to
# see them in order to report that, rather than silently failing to extract
# them. Deliberately does not match a bare `:1591` back-reference to a file
# named earlier in the same sentence -- there is no filename to resolve there.
CITATION_RE = re.compile(
    r"(?P<file>(?:\./)?[A-Za-z0-9_][A-Za-z0-9_./-]*\.[A-Za-z0-9]+):"
    r"(?P<start>-?\d+)(?:-(?P<end>-?\d+))?"
)

# (doc, citation, token): claims worth pinning to specific upstream text -- a
# bounds-valid citation can still point at unrelated code after a vendor bump,
# so these also require the cited excerpt to retain a meaningful token, and
# require the citation to actually appear (in normalized form) in the doc that
# is named.
ANCHORS = [
    ("ARCHITECTURE.md", "vendor/frankenphp/frankenphp.c:1471-1619", "php_thread"),
    ("ARCHITECTURE.md", "vendor/frankenphp/phpmainthread.go:248-257", "state.Rebooting"),
    (
        "ARCHITECTURE.md",
        "vendor/frankenphp/internal/state/state.go:13-37",
        "YieldingForReboot",
    ),
    (
        "ARCHITECTURE.md",
        "vendor/frankenphp/frankenphp_arginfo.h:58",
        "fastcgi_finish_request",
    ),
    (
        "ARCHITECTURE.md",
        "vendor/frankenphp/context.go:135-147",
        "sends the response to the client",
    ),
    (
        "ARCHITECTURE.md",
        "vendor/frankenphp/testdata/finish-request.php:5-15",
        "frankenphp_finish_request",
    ),
    (
        "ARCHITECTURE.md",
        "vendor/frankenphp/threadworker.go:298-327",
        "go_frankenphp_finish_worker_request",
    ),
    ("ARCHITECTURE.md", "vendor/frankenphp/options.go:170", "0 = unlimited"),
    ("PORTING-NOTES.md", "vendor/frankenphp/frankenphp.go:430", "go_ub_write"),
    # Was attributed to PORTING-NOTES.md; that citation actually lives in
    # ARCHITECTURE.md (full form at :166, bare `threadregular.go:91-116` at
    # :270). Re-attributed rather than deleted -- see issue #20.
    ("ARCHITECTURE.md", "vendor/frankenphp/threadregular.go:91-116", "select {"),
]

# The gate must not be quietly narrowed by deleting anchors until it is green.
MIN_ANCHORS = 10


class Citation:
    """One citation plus the documentation location that contains it."""

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


def read_lines(path):
    """Split a file the way an editor numbers it.

    ``read_text`` already normalises CRLF and CR, so splitting on ``\\n`` alone
    gives editor line numbers. ``str.splitlines`` would additionally break on
    form feed and friends, which would offset an anchor's excerpt from the
    range the bounds check validated -- one helper for both uses keeps them in
    agreement.
    """
    lines = path.read_text(errors="replace").split("\n")
    if lines and lines[-1] == "":
        lines.pop()
    return lines


def unsafe_path_reason(file_str):
    """Reject citation paths that could reach outside the tree, before any I/O."""
    bare = file_str[2:] if file_str.startswith("./") else file_str
    if ".." in PurePosixPath(bare).parts:
        return f"'{file_str}' contains a '..' path component; citations must not traverse upwards"
    return None


def candidate_paths(file_str, repo_root, vendor_root):
    """Every file <file_str> could name, per the resolution rule. No precedence:
    every candidate the rule allows is returned, so ambiguity can be detected
    rather than silently resolved by whichever one was found first.
    """
    if file_str.startswith("./"):
        # Explicitly repository-relative: the sole candidate, and vendor
        # lookups are not attempted. Not precedence -- a prefix the author
        # wrote, with exactly one meaning.
        candidate = repo_root / file_str[2:]
        return [candidate] if candidate.is_file() else []

    found = []
    seen = set()

    def offer(path):
        if path.is_file():
            key = path.resolve()
            if key not in seen:
                seen.add(key)
                found.append(path)

    offer(repo_root / file_str)
    offer(vendor_root / file_str)
    if "/" not in file_str:
        for match in sorted(vendor_root.rglob(file_str)):
            offer(match)
    return found


def resolve_file(file_str, repo_root, vendor_root):
    """Resolve a citation's file part. Returns (Path, None) or (None, reason)."""
    reason = unsafe_path_reason(file_str)
    if reason is not None:
        return None, reason

    candidates = candidate_paths(file_str, repo_root, vendor_root)

    if not candidates:
        return None, (
            f"no file found for '{file_str}' (checked repo root, "
            f"vendor/frankenphp/, and, for a bare basename, "
            f"vendor/frankenphp/**/{file_str})"
        )
    if len(candidates) > 1:
        rels = sorted(str(path.relative_to(repo_root)) for path in candidates)
        return None, (
            f"'{file_str}' is ambiguous: matches {rels}; cite the full path "
            f"(or prefix './' for a repo-relative file)"
        )

    resolved = candidates[0]
    repo_real = repo_root.resolve()
    if not resolved.resolve().is_relative_to(repo_real):
        return None, (
            f"'{file_str}' resolves to {resolved.resolve()}, which escapes the "
            f"repository root {repo_real}"
        )
    return resolved, None


def extract_citations(doc_path):
    """Extract citations and their source line numbers from one doc."""
    citations = []
    for lineno, line in enumerate(read_lines(doc_path), start=1):
        for match in CITATION_RE.finditer(line):
            end = int(match.group("end")) if match.group("end") else None
            citations.append(
                Citation(
                    match.group("file"),
                    int(match.group("start")),
                    end,
                    doc_path.name,
                    lineno,
                )
            )
    return citations


def check_bounds(citation, repo_root, vendor_root, errors):
    """Validate one citation and return its resolved path when valid."""
    resolved, reason = resolve_file(citation.file, repo_root, vendor_root)
    if resolved is None:
        errors.append(f"{citation.doc}:{citation.doc_line}: citation '{citation}' -- {reason}")
        return None

    rel = resolved.relative_to(repo_root)
    if citation.start <= 0:
        errors.append(
            f"{citation.doc}:{citation.doc_line}: citation '{citation}' -- "
            f"start line {citation.start} must be positive for {rel}"
        )
        return None
    if citation.end <= 0:
        errors.append(
            f"{citation.doc}:{citation.doc_line}: citation '{citation}' -- "
            f"end line {citation.end} must be positive for {rel}"
        )
        return None
    if citation.end < citation.start:
        errors.append(
            f"{citation.doc}:{citation.doc_line}: citation '{citation}' -- "
            f"range {citation.start}-{citation.end} is reversed for {rel}"
        )
        return None

    n_lines = len(read_lines(resolved))
    if citation.start > n_lines:
        errors.append(
            f"{citation.doc}:{citation.doc_line}: citation '{citation}' -- "
            f"start line {citation.start} out of bounds for {rel} ({n_lines} lines)"
        )
        return None
    if citation.end > n_lines:
        errors.append(
            f"{citation.doc}:{citation.doc_line}: citation '{citation}' -- "
            f"end line {citation.end} out of bounds for {rel} ({n_lines} lines)"
        )
        return None
    return resolved


def check_citations(repo_root, vendor_root, doc_paths, anchors):
    """Check every citation in ``doc_paths`` and every entry in ``anchors``
    against ``repo_root``/``vendor_root``. Returns ``(citations_checked,
    errors)``. Parameterized so the self-test can exercise the real code path
    against throwaway fixture trees instead of the real repo.
    """
    repo_root = Path(repo_root)
    vendor_root = Path(vendor_root)
    doc_paths = sorted((Path(p) for p in doc_paths), key=str)
    anchors = list(anchors)
    errors = []
    checked = 0

    # ANCHORS keys on a doc basename, so two checked docs sharing one basename
    # would make an anchor's "which doc" ambiguous rather than wrong.
    names = [doc_path.name for doc_path in doc_paths]
    duplicated = sorted({name for name in names if names.count(name) > 1})
    if duplicated:
        errors.append(
            f"doc set contains more than one file named {duplicated}; anchors "
            f"identify docs by basename and could not tell them apart"
        )

    normalized_by_doc = {doc_path.name: set() for doc_path in doc_paths}
    for doc_path in doc_paths:
        for citation in extract_citations(doc_path):
            checked += 1
            resolved = check_bounds(citation, repo_root, vendor_root, errors)
            if resolved is not None:
                normalized_by_doc[doc_path.name].add(
                    (resolved.relative_to(repo_root), citation.start, citation.end)
                )

    if checked == 0:
        errors.append(f"no citations found across {len(doc_paths)} doc(s)")

    for doc, citation_str, token in anchors:
        match = CITATION_RE.fullmatch(citation_str)
        if not match:
            errors.append(f"ANCHORS entry for {doc} has an unparseable citation: {citation_str!r}")
            continue

        start = int(match.group("start"))
        end = int(match.group("end")) if match.group("end") else start
        citation = Citation(match.group("file"), start, end, doc, "ANCHORS")
        resolved = check_bounds(citation, repo_root, vendor_root, errors)
        if resolved is None:
            continue

        # Compare normalized (path, start, end), not raw text: docs cite the
        # same file both in full and as a bare basename (house style), and an
        # anchor that only matched verbatim would pin nothing while looking
        # like it did.
        normalized = (resolved.relative_to(repo_root), start, end)
        if normalized not in normalized_by_doc.get(doc, set()):
            errors.append(
                f"ANCHORS: {doc} pins {citation_str}, but that normalized "
                f"citation is absent from the named doc"
            )

        excerpt = "\n".join(read_lines(resolved)[start - 1 : end])
        if token not in excerpt:
            rel = resolved.relative_to(repo_root)
            errors.append(
                f"ANCHORS: {doc} pins {citation_str} to contain {token!r}, but "
                f"{rel}:{start}-{end} no longer does (vendor bump or an edit "
                "invalidated this claim)"
            )

    return checked, errors


SELFTEST_OK = True


def st(desc, ok):
    """Report one TAP-like self-test assertion."""
    global SELFTEST_OK
    if ok:
        print(f"ok - {desc}")
    else:
        print(f"FAIL - {desc}")
        SELFTEST_OK = False


def write_lines(path, count, overrides=None):
    """Write a numbered fixture file, optionally replacing selected lines."""
    path.parent.mkdir(parents=True, exist_ok=True)
    lines = [f"line {number}" for number in range(1, count + 1)]
    for number, value in (overrides or {}).items():
        lines[number - 1] = value
    path.write_text("\n".join(lines) + "\n")


def make_fixture(tmp, doc_text):
    """Create a minimal repository tree and return its check parameters."""
    repo_root = Path(tmp) / "repo"
    vendor_root = repo_root / "vendor"
    doc_path = repo_root / "docs" / "DOC.md"
    doc_path.parent.mkdir(parents=True, exist_ok=True)
    doc_path.write_text(doc_text)
    vendor_root.mkdir()
    return repo_root, vendor_root, doc_path


def expect_one_error(desc, errors, reason):
    """Require exactly one failure, and that it is for the intended reason --
    a fixture that trips two checks at once must not pass for the wrong one.
    """
    st(desc, len(errors) == 1 and reason in errors[0])


def run_selftest():
    """Exercise every failure mode against real files in temporary trees."""
    global SELFTEST_OK
    SELFTEST_OK = True

    with tempfile.TemporaryDirectory() as tmp:
        repo, vendor, doc = make_fixture(tmp, "`missing.go:1`\n")
        _, errors = check_citations(repo, vendor, [doc], [])
        expect_one_error("missing file reports resolution failure", errors, "no file found")

    with tempfile.TemporaryDirectory() as tmp:
        repo, vendor, doc = make_fixture(tmp, "`foo.go:4`\n")
        write_lines(vendor / "foo.go", 3)
        _, errors = check_citations(repo, vendor, [doc], [])
        expect_one_error("start line past EOF is rejected", errors, "start line 4 out of bounds")

    with tempfile.TemporaryDirectory() as tmp:
        repo, vendor, doc = make_fixture(tmp, "`foo.go:2-4`\n")
        write_lines(vendor / "foo.go", 3)
        _, errors = check_citations(repo, vendor, [doc], [])
        expect_one_error("end line past EOF is rejected", errors, "end line 4 out of bounds")

    with tempfile.TemporaryDirectory() as tmp:
        repo, vendor, doc = make_fixture(tmp, "`foo.go:100-50`\n")
        write_lines(vendor / "foo.go", 100)
        _, errors = check_citations(repo, vendor, [doc], [])
        expect_one_error("reversed range is rejected", errors, "range 100-50 is reversed")

    for citation, reason, desc in (
        ("foo.go:0", "start line 0 must be positive", "zero start line is rejected"),
        ("foo.go:-1", "start line -1 must be positive", "negative start line is rejected"),
        ("foo.go:1-0", "end line 0 must be positive", "zero end line is rejected"),
        ("foo.go:1--1", "end line -1 must be positive", "negative end line is rejected"),
    ):
        with tempfile.TemporaryDirectory() as tmp:
            repo, vendor, doc = make_fixture(tmp, f"`{citation}`\n")
            write_lines(vendor / "foo.go", 2)
            _, errors = check_citations(repo, vendor, [doc], [])
            expect_one_error(desc, errors, reason)

    # Three shapes of ambiguity, one per resolution strategy that could
    # shadow another -- these are exactly the shapes that occurred in this
    # repository (deep-vs-deep, vendor-root-vs-deep is the `mercure.go`
    # shape, repo-root-vs-vendor is the `README.md` shape).
    with tempfile.TemporaryDirectory() as tmp:
        repo, vendor, doc = make_fixture(tmp, "`foo.go:1`\n")
        write_lines(vendor / "a" / "foo.go", 1)
        write_lines(vendor / "b" / "foo.go", 1)
        _, errors = check_citations(repo, vendor, [doc], [])
        expect_one_error("basename matching two vendor subdirs is rejected", errors, "is ambiguous")

    with tempfile.TemporaryDirectory() as tmp:
        repo, vendor, doc = make_fixture(tmp, "`foo.go:1`\n")
        write_lines(vendor / "foo.go", 1)
        write_lines(vendor / "sub" / "foo.go", 1)
        _, errors = check_citations(repo, vendor, [doc], [])
        expect_one_error(
            "vendor-root basename shadowing a subdir copy is rejected", errors, "is ambiguous"
        )

    with tempfile.TemporaryDirectory() as tmp:
        repo, vendor, doc = make_fixture(tmp, "`README.md:50`\n")
        write_lines(repo / "README.md", 100)
        write_lines(vendor / "README.md", 3)
        _, errors = check_citations(repo, vendor, [doc], [])
        expect_one_error("repo-root file shadowing a vendor file is rejected", errors, "is ambiguous")

    # `./` is a repo-relative marker, not precedence: with the same shadowing
    # fixture, the qualified form must resolve (and only to the repo-root
    # file) while the bare form must still fail as ambiguous.
    with tempfile.TemporaryDirectory() as tmp:
        repo, vendor, doc = make_fixture(tmp, "`./foo.go:1`\n")
        write_lines(repo / "foo.go", 5)
        write_lines(vendor / "foo.go", 2)
        checked, errors = check_citations(repo, vendor, [doc], [])
        st("'./' prefix resolves to the repo-root file and is not ambiguous", checked == 1 and not errors)

    with tempfile.TemporaryDirectory() as tmp:
        repo, vendor, doc = make_fixture(tmp, "`foo.go:1`\n")
        write_lines(repo / "foo.go", 5)
        write_lines(vendor / "foo.go", 2)
        _, errors = check_citations(repo, vendor, [doc], [])
        expect_one_error("same fixture cited bare (no './') is still ambiguous", errors, "is ambiguous")

    with tempfile.TemporaryDirectory() as tmp:
        repo, vendor, doc = make_fixture(tmp, "`x/../../outside.go:1`\n")
        (repo / "x").mkdir()
        write_lines(Path(tmp) / "outside.go", 5)
        _, errors = check_citations(repo, vendor, [doc], [])
        expect_one_error(
            "'..' traversal is rejected before touching disk",
            errors,
            "contains a '..' path component",
        )

    with tempfile.TemporaryDirectory() as tmp:
        repo, vendor, doc = make_fixture(tmp, "`escape.go:1`\n")
        write_lines(Path(tmp) / "outside.go", 5)
        (repo / "escape.go").symlink_to(Path(tmp) / "outside.go")
        _, errors = check_citations(repo, vendor, [doc], [])
        expect_one_error(
            "symlink out of the repository is rejected",
            errors,
            "escapes the repository root",
        )

    with tempfile.TemporaryDirectory() as tmp:
        repo, vendor, doc = make_fixture(tmp, "`bar.go:1`\n")
        write_lines(vendor / "bar.go", 1)
        write_lines(vendor / "x" / "foo.go", 10, {10: "needle"})
        anchors = [("DOC.md", "vendor/x/foo.go:10", "needle")]
        _, errors = check_citations(repo, vendor, [doc], anchors)
        expect_one_error(
            "anchor absent from its named doc is rejected",
            errors,
            "normalized citation is absent from the named doc",
        )

    with tempfile.TemporaryDirectory() as tmp:
        repo, vendor, doc = make_fixture(tmp, "`foo.go:10`\n")
        write_lines(vendor / "x" / "foo.go", 10, {10: "needle"})
        anchors = [("DOC.md", "vendor/x/foo.go:10", "needle")]
        checked, errors = check_citations(repo, vendor, [doc], anchors)
        st("bare-form doc citation satisfies a normalized full-form anchor", checked == 1 and not errors)

    with tempfile.TemporaryDirectory() as tmp:
        repo, vendor, doc = make_fixture(tmp, "`foo.go:10`\n")
        write_lines(vendor / "x" / "foo.go", 10, {3: "needle"})
        anchors = [("DOC.md", "vendor/x/foo.go:10", "needle")]
        _, errors = check_citations(repo, vendor, [doc], anchors)
        expect_one_error(
            "anchor whose token left the cited range is rejected",
            errors,
            "no longer does",
        )

    with tempfile.TemporaryDirectory() as tmp:
        repo, vendor, doc = make_fixture(tmp, "there are no citations here\n")
        checked, errors = check_citations(repo, vendor, [doc], [])
        st(
            "zero citations is a specific failure",
            checked == 0 and len(errors) == 1 and "no citations found" in errors[0],
        )

    with tempfile.TemporaryDirectory() as tmp:
        repo, vendor, doc = make_fixture(tmp, "`foo.go:1`\n")
        write_lines(vendor / "foo.go", 1)
        other = repo / "elsewhere" / "DOC.md"
        other.parent.mkdir(parents=True)
        other.write_text("`foo.go:1`\n")
        _, errors = check_citations(repo, vendor, [doc, other], [])
        expect_one_error(
            "two docs with the same basename are rejected",
            errors,
            "could not tell them apart",
        )

    print()
    if SELFTEST_OK:
        print("SELFTEST PASS")
    else:
        print("SELFTEST FAIL")
    return 0 if SELFTEST_OK else 1


def main():
    if len(ANCHORS) < MIN_ANCHORS:
        print(
            f"ANCHORS holds {len(ANCHORS)} entries, below the {MIN_ANCHORS} this "
            f"check is required to keep; do not shrink coverage to go green",
            file=sys.stderr,
        )
        return 1

    doc_paths = sorted((REPO_ROOT / "docs").glob("*.md"))
    checked, errors = check_citations(REPO_ROOT, VENDOR_ROOT, doc_paths, ANCHORS)

    if errors:
        print(f"checked {checked} citations across {len(doc_paths)} doc(s); {len(errors)} problem(s):\n")
        for error in errors:
            print(f"  - {error}")
        return 1

    print(
        f"checked {checked} citations across {len(doc_paths)} doc(s) and "
        f"{len(ANCHORS)} anchor(s): all resolve"
    )
    return 0


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--selftest":
        sys.exit(run_selftest())
    sys.exit(main())
