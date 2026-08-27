# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""claude_worker — AI-ingress worker for the multivenue trading engine (8f).

Library core + two frontends (serve daemon, operator verbs) over the same
code path. Offline only: never in the engine hot path.

Convention: full ``import x`` only. No ``from x import y``.
"""
