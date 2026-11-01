# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Browser smoke for the terminal app against multivenue-terminal.

Signs in through the app's own login form (no cookie: the server refuses the
socket, the form appears, the token is typed), then checks the app's
TERMAPP reports: link open, frames decoded, none bad, books and bars
present, no CSP violation, no page error. Screenshots the result.
Usage: python3 app_smoke.py <out_dir> <token> [gl|webgpu] [seconds]"""
import json
import os
import sys
import time

import playwright.sync_api

OUT, TOKEN = sys.argv[1], sys.argv[2]
MODE = sys.argv[3] if len(sys.argv) > 3 else "gl"
SECS = float(sys.argv[4]) if len(sys.argv) > 4 else 30.0
ARGS = ["--enable-unsafe-swiftshader", "--use-angle=swiftshader", "--ignore-gpu-blocklist"]
if MODE == "webgpu":
    ARGS += ["--enable-unsafe-webgpu", "--enable-features=Vulkan", "--use-webgpu-adapter=swiftshader"]
os.makedirs(OUT, exist_ok=True)
reports, errors, csp = [], [], []


def on_console(msg):
    t = msg.text
    if "Content Security Policy" in t or "Refused to" in t:
        csp.append(t)
    # The first socket is refused before login; that refusal is how the app
    # learns to show its form, so the browser's report of it is expected.
    if msg.type == "error" and "HTTP Authentication failed" not in t:
        errors.append(t)
    if t.startswith("TERMAPP "):
        reports.append(json.loads(t[len("TERMAPP "):]))
        print(t, flush=True)


with playwright.sync_api.sync_playwright() as pw:
    browser = pw.chromium.launch(headless=True, args=ARGS)
    page = browser.new_page(viewport={"width": 1600, "height": 1000})
    page.on("console", on_console)
    page.on("pageerror", lambda e: errors.append("pageerror: " + str(e)))
    page.goto("http://127.0.0.1:9393/#" + MODE)
    page.wait_for_timeout(7000)
    page.screenshot(path=os.path.join(OUT, "01-login.png"))
    state_before = reports[-1]["state"] if reports else None
    # The token field sits under the two lines of copy, centred.
    page.mouse.click(800, 372)
    page.wait_for_timeout(300)
    page.keyboard.type(TOKEN, delay=10)
    page.keyboard.press("Enter")
    t0 = time.time()
    while time.time() - t0 < SECS:
        page.wait_for_timeout(1000)
    page.screenshot(path=os.path.join(OUT, "02-terminal.png"))
    browser.close()

last = reports[-1] if reports else {}
books = last.get("books_bids_asks_bars", [])
result = {
    "state_before_login": state_before,
    "last": last,
    "csp_violations": csp,
    "errors": errors,
}
result["pass"] = (state_before == "NeedsLogin" and last.get("state") == "Open" and last.get("frames", 0) > 50
                  and last.get("bad", 1) == 0 and any(b[0] > 0 and b[1] > 0 and b[2] > 0 for b in books)
                  and not csp and not errors)
with open(os.path.join(OUT, "result.json"), "w") as f:
    json.dump(result, f, indent=1)
print("PASS" if result["pass"] else "FAIL", json.dumps(result)[:600])
