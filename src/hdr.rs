// SPDX-License-Identifier: GPL-3.0-or-later

//! HDR output support.
//!
//! Two pieces live here: the GLSL shaders that convert between sRGB and the
//! BT.2020/PQ signal an HDR connector expects, and their CPU twins, used for
//! everything the shaders do not touch (clear colour, solid-colour render
//! elements, capture readbacks).
//!
//! The unit tests below check the twins against a float64 reference
//! implementation of the same spec formulas, and `tests/integration/hdr_shaders.rs`
//! compiles the shaders on a real GLES renderer and checks that the GL inverse
//! produces what its twin computes.

use smithay::{
    backend::renderer::{Color32F, element::Id},
    desktop::{PopupManager, layer_map_for_output},
    output::Output,
    reexports::wayland_server::{Resource, protocol::wl_surface::WlSurface},
    wayland::{
        color::management::{
            Chromaticities, ImageDescription, Primaries, PrimariesOption, TransferFunction,
        },
        compositor::{TraversalAction, with_surface_tree_downward},
    },
};
use std::path::PathBuf;

use crate::window::WindowElement;

/// BT.2408 HDR reference white (nits): the luminance 1.0 sRGB is mapped to.
///
/// 203 is what BT.2408 specifies for grading SDR into an HDR grade, and what the
/// desktop compositors that have shipped HDR use by default. It is only a
/// default: a panel whose SDR picture mode is much brighter than 203 nits will
/// show HDR-mode SDR content as dimmer than the same content in SDR mode, which
/// is why [`ref_white`] reads `PINNACLE_HDR_REF_WHITE`.
pub const DEFAULT_REF_WHITE: f64 = 203.0;

/// Default peak luminance advertised to the connector, in nits.
pub const DEFAULT_MAX_LUMINANCE: u16 = 1000;

/// Reference white an HDR-signalling description reports, in nits.
///
/// This is BT.2408's 203, and it is deliberately not [`ref_white`]: the two answer
/// different questions. `ref_white` says where *this compositor* puts SDR white
/// when it encodes the desktop into PQ, which is a local brightness preference.
/// The description's reference white is the number clients read, and they convert
/// it into their own terms: mpv takes the advertised peak and rescales it by
/// `203 / reference`, so advertising a 400-nit reference made it tone map 1000-nit
/// content to 507 nits. Advertising the convention keeps the advertised peak
/// meaning what it says.
pub const PROTOCOL_REFERENCE_WHITE: u32 = 203;

/// Default full-frame average peak advertised to the connector, in nits.
///
/// There is no way to read this panel's real value without parsing the EDID's
/// HDR static metadata block, so it is a claim rather than a measurement.
pub const DEFAULT_MAX_FALL: u32 = 400;

/// Lowest and highest paper white accepted, in nits.
///
/// Below 80 the PQ code space under SDR white is crushed; above 1000 an encoded
/// SDR white leaves a 1000-nit panel no highlight headroom at all.
pub const REF_WHITE_MIN: u32 = 80;
pub const REF_WHITE_MAX: u32 = 1000;

/// Lowest and highest advertised peak accepted, in nits.
pub const MAX_LUMINANCE_MIN: u32 = 100;
pub const MAX_LUMINANCE_MAX: u32 = 4000;

/// Lowest and highest advertised frame average accepted, in nits.
pub const MAX_FALL_MIN: u32 = 1;
pub const MAX_FALL_MAX: u32 = 10_000;

/// Per-output HDR configuration.
///
/// The compositor keeps one of these per output. It is set by the config client
/// through the `output` API (`OutputHandle::set_hdr`), and initialised from the
/// environment at output creation so the pre-API launcher keeps working.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HdrConfig {
    /// Whether the output is driven in BT.2020/PQ.
    pub enabled: bool,
    /// Where this compositor maps SDR white when it encodes content into PQ, nits.
    pub reference_white: f64,
    /// Peak luminance advertised to the connector, nits.
    pub max_luminance: u16,
    /// Full-frame average peak advertised to the connector, nits.
    pub max_fall: u16,
}

impl HdrConfig {
    /// HDR off, with the default values an `Off` output carries so that
    /// turning it back on does not need the caller to resupply them.
    pub const OFF: HdrConfig = HdrConfig {
        enabled: false,
        reference_white: DEFAULT_REF_WHITE,
        max_luminance: DEFAULT_MAX_LUMINANCE,
        max_fall: DEFAULT_MAX_FALL as u16,
    };

    /// The configuration an output starts with, read from the environment.
    pub fn from_env(output: &Output) -> Self {
        if !hdr_requested_for(output) {
            return Self::OFF;
        }
        Self {
            enabled: true,
            reference_white: ref_white(),
            max_luminance: max_luminance(),
            max_fall: max_fall() as u16,
        }
    }

    /// Fills the luminance fields from the EDID's HDR static metadata block.
    ///
    /// Only applied where the environment did not set an explicit value: the
    /// EDID is the panel's own claim, the environment is the operator
    /// overriding it.
    pub fn apply_edid(&mut self, max_luminance: f32, max_fall: f32) {
        if std::env::var_os("PINNACLE_HDR_MAX_LUMINANCE").is_none() && max_luminance > 0.0 {
            self.max_luminance = (max_luminance.round() as i64)
                .clamp(MAX_LUMINANCE_MIN as i64, MAX_LUMINANCE_MAX as i64)
                as u16;
        }
        if std::env::var_os("PINNACLE_HDR_DESC_MAX_FALL").is_none() && max_fall > 0.0 {
            self.max_fall =
                (max_fall.round() as i64).clamp(MAX_FALL_MIN as i64, MAX_FALL_MAX as i64) as u16;
        }
        self.raise_peak_to_paper_white();
    }

    /// Keeps the advertised peak at or above the paper white.
    ///
    /// A peak under the SDR white claims the panel can show less than the desktop
    /// already uses, which asks it to tone map content that is not meant to be
    /// mapped.
    fn raise_peak_to_paper_white(&mut self) {
        let floor = self.reference_white.round().clamp(0.0, u16::MAX as f64) as u16;
        self.max_luminance = self.max_luminance.max(floor);
    }

    /// Applies luminance overrides in nits, clamped to the accepted ranges.
    ///
    /// The protocol carries `u32` while the configuration stores `u16`, so values
    /// are clamped *before* the conversion: a large value would otherwise wrap into
    /// a small one. The advertised peak is raised to the paper white when it would
    /// sit under it, which is the same invariant [`max_luminance`] enforces.
    ///
    /// Returns the fields that had to be adjusted, for the caller to log.
    pub fn apply_luminance_overrides(
        &mut self,
        reference_white: Option<u32>,
        max_luminance: Option<u32>,
        max_fall: Option<u32>,
    ) -> Vec<&'static str> {
        let mut adjusted = Vec::new();

        if let Some(nits) = reference_white {
            let clamped = nits.clamp(REF_WHITE_MIN, REF_WHITE_MAX);
            if clamped != nits {
                adjusted.push("reference_white");
            }
            self.reference_white = f64::from(clamped);
        }
        if let Some(nits) = max_luminance {
            let clamped = nits.clamp(MAX_LUMINANCE_MIN, MAX_LUMINANCE_MAX);
            if clamped != nits {
                adjusted.push("max_luminance");
            }
            self.max_luminance = clamped as u16;
        }
        if let Some(nits) = max_fall {
            let clamped = nits.clamp(MAX_FALL_MIN, MAX_FALL_MAX);
            if clamped != nits {
                adjusted.push("max_fall");
            }
            self.max_fall = clamped as u16;
        }

        let before = self.max_luminance;
        self.raise_peak_to_paper_white();
        if self.max_luminance != before {
            adjusted.push("max_luminance (raised to the paper white)");
        }

        adjusted
    }

    /// The image description this output advertises to clients.
    pub fn description(&self) -> ImageDescription {
        ImageDescription {
            transfer: TransferFunction::St2084Pq,
            primaries: PrimariesOption {
                named: Some(Primaries::Bt2020),
                values: None,
            },
            max_cll: Some(self.max_luminance as u32),
            max_fall: Some(self.max_fall as u32),
            mastering_luminance: Some((50, self.max_luminance as u32)),
            mastering_primaries: None,
            luminances: Some((50, self.max_luminance as u32, protocol_reference_white())),
            windows_scrgb: false,
            windows_bt2100: false,
        }
    }

    /// The texture program that converts sRGB elements for this configuration.
    pub fn shader(&self) -> String {
        texture_shader(self.reference_white)
    }
}

/// Reference white the output description reports, in nits.
///
/// Separate from [`ref_white`]: that one decides where *this compositor* puts SDR
/// white when it encodes the desktop, while this one is what a client reads. The
/// knob exists because a client can use it for its own paper white, and this is
/// the cheapest way to find out whether it does.
pub fn protocol_reference_white() -> u32 {
    env_number(
        "PINNACLE_HDR_DESC_REF_WHITE",
        PROTOCOL_REFERENCE_WHITE as f64,
        1.0,
        10_000.0,
    )
    .round() as u32
}

/// Full-frame average peak the output description reports, in nits.
pub fn max_fall() -> u32 {
    env_number(
        "PINNACLE_HDR_DESC_MAX_FALL",
        DEFAULT_MAX_FALL as f64,
        MAX_FALL_MIN as f64,
        MAX_FALL_MAX as f64,
    )
    .round() as u32
}

/// The line [`texture_shader`] rewrites to bake in the reference white.
///
/// Keeping the default in the template means the template itself is the shader
/// the numeric oracle verified, and the rewritten variants only differ in this
/// one numeric literal.
const SHADER_REF_WHITE_LINE: &str = "const float REF_WHITE = 203.0;";

/// GLSL ES 1.00 shader template that converts a texture from sRGB into BT.2020/PQ.
///
/// [`texture_shader`] bakes the configured reference white into it; the literal
/// below is [`DEFAULT_REF_WHITE`].
///
/// The `//_DEFINES_` line is required by [`smithay::backend::renderer::gles::GlesRenderer::compile_custom_texture_shader`]:
/// it is replaced with the renderer's own defines. Uniform and varying names must
/// match the default texture shader (`tex`, `alpha`, `v_coords`).
pub const HDR_TEXTURE_SHADER_TEMPLATE: &str = r#"#version 100

//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif

precision highp float;

#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;

varying vec2 v_coords;

const float REF_WHITE = 203.0;

const float PQ_M1 = 2610.0 / 16384.0;
const float PQ_M2 = 2523.0 / 4096.0 * 128.0;
const float PQ_C1 = 3424.0 / 4096.0;
const float PQ_C2 = 2413.0 / 4096.0 * 32.0;
const float PQ_C3 = 2392.0 / 4096.0 * 32.0;

// BT.2087 Annex 1: BT.709 -> BT.2020, linear light, D65 to D65.
const mat3 BT709_TO_BT2020 = mat3(
    0.6274040, 0.0690970, 0.0163916,
    0.3292820, 0.9195400, 0.0880132,
    0.0433136, 0.0113612, 0.8955950
);

vec3 srgb_to_linear(vec3 c) {
    vec3 lo = c / 12.92;
    vec3 hi = pow((c + 0.055) / 1.055, vec3(2.4));
    return mix(lo, hi, step(vec3(0.04045), c));
}

vec3 pq_encode(vec3 nits) {
    vec3 y = clamp(nits / 10000.0, 0.0, 1.0);
    vec3 ym = pow(y, vec3(PQ_M1));
    vec3 n = (PQ_C1 + PQ_C2 * ym) / (1.0 + PQ_C3 * ym);
    return pow(n, vec3(PQ_M2));
}

void main() {
    vec4 color = texture2D(tex, v_coords);

#if defined(NO_ALPHA)
    color = vec4(color.rgb, 1.0);
#endif

    // The texture is premultiplied and PQ is nonlinear, so unpremultiply first.
    vec3 straight = color.rgb / max(color.a, 1.0 / 255.0);
    vec3 lin = srgb_to_linear(straight);
    vec3 lin2020 = BT709_TO_BT2020 * lin;
    vec3 pq = pq_encode(lin2020 * REF_WHITE);

    gl_FragColor = vec4(pq * color.a, color.a) * alpha;
}
"#;

/// The shader above with `ref_white` baked in as the reference white.
///
/// Smithay's custom texture programs expose only the default uniforms (`tex`,
/// `alpha`), so the value cannot be a uniform; it is compiled per output instead,
/// which is once per surface, not once per frame.
pub fn texture_shader(ref_white: f64) -> String {
    HDR_TEXTURE_SHADER_TEMPLATE.replace(
        SHADER_REF_WHITE_LINE,
        &format!("const float REF_WHITE = {ref_white:.1};"),
    )
}

/// GLSL ES 1.00 shader template that converts an already-composed BT.2020/PQ
/// frame back into sRGB.
///
/// The exact inverse of [`HDR_TEXTURE_SHADER_TEMPLATE`]: PQ decode, BT.2020 to
/// BT.709, divide by the paper white, sRGB encode. Both capture protocols use
/// it, so the pixels a client receives match what the same content looks like at
/// that paper white in SDR. Everything above the paper white clips; there is no
/// tone mapping, because a capture has no headroom to map to.
pub const PQ_TO_SRGB_TEXTURE_SHADER_TEMPLATE: &str = r#"#version 100

//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif

precision highp float;

#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;

varying vec2 v_coords;

const float REF_WHITE = 203.0;

const float PQ_M1 = 2610.0 / 16384.0;
const float PQ_M2 = 2523.0 / 4096.0 * 128.0;
const float PQ_C1 = 3424.0 / 4096.0;
const float PQ_C2 = 2413.0 / 4096.0 * 32.0;
const float PQ_C3 = 2392.0 / 4096.0 * 32.0;

// The exact inverse of the BT.709 -> BT.2020 matrix in the forward shader.
const mat3 BT2020_TO_BT709 = mat3(
    1.6604903, -0.1245500, -0.0181511,
    -0.5876391, 1.1328999, -0.1005787,
    -0.0728516, -0.0083480, 1.1187299
);

vec3 pq_decode(vec3 v) {
    vec3 e = pow(clamp(v, 0.0, 1.0), vec3(1.0 / PQ_M2));
    vec3 n = max(e - PQ_C1, 0.0) / max(PQ_C2 - PQ_C3 * e, 1e-6);
    return pow(n, vec3(1.0 / PQ_M1));
}

vec3 linear_to_srgb(vec3 c) {
    vec3 lo = c * 12.92;
    vec3 hi = 1.055 * pow(c, vec3(1.0 / 2.4)) - 0.055;
    return mix(lo, hi, step(vec3(0.0031308), c));
}

void main() {
    vec4 color = texture2D(tex, v_coords);

    // The composed frame is premultiplied and PQ is nonlinear, so unpremultiply
    // before decoding and premultiply the sRGB result afterwards. This is the
    // same order [`pq_bgra_to_srgb_in_place`] uses for the CPU path.
    vec3 pq = color.rgb / max(color.a, 1.0 / 255.0);

    // PQ carries absolute luminance: decode to nits, then express it relative to
    // the paper white the compositor encoded sRGB 1.0 at.
    vec3 nits = pq_decode(pq) * 10000.0;
    vec3 lin2020 = nits / REF_WHITE;
    vec3 lin709 = BT2020_TO_BT709 * lin2020;
    vec3 srgb = linear_to_srgb(clamp(lin709, 0.0, 1.0));

    gl_FragColor = vec4(srgb * color.a, color.a) * alpha;
}
"#;

/// [`PQ_TO_SRGB_TEXTURE_SHADER_TEMPLATE`] with `ref_white` baked in.
pub fn inverse_texture_shader(ref_white: f64) -> String {
    PQ_TO_SRGB_TEXTURE_SHADER_TEMPLATE.replace(
        SHADER_REF_WHITE_LINE,
        &format!("const float REF_WHITE = {ref_white:.1};"),
    )
}

/// The CPU twin of [`inverse_texture_shader`], for tests and diagnostics.
pub fn pq_to_srgb(pq: [f64; 3], ref_white: f64) -> [f64; 3] {
    const M1: f64 = 2610.0 / 16384.0;
    const M2: f64 = 2523.0 / 4096.0 * 128.0;
    const C1: f64 = 3424.0 / 4096.0;
    const C2: f64 = 2413.0 / 4096.0 * 32.0;
    const C3: f64 = 2392.0 / 4096.0 * 32.0;
    const M: [[f64; 3]; 3] = [
        [1.6604903, -0.5876391, -0.0728516],
        [-0.1245500, 1.1328999, -0.0083480],
        [-0.0181511, -0.1005787, 1.1187299],
    ];

    let decode = |v: f64| {
        let e = v.clamp(0.0, 1.0).powf(1.0 / M2);
        let n = (e - C1).max(0.0) / (C2 - C3 * e).max(1e-6);
        n.powf(1.0 / M1) * 10_000.0
    };

    let nits = [decode(pq[0]), decode(pq[1]), decode(pq[2])];
    let lin2020 = [
        nits[0] / ref_white,
        nits[1] / ref_white,
        nits[2] / ref_white,
    ];

    let mut out = [0.0; 3];
    for (i, row) in M.iter().enumerate() {
        let lin = (row[0] * lin2020[0] + row[1] * lin2020[1] + row[2] * lin2020[2]).clamp(0.0, 1.0);
        out[i] = if lin <= 0.0031308 {
            lin * 12.92
        } else {
            1.055 * lin.powf(1.0 / 2.4) - 0.055
        };
    }
    out
}

/// Converts premultiplied BGRA pixels from BT.2020/PQ to sRGB in place.
///
/// This is what a capture of an HDR output goes through: the composed frame is
/// PQ, and neither wlr-screencopy nor image-copy-capture carries a colour
/// description, so the client gets sRGB. `bytes` are in the layout
/// `copy_framebuffer` produces for `Argb8888`, so the channel order in memory is
/// B, G, R, A.
///
/// The frame is premultiplied in the PQ domain (that is how the forward shader
/// writes it), so the colour is straightened before decoding and premultiplied
/// again afterwards. Without that a translucent window or a rounded corner would
/// composite darker than the same content does on screen.
pub fn pq_bgra_to_srgb_in_place(bytes: &mut [u8], ref_white: f64) {
    for px in bytes.chunks_exact_mut(4) {
        let alpha = px[3] as f64 / 255.0;
        if alpha <= 0.0 {
            px[0] = 0;
            px[1] = 0;
            px[2] = 0;
            continue;
        }
        let pq = [
            px[2] as f64 / 255.0 / alpha,
            px[1] as f64 / 255.0 / alpha,
            px[0] as f64 / 255.0 / alpha,
        ];
        let srgb = pq_to_srgb(pq, ref_white);
        px[2] = (srgb[0] * alpha * 255.0).round().clamp(0.0, 255.0) as u8;
        px[1] = (srgb[1] * alpha * 255.0).round().clamp(0.0, 255.0) as u8;
        px[0] = (srgb[2] * alpha * 255.0).round().clamp(0.0, 255.0) as u8;
    }
}

fn env_number(key: &str, default: f64, min: f64, max: f64) -> f64 {
    let Ok(value) = std::env::var(key) else {
        return default;
    };
    match value.trim().parse::<f64>() {
        Ok(nits) if nits.is_finite() => nits.clamp(min, max),
        Ok(_) => default,
        Err(err) => {
            tracing::warn!("{key}: not a number ({err}), using {default}");
            default
        }
    }
}

fn env_u64(key: &str, default: u64) -> u64 {
    let Ok(value) = std::env::var(key) else {
        return default;
    };
    match value.trim().parse::<u64>() {
        Ok(n) => n,
        Err(err) => {
            tracing::warn!("{key}: not an integer ({err}), using {default}");
            default
        }
    }
}

/// Luminance 1.0 sRGB maps to on HDR outputs, in nits.
///
/// `PINNACLE_HDR_REF_WHITE` overrides it; the value is clamped to 80..=1000 nits,
/// the range where a PQ-encoded SDR white still leaves highlight headroom.
pub fn ref_white() -> f64 {
    env_number(
        "PINNACLE_HDR_REF_WHITE",
        DEFAULT_REF_WHITE,
        REF_WHITE_MIN as f64,
        REF_WHITE_MAX as f64,
    )
}

/// Peak luminance advertised to the connector in the HDR metadata, in nits.
///
/// `PINNACLE_HDR_MAX_LUMINANCE` overrides it. It is never below [`ref_white`],
/// because advertising a peak under the SDR white would ask the panel to tone
/// map content that is not supposed to be mapped.
pub fn max_luminance() -> u16 {
    let nits = env_number(
        "PINNACLE_HDR_MAX_LUMINANCE",
        DEFAULT_MAX_LUMINANCE as f64,
        MAX_LUMINANCE_MIN as f64,
        MAX_LUMINANCE_MAX as f64,
    );
    (nits.round() as u16).max(ref_white().round() as u16)
}

fn srgb_to_linear_one(c: f64) -> f64 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn pq_encode_one(nits: f64) -> f64 {
    const M1: f64 = 2610.0 / 16384.0;
    const M2: f64 = 2523.0 / 4096.0 * 128.0;
    const C1: f64 = 3424.0 / 4096.0;
    const C2: f64 = 2413.0 / 4096.0 * 32.0;
    const C3: f64 = 2392.0 / 4096.0 * 32.0;
    let y = (nits / 10000.0).clamp(0.0, 1.0);
    let ym = y.powf(M1);
    ((C1 + C2 * ym) / (1.0 + C3 * ym)).powf(M2)
}

/// sRGB-encoded RGB in 0..1 to BT.2020 PQ code, with 1.0 sRGB = `ref_white` nits.
pub fn srgb_to_pq(rgb: [f64; 3], ref_white: f64) -> [f64; 3] {
    const M: [[f64; 3]; 3] = [
        [0.6274040, 0.3292820, 0.0433136],
        [0.0690970, 0.9195400, 0.0113612],
        [0.0163916, 0.0880132, 0.8955950],
    ];
    let lin = [
        srgb_to_linear_one(rgb[0]),
        srgb_to_linear_one(rgb[1]),
        srgb_to_linear_one(rgb[2]),
    ];
    let lin2020 = [
        M[0][0] * lin[0] + M[0][1] * lin[1] + M[0][2] * lin[2],
        M[1][0] * lin[0] + M[1][1] * lin[1] + M[1][2] * lin[2],
        M[2][0] * lin[0] + M[2][1] * lin[1] + M[2][2] * lin[2],
    ];
    [
        pq_encode_one(lin2020[0] * ref_white),
        pq_encode_one(lin2020[1] * ref_white),
        pq_encode_one(lin2020[2] * ref_white),
    ]
}

/// Convert a premultiplied sRGB colour into premultiplied BT.2020/PQ.
///
/// Installed as the renderer's solid-colour transform, which covers
/// `Frame::clear` and every `SolidColorRenderElement`. Those never reach the
/// texture shader, and the transform is applied by Smithay to the SDR colour, so
/// the caller must pass the clear colour unconverted.
pub fn premultiplied_srgb_to_pq(color: Color32F, ref_white: f64) -> Color32F {
    let a = color.a() as f64;
    if a <= 0.0 {
        return Color32F::TRANSPARENT;
    }
    let rgb = [
        color.r() as f64 / a,
        color.g() as f64 / a,
        color.b() as f64 / a,
    ];
    let pq = srgb_to_pq(rgb, ref_white);
    Color32F::new(
        (pq[0] * a) as f32,
        (pq[1] * a) as f32,
        (pq[2] * a) as f32,
        color.a(),
    )
}

/// The image description of an HDR output we drive in BT.2020/PQ mode, using
/// the environment's values. Outputs carry their own [`HdrConfig`]; this is for
/// the paths that have no output at hand (tests, diagnostics).
pub fn bt2020_pq_description() -> ImageDescription {
    HdrConfig {
        enabled: true,
        reference_white: ref_white(),
        max_luminance: max_luminance(),
        max_fall: max_fall() as u16,
    }
    .description()
}

/// The file whose existence requests one dump right now.
pub const TRIGGER_FILE: &str = "dump";

/// Whether HDR diagnostics were requested (`PINNACLE_HDR_LOG`).
///
/// When on, the compositor logs the scanout decision for HDR outputs and every
/// colour description a client sets. Off by default: the per-frame path should
/// not print.
pub fn verbose() -> bool {
    matches!(
        std::env::var_os("PINNACLE_HDR_LOG").as_deref().and_then(std::ffi::OsStr::to_str),
        Some(value) if !value.is_empty() && value != "0"
    )
}

/// Single-shot or rate-limited framebuffer dumps for the on-glass checkpoints.
///
/// A dump re-renders the whole frame into an offscreen buffer, waits for the GPU,
/// reads the result back to the CPU and writes a PPM. At 4K that is a full second
/// render pass, a pipeline stall and a 25 MB write, so dumping every frame caps the
/// whole compositor at the speed of the readback (10 fps and a stutter on every
/// output, since all outputs share the render loop). It therefore fires once,
/// after `PINNACLE_HDR_DUMP_AFTER` presented frames (default 180), and only more
/// often if `PINNACLE_HDR_DUMP_EVERY` says so.
#[derive(Debug)]
pub struct FrameDump {
    dir: PathBuf,
    trigger: PathBuf,
    every: u64,
    presented: u64,
    next: u64,
}

impl FrameDump {
    /// `PINNACLE_HDR_DUMP=<dir>` enables dumping; unset or empty disables it.
    pub fn from_env() -> Option<Self> {
        let dir = std::env::var("PINNACLE_HDR_DUMP").ok()?;
        let dir = dir.trim();
        if dir.is_empty() {
            return None;
        }
        let dir = PathBuf::from(dir);
        let trigger = dir.join(TRIGGER_FILE);
        Some(Self {
            dir,
            trigger,
            every: env_u64("PINNACLE_HDR_DUMP_EVERY", 0),
            presented: 0,
            next: env_u64("PINNACLE_HDR_DUMP_AFTER", 180),
        })
    }

    /// Count a presented frame and say whether this one is dumped.
    ///
    /// A file called `dump` inside the dump directory triggers a dump on the next
    /// frame and is removed, which is the only practical way to capture a frame
    /// that is on screen right now: the interesting content is rarely up at frame
    /// 180. The check is one `stat` per frame.
    ///
    /// Separate from [`FrameDump::path_for`] so the per-frame path allocates
    /// nothing unless a dump is actually due.
    pub fn poll(&mut self) -> bool {
        if self.trigger.exists() {
            let _ = std::fs::remove_file(&self.trigger);
            return true;
        }
        self.presented += 1;
        if self.presented < self.next {
            return false;
        }
        self.next = if self.every == 0 {
            u64::MAX
        } else {
            self.presented.saturating_add(self.every)
        };
        true
    }

    /// The file a dump of `output_name` is written to.
    pub fn path_for(&self, output_name: &str) -> PathBuf {
        self.dir.join(format!("{output_name}-hdr.ppm"))
    }
}

/// Whether these pixels are already in the encoding we drive an HDR output with.
///
/// Only PQ with BT.2020 primaries qualifies: that is what the connector is told to
/// expect, so the client's pixels can be copied into the framebuffer instead of
/// being converted. A PQ surface in sRGB primaries, an scRGB surface and anything
/// HLG still need a conversion, and are deliberately excluded.
pub fn description_matches_output_encoding(description: &ImageDescription) -> bool {
    if !matches!(description.transfer, TransferFunction::St2084Pq) {
        return false;
    }
    if description.windows_bt2100 {
        return true;
    }
    match description.primaries.named {
        Some(Primaries::Bt2020) => true,
        Some(_) => false,
        // Clients may describe the same primaries with raw coordinates instead of
        // the named set; mpv does exactly that, so a named-only check rejects it.
        None => description
            .primaries
            .values
            .is_some_and(primaries_are_bt2020),
    }
}

/// Whether raw chromaticities are BT.2020, allowing for rounding in the 1e6-encoded
/// coordinates the protocol uses.
fn primaries_are_bt2020(values: Chromaticities) -> bool {
    let target = Chromaticities::from_named(Primaries::Bt2020);
    let close = |a: (i32, i32), b: (i32, i32)| (a.0 - b.0).abs() <= 500 && (a.1 - b.1).abs() <= 500;
    close(values.red, target.red)
        && close(values.green, target.green)
        && close(values.blue, target.blue)
        && close(values.white, target.white)
}

/// Whether a client surface declares the encoding this output is driven with.
pub fn surface_matches_output_encoding(
    surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
) -> bool {
    smithay::wayland::color::management::get_surface_description(surface)
        .0
        .is_some_and(|description| description_matches_output_encoding(&description))
}

/// Short tag for a client's description, for the HDR log.
pub fn description_tag(description: &ImageDescription) -> String {
    let mut tag = format!(
        "{:?}/{:?}",
        description.transfer, description.primaries.named
    );
    if description.windows_bt2100 {
        tag.push_str("+bt2100");
    } else if description.windows_scrgb {
        tag.push_str("+scrgb");
    }
    if description_matches_output_encoding(description) {
        tag.push_str("=native");
    }
    tag
}

/// The encoding a surface is actually composited with.
///
/// A surface's own description wins; a surface without one inherits the nearest
/// ancestor's, because a subsurface is part of the same buffer chain as its parent
/// and clients rely on that.
///
/// **Never call this from inside a surface-tree traversal.** The traversal holds the
/// user-data mutex of every surface in the subtree, and this takes it again per
/// surface, which deadlocks the compositor with the display frozen mid-frame rather
/// than failing. Collect the surfaces first and call it afterwards.
pub fn surface_description_in_tree(
    surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
) -> Option<ImageDescription> {
    let mut current = Some(surface.clone());
    while let Some(surface) = current {
        if let Some(description) =
            smithay::wayland::color::management::get_surface_description(&surface).0
        {
            return Some(description);
        }
        current = smithay::wayland::compositor::get_parent(&surface);
    }
    None
}

/// The surfaces of these windows whose committed description is already in the
/// output's BT.2020/PQ encoding, grouped the way the HDR log prints them.
///
/// The description is looked up per surface, not per window: clients attach it to
/// the surface they render into, which is not always the window's own surface
/// (Wine puts it on the swapchain's surface), and a surface without one inherits
/// the nearest ancestor's.
///
/// **Never build this from inside a surface-tree traversal.** The traversal holds
/// the user-data mutex of every surface in the subtree and this takes it again per
/// surface, which deadlocks the compositor with the display frozen mid-frame.
#[derive(Default)]
pub struct EncodedSurfaces {
    /// Element ids for `GlesRenderer::set_already_encoded_elements`.
    pub ids: Vec<Id>,
    /// `id:class` of every surface that is already encoded.
    pub marked: Vec<String>,
    /// `id:class:description` of every surface that declares something else.
    pub rejected: Vec<String>,
    /// `class` or `id:class` of every surface that declares nothing.
    pub undescribed: Vec<String>,
}

impl EncodedSurfaces {
    /// Collects the native-encoding surfaces of `windows`, their popups, and the
    /// layer surfaces of `output` together with theirs.
    ///
    /// Layer surfaces and popups are not windows, so they are never visited by the
    /// window traversal; without them the top bar and every tooltip would be drawn
    /// through the SDR conversion shader even when the client declared them native,
    /// and an HDR popup over an HDR window would be encoded twice.
    pub fn for_output<'a>(
        windows: impl IntoIterator<Item = &'a WindowElement>,
        output: &Output,
    ) -> Self {
        let mut encoded = Self::for_windows(windows);
        for layer in layer_map_for_output(output).layers() {
            let surface = layer.wl_surface();
            encoded.add_surface_tree(surface, "layer");
            encoded.add_popups(surface, "layer");
        }
        encoded
    }

    /// Collects the native-encoding surfaces of `windows` and their popups.
    pub fn for_windows<'a>(windows: impl IntoIterator<Item = &'a WindowElement>) -> Self {
        let mut encoded = Self::default();
        for window in windows {
            let class = window.class().unwrap_or_else(|| "?".to_string());
            let Some(toplevel) = window.toplevel() else {
                encoded.undescribed.push(format!("{class}:x11"));
                continue;
            };

            let root = toplevel.wl_surface();
            encoded.add_surface_tree(root, &class);
            encoded.add_popups(root, &class);
        }
        encoded
    }

    /// Adds the popup trees of `root`, which are separate surface trees.
    fn add_popups(&mut self, root: &WlSurface, class: &str) {
        for (popup, _) in PopupManager::popups_for_surface(root) {
            self.add_surface_tree(popup.wl_surface(), &format!("{class}:popup"));
        }
    }

    /// Adds `root` and every subsurface under it, classified by their description.
    fn add_surface_tree(&mut self, root: &WlSurface, class: &str) {
        let mut tree = Vec::new();
        with_surface_tree_downward(
            root,
            (),
            |_, _, _| TraversalAction::DoChildren(()),
            |surface, _, _| tree.push(surface.clone()),
            |_, _, _| true,
        );

        for surface in &tree {
            let id = Resource::id(surface).protocol_id();
            let Some(description) = surface_description_in_tree(surface) else {
                self.undescribed.push(format!("{id}:{class}:none"));
                continue;
            };
            if !description_matches_output_encoding(&description) {
                self.rejected
                    .push(format!("{id}:{class}:{}", description_tag(&description)));
                continue;
            }
            self.marked.push(format!("{id}:{class}"));
            self.ids.push(Id::from_wayland_resource(surface));
        }
    }

    /// A signature of the three groups, so a repeat of the same set prints nothing.
    pub fn signature(&self) -> String {
        format!(
            "{}|{}|{}",
            self.marked.join(","),
            self.rejected.join(","),
            self.undescribed.join(",")
        )
    }
}

/// Whether HDR output was requested for this output by the environment.
///
/// `PINNACLE_HDR` unset or empty disables HDR. `PINNACLE_HDR=1` enables every
/// output that supports the connector properties; a comma-separated list of
/// output names (`PINNACLE_HDR=DP-1,DP-2`) enables just those.
pub fn hdr_requested_for(output: &Output) -> bool {
    let Ok(value) = std::env::var("PINNACLE_HDR") else {
        return false;
    };
    let value = value.trim();
    if value.is_empty() {
        return false;
    }
    if value == "1" || value.eq_ignore_ascii_case("all") {
        return true;
    }
    value
        .split(',')
        .map(str::trim)
        .any(|name| name == output.name())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(got: f64, want: f64) {
        assert!((got - want).abs() < 1e-4, "got {got}, want {want}");
    }
    #[test]
    fn srgb_round_trips_through_pq() {
        // The capture path is the inverse of the composition path; if these drift
        // apart, screenshots of an HDR desktop stop matching the desktop.
        for ref_white in [203.0, 450.0, 600.0] {
            for value in [0.0, 0.02, 0.18, 0.5, 0.75, 1.0] {
                let srgb = [value, value, value];
                let pq = srgb_to_pq(srgb, ref_white);
                let back = pq_to_srgb(pq, ref_white);
                for channel in back {
                    assert!(
                        (channel - value).abs() < 5e-3,
                        "ref_white {ref_white}, value {value}: got {channel}"
                    );
                }
            }
        }
    }

    #[test]
    fn bgra_conversion_round_trips_a_pixel() {
        let ref_white = 450.0;
        let srgb = [0.8, 0.25, 0.5];
        let pq = srgb_to_pq(srgb, ref_white);
        let mut bytes = [
            (pq[2] * 255.0).round() as u8,
            (pq[1] * 255.0).round() as u8,
            (pq[0] * 255.0).round() as u8,
            255,
        ];
        pq_bgra_to_srgb_in_place(&mut bytes, ref_white);
        let got = [
            bytes[2] as f64 / 255.0,
            bytes[1] as f64 / 255.0,
            bytes[0] as f64 / 255.0,
        ];
        for (got, want) in got.iter().zip(srgb) {
            assert!((got - want).abs() < 6e-3, "got {got}, want {want}");
        }
    }

    #[test]
    fn bgra_conversion_keeps_translucent_edges_consistent() {
        // A capture of a translucent window has to composite the way the window
        // does: premultiplied in, premultiplied out.
        let ref_white = DEFAULT_REF_WHITE;
        let srgb = [0.8, 0.25, 0.5];
        let alpha = 0.5;
        let pq = srgb_to_pq(srgb, ref_white);
        let mut bytes = [
            (pq[2] * alpha * 255.0).round() as u8,
            (pq[1] * alpha * 255.0).round() as u8,
            (pq[0] * alpha * 255.0).round() as u8,
            (alpha * 255.0).round() as u8,
        ];
        pq_bgra_to_srgb_in_place(&mut bytes, ref_white);
        let got = [
            bytes[2] as f64 / 255.0 / alpha,
            bytes[1] as f64 / 255.0 / alpha,
            bytes[0] as f64 / 255.0 / alpha,
        ];
        for (got, want) in got.iter().zip(srgb) {
            assert!(
                (got - want).abs() < 2e-2,
                "premultiplied round trip: got {got}, want {want}"
            );
        }

        // Fully transparent pixels stay fully transparent and black, rather than
        // turning into whatever the division by a zero alpha would produce.
        let mut transparent = [3u8, 7, 11, 0];
        pq_bgra_to_srgb_in_place(&mut transparent, ref_white);
        assert_eq!(transparent, [0, 0, 0, 0]);
    }

    #[test]
    fn luminance_overrides_are_clamped_before_the_u16_conversion() {
        // The protocol carries u32 nits; 70000 used to wrap into 4464 and 65536
        // into 0, which is an invalid piece of HDR metadata.
        let mut config = HdrConfig::OFF;
        let adjusted = config.apply_luminance_overrides(Some(0), Some(70_000), Some(65_536));
        assert_eq!(config.reference_white, f64::from(REF_WHITE_MIN));
        assert_eq!(config.max_luminance, MAX_LUMINANCE_MAX as u16);
        assert_eq!(config.max_fall, MAX_FALL_MAX as u16);
        assert_eq!(
            adjusted,
            vec!["reference_white", "max_luminance", "max_fall"]
        );

        let mut config = HdrConfig::OFF;
        let adjusted = config.apply_luminance_overrides(Some(400), Some(300), None);
        assert_eq!(config.reference_white, 400.0);
        assert_eq!(config.max_luminance, 400, "peak raised to the paper white");
        assert_eq!(adjusted, vec!["max_luminance (raised to the paper white)"]);
    }

    #[test]
    fn luminance_overrides_in_range_report_nothing() {
        let mut config = HdrConfig::OFF;
        let adjusted = config.apply_luminance_overrides(Some(203), Some(1000), Some(400));
        assert!(adjusted.is_empty());
        assert_eq!(config.reference_white, 203.0);
        assert_eq!(config.max_luminance, 1000);
        assert_eq!(config.max_fall, 400);
    }

    #[test]
    fn edid_peak_is_raised_above_the_paper_white() {
        // A panel claiming a 100-nit peak cannot also advertise a 450-nit SDR
        // white; the peak has to follow the white.
        let mut config = HdrConfig::OFF;
        config.reference_white = 450.0;
        config.apply_edid(100.0, 80.0);
        assert_eq!(config.max_luminance, 450);
        assert_eq!(config.max_fall, 80);
    }

    #[test]
    fn pq_above_paper_white_clips() {
        // 1000 nits of PQ on a 203-nit paper white has nowhere to go in sRGB.
        let pq = srgb_to_pq([1.0, 1.0, 1.0], 1000.0);
        let out = pq_to_srgb(pq, 203.0);
        assert!(out.iter().all(|c| *c > 1.0 - 1e-3));
    }

    #[test]
    fn golden_values_match_the_oracle() {
        let rw = DEFAULT_REF_WHITE;
        close(srgb_to_pq([1.0, 1.0, 1.0], rw)[0], 0.580689);
        close(srgb_to_pq([128.0 / 255.0; 3], rw)[0], 0.427814);
        close(srgb_to_pq([1.0, 0.0, 0.0], rw)[0], 0.532546);
        close(srgb_to_pq([1.0, 0.0, 0.0], rw)[1], 0.327023);
        close(srgb_to_pq([0.0, 1.0, 0.0], rw)[1], 0.571939);
        close(srgb_to_pq([0.0, 0.0, 1.0], rw)[2], 0.569194);
    }

    /// Raising the reference white must move white up the PQ curve by exactly the
    /// amount the spec formula says, and leave black at black.
    #[test]
    fn reference_white_scales_white_in_pq() {
        let at = |nits| srgb_to_pq([1.0, 1.0, 1.0], nits)[0];
        close(at(100.0), 0.508078);
        close(at(203.0), 0.580689);
        close(at(400.0), 0.652579);
        close(at(1000.0), 0.751827);
        close(srgb_to_pq([0.0, 0.0, 0.0], 400.0)[0], 0.0);
        // 203 -> 406 nits is one stop of light: PQ(406) is the value a doubled
        // paper white has to produce, and it is what the panel then shows as 406.
        assert!(at(406.0) > at(203.0) + 0.07);
    }

    /// The description must report the protocol's reference white, not the
    /// compositor's SDR white, or clients rescale the advertised peak.
    #[test]
    fn description_reports_the_protocol_reference_white() {
        let description = bt2020_pq_description();
        let (min, max, reference) = description.luminances.unwrap();
        assert_eq!(reference, PROTOCOL_REFERENCE_WHITE);
        assert_eq!(min, 50);
        assert!(max >= PROTOCOL_REFERENCE_WHITE);
        assert_eq!(description.max_cll, Some(max));
    }

    /// Only PQ with BT.2020 primaries is a copy; everything else converts.
    #[test]
    fn only_pq_bt2020_counts_as_already_encoded() {
        let pq_2020 = bt2020_pq_description();
        assert!(description_matches_output_encoding(&pq_2020));
        assert!(description_matches_output_encoding(
            &ImageDescription::WINDOWS_BT2100
        ));

        let mut pq_srgb = pq_2020;
        pq_srgb.primaries = PrimariesOption {
            named: Some(Primaries::Srgb),
            values: None,
        };
        assert!(!description_matches_output_encoding(&pq_srgb));

        // Raw coordinates for the same primaries must pass: clients, mpv included,
        // may send set_primaries instead of set_primaries_named.
        let mut pq_raw = pq_2020;
        pq_raw.primaries = PrimariesOption {
            named: None,
            values: Some(Chromaticities::from_named(Primaries::Bt2020)),
        };
        assert!(description_matches_output_encoding(&pq_raw));

        let mut pq_raw_p3 = pq_2020;
        pq_raw_p3.primaries = PrimariesOption {
            named: None,
            values: Some(Chromaticities::from_named(Primaries::DisplayP3)),
        };
        assert!(!description_matches_output_encoding(&pq_raw_p3));

        assert!(!description_matches_output_encoding(
            &ImageDescription::SRGB
        ));
        assert!(!description_matches_output_encoding(
            &ImageDescription::WINDOWS_SCRGB
        ));

        // HLG is HDR, but it is not the encoding the connector is driven with, so
        // its pixels still need converting.
        let mut hlg = pq_2020;
        hlg.transfer = TransferFunction::Hlg;
        assert!(!description_matches_output_encoding(&hlg));

        // BT.2020 primaries with an SDR transfer function: the transfer decides,
        // primaries alone do not.
        let mut bt2020_sdr = pq_2020;
        bt2020_sdr.transfer = TransferFunction::Srgb;
        assert!(!description_matches_output_encoding(&bt2020_sdr));

        // The mirror image: PQ in sRGB primaries is the third combination that is
        // not the connector's encoding.
        let mut srgb_pq = pq_2020;
        srgb_pq.primaries = PrimariesOption {
            named: Some(Primaries::Srgb),
            values: None,
        };
        assert!(!description_matches_output_encoding(&srgb_pq));
    }

    #[test]
    fn premultiplied_round_trip_of_white() {
        let white = Color32F::new(1.0, 1.0, 1.0, 1.0);
        let pq = premultiplied_srgb_to_pq(white, DEFAULT_REF_WHITE);
        close(pq.r() as f64, 0.580689);
        let half = Color32F::new(0.5, 0.5, 0.5, 0.5);
        let pq = premultiplied_srgb_to_pq(half, DEFAULT_REF_WHITE);
        close(pq.r() as f64, 0.580689 * 0.5);
        close(pq.a() as f64, 0.5);
    }

    /// The template must be the shader the oracle verified, and the rewrite must
    /// only touch the reference-white literal.
    #[test]
    fn shader_template_bakes_in_the_reference_white() {
        assert!(HDR_TEXTURE_SHADER_TEMPLATE.contains(SHADER_REF_WHITE_LINE));

        let at_400 = texture_shader(400.0);
        assert!(at_400.contains("const float REF_WHITE = 400.0;"));
        assert_eq!(
            at_400.replace("400.0;", "203.0;"),
            HDR_TEXTURE_SHADER_TEMPLATE
        );
        assert_eq!(
            texture_shader(DEFAULT_REF_WHITE),
            HDR_TEXTURE_SHADER_TEMPLATE
        );
        // The one line Smithay requires to substitute its own defines.
        assert!(at_400.lines().any(|line| line.trim() == "//_DEFINES_"));
    }

    #[test]
    fn frame_dump_fires_once_after_the_delay() {
        let mut dump = FrameDump {
            dir: PathBuf::from("/tmp/hdr"),
            trigger: PathBuf::from("/tmp/hdr/definitely-not-here"),
            every: 0,
            presented: 0,
            next: 2,
        };
        assert!(!dump.poll());
        assert!(dump.poll());
        assert_eq!(
            dump.path_for("DP-2"),
            PathBuf::from("/tmp/hdr/DP-2-hdr.ppm")
        );
        for _ in 0..100 {
            assert!(!dump.poll());
        }
    }

    #[test]
    fn frame_dump_rate_limits_when_asked_to_repeat() {
        let mut dump = FrameDump {
            dir: PathBuf::from("/tmp/hdr"),
            trigger: PathBuf::from("/tmp/hdr/definitely-not-here"),
            every: 3,
            presented: 0,
            next: 1,
        };
        let mut fired = Vec::new();
        for frame in 1..=10 {
            if dump.poll() {
                fired.push(frame);
            }
        }
        assert_eq!(fired, vec![1, 4, 7, 10]);
    }

    /// A trigger file fires a dump immediately and is consumed, so the next frame
    /// is not dumped again.
    #[test]
    fn frame_dump_trigger_file_fires_once() {
        let dir = std::env::temp_dir().join(format!("pinnacle-hdr-trigger-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut dump = FrameDump {
            dir: dir.clone(),
            trigger: dir.join(TRIGGER_FILE),
            every: 0,
            presented: 0,
            next: 1_000_000,
        };

        assert!(!dump.poll());
        std::fs::write(&dump.trigger, b"").unwrap();
        assert!(dump.poll());
        assert!(!dump.trigger.exists(), "trigger must be consumed");
        assert!(!dump.poll());
        std::fs::remove_dir_all(&dir).ok();
    }
}
