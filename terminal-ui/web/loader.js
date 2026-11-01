// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
// The one hand-written JS file (T-L14): load the generated glue, start the app.
import init, { start } from "./term_ui.js";
await init();
await start(document.getElementById("term"));
