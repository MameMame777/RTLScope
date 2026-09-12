"""Writes THIRD-PARTY-NOTICES.md from what `cargo metadata` says we depend on.

Every crate that ends up in a binary is listed under the licence that binds it,
and the text of each such licence is reproduced once. Where a crate offers a
choice (`MIT OR Apache-2.0`) this project takes the most permissive option, in
the order of PREFERENCE below, and says so; where it makes no choice (`X AND
Y`) every term is listed. The texts come from the crates themselves — a crate
under the registry that ships a `LICENSE-*` file — so that what is reproduced
is what was received, not a copy typed in here.

The fonts get their own section with their licence files verbatim. They are
the one dependency whose licence *requires* the notice to travel with the
binary (OFL-1.1, the Ubuntu Font Licence), and the notice they require names
the font, not the crate.

    python scripts/third_party_notices.py            # writes the file
    python scripts/third_party_notices.py --check    # exits 1 if it is stale

Nothing here is a substitute for reading a licence. It is bookkeeping, and it
is only as right as `cargo metadata` and the crates' own files.
"""

from __future__ import annotations

import io
import json
import re
import subprocess
import sys
from collections import defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "THIRD-PARTY-NOTICES.md"

# When a crate offers a choice, the option this project takes: first match wins.
PREFERENCE = [
    "MIT",
    "Apache-2.0",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "Zlib",
    "ISC",
    "MIT-0",
    "0BSD",
    "Unlicense",
    "CC0-1.0",
    "BSL-1.0",
    "Unicode-3.0",
    "OFL-1.1",
    "Ubuntu-font-1.0",
]

# The file names crates use for each licence's text, most specific first.
TEXT_FILES = {
    "MIT": ["LICENSE-MIT", "LICENSE-MIT.md", "LICENSE-MIT.txt", "LICENSE.MIT"],
    "Apache-2.0": ["LICENSE-APACHE", "LICENSE-APACHE.md", "LICENSE-APACHE.txt", "LICENSE.Apache-2.0"],
    "BSD-2-Clause": ["LICENSE-BSD-2-Clause", "LICENSE.BSD-2-Clause", "LICENSE-BSD"],
    "BSD-3-Clause": ["LICENSE-BSD-3-Clause", "LICENSE.BSD-3-Clause", "LICENSE-BSD"],
    "Zlib": ["LICENSE-ZLIB", "LICENSE-ZLIB.md", "LICENSE-Zlib"],
    "ISC": ["LICENSE-ISC", "LICENSE.ISC"],
    "MIT-0": ["LICENSE-MIT-0", "LICENSE-MIT0"],
    "0BSD": ["LICENSE-0BSD"],
    "Unlicense": ["UNLICENSE", "LICENSE-UNLICENSE"],
    "CC0-1.0": ["LICENSE-CC0", "LICENSE-CC0-1.0", "COPYING"],
    "BSL-1.0": ["LICENSE-BOOST", "LICENSE_1_0.txt", "LICENSE-BSL-1.0"],
    "Unicode-3.0": ["LICENSE"],
}

# A single LICENSE file names the crate's one licence; accept it when the
# crate declares exactly that licence, so a MIT text is not taken from a crate
# that meant Apache.
FALLBACK_FILES = ["LICENSE", "LICENSE.md", "LICENSE.txt", "LICENCE", "COPYING"]

# Licences whose text lives with the font it covers, under "Fonts".
IN_FONTS = {"OFL-1.1", "Ubuntu-font-1.0"}

# Fonts: the crate, the file, and what it licenses.
FONTS = [
    ("epaint_default_fonts", "fonts/UFL.txt", "Ubuntu Font Licence 1.0", "Ubuntu-Light.ttf (Ubuntu Light)"),
    ("epaint_default_fonts", "fonts/OFL.txt", "SIL Open Font License 1.1", "NotoEmoji-Regular.ttf (Noto Emoji)"),
    ("epaint_default_fonts", "fonts/Hack-Regular.txt", "MIT and Bitstream Vera", "Hack-Regular.ttf (Hack)"),
    ("epaint_default_fonts", "fonts/emoji-icon-font-mit-license.txt", "MIT", "emoji-icon-font.ttf"),
]


def metadata() -> dict:
    out = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--all-features"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
        encoding="utf-8",
    ).stdout
    return json.loads(out)


def binding_terms(expr: str) -> list[str]:
    """Every licence that binds, given an SPDX-ish expression.

    `A OR B` is a choice and PREFERENCE makes it; `A AND B` binds both. The
    older `A/B` spelling is a choice too.
    """
    expr = expr.replace("(", " ").replace(")", " ")
    terms = []
    for conj in re.split(r"\s+AND\s+", expr):
        options = [o.strip() for o in re.split(r"\s+OR\s+|/", conj) if o.strip()]
        if not options:
            continue
        pick = next((p for p in PREFERENCE if p in options), None)
        if pick is None:
            # Something not in the table: keep it and let the reader see it.
            pick = options[0]
        terms.append(pick)
    return terms


def read(path: Path) -> str:
    return io.open(path, encoding="utf-8", errors="replace").read().rstrip() + "\n"


def licence_text(licence: str, packages: list[dict]) -> tuple[str, str] | None:
    """The text of a licence, from the first crate under it that ships one."""
    names = TEXT_FILES.get(licence, [])
    for pkg in packages:
        crate_dir = Path(pkg["manifest_path"]).parent
        for name in names:
            candidate = crate_dir / name
            if candidate.is_file():
                return read(candidate), f"{pkg['name']} {pkg['version']}"
    # A crate declaring exactly this licence and shipping one LICENSE file.
    for pkg in packages:
        if (pkg.get("license") or "").strip() != licence:
            continue
        crate_dir = Path(pkg["manifest_path"]).parent
        for name in FALLBACK_FILES:
            candidate = crate_dir / name
            if candidate.is_file():
                return read(candidate), f"{pkg['name']} {pkg['version']}"
    return None


def build() -> str:
    meta = metadata()
    workspace = set(meta.get("workspace_members", []))
    packages = [p for p in meta["packages"] if p["id"] not in workspace]
    packages.sort(key=lambda p: (p["name"], p["version"]))

    by_licence: dict[str, list[dict]] = defaultdict(list)
    undeclared = []
    for pkg in packages:
        expr = (pkg.get("license") or "").strip()
        if not expr:
            undeclared.append(pkg)
            continue
        for term in binding_terms(expr):
            by_licence[term].append(pkg)

    lines: list[str] = []
    w = lines.append
    w("# Third-party notices")
    w("")
    w("RTLScope is built from the Rust crates below. Each is listed under the licence")
    w("that binds this project's use of it, and the text of each such licence follows")
    w("its list. Where a crate offers a choice of licences, this project takes the most")
    w("permissive; where it requires several, every one is listed. The copyright holder")
    w("of each crate is named in that crate's own package, which its repository link")
    w("leads to.")
    w("")
    w("The fonts compiled into `rtlscope-gui` are listed last, with their licence files")
    w("reproduced in full, because those licences ask for exactly that.")
    w("")
    w("This file is written by `scripts/third_party_notices.py` from `cargo metadata`")
    w("and the crates' own licence files. Edit the script, not the file.")
    w("")
    w(f"{len(packages)} crates, {len(by_licence)} licences.")
    w("")

    order = sorted(by_licence, key=lambda k: (-len(by_licence[k]), k))
    w("## Contents")
    w("")
    for licence in order:
        w(f"- [{licence}](#{anchor(licence)}) — {len(by_licence[licence])} crate(s)")
    w("- [Fonts](#fonts)")
    w("")

    for licence in order:
        crates = by_licence[licence]
        w(f"## {licence}")
        w("")
        for pkg in crates:
            repo = pkg.get("repository") or ""
            link = f" — <{repo}>" if repo else ""
            w(f"- {pkg['name']} {pkg['version']}{link}")
        w("")
        found = licence_text(licence, crates)
        if licence in IN_FONTS:
            w("Reproduced in full under [Fonts](#fonts), against the font it covers.")
        elif found is None:
            w(f"_No crate under {licence} ships its text in the package; see the crates'")
            w("repositories._")
        else:
            text, source = found
            w(f"Text as shipped by {source}:")
            w("")
            w("```text")
            w(text.rstrip())
            w("```")
        w("")

    if undeclared:
        w("## No licence declared")
        w("")
        for pkg in undeclared:
            w(f"- {pkg['name']} {pkg['version']}")
        w("")

    w("## Fonts")
    w("")
    w("`rtlscope-gui` compiles egui's default fonts into the binary through")
    w("`epaint_default_fonts`. Their licences are reproduced here in full.")
    w("")
    by_name = {p["name"]: p for p in packages}
    for crate, rel, title, what in FONTS:
        pkg = by_name.get(crate)
        if pkg is None:
            w(f"_{crate} is not in this build._")
            continue
        path = Path(pkg["manifest_path"]).parent / rel
        w(f"### {what} — {title}")
        w("")
        if path.is_file():
            w("```text")
            w(read(path).rstrip())
            w("```")
        else:
            w(f"_{rel} was not found in {crate} {pkg['version']}._")
        w("")

    return "\n".join(lines).rstrip() + "\n"


def anchor(text: str) -> str:
    return re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-")


def main() -> int:
    content = build()
    if "--check" in sys.argv:
        current = io.open(OUT, encoding="utf-8").read() if OUT.is_file() else ""
        if current != content:
            print(f"{OUT.name} is stale; run scripts/third_party_notices.py", file=sys.stderr)
            return 1
        print(f"{OUT.name} is current")
        return 0
    io.open(OUT, "w", encoding="utf-8", newline="\n").write(content)
    print(f"wrote {OUT.relative_to(ROOT)} ({len(content.splitlines())} lines)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
