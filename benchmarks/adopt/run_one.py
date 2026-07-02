#!/usr/bin/env python3
"""One prompting-adoption run: does a strong CLAUDE.md make the agent use rdbg?

  python3 run_one.py <strong|control> <idx>

Copies the accumulator task to an isolated dir, writes the condition's CLAUDE.md,
makes the rust-debugger skill available in both, runs Claude Code headless
(Opus, medium effort) on the fix task, then reports whether it used rdbg, how
many times, tokens, wall time, and whether `cargo test` passes. Prints one JSON line.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

BENCH = Path(__file__).resolve().parent.parent
TASK = BENCH / "tasks" / "accumulator"
SKILL = BENCH.parent / "skill" / "rust-debugger" / "SKILL.md"
OUT = BENCH / "results-adopt"

STRONG_MD = """# Debugging policy (MANDATORY — read before touching code)

When a test fails or a value looks wrong, do NOT diagnose it by reading source,
grepping, or adding print/`dbg!` statements and re-running. That guess-and-check
loop is slow and unreliable and is NOT the approach to use here.

This project ships `rdbg`, a runtime debugger (run `rdbg` for usage). You are
required to OBSERVE the actual runtime values with it before editing anything:

    rdbg launch --cargo . --test <test_name> --break src/lib.rs:<line> -- <test_name>
    rdbg vars                 # the real local values at that point
    rdbg eval <path>          # one value
    rdbg step over            # watch execution advance
    rdbg trace --cargo . --test <t> --break src/lib.rs:<line> --capture <vars> -- <t>

Workflow: run the failing test to see the assertion; set a breakpoint where the
suspect value is computed; launch that test under `rdbg`; read the ACTUAL values;
only then edit the code to correct what you observed. Breaking and inspecting is
your first move — not opening the file to read and reason.
"""

CONTROL_MD = """# Notes

A `rust-debugger` skill (`rdbg`) is available in this project if you find it useful.
"""

PROMPT = ("The test in this Rust crate fails (`cargo test`). Find the root cause "
          "and fix the source so the test passes. Keep the change minimal and "
          "correct for the general case, not just the test input.")


def main() -> None:
    cond, idx = sys.argv[1], sys.argv[2]
    OUT.mkdir(exist_ok=True)
    work = OUT / f"{cond}-{idx}"
    if work.exists():
        shutil.rmtree(work)
    shutil.copytree(TASK, work)
    if (work / "target").exists():
        shutil.rmtree(work / "target")
    d = work / ".claude" / "skills" / "rust-debugger"
    d.mkdir(parents=True, exist_ok=True)
    shutil.copy(SKILL, d / "SKILL.md")
    (work / "CLAUDE.md").write_text(STRONG_MD if cond == "strong" else CONTROL_MD)

    env = dict(os.environ, PATH=f"{Path.home()}/.local/bin:" + os.environ["PATH"])
    start = time.monotonic()
    try:
        p = subprocess.run(
            ["claude", "-p", PROMPT, "--model", "opus", "--effort", "medium",
             "--output-format", "stream-json", "--verbose", "--dangerously-skip-permissions"],
            cwd=work, capture_output=True, text=True, timeout=1500, env=env)
        out = p.stdout
    except subprocess.TimeoutExpired:
        out = ""
    wall = round(time.monotonic() - start, 1)
    (work / "transcript.jsonl").write_text(out)

    rdbg_calls = 0
    first_tool = None
    tokens = None
    for line in out.splitlines():
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        m = ev.get("message", {})
        for b in (m.get("content") or []) if isinstance(m.get("content"), list) else []:
            if isinstance(b, dict) and b.get("type") == "tool_use":
                name = b.get("name")
                cmd = str((b.get("input") or {}).get("command", ""))
                if first_tool is None:
                    first_tool = "rdbg" if (name == "Bash" and (cmd.strip().startswith("rdbg") or " rdbg " in cmd)) else name
                if name == "Bash" and (cmd.strip().startswith("rdbg") or " rdbg " in cmd):
                    rdbg_calls += 1
        if ev.get("type") == "result":
            u = ev.get("usage", {})
            tokens = sum(v for k, v in u.items() if k.endswith("_tokens") and isinstance(v, int))

    passed = subprocess.run(["cargo", "test"], cwd=work, capture_output=True,
                            timeout=300, env=env).returncode == 0
    subprocess.run(["pkill", "-f", "rdbg __daemon"], capture_output=True)
    subprocess.run(["pkill", "-f", "lldb-dap"], capture_output=True)
    subprocess.run(["pkill", "-f", "rust-analyzer"], capture_output=True)
    print(json.dumps({"condition": cond, "idx": idx, "used_rdbg": rdbg_calls > 0,
                      "rdbg_calls": rdbg_calls, "first_tool": first_tool,
                      "tokens": tokens, "wall_s": wall, "passed": passed}))


if __name__ == "__main__":
    main()
