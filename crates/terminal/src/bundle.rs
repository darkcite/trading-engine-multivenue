// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! The WASM bundle the server serves (TERM plan §4.11.1): loaded once at
//! boot from `terminal-ui/dist/` and refused unless every file matches its
//! SHA-256 in `dist/MANIFEST`. (Embedding with `build.rs` under a
//! `web-bundle` feature replaces the load for release builds.)

pub struct File {
    pub path: &'static str,
    pub ctype: &'static str,
    pub body: Vec<u8>,
}

pub const FILES: [(&str, &str); 5] = [
    ("index.html", "text/html; charset=utf-8"),
    ("loader.js", "text/javascript; charset=utf-8"),
    ("term.css", "text/css; charset=utf-8"),
    ("term_ui.js", "text/javascript; charset=utf-8"),
    ("term_ui_bg.wasm", "application/wasm"),
];

pub struct Bundle {
    pub files: Vec<File>,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl Bundle {
    /// An empty bundle: every page answers 503 with a build hint.
    pub fn none() -> Bundle {
        Bundle { files: Vec::new() }
    }

    pub fn load(dir: &std::path::Path) -> Result<Bundle, String> {
        let manifest = std::fs::read_to_string(dir.join("MANIFEST")).map_err(|e| format!("MANIFEST: {e}"))?;
        let mut files = Vec::with_capacity(FILES.len());
        for (name, ctype) in FILES {
            let body = std::fs::read(dir.join(name)).map_err(|e| format!("{name}: {e}"))?;
            let want = manifest
                .lines()
                .find_map(|l| l.split_once("  ").filter(|(_, n)| *n == name).map(|(h, _)| h.to_owned()))
                .ok_or_else(|| format!("{name}: not in MANIFEST"))?;
            let got = hex(&core_crypto::sha256(&body));
            if got != want {
                return Err(format!("{name}: sha256 {got} ≠ MANIFEST {want}"));
            }
            files.push(File { path: name, ctype, body });
        }
        Ok(Bundle { files })
    }

    pub fn get(&self, url_path: &[u8]) -> Option<&File> {
        let name = if url_path == b"/" { &b"index.html"[..] } else { url_path.strip_prefix(b"/")? };
        self.files.iter().find(|f| f.path.as_bytes() == name)
    }
}
