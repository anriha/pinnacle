//! Compiles the HDR shaders on a real GLES renderer and checks the GL inverse
//! against its CPU twin.
//!
//! These tests need a GPU or a software GLES driver; when the machine has none
//! (a bare CI container, for instance) they report the reason and do nothing
//! rather than failing, because the shaders are also covered numerically by the
//! unit tests in `src/hdr.rs`.

use std::panic::{AssertUnwindSafe, catch_unwind};

use pinnacle::hdr::{
    inverse_texture_shader, linear_texture_shader, linear_to_pq, pq_to_srgb, srgb_to_pq,
    texture_shader,
};
use smithay::{
    backend::{
        allocator::Fourcc,
        egl::{EGLContext, EGLDisplay as EglDisplay, native::EGLSurfacelessDisplay},
        renderer::{
            Bind, ExportMem, Frame, ImportMem, Offscreen, Renderer,
            gles::{GlesRenderbuffer, GlesRenderer, GlesTexProgram, GlesTexture},
        },
    },
    utils::{Rectangle, Size, Transform},
};

/// A GLES renderer on a surfaceless EGL display, if this machine can make one.
///
/// Smithay loads libEGL through a `OnceCell` that panics when the library is
/// missing, and a missing library is a machine that cannot run these tests, so
/// the panic is caught and reported as a skip.
fn gles_renderer() -> Option<GlesRenderer> {
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let created = catch_unwind(AssertUnwindSafe(|| unsafe {
        let display = EglDisplay::new(EGLSurfacelessDisplay).map_err(|err| err.to_string())?;
        let context = EGLContext::new(&display).map_err(|err| err.to_string())?;
        GlesRenderer::new(context).map_err(|err| err.to_string())
    }));
    std::panic::set_hook(hook);

    match created {
        Ok(Ok(renderer)) => Some(renderer),
        Ok(Err(err)) => {
            eprintln!("skipping GL test: no usable EGL/GLES: {err}");
            None
        }
        Err(_) => {
            eprintln!("skipping GL test: libEGL is not available");
            None
        }
    }
}

fn compile(renderer: &mut GlesRenderer, source: String) -> GlesTexProgram {
    renderer
        .compile_custom_texture_shader(source, &[])
        .expect("shader compiles")
}

/// Draws `source` through `program` into one pixel and returns the BGRA codes.
fn render_through(
    renderer: &mut GlesRenderer,
    program: &GlesTexProgram,
    source: &GlesTexture,
) -> [u8; 4] {
    let size = Size::<i32, smithay::utils::Buffer>::from((1, 1));
    let mut offscreen: GlesRenderbuffer = renderer
        .create_buffer(Fourcc::Argb8888, size)
        .expect("offscreen buffer");
    let mut target = renderer.bind(&mut offscreen).expect("bind target");

    {
        let mut frame = renderer
            .render(&mut target, (1, 1).into(), Transform::Normal)
            .expect("frame");
        let physical = Rectangle::<i32, smithay::utils::Physical>::from_size((1, 1).into());
        frame
            .clear(
                smithay::backend::renderer::Color32F::TRANSPARENT,
                &[physical],
            )
            .expect("clear");
        frame
            .render_texture_from_to(
                source,
                Rectangle::<f64, smithay::utils::Buffer>::from_size(size.to_f64()),
                physical,
                &[physical],
                &[],
                Transform::Normal,
                1.0,
                Some(program),
                &[],
            )
            .expect("draw");
        let _ = frame.finish();
    }

    let mapping = renderer
        .copy_framebuffer(
            &target,
            Rectangle::<i32, smithay::utils::Buffer>::from_size(size),
            Fourcc::Argb8888,
        )
        .expect("readback");

    let bytes = renderer.map_texture(&mapping).expect("map").to_vec();
    [bytes[0], bytes[1], bytes[2], bytes[3]]
}

fn import_pixel(renderer: &mut GlesRenderer, pixel: [u8; 4]) -> GlesTexture {
    renderer
        .import_memory(
            &pixel,
            Fourcc::Argb8888,
            Size::<i32, smithay::utils::Buffer>::from((1, 1)),
            false,
        )
        .expect("import")
}

/// The linear path has to encode 0.18 as 14.4 nits, not the 5.5 the sRGB
/// program would make of it. The readback is 8-bit PQ, so the tolerance is two
/// codes; the gap between the two programs is more than ten, which is what makes
/// this a discriminating test rather than a smoke test.
#[test]
fn linear_shader_encodes_scRGB_at_eighty_nits() {
    let Some(mut renderer) = gles_renderer() else {
        return;
    };

    let program = compile(&mut renderer, linear_texture_shader());
    let source = import_pixel(&mut renderer, [46, 46, 46, 255]);
    let out = render_through(&mut renderer, &program, &source);

    let value = 46.0 / 255.0;
    let expected = linear_to_pq([value; 3])[0];
    let got = out[0] as f64 / 255.0;
    assert!(
        (got - expected).abs() <= 2.0 / 255.0,
        "linear 0.18: got {got}, the CPU twin says {expected}"
    );

    let srgb = srgb_to_pq([value; 3], 203.0)[0];
    assert!(
        (got - srgb).abs() > 5.0 / 255.0,
        "the linear program must not agree with the sRGB one: {got} vs {srgb}"
    );
}

#[test]
fn hdr_shaders_match_the_cpu_conversion() {
    let Some(mut renderer) = gles_renderer() else {
        return;
    };

    // Every shader shape the compositor compiles at runtime.
    compile(&mut renderer, texture_shader(203.0));
    compile(&mut renderer, texture_shader(400.0));
    compile(&mut renderer, inverse_texture_shader(203.0));
    compile(&mut renderer, inverse_texture_shader(400.0));
    compile(&mut renderer, linear_texture_shader());

    // Opaque and translucent PQ, as the capture path hands them over.
    let cases: [(&str, [u8; 4], f32, f64); 3] = [
        ("paper white", [148, 148, 148, 255], 0.58, 1.0),
        ("a thousand nits", [191, 191, 191, 255], 0.75, 1.0),
        (
            "translucent half white",
            [74, 74, 74, 128],
            74.0 / 128.0,
            128.0 / 255.0,
        ),
    ];

    let program = compile(&mut renderer, inverse_texture_shader(203.0));
    for (case, pixel, pq, alpha) in cases {
        let source = import_pixel(&mut renderer, pixel);
        let out = render_through(&mut renderer, &program, &source);

        let expected = pq_to_srgb([pq as f64; 3], 203.0);
        for (channel, expected) in out[..3].iter().zip(expected) {
            let got = *channel as f64 / 255.0;
            let expected = expected * alpha;
            // The shader and the CPU twin are the same arithmetic in different
            // precisions and the input is quantised to a byte; one code of
            // slack covers both, and more would hide a real difference.
            assert!(
                (got - expected).abs() <= 1.5 / 255.0,
                "{case}: got {got}, the CPU twin says {expected}"
            );
        }

        let got_alpha = out[3] as f64 / 255.0;
        assert!(
            (got_alpha - alpha).abs() <= 1.5 / 255.0,
            "{case}: alpha {got_alpha}, expected {alpha}"
        );
    }
}
