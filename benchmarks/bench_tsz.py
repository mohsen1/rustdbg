#!/usr/bin/env python3
"""tsz with/without-rdbg FIX-RATE benchmark — contamination-isolated, parallelizable.

Each case is a real merged bug-fix from tsz's recent history (chosen post-training-cutoff).
For each case the harness builds a CLEAN single-commit checkout at the fix's PARENT and
overlays only the regression test — so:
  * the fix commit is not present (git show <sha> fails),
  * there is no future history to git log / git blame toward the answer,
  * web tools are disallowed (no GitHub / issue lookup).
The agent then fixes the bug for real, twice: WITHOUT rdbg and WITH rdbg (+ the
fingerprint-trace recipe). We record pass/fail (fix rate), tokens, and wall time.

Everything for a slot lives inside one capped disk image, so a runaway build fails in
the image instead of filling the host. Launch two slots on disjoint cases to parallelize:

  python3 bench_tsz.py --slot A --image /Volumes/tszA --cases 0,2
  python3 bench_tsz.py --slot B --image /Volumes/tszB --cases 1,3    # in parallel
"""
from __future__ import annotations
import argparse, json, os, re, shutil, subprocess, threading, time
from pathlib import Path

TSZ = str(Path.home() / "code" / "tsz")
ROOT = Path(__file__).resolve().parent
SKILL = ROOT.parent / "skill" / "rust-debugger" / "SKILL.md"
RESULTS = ROOT / "results-tsz"
MIN_FREE_GB = 3

BASE_PROMPT = """A regression test in this repository fails:

    cargo nextest run {filter}

Diagnose the root cause and fix the source so that test passes. Do NOT modify the test
file. Match the TypeScript compiler (tsc). The workspace is very large: only ever run
that one narrowed test command above, never the full test suite or a broad build."""

RDBG_NOTE = """

You also have `rdbg`, a Rust debugger for this project (run `rdbg` for usage). Most tsz
diagnostics funnel through a sink — `push_diagnostic` — so for a wrong / extra / missing
diagnostic ("fingerprint") mismatch, TRACE it at runtime instead of grepping:

  rdbg launch --cargo crates/tsz-checker --test {stem} --break-fn push_diagnostic -- {testfn}
  rdbg eval diag.code          # the code being emitted; not the one you expect?
  rdbg continue ; rdbg eval diag.code   # …continue until it is
  rdbg bt                      # walks back to the exact decision that emitted it
  rdbg frame <n> ; rdbg vars   # at that frame, inspect the types/flags that produced it

Not every diagnostic goes through push_diagnostic. If the launch reports
`push_diagnostic — bound, 0 hits`, this one uses a different emit path: break on
`emit_render_request` instead, or grep the emitted code to find the emit site and break
there. rdbg tells you when a breakpoint never fired, so use that signal to re-target.

For an EXTRA (false-positive) diagnostic the backtrace shows what decided to emit it;
for a WRONG value inspect the type being formatted; for a MISSING one break where the
check should fire and see why its condition is false. Prefer this over adding prints."""


def git_slot(slot: Path, *a):
    return subprocess.run(["git", "-C", str(slot), *a], capture_output=True, text=True)


def setup_case(slot: Path, case: dict):
    """Pristine tree at the parent (no history) + overlaid regression test, as one commit."""
    if slot.exists():
        shutil.rmtree(slot)
    slot.mkdir(parents=True)
    subprocess.run(f"git -C {TSZ} archive {case['parent']} | tar -x -C {slot}", shell=True, check=True)
    # strip tsz's own .claude: its tsz-tracing/tsz-emit skills would both trip the
    # workspace-trust block AND give the WITHOUT condition tsz-specific debugging help,
    # muddying a clean rdbg A/B. Both conditions get a plain agent; WITH adds only rdbg.
    shutil.rmtree(slot / ".claude", ignore_errors=True)
    for f in case["test_files"] + case["cargo_files_to_checkout"]:
        blob = subprocess.run(["git", "-C", TSZ, "show", f"{case['sha']}:{f}"], capture_output=True).stdout
        dst = slot / f
        dst.parent.mkdir(parents=True, exist_ok=True)
        dst.write_bytes(blob)
    git_slot(slot, "init", "-q")
    git_slot(slot, "config", "user.email", "b@b.co")
    git_slot(slot, "config", "user.name", "bench")
    git_slot(slot, "add", "-A")
    git_slot(slot, "commit", "-qm", "baseline", "--no-verify")


def reset_slot(slot: Path):
    git_slot(slot, "reset", "--hard", "HEAD")
    git_slot(slot, "clean", "-fd")


def first_testfn(slot: Path, case: dict) -> str:
    for tf in case["test_files"]:
        p = slot / tf
        if p.exists():
            m = re.search(r"fn ([a-z0-9_]+)\s*\(", p.read_text())
            if m:
                return m.group(1)
    return ""


def watched(cmd, cwd, env, image, timeout):
    proc = subprocess.Popen(cmd, cwd=str(cwd), env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    state = {"disk": False}

    def watch():
        while proc.poll() is None:
            try:
                free = shutil.disk_usage(image).free / 1e9
            except FileNotFoundError:
                free = 99
            if free < MIN_FREE_GB:
                state["disk"] = True
                subprocess.run(["pkill", "-9", "-f", "cargo"], capture_output=True)
                subprocess.run(["pkill", "-9", "-f", "rustc"], capture_output=True)
                proc.kill()
                return
            time.sleep(5)

    t = threading.Thread(target=watch, daemon=True)
    t.start()
    try:
        out, _ = proc.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        proc.kill()
        out, _ = proc.communicate()
        return out or "", state["disk"], True, -1
    return out or "", state["disk"], False, proc.returncode


def verify(slot: Path, image: str, case: dict, env) -> bool:
    """nextest exits 0 iff all selected tests pass — the robust green signal."""
    args = case["nextest_filter"].split()
    _, _, _, rc = watched(["cargo", "nextest", "run", *args], slot, env, image, 1800)
    return rc == 0


def run_agent(slot: Path, image: str, prompt: str, tpath: Path, env, timeout=2700):
    cmd = ["claude", "-p", prompt, "--model", "opus", "--effort", "medium",
           "--dangerously-skip-permissions", "--disallowedTools", "WebSearch", "WebFetch",
           "--output-format", "stream-json", "--verbose"]
    start = time.monotonic()
    out, disk, timed, _ = watched(cmd, slot, env, image, timeout)
    wall = round(time.monotonic() - start, 1)
    tpath.write_text(out)
    tokens = None
    for line in out.splitlines():
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        if ev.get("type") == "result":
            u = ev.get("usage", {})
            tokens = sum(v for k, v in u.items() if k.endswith("_tokens") and isinstance(v, int))
    return {"tokens": tokens, "wall_s": wall, "disk_killed": disk, "timed_out": timed}


def env_for(image: str):
    return dict(os.environ,
                PATH=f"{Path.home()}/.local/bin:" + os.environ["PATH"],
                CARGO_TARGET_DIR=f"{image}/target")


def one_case(slot: Path, image: str, case: dict, conds, results_path: Path, lock: threading.Lock):
    env = env_for(image)
    shutil.rmtree(f"{image}/target", ignore_errors=True)  # fresh build per case — bound image usage
    setup_case(slot, case)
    base_red = not verify(slot, image, case, env)  # must start failing
    sha = case["sha"][:10]
    if not base_red:
        with lock:
            existing = json.loads(results_path.read_text()) if results_path.exists() else []
            existing.append({"case": sha, "subject": case["subject"], "cond": "-",
                             "baseline_red": False, "passed": None, "note": "not red at parent — skipped"})
            results_path.write_text(json.dumps(existing, indent=2))
        print(f"[{sha}] NOT RED at parent — skipping", flush=True)
        return
    stem = case["test_stems"][0] if case["test_stems"] else ""
    testfn = first_testfn(slot, case)
    with lock:
        done = {r["cond"] for r in (json.loads(results_path.read_text()) if results_path.exists() else [])
                if r.get("case") == sha and r.get("passed") is not None
                and not r.get("disk_killed") and not r.get("timed_out")}
    for i, cond in enumerate(conds):
        if cond in done:
            print(f"[{sha}/{cond}] already done — skipping", flush=True)
            continue
        if i > 0:  # fresh target for the 2nd condition — bound disk, no cross-condition build accumulation
            shutil.rmtree(f"{image}/target", ignore_errors=True)
        reset_slot(slot)
        prompt = BASE_PROMPT.format(filter=case["nextest_filter"])
        if cond == "with":
            d = slot / ".agents" / "skills" / "rust-debugger"
            d.mkdir(parents=True, exist_ok=True)
            shutil.copy(SKILL, d / "SKILL.md")
            prompt += RDBG_NOTE.format(stem=stem, testfn=testfn or "<failing_test_name>")
        tpath = RESULTS / "transcripts" / f"{sha}-{cond}.jsonl"
        tpath.parent.mkdir(parents=True, exist_ok=True)
        info = run_agent(slot, image, prompt, tpath, env)
        passed = verify(slot, image, case, env) if not info["disk_killed"] else False
        subprocess.run(["pkill", "-f", "rdbg __daemon"], capture_output=True)
        row = {"case": sha, "subject": case["subject"], "cond": cond, "baseline_red": base_red,
               "passed": passed, **info}
        with lock:
            existing = json.loads(results_path.read_text()) if results_path.exists() else []
            existing.append(row)
            results_path.write_text(json.dumps(existing, indent=2))
        print(f"[{sha}/{cond}] red={base_red} passed={passed} wall={info['wall_s']}s "
              f"tok={info['tokens']} {'DISK' if info['disk_killed'] else ''}{'TIMEOUT' if info['timed_out'] else ''}",
              flush=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--slot", required=True)
    ap.add_argument("--image", required=True)
    ap.add_argument("--cases", required=True, help="comma indices into cases-tsz.json")
    ap.add_argument("--conditions", default="without,with")
    a = ap.parse_args()
    RESULTS.mkdir(exist_ok=True)
    cases = json.loads((RESULTS / "cases-tsz.json").read_text())
    idxs = [int(x) for x in a.cases.split(",")]
    conds = a.conditions.split(",")
    slot = Path(a.image) / "slot"
    results_path = RESULTS / f"runs-{a.slot}.json"
    lock = threading.Lock()
    for i in idxs:
        one_case(slot, a.image, cases[i], conds, results_path, lock)
    subprocess.run(["pkill", "-f", "rdbg __daemon"], capture_output=True)
    subprocess.run(["pkill", "-f", "lldb-dap"], capture_output=True)
    print(f"slot {a.slot} done ({len(idxs)} cases)")


if __name__ == "__main__":
    main()
