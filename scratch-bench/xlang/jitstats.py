#!/usr/bin/env python3
"""Read `noeta run --jit-stats` — one parser, two consumers (`xrun3.py` and `measure.sh`).

The point of this module is that **an empty answer is never silent**. What it replaces was a grep
for a fixed list of op names; the list stopped matching what the runner emits, so `measure.sh`'s
`[tier-1 decline]` section printed an empty column for every benchmark and no reader could tell
"nothing declined" from "the grep missed". An instrument that cannot fail visibly reports a pass
over what it never examined, which is worse than no instrument because it gets counted.

So the parse returns one of five **states**, never a possibly-empty list, and two of them are loud:

    DECLINED   tier 1 declined a loop; `ops` names the blocking ops
    NONE       the report is there and has no decline section — nothing declined, stated
    NO_JIT     the binary says it was built without the JIT, so tier 1 cannot have run
    NO_REPORT  no report and no such disclaimer: the binary printed something else  (loud)
    UNPARSED   the report is there but its shape no longer matches this parser      (loud)

`UNPARSED` is the state the old grep could not express. Every format detail this parser depends on
is a named constant below, checked against what `noeta_runner::render_jit_report` writes:

    ── JIT report ──
    tier 1: 1 of 1 compiled prototypes native (0 bail stubs), 2 OSR loop windows, compile time 0.4 ms
    ...
    loops declined tier 1 (every loop contains a non-native op; the prototype ran interpreted):
      main — blocked by:
        assoc.noe:7  CallMethod  r4 <- r2."set"(r3)

If the runner changes any of them, a benchmark harness must hear about it rather than quietly
measure nothing: the headline that fails to parse and the decline section with no parsable site are
both reported as `UNPARSED`, and the CLI form exits non-zero for it.

    usage: jitstats.py <noeta-binary> <program.noe>      # one summary line; exit 3 if loud
           jitstats.py --selftest                       # each of the five states, on recorded output
"""
import os
import re
import subprocess
import sys
from dataclasses import dataclass, field
from enum import Enum

# --- the output shape this parser depends on ---------------------------------------------------
# Anchors, not full lines: the box-drawing rule and the exact wording around them may be restyled,
# but the identifying phrase is what the runner's own doc comment promises.
REPORT_HEADER = "JIT report"
NO_JIT_MARKER = "built without the JIT"
DECLINE_HEADER = "loops declined tier 1"
# `tier 1: N of M compiled prototypes native (S bail stubs), W OSR loop windows, compile time X ms`
HEADLINE_RE = re.compile(
    r"tier 1:\s*(\d+)\s+of\s+(\d+)\s+compiled prototypes native"
    r".*?(\d+)\s+OSR loop windows"
)
# A decline site: `    <file>:<line>  <OpName>  <operands…>`, or `    ?  <OpName> …` for a site with
# no line entry. The op mnemonic is the first token after the site, which is how `op_repr` renders
# every op — matching the mnemonic *shape* rather than a list of known names is the whole fix.
SITE_RE = re.compile(r"^\s+(?:\S+:\d+|\?)\s+([A-Za-z][A-Za-z0-9_]*)\b")
# `  <fn> — blocked by:` — the prototype header inside the decline section.
BLOCKED_BY = "blocked by:"


class DeclineStatus(Enum):
    """What `--jit-stats` said about tier-1 declines, including the ways it said nothing."""

    DECLINED = "declined"
    NONE = "none"
    NO_JIT = "no-jit"
    NO_REPORT = "no-report"
    UNPARSED = "unparsed"

    @property
    def loud(self):
        """States that mean *the instrument failed*, not *the JIT behaved*."""
        return self in (DeclineStatus.NO_REPORT, DeclineStatus.UNPARSED)


@dataclass
class JitStats:
    """One `--jit-stats` reading: the decline state plus the tier-1 headline counters."""

    status: DeclineStatus
    ops: list = field(default_factory=list)
    protos: list = field(default_factory=list)
    native: int = None
    compiled: int = None
    osr_windows: int = None
    detail: str = ""

    def summary(self):
        """One line, safe to print in a column — and never blank."""
        if self.status is DeclineStatus.DECLINED:
            return f"declined: {', '.join(self.ops)}  (in {', '.join(self.protos)})"
        if self.status is DeclineStatus.NONE:
            return "none — every loop is native-sustainable (report present, no decline section)"
        if self.status is DeclineStatus.NO_JIT:
            return "n/a — this binary was built without the JIT"
        if self.status is DeclineStatus.NO_REPORT:
            return ("!! NO REPORT — the binary printed neither a JIT report nor the no-JIT "
                    "disclaimer; --jit-stats output changed" + (f" [{self.detail}]" if self.detail else ""))
        return ("!! FORMAT CHANGED — a JIT report is there but this parser could not read "
                f"{self.detail}; fix jitstats.py rather than trusting an empty list")

    def tier1_note(self):
        """Short corroboration of the headline counters, for a caller printing its own verdict."""
        if self.status is DeclineStatus.NO_JIT:
            return "no JIT in this binary"
        if self.status.loud:
            return self.summary()
        return (f"{self.native} of {self.compiled} prototypes native, "
                f"{self.osr_windows} OSR loop windows"
                + (f"; declined: {', '.join(self.ops)}" if self.ops else "; nothing declined"))


def parse(text):
    """Parse the stderr of a `noeta run --jit-stats` into a [`JitStats`]."""
    if NO_JIT_MARKER in text:
        return JitStats(DeclineStatus.NO_JIT)
    if REPORT_HEADER not in text:
        first = next((ln.strip() for ln in text.splitlines() if ln.strip()), "no output at all")
        return JitStats(DeclineStatus.NO_REPORT, detail=first[:80])

    head = HEADLINE_RE.search(text)
    if not head:
        return JitStats(DeclineStatus.UNPARSED, detail="its `tier 1:` headline")
    native, compiled, osr = (int(g) for g in head.groups())

    if DECLINE_HEADER not in text:
        return JitStats(DeclineStatus.NONE, native=native, compiled=compiled, osr_windows=osr)

    ops, protos = [], []
    for line in text.split(DECLINE_HEADER, 1)[1].splitlines()[1:]:
        if not line.strip():
            break  # the section ends at the first blank line
        if BLOCKED_BY in line:
            name = line.split(BLOCKED_BY)[0].strip().strip("—- ").strip()
            if name and name not in protos:
                protos.append(name)
            continue
        m = SITE_RE.match(line)
        if not m:
            continue
        if m.group(1) not in ops:
            ops.append(m.group(1))
    if not ops:
        return JitStats(DeclineStatus.UNPARSED, protos=protos, native=native, compiled=compiled,
                        osr_windows=osr,
                        detail="a single blocking op out of its decline section (the section is "
                               "there, so something declined and this list must not print empty)")
    return JitStats(DeclineStatus.DECLINED, ops=ops, protos=protos, native=native,
                    compiled=compiled, osr_windows=osr)


def read(binary, program, timeout=300):
    """Run `<binary> run --jit-stats <program>` and parse its report.

    The program's own stdout is discarded; the report goes to stderr. A binary that cannot be run
    at all is `NO_REPORT` — an absent instrument, not an absent decline.
    """
    if not os.path.isfile(binary):
        return JitStats(DeclineStatus.NO_REPORT, detail=f"no such binary: {binary}")
    try:
        p = subprocess.run([binary, "run", "--jit-stats", program],
                           stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=timeout)
    except Exception as exc:  # a timeout or a missing exec bit is still an instrument failure
        return JitStats(DeclineStatus.NO_REPORT, detail=f"{type(exc).__name__}: {exc}"[:80])
    return parse(p.stderr.decode(errors="replace"))


# --- self-test ----------------------------------------------------------------------------------
# Verbatim `--jit-stats` stderr, captured from a release binary: a run where nothing declined, and
# the same program under `NOETA_JIT_ABLATE=0x7f`, which turns the leaf-op set back off and makes
# tier 1 decline for real. The two mutated copies below are the point of the exercise: an instrument
# whose only test is "run it once and see output" cannot tell you what it does when the thing it
# reads has changed, which is precisely how the grep this replaces came to report nothing forever.
_REPORT_CLEAN = """4999950000
── JIT report ──
tier 1: 0 of 0 compiled prototypes native (0 bail stubs), 2 OSR loop windows, compile time 1.7 ms

no bail events — native code never fell back mid-frame
"""
_REPORT_DECLINED = """4999950000
── JIT report ──
tier 1: 0 of 0 compiled prototypes native (0 bail stubs), 0 OSR loop windows, compile time 0.0 ms

no bail events — native code never fell back mid-frame

loops declined tier 1 (every loop contains a non-native op; the prototype ran interpreted):
  main — blocked by:
    ./assoc.noe:4  Stringify   r1 <- display(r0)
    ./assoc.noe:4  BuildString r0 <- k2 ~ display(r1)
    ./assoc.noe:4  CallMethod  r1 <- r4.set(r0, r3) [reuse,consume_key]
"""
_REPORT_NO_JIT = "4999950000\nnoeta: --jit-stats: this binary was built without the JIT (no report)\n"

_SELFTEST = [
    ("nothing declined", _REPORT_CLEAN, DeclineStatus.NONE),
    ("a real decline", _REPORT_DECLINED, DeclineStatus.DECLINED),
    ("a JIT-less binary", _REPORT_NO_JIT, DeclineStatus.NO_JIT),
    ("no report at all", "4999950000\n", DeclineStatus.NO_REPORT),
    # The two format changes that would have made the old grep print an empty column forever.
    ("the site line reshaped",
     _REPORT_DECLINED.replace("    ./assoc.noe:4  ", "    at assoc.noe line 4 -> "),
     DeclineStatus.UNPARSED),
    ("the headline reshaped",
     _REPORT_DECLINED.replace("tier 1: 0 of 0 compiled prototypes native",
                              "tier 1: native 0/0 prototypes"),
     DeclineStatus.UNPARSED),
]


def selftest():
    """Assert each of the five states is reachable and reported, including the loud ones."""
    bad = 0
    for name, text, want in _SELFTEST:
        got = parse(text)
        ok = got.status is want
        bad += not ok
        print(f"  {'ok  ' if ok else 'FAIL'} {name:24s} -> {got.status.value:9s} "
              f"(want {want.value})\n       {got.summary()}")
    print("selftest: " + ("all states reported" if not bad else f"{bad} FAILED"))
    return 1 if bad else 0


def main(argv):
    if len(argv) == 2 and argv[1] == "--selftest":
        return selftest()
    if len(argv) != 3:
        print("usage: jitstats.py <noeta-binary> <program.noe> | --selftest", file=sys.stderr)
        return 2
    stats = read(argv[1], argv[2])
    print(stats.summary())
    return 3 if stats.status.loud else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
