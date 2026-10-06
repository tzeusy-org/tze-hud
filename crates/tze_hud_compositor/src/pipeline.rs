//! Render pipeline — vertex types and shader configuration.

use bytemuck::{Pod, Zeroable};

/// Vertex for rendering colored rectangles.
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub(crate) struct RectVertex {
    pub(crate) position: [f32; 2],
    pub(crate) color: [f32; 4],
}

impl RectVertex {
    pub(crate) fn desc() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<RectVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                wgpu::VertexAttribute {
                    offset: 0,
                    shader_location: 0,
                    format: wgpu::VertexFormat::Float32x2,
                },
                wgpu::VertexAttribute {
                    offset: std::mem::size_of::<[f32; 2]>() as wgpu::BufferAddress,
                    shader_location: 1,
                    format: wgpu::VertexFormat::Float32x4,
                },
            ],
        }
    }
}

/// Generate vertices for a filled rectangle.
/// Coordinates are in NDC: x in [-1, 1], y in [-1, 1].
pub(crate) fn rect_vertices(
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    screen_w: f32,
    screen_h: f32,
    color: [f32; 4],
) -> [RectVertex; 6] {
    // Convert from pixel coordinates to NDC
    let left = (x / screen_w) * 2.0 - 1.0;
    let right = ((x + w) / screen_w) * 2.0 - 1.0;
    let top = 1.0 - (y / screen_h) * 2.0;
    let bottom = 1.0 - ((y + h) / screen_h) * 2.0;

    [
        // Triangle 1
        RectVertex {
            position: [left, top],
            color,
        },
        RectVertex {
            position: [right, top],
            color,
        },
        RectVertex {
            position: [left, bottom],
            color,
        },
        // Triangle 2
        RectVertex {
            position: [right, top],
            color,
        },
        RectVertex {
            position: [right, bottom],
            color,
        },
        RectVertex {
            position: [left, bottom],
            color,
        },
    ]
}

/// Vertex for rendering textured rectangles (images).
///
/// Carries position (NDC), UV coordinates for texture sampling, and a tint
/// color that is multiplied with the sampled texel. A tint of `[1,1,1,1]`
/// renders the texture unmodified; a tint with `a < 1` can be used for
/// fade-in/fade-out animations.
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub(crate) struct TexturedRectVertex {
    pub(crate) position: [f32; 2],
    pub(crate) uv: [f32; 2],
    pub(crate) tint: [f32; 4],
}

impl TexturedRectVertex {
    pub(crate) fn desc() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<TexturedRectVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                // position: vec2<f32> at location 0
                wgpu::VertexAttribute {
                    offset: 0,
                    shader_location: 0,
                    format: wgpu::VertexFormat::Float32x2,
                },
                // uv: vec2<f32> at location 1
                wgpu::VertexAttribute {
                    offset: std::mem::size_of::<[f32; 2]>() as wgpu::BufferAddress,
                    shader_location: 1,
                    format: wgpu::VertexFormat::Float32x2,
                },
                // tint: vec4<f32> at location 2
                wgpu::VertexAttribute {
                    offset: (std::mem::size_of::<[f32; 2]>() + std::mem::size_of::<[f32; 2]>())
                        as wgpu::BufferAddress,
                    shader_location: 2,
                    format: wgpu::VertexFormat::Float32x4,
                },
            ],
        }
    }
}

/// Generate vertices for a textured rectangle.
///
/// Coordinates are in pixels; they are converted to NDC internally.
/// `uv_rect` is `(u_min, v_min, u_max, v_max)` — use `(0,0,1,1)` for the
/// full texture, or custom values for fit-mode cropping / letterboxing.
#[allow(clippy::too_many_arguments)]
pub(crate) fn textured_rect_vertices(
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    screen_w: f32,
    screen_h: f32,
    uv_rect: [f32; 4],
    tint: [f32; 4],
) -> [TexturedRectVertex; 6] {
    let left = (x / screen_w) * 2.0 - 1.0;
    let right = ((x + w) / screen_w) * 2.0 - 1.0;
    let top = 1.0 - (y / screen_h) * 2.0;
    let bottom = 1.0 - ((y + h) / screen_h) * 2.0;

    let [u0, v0, u1, v1] = uv_rect;

    [
        // Triangle 1
        TexturedRectVertex {
            position: [left, top],
            uv: [u0, v0],
            tint,
        },
        TexturedRectVertex {
            position: [right, top],
            uv: [u1, v0],
            tint,
        },
        TexturedRectVertex {
            position: [left, bottom],
            uv: [u0, v1],
            tint,
        },
        // Triangle 2
        TexturedRectVertex {
            position: [right, top],
            uv: [u1, v0],
            tint,
        },
        TexturedRectVertex {
            position: [right, bottom],
            uv: [u1, v1],
            tint,
        },
        TexturedRectVertex {
            position: [left, bottom],
            uv: [u0, v1],
            tint,
        },
    ]
}

/// WGSL shader for rendering textured rectangles (images).
///
/// Samples from a 2D texture at the interpolated UV coordinates and multiplies
/// the result by the per-vertex tint color. This enables fade and opacity
/// control without a separate uniform buffer.
pub(crate) const TEXTURE_RECT_SHADER: &str = r#"
@group(0) @binding(0)
var t_texture: texture_2d<f32>;
@group(0) @binding(1)
var s_sampler: sampler;

struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) tint: vec4<f32>,
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) tint: vec4<f32>,
};

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.clip_position = vec4<f32>(in.position, 0.0, 1.0);
    out.uv = in.uv;
    out.tint = in.tint;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let texel = textureSample(t_texture, s_sampler, in.uv);
    return texel * in.tint;
}
"#;

/// Create the bind group layout for the texture rect pipeline.
///
/// Binding 0: 2D float texture (filterable)
/// Binding 1: Filtering sampler
pub(crate) fn create_texture_rect_bind_group_layout(
    device: &wgpu::Device,
) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("image_texture_bgl"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    })
}

/// Create the render pipeline for textured rectangles (image rendering).
pub(crate) fn create_texture_rect_pipeline(
    device: &wgpu::Device,
    bind_group_layout: &wgpu::BindGroupLayout,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("texture_rect_shader"),
        source: wgpu::ShaderSource::Wgsl(TEXTURE_RECT_SHADER.into()),
    });

    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("texture_rect_pipeline_layout"),
        bind_group_layouts: &[bind_group_layout],
        push_constant_ranges: &[],
    });

    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("texture_rect_pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            buffers: &[TexturedRectVertex::desc()],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            strip_index_format: None,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: None,
            polygon_mode: wgpu::PolygonMode::Fill,
            unclipped_depth: false,
            conservative: false,
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
        cache: None,
    })
}

/// A rounded rectangle draw command: an SDF-shaded rectangle with a corner
/// radius, a fill, and an optional inside border.
///
/// Produced for zone backdrops with `backdrop_radius`, tile rounded nodes,
/// notification card borders and the drag highlight. Consumed by the SDF
/// pipeline in `Compositor::encode_rounded_rect_pass`. A border-only shape has
/// a transparent `color` (`[0.0; 4]`).
#[derive(Clone, Debug)]
pub(crate) struct RoundedRectDrawCmd {
    /// Original rounded rectangle shape used by the SDF.
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
    pub(crate) radius: f32,
    /// Fill colour, in `gpu_color` form (premultiplied in overlay mode).
    pub(crate) color: [f32; 4],
    /// Inside border, following the rounded edge.
    pub(crate) border: Option<RoundedRectBorder>,
    /// Optional raster bounds. When present, the draw quad is clipped to this
    /// rectangle while the SDF still evaluates against the original shape.
    pub(crate) clip: Option<RoundedRectClip>,
}

/// An inside border drawn by the SDF shader: the band where
/// `-width < sdf <= 0`, anti-aliased like the fill edge. Its inner edge is the
/// shape offset inward by `width`, so corners stay concentric.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct RoundedRectBorder {
    /// Border width in physical pixels.
    pub(crate) width: f32,
    /// Border colour, in `gpu_color` form like the fill.
    pub(crate) color: [f32; 4],
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RoundedRectClip {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
}

/// Vertex for rendering SDF rounded rectangles.
///
/// The fragment shader receives per-vertex geometry (rect center + half-size +
/// radius + border) and recomputes the SDF at each pixel to produce
/// anti-aliased rounded corners and borders. All positional fields use pixel
/// coordinates; they are converted to NDC in the vertex shader.
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub(crate) struct RoundedRectVertex {
    /// NDC position of this vertex (computed by `rounded_rect_cmd_vertices`).
    pub(crate) position: [f32; 2],
    /// Pixel-space position of this vertex (passed through to fragment shader).
    pub(crate) frag_pos: [f32; 2],
    /// Center of the rectangle in pixel space.
    pub(crate) rect_center: [f32; 2],
    /// Half-size (half-width, half-height) of the rectangle in pixel space.
    pub(crate) rect_half_size: [f32; 2],
    /// Corner radius in pixels.
    pub(crate) radius: f32,
    /// Fill RGBA as returned by `gpu_color` (non-premultiplied in fullscreen
    /// mode; premultiplied in overlay mode).
    pub(crate) color: [f32; 4],
    /// Inside border width in pixels; 0 draws no border.
    pub(crate) border_width: f32,
    /// Border RGBA, same form as `color`.
    pub(crate) border_color: [f32; 4],
}

// RoundedRectVertex size: 2+2+2+2+1+4+1+4 = 18 f32 = 72 bytes (no padding).

impl RoundedRectVertex {
    pub(crate) fn desc() -> wgpu::VertexBufferLayout<'static> {
        use std::mem::size_of;
        const fn at(
            floats: usize,
            location: u32,
            format: wgpu::VertexFormat,
        ) -> wgpu::VertexAttribute {
            wgpu::VertexAttribute {
                offset: (floats * size_of::<f32>()) as wgpu::BufferAddress,
                shader_location: location,
                format,
            }
        }
        const ATTRIBUTES: [wgpu::VertexAttribute; 8] = [
            at(0, 0, wgpu::VertexFormat::Float32x2),  // position
            at(2, 1, wgpu::VertexFormat::Float32x2),  // frag_pos
            at(4, 2, wgpu::VertexFormat::Float32x2),  // rect_center
            at(6, 3, wgpu::VertexFormat::Float32x2),  // rect_half_size
            at(8, 4, wgpu::VertexFormat::Float32),    // radius
            at(9, 5, wgpu::VertexFormat::Float32x4),  // color
            at(13, 6, wgpu::VertexFormat::Float32),   // border_width
            at(14, 7, wgpu::VertexFormat::Float32x4), // border_color
        ];
        wgpu::VertexBufferLayout {
            array_stride: size_of::<RoundedRectVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &ATTRIBUTES,
        }
    }
}

/// Generate 6 vertices (2 triangles) for one rounded-rectangle command, or
/// `None` when the shape or its clip is empty.
///
/// The raster quad covers `cmd.clip` when set, else the shape; the SDF always
/// evaluates against the shape, so a clipped edge does not become a new
/// rounded corner or border. `screen_w` / `screen_h` convert pixels to NDC.
pub(crate) fn rounded_rect_cmd_vertices(
    cmd: &RoundedRectDrawCmd,
    screen_w: f32,
    screen_h: f32,
) -> Option<[RoundedRectVertex; 6]> {
    if cmd.width <= 0.0 || cmd.height <= 0.0 {
        return None;
    }
    let (draw_x, draw_y, draw_w, draw_h) = match cmd.clip {
        Some(c) if c.width <= 0.0 || c.height <= 0.0 => return None,
        Some(c) => (c.x, c.y, c.width, c.height),
        None => (cmd.x, cmd.y, cmd.width, cmd.height),
    };

    // NDC corners
    let left_ndc = (draw_x / screen_w) * 2.0 - 1.0;
    let right_ndc = ((draw_x + draw_w) / screen_w) * 2.0 - 1.0;
    let top_ndc = 1.0 - (draw_y / screen_h) * 2.0;
    let bottom_ndc = 1.0 - ((draw_y + draw_h) / screen_h) * 2.0;

    let (border_width, border_color) = cmd
        .border
        .filter(|b| b.width > 0.0)
        .map_or((0.0, [0.0; 4]), |b| (b.width, b.color));

    let v = |px: f32, py: f32, ndc_x: f32, ndc_y: f32| RoundedRectVertex {
        position: [ndc_x, ndc_y],
        frag_pos: [px, py],
        rect_center: [cmd.x + cmd.width * 0.5, cmd.y + cmd.height * 0.5],
        rect_half_size: [cmd.width * 0.5, cmd.height * 0.5],
        radius: cmd.radius,
        color: cmd.color,
        border_width,
        border_color,
    };

    Some([
        // Triangle 1
        v(draw_x, draw_y, left_ndc, top_ndc),
        v(draw_x + draw_w, draw_y, right_ndc, top_ndc),
        v(draw_x, draw_y + draw_h, left_ndc, bottom_ndc),
        // Triangle 2
        v(draw_x + draw_w, draw_y, right_ndc, top_ndc),
        v(draw_x + draw_w, draw_y + draw_h, right_ndc, bottom_ndc),
        v(draw_x, draw_y + draw_h, left_ndc, bottom_ndc),
    ])
}

/// WGSL shared by both rounded-rect shaders: vertex stage, SDF, and the
/// border/fill split. Each shader appends its own `fs_main`.
macro_rules! rounded_rect_shader_common {
    () => {
        r#"
struct VertexInput {
    @location(0) position:       vec2<f32>,
    @location(1) frag_pos:       vec2<f32>,
    @location(2) rect_center:    vec2<f32>,
    @location(3) rect_half_size: vec2<f32>,
    @location(4) radius:         f32,
    @location(5) color:          vec4<f32>,
    @location(6) border_width:   f32,
    @location(7) border_color:   vec4<f32>,
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) frag_pos:       vec2<f32>,
    @location(1) rect_center:    vec2<f32>,
    @location(2) rect_half_size: vec2<f32>,
    @location(3) radius:         f32,
    @location(4) color:          vec4<f32>,
    @location(5) border_width:   f32,
    @location(6) border_color:   vec4<f32>,
};

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.clip_position  = vec4<f32>(in.position, 0.0, 1.0);
    out.frag_pos       = in.frag_pos;
    out.rect_center    = in.rect_center;
    out.rect_half_size = in.rect_half_size;
    out.radius         = in.radius;
    out.color          = in.color;
    out.border_width   = in.border_width;
    out.border_color   = in.border_color;
    return out;
}

/// Standard 2D SDF for a rounded rectangle.
///
/// `p` — point to evaluate (pixel space, origin = rect center).
/// `b` — half-size of the rectangle (positive).
/// `r` — corner radius.
/// Returns the signed distance: negative inside, positive outside.
fn sdf_rounded_box(p: vec2<f32>, b: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - b + vec2<f32>(r, r);
    return length(max(q, vec2<f32>(0.0, 0.0))) + min(max(q.x, q.y), 0.0) - r;
}

/// Shape coverage (x) and fill weight (y) for this fragment.
///
/// Coverage is a 1 px feather around the outer edge: smoothstep(0.5, -0.5, d)
/// goes 0 -> 1 as d goes +0.5 -> -0.5. The fill weight is the same feather
/// around the inner edge `d = -border_width`, so the border is the band
/// `-border_width < d <= 0` with the fill edge's AA on both sides. Offsetting
/// the SDF keeps inner corners concentric (radius r - border_width).
fn coverage_and_fill(in: VertexOutput) -> vec2<f32> {
    let d = sdf_rounded_box(in.frag_pos - in.rect_center, in.rect_half_size, in.radius);
    let coverage = smoothstep(0.5, -0.5, d);
    var fill = 1.0;
    if (in.border_width > 0.0) {
        fill = smoothstep(0.5, -0.5, d + in.border_width);
    }
    return vec2<f32>(coverage, fill);
}
"#
    };
}

/// WGSL shader for SDF rounded rectangle rendering (fullscreen / straight-alpha mode).
///
/// The pipeline uses `BlendState::ALPHA_BLENDING` (straight alpha), so the
/// output must be non-premultiplied with coverage applied to alpha only;
/// scaling RGB too would apply coverage twice and darken edges. Border and
/// fill are mixed in premultiplied space (so a transparent fill does not tint
/// the border) and un-premultiplied for output.
///
/// In overlay mode use `ROUNDED_RECT_OVERLAY_SHADER` + `PREMULTIPLIED_ALPHA_BLENDING`
/// instead — see `create_rounded_rect_overlay_pipeline`.
pub(crate) const ROUNDED_RECT_SHADER: &str = concat!(
    rounded_rect_shader_common!(),
    r#"
@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let cf = coverage_and_fill(in);
    let a = mix(in.border_color.a, in.color.a, cf.y);
    let rgb_pm = mix(in.border_color.rgb * in.border_color.a, in.color.rgb * in.color.a, cf.y);
    var rgb = vec3<f32>(0.0, 0.0, 0.0);
    if (a > 0.0) {
        rgb = rgb_pm / a;
    }
    return vec4<f32>(rgb, a * cf.x);
}
"#
);

/// WGSL shader for SDF rounded rectangle rendering in overlay / premultiplied-alpha mode.
///
/// Same geometry as `ROUNDED_RECT_SHADER`. Vertex colours are premultiplied by
/// `gpu_color` (`src.rgb = actual.rgb * actual.a`), so border and fill mix
/// directly and all four channels scale by coverage, which the
/// `PREMULTIPLIED_ALPHA_BLENDING` equation composites correctly:
///
/// ```text
/// result.rgb = premul_rgb * cov + dst.rgb * (1 - premul_a * cov)
/// ```
///
/// DWM then composites the framebuffer (already premultiplied) with the desktop.
pub(crate) const ROUNDED_RECT_OVERLAY_SHADER: &str = concat!(
    rounded_rect_shader_common!(),
    r#"
@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let cf = coverage_and_fill(in);
    return mix(in.border_color, in.color, cf.y) * cf.x;
}
"#
);

/// The shader source for rendering colored rectangles.
pub(crate) const RECT_SHADER: &str = r#"
struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) color: vec4<f32>,
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.clip_position = vec4<f32>(in.position, 0.0, 1.0);
    out.color = in.color;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return in.color;
}
"#;
