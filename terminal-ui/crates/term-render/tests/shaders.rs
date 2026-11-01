// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! Every WGSL module parses and validates under naga with the capabilities a
//! WebGL2 (GLES 3.0) device offers: no storage buffers, no compute. Runs on
//! the host with no GPU.

fn validate(name: &str, src: &str) {
    let module = naga::front::wgsl::parse_str(src).unwrap_or_else(|e| panic!("{name}: {}", e.emit_to_string(src)));
    let caps = naga::valid::Capabilities::empty();
    naga::valid::Validator::new(naga::valid::ValidationFlags::all(), caps)
        .validate(&module)
        .unwrap_or_else(|e| panic!("{name}: {e:?}"));
    for gv in module.global_variables.iter() {
        assert!(
            !matches!(gv.1.space, naga::AddressSpace::Storage { .. }),
            "{name}: storage buffers are not available on WebGL2"
        );
    }
    for ep in &module.entry_points {
        assert!(ep.stage != naga::ShaderStage::Compute, "{name}: no compute on WebGL2");
    }
}

#[test]
fn rects() {
    validate("rects", include_str!("../shaders/rects.wgsl"));
}
#[test]
fn polyline() {
    validate("polyline", include_str!("../shaders/polyline.wgsl"));
}
#[test]
fn circles() {
    validate("circles", include_str!("../shaders/circles.wgsl"));
}
#[test]
fn heatmap() {
    validate("heatmap", include_str!("../shaders/heatmap.wgsl"));
}
