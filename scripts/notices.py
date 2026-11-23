#!/usr/bin/env python3
"""Builds THIRD-PARTY-NOTICES.txt; scripts/generate-notices.sh collects its inputs.

usage: notices.py <repo> <linked-ids> <cargo-metadata.json> <output>

Every linked crate must pass, or nothing is written:

- its license expression, with Cargo's legacy `/` read as OR, is satisfied by terms in deny.toml's
  allow list that also have a non-empty text in the crate's license files;
- each license-like file below the crate root is listed in packaging/notices/embedded.tsv for that
  exact name, version and Cargo.lock checksum, with a reason and an action: `license` (the crate's
  own license text), `include` (a notice for code bundled in it) or `exclude` (not shipped);
- a crate with no license file at its root, or whose root files lack a declared license's text,
  has entries in packaging/notices/overrides.tsv for that exact name, version and checksum, and
  each vendored text matches its recorded SHA-256.
"""
import hashlib
import json
import os
import re
import sys
import tomllib
from collections import defaultdict

ROOT_FILE = re.compile(r"^(licen[cs]e|copying|notice|copyright|unlicense|authors-license)[^/]*$", re.I)
LICENSES_DIR = re.compile(r"^licen[cs]es?/[^/]+$", re.I)
LICENSE_DIR = re.compile(r"(^|/)\.?licen[cs]es?/", re.I)
NOT_A_TEXT = re.compile(r"\.(rs|toml|json|lock|html|png|svg)$", re.I)
NOTICE_LIKE = re.compile(r"licen[cs]e|copying|notice|copyright|third.?party|patents", re.I)
SOURCE_FILE = re.compile(r"\.(rs|toml|json|lock|html|png|svg|c|h|cc|cpp|hpp|py|js|ts|sh|yml|yaml|xml|s|asm|pl)$", re.I)

MIT = "permission is hereby granted, free of charge"
APACHE = ("apache license", "terms and conditions for use, reproduction")
MIT_KEEP = "above copyright notice and this permission notice shall be included"
ISC_KEEP = "above copyright notice and this permission notice appear in all copies"
BSD = "redistribution and use in source and binary forms"
# The phrases a text must contain to count as that license. Every term deny.toml allows needs one.
TERM_TEXT = {
    "Apache-2.0": APACHE,
    "Apache-2.0 WITH LLVM-exception": (*APACHE, "llvm exceptions"),
    "MIT": (MIT, MIT_KEEP),
    "MIT-0": (MIT,),
    "BSD-2-Clause": (BSD,),
    "BSD-3-Clause": (BSD, "contributors may be used"),
    "0BSD": ("permission to use, copy, modify, and/or distribute",),
    "ISC": ("permission to use, copy, modify, and", ISC_KEEP),
    "Zlib": ("altered source versions must be plainly marked",),
    "Unicode-3.0": ("unicode", MIT),
    "Unlicense": ("free and unencumbered software released into the public domain",),
    "CC0-1.0": ("creative commons", "cc0"),
    "BSL-1.0": ("boost software license",),
    "NCSA": ("university of illinois", MIT),
    "CDLA-Permissive-2.0": ("community data license agreement",),
    "MPL-2.0": ("mozilla public license", "version 2.0"),
}


def tokens(expression):
    return re.findall(r"[()]|[^\s()/]+|/", expression)


def parse_expression(expression):
    """Returns a nested ("or"|"and", [...]) tree with string leaves; raises ValueError."""
    items = ["OR" if t == "/" else t for t in tokens(expression)]
    at = 0

    def peek():
        return items[at] if at < len(items) else None

    def take():
        nonlocal at
        at += 1
        return items[at - 1]

    def term():
        if peek() == "(":
            take()
            node = either()
            if peek() != ")":
                raise ValueError("unbalanced parentheses")
            take()
            return node
        name = peek()
        if name is None or name in (")", "AND", "OR", "WITH"):
            raise ValueError(f"expected a license at {name or 'end'}")
        take()
        if peek() == "WITH":
            take()
            if peek() is None or peek() in ("(", ")", "AND", "OR", "WITH"):
                raise ValueError("expected an exception after WITH")
            name = f"{name} WITH {take()}"
        return name

    def both():
        parts = [term()]
        while peek() == "AND":
            take()
            parts.append(term())
        return parts[0] if len(parts) == 1 else ("and", parts)

    def either():
        parts = [both()]
        while peek() == "OR":
            take()
            parts.append(both())
        return parts[0] if len(parts) == 1 else ("or", parts)

    node = either()
    if at != len(items):
        raise ValueError(f"unexpected {items[at]}")
    return node


def show(node, nested=False):
    if isinstance(node, str):
        return node
    text = f" {node[0].upper()} ".join(show(part, True) for part in node[1])
    return f"({text})" if nested else text


def satisfied(node, usable):
    if isinstance(node, str):
        return node in usable
    results = (satisfied(part, usable) for part in node[1])
    return any(results) if node[0] == "or" else all(results)


def normalized(data):
    # Some crates ship their license as a comment block.
    text = re.sub(r"(?m)^\s*(//+|#+|\*+)[ \t]?", "", data.decode("utf-8", "replace"))
    return " ".join(text.lower().split())


def terms_in(text, allowed):
    found = {term for term in allowed if all(phrase in text for phrase in TERM_TEXT[term])}
    # These two are the MIT and ISC grants without the condition to keep the notice.
    if MIT_KEEP in text:
        found.discard("MIT-0")
    if ISC_KEEP in text:
        found.discard("0BSD")
    return found


def read_table(path, columns):
    rows = []
    with open(path, encoding="utf-8") as table:
        for number, line in enumerate(table, 1):
            line = line.rstrip("\n")
            if not line or line.startswith("#"):
                continue
            row = line.split("\t")
            if len(row) != len(columns):
                raise SystemExit(f"error: {path}:{number}: expected {len(columns)} columns")
            rows.append(dict(zip(columns, row)))
    return rows


def scan(directory):
    """Returns (root license files, license-like files elsewhere), as relative paths."""
    root, embedded = [], []
    for base, _, names in os.walk(directory, followlinks=True):
        for name in names:
            path = os.path.join(base, name)
            if not os.path.isfile(path):
                continue
            rel = os.path.relpath(path, directory).replace(os.sep, "/")
            if (ROOT_FILE.match(rel) or LICENSES_DIR.match(rel)) and not NOT_A_TEXT.search(rel):
                root.append(rel)
            elif LICENSE_DIR.search(rel) or (NOTICE_LIKE.search(name) and not SOURCE_FILE.search(name)):
                embedded.append(rel)
    return sorted(root), sorted(embedded)


def read(path):
    with open(path, "rb") as file:
        return file.read()


def main(repo, linked_path, metadata_path, output):
    with open(os.path.join(repo, "deny.toml"), "rb") as file:
        allowed = set(tomllib.load(file)["licenses"]["allow"])
    if allowed - set(TERM_TEXT):
        raise SystemExit(f"error: no text check for {sorted(allowed - set(TERM_TEXT))}")
    with open(os.path.join(repo, "Cargo.lock"), "rb") as file:
        lock = {(p["name"], p["version"]): p.get("checksum", p.get("source", ""))
                for p in tomllib.load(file)["package"]}
    with open(metadata_path, encoding="utf-8") as file:
        packages = {f"{p['name']}@{p['version']}": p for p in json.load(file)["packages"]}

    notices = os.path.join(repo, "packaging", "notices")
    overrides = defaultdict(list)
    for row in read_table(os.path.join(notices, "overrides.tsv"),
                          ("name", "version", "checksum", "file", "sha256", "provenance", "source")):
        overrides[row["name"], row["version"], row["checksum"]].append(row)
    audited = defaultdict(dict)
    for row in read_table(os.path.join(notices, "embedded.tsv"),
                          ("name", "version", "checksum", "path", "action", "reason")):
        if row["action"] not in ("license", "include", "exclude"):
            raise SystemExit(f"error: embedded.tsv: bad action for {row['name']}: {row['action']}")
        audited[row["name"], row["version"], row["checksum"]][row["path"]] = row["action"]

    problems, crates = [], []
    texts, users = {}, defaultdict(set)
    linked = sorted({line.strip() for line in open(linked_path, encoding="utf-8") if line.strip()})
    used = set()
    for crate in linked:
        package = packages.get(crate)
        if package is None:
            problems.append(f"{crate} is not in cargo metadata")
            continue
        if package["source"] is None:
            continue
        name, version = package["name"], package["version"]
        checksum = lock.get((name, version))
        if not checksum:
            problems.append(f"{crate} is not in Cargo.lock")
            continue
        key = (name, version, checksum)
        declared = package.get("license")
        if not declared:
            problems.append(f"{crate} declares no license")
            continue
        try:
            expression = parse_expression(declared)
        except ValueError as error:
            problems.append(f"{crate} has an invalid license expression {declared!r}: {error}")
            continue

        directory = os.path.dirname(package["manifest_path"])
        root, embedded = scan(directory)
        files = []  # (label, bytes)
        for rel in root:
            files.append((rel, read(os.path.join(directory, rel))))
        if not root and key not in overrides:
            problems.append(f"{crate} ({declared}) ships no license file and has no override "
                            f"for checksum {checksum}")
            continue
        if key in overrides:
            used.add(key)
        for row in overrides.get(key, ()):
            data = read(os.path.join(notices, "texts", row["sha256"]))
            if hashlib.sha256(data).hexdigest() != row["sha256"]:
                problems.append(f"{crate}: vendored {row['file']} does not match its SHA-256")
                continue
            label = row["file"] if row["provenance"] == "release" else f"{row['file']}, {row['provenance']}"
            files.append((label, data))

        entries = audited.get(key, {})
        if entries:
            used.add(key)
        for rel in embedded:
            if rel not in entries:
                problems.append(f"{crate}: unaudited license-like file {rel}")
        # Texts of bundled third-party code are carried but do not license the crate itself.
        own = list(files)
        for rel, action in sorted(entries.items()):
            if action == "exclude":
                continue
            path = os.path.join(directory, rel)
            if not os.path.isfile(path) or not read(path).strip():
                problems.append(f"{crate}: audited file {rel} is missing or empty")
                continue
            files.append((rel, read(path)))
            if action == "license":
                own.append((rel, read(path)))

        usable = set()
        for _, data in own:
            usable |= terms_in(normalized(data), allowed)
        if not satisfied(expression, usable):
            problems.append(f"{crate} ({show(expression)}): no allowed term with a non-empty text; "
                            f"texts found for {sorted(usable) or 'none'}")
            continue

        crates.append(f"{crate}: {show(expression)}")
        for label, data in files:
            if not data.strip():
                continue
            digest = hashlib.sha256(data).hexdigest()
            texts[digest] = data
            users[digest].add(f"{crate} ({label})")

    for key in sorted(set(overrides) | set(audited)):
        if key not in used:
            print(f"warning: unused notice entry for {key[0]}@{key[1]}", file=sys.stderr)

    if problems:
        print("error: cannot write complete third-party notices:", file=sys.stderr)
        for problem in sorted(problems):
            print(problem, file=sys.stderr)
        return 1

    out = bytearray()
    out += (
        "THIRD-PARTY NOTICES FOR FLINT\n\n"
        "Flint's release binaries link the Rust crates listed here. Each crate's license and notice\n"
        "files follow, copied from its source. A label after a comma says where a file came from\n"
        "when the crate's own release did not carry it. Identical texts appear once, under every\n"
        "crate and file that carries them. The Rust standard library's notices are in\n"
        "COPYRIGHT-library.html, next to this file.\n\nCRATES\n\n"
    ).encode()
    out += ("\n".join(sorted(crates)) + "\n").encode()
    for digest in sorted(texts, key=lambda d: sorted(users[d])[0]):
        out += b"\n" + b"=" * 80 + b"\n"
        out += ("\n".join(sorted(users[digest])) + "\n").encode()
        out += b"-" * 80 + b"\n"
        out += texts[digest]
        if not texts[digest].endswith(b"\n"):
            out += b"\n"
    temporary = f"{output}.tmp"
    with open(temporary, "wb") as file:
        file.write(out)
    os.replace(temporary, output)
    print(f"notices: {len(crates)} crates, {len(out)} bytes, {output}")
    return 0


if __name__ == "__main__":
    if len(sys.argv) != 5:
        raise SystemExit(__doc__)
    sys.exit(main(*sys.argv[1:]))
