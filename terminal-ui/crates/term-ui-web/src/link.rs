// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! The browser's link to `multivenue-terminal` (TERM plan §4.11.3).
//!
//! - One WebSocket, `binaryType = arraybuffer`. `onmessage` copies each
//!   frame into a preallocated receive ring in linear memory — the one copy
//!   across the JS↔WASM boundary (see the `COPY:` note) — and the app
//!   drains the ring once per paint.
//! - A `Heartbeat` is answered with an `Echo` from `onmessage` itself, so a
//!   hidden tab whose painting is paused still echoes; only a frozen tab
//!   stops, and that is what "UI lost" should mean (§4.6).
//! - When the server refuses the socket (no session) the app shows its
//!   login form; `login` POSTs the token with `fetch` (same-origin, so the
//!   browser stores the HttpOnly cookie) and reconnects.

use std::cell::RefCell;
use std::rc::Rc;

use term_proto::ctl::Beat;
use term_proto::Kind;
use term_ui_core::{Link, LinkState};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

const RING: usize = 8 << 20;

struct Shared {
    ring: Vec<u8>,
    /// (offset, len) of each frame waiting in `ring`, oldest first.
    queue: Vec<(u32, u32)>,
    write: usize,
    dropped: u32,
    state: LinkState,
    opened: bool,
    ws: Option<web_sys::WebSocket>,
    // Kept alive for the socket's lifetime; replaced on reconnect.
    cbs: Vec<Closure<dyn FnMut(JsValue)>>,
}

#[derive(Clone)]
pub struct WebLink {
    sh: Rc<RefCell<Shared>>,
}

impl WebLink {
    pub fn new() -> WebLink {
        let sh = Shared { ring: vec![0u8; RING], queue: Vec::with_capacity(16 * 1024), write: 0, dropped: 0, state: LinkState::Connecting, opened: false, ws: None, cbs: Vec::new() };
        let l = WebLink { sh: Rc::new(RefCell::new(sh)) };
        l.connect();
        l
    }

    fn connect(&self) {
        let Some(win) = web_sys::window() else { return };
        let host = win.location().host().unwrap_or_default();
        let Ok(ws) = web_sys::WebSocket::new(&format!("ws://{host}/ws")) else {
            self.sh.borrow_mut().state = LinkState::Closed;
            return;
        };
        ws.set_binary_type(web_sys::BinaryType::Arraybuffer);
        let mut cbs: Vec<Closure<dyn FnMut(JsValue)>> = Vec::new();

        let sh = self.sh.clone();
        let on_open = Closure::<dyn FnMut(JsValue)>::new(move |_| {
            let mut s = sh.borrow_mut();
            s.state = LinkState::Open;
            s.opened = true;
        });
        ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        cbs.push(on_open);

        let sh = self.sh.clone();
        let on_close = Closure::<dyn FnMut(JsValue)>::new(move |_| {
            let mut s = sh.borrow_mut();
            // Refused before it opened: the session is missing or stale.
            s.state = if s.opened { LinkState::Closed } else { LinkState::NeedsLogin };
            s.ws = None;
        });
        ws.set_onclose(Some(on_close.as_ref().unchecked_ref()));
        cbs.push(on_close);

        let sh = self.sh.clone();
        let on_msg = Closure::<dyn FnMut(JsValue)>::new(move |ev: JsValue| {
            let Ok(ev) = ev.dyn_into::<web_sys::MessageEvent>() else { return };
            let Ok(ab) = ev.data().dyn_into::<js_sys::ArrayBuffer>() else { return };
            let src = js_sys::Uint8Array::new(&ab);
            let len = src.length() as usize;
            let mut s = sh.borrow_mut();
            let s = &mut *s;
            if len == 0 || len > term_proto::MAX_FRAME {
                return;
            }
            if s.queue.is_empty() {
                s.write = 0;
            }
            if s.write + len > s.ring.len() {
                s.dropped += 1;
                return;
            }
            let at = s.write;
            // COPY: ws payload ≤ 64 KiB — JS heap → linear memory —
            // unavoidable on the web; a JS-side decoder rejected (O-T9).
            src.copy_to(&mut s.ring[at..at + len]);
            s.write += len;
            if s.ring[at] == Kind::Heartbeat as u8 {
                // Echo from the socket handler, never from the paint loop.
                if let Ok((f, _)) = term_proto::decode(&s.ring[at..at + len]) {
                    if let Ok(b) = Beat::parse(f.body) {
                        let mut out = [0u8; 64];
                        if let (Ok(n), Some(ws)) = (b.encode(true, 0, 0, &mut out), s.ws.as_ref()) {
                            let _ = ws.send_with_u8_array(&out[..n]);
                        }
                    }
                }
                s.write = at; // the app never needs heartbeats
                return;
            }
            s.queue.push((at as u32, len as u32));
        });
        ws.set_onmessage(Some(on_msg.as_ref().unchecked_ref()));
        cbs.push(on_msg);

        let mut s = self.sh.borrow_mut();
        s.ws = Some(ws);
        s.state = LinkState::Connecting;
        s.opened = false;
        s.cbs = cbs;
    }
}

impl Link for WebLink {
    fn state(&self) -> LinkState {
        self.sh.borrow().state
    }

    fn send(&mut self, frame: &[u8]) {
        if let Some(ws) = self.sh.borrow().ws.as_ref() {
            let _ = ws.send_with_u8_array(frame);
        }
    }

    fn drain<F: FnMut(&[u8])>(&mut self, max: usize, mut f: F) {
        let mut s = self.sh.borrow_mut();
        let n = s.queue.len().min(max);
        for k in 0..n {
            let (o, l) = s.queue[k];
            f(&s.ring[o as usize..(o + l) as usize]);
        }
        s.queue.drain(..n);
    }

    fn take_dropped(&mut self) -> u32 {
        std::mem::take(&mut self.sh.borrow_mut().dropped)
    }

    fn login(&mut self, token: &str) {
        let this = self.clone();
        let token = token.to_owned();
        wasm_bindgen_futures::spawn_local(async move {
            let ok = post_login(&token).await.unwrap_or(false);
            if ok {
                this.connect();
            } else {
                this.sh.borrow_mut().state = LinkState::LoginFailed;
            }
        });
    }
}

async fn post_login(token: &str) -> Result<bool, JsValue> {
    let win = web_sys::window().ok_or("no window")?;
    let init = web_sys::RequestInit::new();
    init.set_method("POST");
    init.set_mode(web_sys::RequestMode::SameOrigin);
    init.set_credentials(web_sys::RequestCredentials::SameOrigin);
    init.set_body(&JsValue::from_str(token));
    let req = web_sys::Request::new_with_str_and_init("/login", &init)?;
    req.headers().set("Content-Type", "text/plain")?;
    let resp = wasm_bindgen_futures::JsFuture::from(win.fetch_with_request(&req)).await?;
    let resp: web_sys::Response = resp.dyn_into()?;
    Ok(resp.status() == 204)
}
