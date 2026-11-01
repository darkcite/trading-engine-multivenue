# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Headless browser smoke for the T0 spike (plan v2.1 §4.11.8, cloud half).

Loads the bundle from the CSP harness, runs the spike's scripted phases,
collects every `TERMSPIKE` line, fails on any CSP violation or page error,
screenshots each phase, then checks Hidden/Visible reporting.
Usage: python3 smoke.py <out_dir> <gl|webgpu> [timeout_s]"""
import json
import os
import sys
import time

import playwright.sync_api

OUT = sys.argv[1]
MODE = sys.argv[2]
TIMEOUT = float(sys.argv[3]) if len(sys.argv) > 3 else 1500.0
ARGS = ["--enable-unsafe-swiftshader", "--use-angle=swiftshader", "--ignore-gpu-blocklist"]
if MODE == "webgpu":
    ARGS += ["--enable-unsafe-webgpu", "--enable-features=Vulkan", "--use-webgpu-adapter=swiftshader"]

os.makedirs(OUT, exist_ok=True)
lines, errors, csp = [], [], []
pending_shots = []
# A screenshot can fail on a busy WebGPU canvas in headless Chromium; that
# is recorded, not fatal: the gate is the phases, CSP and errors.
shot_errors = []


def on_console(msg):
    text = msg.text
    if "Content Security Policy" in text or "Refused to" in text:
        csp.append(text)
    if msg.type == "error":
        errors.append(text)
    if text.startswith("TERMSPIKE "):
        rec = json.loads(text[len("TERMSPIKE "):])
        rec["t_wall"] = round(time.time() - T0, 1)
        lines.append(rec)
        print(json.dumps(rec), flush=True)
        if rec.get("event") == "phase":
            pending_shots.append((time.time() + 2.0, rec["phase"]))


with playwright.sync_api.sync_playwright() as pw:
    browser = pw.chromium.launch(headless=True, args=ARGS)
    page = browser.new_page(viewport={"width": 1600, "height": 1000})
    page.on("console", on_console)
    page.on("pageerror", lambda e: errors.append("pageerror: " + str(e)))
    T0 = time.time()
    page.goto("http://127.0.0.1:9393/#" + ("gl" if MODE == "gl" else "webgpu"))
    page.evaluate("""() => { window.__csp = [];
        document.addEventListener('securitypolicyviolation', e => window.__csp.push(e.violatedDirective + ' ' + e.blockedURI)); }""")
    shot_n = 0
    while time.time() - T0 < TIMEOUT:
        page.wait_for_timeout(500)
        now = time.time()
        while pending_shots and pending_shots[0][0] <= now:
            _, after = pending_shots.pop(0)
            shot_n += 1
            try:
                page.screenshot(path=os.path.join(OUT, f"{shot_n:02d}-after-{after}.png"))
            except playwright.sync_api.Error as e:
                shot_errors.append(f"{after}: {e}".splitlines()[0])
        if any(r.get("event") == "done" for r in lines) and not pending_shots:
            break
        if errors and not lines:
            break
    page.wait_for_timeout(3000)
    try:
        page.screenshot(path=os.path.join(OUT, f"{shot_n + 1:02d}-interactive.png"))
    except playwright.sync_api.Error as e:
        shot_errors.append(f"interactive: {e}".splitlines()[0])
    # Hidden / Visible reporting: override the read-only property and fire.
    for state in ("hidden", "visible"):
        page.evaluate("""s => { Object.defineProperty(document, 'visibilityState', {value: s, configurable: true});
            document.dispatchEvent(new Event('visibilitychange')); }""", state)
        page.wait_for_timeout(300)
    csp += page.evaluate("() => window.__csp")
    browser.close()

controls = [r["kind"] for r in lines if r.get("event") == "control"]
result = {
    "mode": MODE,
    "wall_s": round(time.time() - T0, 1),
    "done": any(r.get("event") == "done" for r in lines),
    "init": next((r for r in lines if r.get("event") == "init"), None),
    "phases": [r for r in lines if r.get("event") == "phase"],
    "controls": controls,
    "csp_violations": csp,
    "errors": errors,
    "screenshot_errors": shot_errors,
}
result["pass"] = (result["done"] and not csp and not errors
                  and controls[-2:] == ["Hidden", "Visible"])
with open(os.path.join(OUT, "result.json"), "w") as f:
    json.dump(result, f, indent=1)
print("PASS" if result["pass"] else "FAIL", json.dumps({k: result[k] for k in ("done", "controls", "csp_violations", "errors")}))
