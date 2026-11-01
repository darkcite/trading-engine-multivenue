// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! `term-ui-web` — the only crate that touches `web-sys` (plan v2.1 §4.11.1):
//! canvas start, the clock, the console, and focus/visibility reporting.
//! Everything is wasm32-only; on the host this crate is empty, so a host
//! build never compiles eframe or winit.
#![cfg(target_arch = "wasm32")]

use std::sync::Arc;

use eframe::egui;
use eframe::egui_wgpu::{self, wgpu};
use term_proto::ctl::{Control, CtlOp};
use term_ui_core::spike::Spike;
use term_ui_core::Platform;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

struct WebPlatform {
    perf: web_sys::Performance,
}

impl Platform for WebPlatform {
    fn now_ms(&self) -> f64 {
        self.perf.now()
    }
    fn log(&self, line: &str) {
        web_sys::console::log_1(&JsValue::from_str(line));
    }
}

struct App {
    spike: Spike<WebPlatform>,
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        let cpu_ms = frame.info().cpu_usage.map(|s| s * 1000.0);
        self.spike.ui(ctx, frame.wgpu_render_state(), cpu_ms);
    }
}

/// Encode a `Control` frame (what the client will send on the socket in
/// T1a) and log it; the spike has no server.
fn report(kind: CtlOp) {
    let mut buf = [0u8; term_proto::HDR_LEN + term_proto::ctl::Control::LEN];
    let seq = 0;
    let ts = 0;
    if let Ok(n) = Control::op(kind).encode(seq, ts, &mut buf) {
        let ok = term_proto::decode(&buf[..n]).is_ok();
        web_sys::console::log_1(&JsValue::from_str(&format!(
            "TERMSPIKE {{\"event\":\"control\",\"kind\":\"{kind:?}\",\"bytes\":{n},\"decodes\":{ok}}}"
        )));
    }
}

fn listen(target: &web_sys::EventTarget, event: &str, f: impl FnMut() + 'static) -> Result<(), JsValue> {
    let cb = Closure::<dyn FnMut()>::new(f);
    target.add_event_listener_with_callback(event, cb.as_ref().unchecked_ref())?;
    cb.forget(); // one listener for the page's lifetime
    Ok(())
}

/// `#gl` → WebGL2 only, `#webgpu` → WebGPU only, otherwise WebGPU first with
/// the WebGL2 fallback.
fn backends(hash: &str) -> wgpu::Backends {
    match hash {
        "#gl" => wgpu::Backends::GL,
        "#webgpu" => wgpu::Backends::BROWSER_WEBGPU,
        _ => wgpu::Backends::BROWSER_WEBGPU | wgpu::Backends::GL,
    }
}

#[wasm_bindgen]
pub async fn start(canvas: web_sys::HtmlCanvasElement) -> Result<(), JsValue> {
    let window = web_sys::window().ok_or("no window")?;
    let document = window.document().ok_or("no document")?;
    let perf = window.performance().ok_or("no performance")?;
    let hash = window.location().hash().unwrap_or_default();

    let mut setup = egui_wgpu::WgpuSetupCreateNew::default();
    setup.instance_descriptor.backends = backends(&hash);
    // Plan §4.11.4: the pipelines use only the WebGL2 subset; request exactly
    // those limits on every backend, so WebGPU cannot hide an overreach.
    setup.device_descriptor = Arc::new(|_adapter| wgpu::DeviceDescriptor {
        label: Some("term device (webgl2 limits)"),
        required_limits: wgpu::Limits { max_texture_dimension_2d: 8192, ..wgpu::Limits::downlevel_webgl2_defaults() },
        ..Default::default()
    });
    let mut opts = eframe::WebOptions::default();
    opts.wgpu_options.wgpu_setup = egui_wgpu::WgpuSetup::CreateNew(setup);

    let doc_t: web_sys::EventTarget = document.clone().into();
    let d2 = document.clone();
    listen(&doc_t, "visibilitychange", move || {
        report(match d2.visibility_state() {
            web_sys::VisibilityState::Hidden => CtlOp::Hidden,
            _ => CtlOp::Visible,
        })
    })?;
    listen(&doc_t, "freeze", || report(CtlOp::Frozen))?;
    listen(&doc_t, "resume", || report(CtlOp::Resumed))?;

    let plat = WebPlatform { perf };
    eframe::WebRunner::new()
        .start(canvas, opts, Box::new(move |_cc| Ok(Box::new(App { spike: Spike::new(plat) }))))
        .await
}
