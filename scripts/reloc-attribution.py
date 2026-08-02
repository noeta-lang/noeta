#!/usr/bin/env python3
"""Attribute a binary's dynamic relocations to the crate that contributed each patched word.

**Why this exists.** Process init is a real line item in `noeta --version`, and most of it is
`ld.so` applying `R_X86_64_RELATIVE` relocations — a measured ~12.0 instructions retired each, a
constant that has now reproduced to three digits across four independent changes. Deciding *what to
fix* needs to know which crate's static data owns those relocations, and that question has been
answered by hand twice. This is the instrument, so the third time takes three seconds.

**What a relocation costs, and what it doesn't.** The 12 instructions are the *application* — load
the addend, add the load base, store — not the reading of the relocation table. Packing the table
with `-Wl,-z,pack-relative-relocs` (DT_RELR) was measured on this binary: it erased the entire
1.2 MB `.rela.dyn` table and bought **0.23%**. So the only lever that moves this number is emitting
*fewer relocatable pointers into static data*, which in practice means replacing a
`&[(&str, &[T])]` table with one blob plus integer offsets (the shape `noeta-mcp`'s embedded corpus
was rewritten into, worth ~4,700 relocations on its own).

**Usage.** Link the binary with a map, then attribute it:

    CARGO_TARGET_DIR=<your-target> cargo +1.97.0 rustc --release -p noeta-cli --bin noeta -- \\
        -Clink-arg=-Wl,-Map=/tmp/noeta.map
    scripts/reloc-attribution.py /tmp/noeta.map <your-target>/release/noeta

`cargo rustc` applies the extra flag to the selected crate only, so the dependency graph is reused
from the existing target directory — this is a relink, not a rebuild.

Reads the modern 5-column GNU ld map form (`VMA LMA Size Align <object>:(<section>)`). Every output
fragment maps back to an input object, including the anonymous `.Lanon.*` const fragments that carry
most of the relocated pointers and have no symbol-table entry — which is why a `.symtab` bisect
cannot do this job (it leaves ~80% unattributed). A run that reports any `<unmapped>` means the map
and the binary do not correspond; relink and try again.

`<internal>` is the linker's own output (`.got` and friends), not a crate.
"""

import bisect
import re
import subprocess
import sys
from collections import Counter

USAGE = "usage: reloc-attribution.py <map-file> <binary> [top-n]"

# `  <vma>  <lma>  <size>  <align>  <object>:(<section>)`
ROW = re.compile(r"^\s+([0-9a-f]+)\s+([0-9a-f]+)\s+([0-9a-f]+)\s+(\d+)\s+(\S.*)$")
# rustc names an object `<bin>-<hash>.<crate>-<hash>.<crate>.<hash>-cgu.N.rcgu.o`.
CRATE_IN_OBJECT = re.compile(r"^(.*)-[0-9a-f]{8,}$")
ARCHIVE_SUFFIX = re.compile(r"\.(rlib|a|o)$")
HASH_SUFFIX = re.compile(r"-[0-9a-f]{8,}.*$")


def crate_of(obj: str) -> str:
    """The crate an input object belongs to.

    Handles rustc's `<bin>-<hash>.<crate>-<hash>.…-cgu.N.rcgu.o`, plain `libfoo-<hash>.rlib`, and
    C archives folded into an rlib (`libaws_lc_sys-<hash>.rlib(<hash>-obj.o)`), which are named for
    the rlib rather than the member so a C dependency lands under one heading.
    """
    if ".rlib(" in obj:
        obj = obj.split(".rlib(")[0] + ".rlib"
    base = obj.rsplit("/", 1)[-1]
    parts = base.split(".")
    if len(parts) >= 2:
        m = CRATE_IN_OBJECT.match(parts[1])
        if m:
            return m.group(1)
    if base.startswith("lib"):
        base = base[3:]
    return HASH_SUFFIX.sub("", ARCHIVE_SUFFIX.sub("", base))


def fragments(map_path: str) -> list[tuple[int, int, str, str]]:
    """Every sized output fragment as `(addr, size, object, section)`, sorted by address."""
    frags = []
    with open(map_path, errors="replace") as fh:
        for line in fh:
            m = ROW.match(line.rstrip("\n"))
            if not m:
                continue
            addr, size, origin = int(m.group(1), 16), int(m.group(3), 16), m.group(5)
            if size == 0 or ":(" not in origin:
                continue
            obj, section = origin.split(":(", 1)
            frags.append((addr, size, obj, section.rstrip(")")))
    frags.sort()
    return frags


def relative_relocations(binary: str) -> list[int]:
    """The offsets `ld.so` patches at load — the ones that cost ~12 instructions each."""
    out = subprocess.run(
        ["readelf", "-rW", binary], capture_output=True, text=True, check=True
    ).stdout
    return [
        int(line.split()[0], 16) for line in out.splitlines() if "R_X86_64_RELATIVE" in line
    ]


def main() -> int:
    if len(sys.argv) < 3:
        print(USAGE, file=sys.stderr)
        return 2
    map_path, binary = sys.argv[1], sys.argv[2]
    top_n = int(sys.argv[3]) if len(sys.argv) > 3 else 30

    frags = fragments(map_path)
    if not frags:
        print(f"{map_path}: no fragments parsed — is this a GNU ld -Map file?", file=sys.stderr)
        return 2
    addrs = [f[0] for f in frags]

    relocs: Counter[str] = Counter()
    unmapped = 0
    offsets = relative_relocations(binary)
    for off in offsets:
        i = bisect.bisect_right(addrs, off) - 1
        if i >= 0 and frags[i][0] + frags[i][1] > off:
            relocs[crate_of(frags[i][2])] += 1
        else:
            unmapped += 1
            relocs["<unmapped>"] += 1

    text_bytes: Counter[str] = Counter()
    all_bytes: Counter[str] = Counter()
    for _addr, size, obj, section in frags:
        crate = crate_of(obj)
        all_bytes[crate] += size
        if section.startswith(".text"):
            text_bytes[crate] += size

    total = len(offsets)
    print(f"{binary}")
    print(f"R_X86_64_RELATIVE: {total}  (~{total * 12.0 / 1000:.0f}k instructions at load)")
    if unmapped:
        print(f"WARNING: {unmapped} unmapped — the map does not match the binary; relink.")
    print()
    print(f"{'relocs':>8} {'pct':>7} {'~kinstr':>8} {'.text KiB':>10} {'all KiB':>9}  crate")
    for crate, n in relocs.most_common(top_n):
        print(
            f"{n:8d} {100.0 * n / total:6.2f}% {n * 12.0 / 1000:8.1f} "
            f"{text_bytes.get(crate, 0) / 1024:10.0f} {all_bytes.get(crate, 0) / 1024:9.0f}  {crate}"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
