# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Kronos adapter package (Kronos lanes plan §3a, ruling D13).

Upstream `shiyu-coder/Kronos` is used **as-is**: it is not vendored into
this tree and never modified. It is a pinned, hash-verified runtime
artifact outside git — exactly like the weights — installed by
:mod:`claude_worker.kronos.install` from ``claude-worker/kronos.lock``.
Everything in this package is *adapter*: path handling, hash
verification, eval-mode discipline, the per-path autoregressive loop.

Nothing here is on any engine hot path. The engine never links torch;
the sidecar sends forecasts to it as AI command frames.

Convention: full ``import x`` only. No ``from x import y``.
"""
