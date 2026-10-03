//! Render pipeline — vertex types and shader configuration.

use bytemuck::{Pod, Zeroable};

/// Vertex for rendering colored rectangles.
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct RectVertex {
    pub position: [f32; 2],
    pub color: [f32; 4],
}

impl RectVertex {
    pub fn desc() -> wgpu::VertexBufferLayout<'static> {
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
pub fn rect_vertices(
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
pub struct TexturedRectVertex {
    pub position: [f32; 2],
    pub uv: [f32; 2],
    pub tint: [f32; 4],
}

impl TexturedRectVertex {
    pub fn desc() -> wgpu::VertexBufferLayout<'static> {
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
pub fn textured_rect_vertices(
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
pub const TEXTURE_RECT_SHADER: &str = r#"
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
pub fn create_texture_rect_bind_group_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
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
pub fn create_texture_rect_pipeline(
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

/// A rounded rectangle draw command: a colored rectangle with SDF corner radius.
///
/// Produced by the compositor when a zone's `RenderingPolicy` has
/// `backdrop_radius` set.  Consumed by the SDF pipeline in
/// `Compositor::encode_rounded_rect_pass`.
#[derive(Clone, Debug)]
pub struct RoundedRectDrawCmd {
    /// Original rounded rectangle shape used by the SDF.
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub radius: f32,
    pub color: [f32; 4],
    /// Optional raster bounds. When present, the draw quad is clipped to this
    /// rectangle while the SDF still evaluates against the original shape.
    pub clip: Option<RoundedRectClip>,
}

#[derive(Clone, Copy, Debug)]
pub struct RoundedRectClip {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Vertex for rendering SDF rounded rectangles.
///
/// The fragment shader receives per-vertex geometry (rect center + half-size +
/// radius) and recomputes the SDF at each pixel to produce anti-aliased rounded
/// corners.  All positional fields use pixel coordinates; they are converted to
/// NDC in the vertex shader.
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct RoundedRectVertex {
    /// NDC position of this vertex (computed by `rounded_rect_vertices`).
    pub position: [f32; 2],
    /// Pixel-space position of this vertex (passed through to fragment shader).
    pub frag_pos: [f32; 2],
    /// Center of the rectangle in pixel space.
    pub rect_center: [f32; 2],
    /// Half-size (half-width, half-height) of the rectangle in pixel space.
    pub rect_half_size: [f32; 2],
    /// Corner radius in pixels.
    pub radius: f32,
    /// RGBA color as returned by `gpu_color` (non-premultiplied in fullscreen
    /// mode; premultiplied in overlay mode).
    pub color: [f32; 4],
}

// Pod requires no padding; add a manual size assertion if needed.
// RoundedRectVertex size: 2+2+2+2+1+4 = 13 f32 = 52 bytes.

impl RoundedRectVertex {
    pub fn desc() -> wgpu::VertexBufferLayout<'static> {
        use std::mem::size_of;
        wgpu::VertexBufferLayout {
            array_stride: size_of::<RoundedRectVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                // @location(0) position: vec2<f32>
                wgpu::VertexAttribute {
                    offset: 0,
                    shader_location: 0,
                    format: wgpu::VertexFormat::Float32x2,
                },
                // @location(1) frag_pos: vec2<f32>
                wgpu::VertexAttribute {
                    offset: size_of::<[f32; 2]>() as wgpu::BufferAddress,
                    shader_location: 1,
                    format: wgpu::VertexFormat::Float32x2,
                },
                // @location(2) rect_center: vec2<f32>
                wgpu::VertexAttribute {
                    offset: size_of::<[f32; 4]>() as wgpu::BufferAddress,
                    shader_location: 2,
                    format: wgpu::VertexFormat::Float32x2,
                },
                // @location(3) rect_half_size: vec2<f32>
                wgpu::VertexAttribute {
                    offset: size_of::<[f32; 6]>() as wgpu::BufferAddress,
                    shader_location: 3,
                    format: wgpu::VertexFormat::Float32x2,
                },
                // @location(4) radius: f32
                wgpu::VertexAttribute {
                    offset: size_of::<[f32; 8]>() as wgpu::BufferAddress,
                    shader_location: 4,
                    format: wgpu::VertexFormat::Float32,
                },
                // @location(5) color: vec4<f32>
                wgpu::VertexAttribute {
                    offset: size_of::<[f32; 9]>() as wgpu::BufferAddress,
                    shader_location: 5,
                    format: wgpu::VertexFormat::Float32x4,
                },
            ],
        }
    }
}

/// Generate 6 vertices (2 triangles) for a rounded-rectangle quad.
///
/// `x`, `y`, `w`, `h` are in pixel coordinates (top-left origin).
/// `screen_w` / `screen_h` are the surface dimensions used to convert to NDC.
/// `radius` is the corner radius in pixels.
/// `color` is premultiplied RGBA.
#[allow(clippy::too_many_arguments)]
pub fn rounded_rect_vertices(
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    screen_w: f32,
    screen_h: f32,
    radius: f32,
    color: [f32; 4],
) -> [RoundedRectVertex; 6] {
    rounded_rect_vertices_with_draw_bounds(
        x, y, w, h, x, y, w, h, screen_w, screen_h, radius, color,
    )
}

/// Generate 6 vertices for a rounded rectangle whose raster quad may be
/// smaller than the original SDF shape.
///
/// `draw_*` bounds determine where fragments are emitted. `shape_*` bounds
/// determine the rounded rectangle SDF center and half-size. This is used when
/// tile scrolling clips a rounded node to the tile viewport: the clipped edge
/// must not become a new rounded corner.
#[allow(clippy::too_many_arguments)]
pub fn rounded_rect_vertices_with_draw_bounds(
    draw_x: f32,
    draw_y: f32,
    draw_w: f32,
    draw_h: f32,
    shape_x: f32,
    shape_y: f32,
    shape_w: f32,
    shape_h: f32,
    screen_w: f32,
    screen_h: f32,
    radius: f32,
    color: [f32; 4],
) -> [RoundedRectVertex; 6] {
    // NDC corners
    let left_ndc = (draw_x / screen_w) * 2.0 - 1.0;
    let right_ndc = ((draw_x + draw_w) / screen_w) * 2.0 - 1.0;
    let top_ndc = 1.0 - (draw_y / screen_h) * 2.0;
    let bottom_ndc = 1.0 - ((draw_y + draw_h) / screen_h) * 2.0;

    // Pixel-space center and half-size for the SDF.
    let cx = shape_x + shape_w * 0.5;
    let cy = shape_y + shape_h * 0.5;
    let hx = shape_w * 0.5;
    let hy = shape_h * 0.5;

    let v = |px: f32, py: f32, ndc_x: f32, ndc_y: f32| RoundedRectVertex {
        position: [ndc_x, ndc_y],
        frag_pos: [px, py],
        rect_center: [cx, cy],
        rect_half_size: [hx, hy],
        radius,
        color,
    };

    [
        // Triangle 1
        v(draw_x, draw_y, left_ndc, top_ndc),
        v(draw_x + draw_w, draw_y, right_ndc, top_ndc),
        v(draw_x, draw_y + draw_h, left_ndc, bottom_ndc),
        // Triangle 2
        v(draw_x + draw_w, draw_y, right_ndc, top_ndc),
        v(draw_x + draw_w, draw_y + draw_h, right_ndc, bottom_ndc),
        v(draw_x, draw_y + draw_h, left_ndc, bottom_ndc),
    ]
}

/// WGSL shader for SDF rounded rectangle rendering (fullscreen / straight-alpha mode).
///
/// Fragment stage:
/// - Computes the signed distance from `frag_pos` to the nearest point on the
///   rounded rectangle (standard box SDF with per-corner radius).
/// - Converts distance to an alpha via `smoothstep` for sub-pixel anti-aliasing.
/// - Applies coverage to the alpha channel only (RGB passes through unchanged).
///
/// The pipeline uses `BlendState::ALPHA_BLENDING` (straight-alpha), so the
/// fragment output must be non-premultiplied: keeping RGB unmodified and only
/// scaling alpha ensures the GPU blend equation applies coverage exactly once.
///
/// In overlay mode use `ROUNDED_RECT_OVERLAY_SHADER` + `PREMULTIPLIED_ALPHA_BLENDING`
/// instead — see `create_rounded_rect_overlay_pipeline`.
pub const ROUNDED_RECT_SHADER: &str = r#"
struct VertexInput {
    @location(0) position:       vec2<f32>,
    @location(1) frag_pos:       vec2<f32>,
    @location(2) rect_center:    vec2<f32>,
    @location(3) rect_half_size: vec2<f32>,
    @location(4) radius:         f32,
    @location(5) color:          vec4<f32>,
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) frag_pos:       vec2<f32>,
    @location(1) rect_center:    vec2<f32>,
    @location(2) rect_half_size: vec2<f32>,
    @location(3) radius:         f32,
    @location(4) color:          vec4<f32>,
};

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.clip_position = vec4<f32>(in.position, 0.0, 1.0);
    out.frag_pos       = in.frag_pos;
    out.rect_center    = in.rect_center;
    out.rect_half_size = in.rect_half_size;
    out.radius         = in.radius;
    out.color          = in.color;
    return out;
}

/// Standard 2D SDF for a rounded rectangle.
///
/// `p`    — point to evaluate (pixel space, origin = rect center).
/// `b`    — half-size of the rectangle (positive).
/// `r`    — corner radius.
/// Returns the signed distance: negative inside, positive outside.
fn sdf_rounded_box(p: vec2<f32>, b: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - b + vec2<f32>(r, r);
    return length(max(q, vec2<f32>(0.0, 0.0))) + min(max(q.x, q.y), 0.0) - r;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    // Compute signed distance from this fragment to the rounded rect boundary.
    let p = in.frag_pos - in.rect_center;
    let d = sdf_rounded_box(p, in.rect_half_size, in.radius);

    // Anti-aliased edge: 1 pixel feather.
    // smoothstep(0.5, -0.5, d) transitions from 0→1 as d goes from +0.5→-0.5.
    let alpha = smoothstep(0.5, -0.5, d);

    // Apply coverage to alpha only.  The pipeline uses ALPHA_BLENDING, which
    // performs: result = src.rgb * src.a + dst.rgb * (1 - src.a).
    // The vertex color (in.color) arrives as non-premultiplied RGBA, so we
    // must keep RGB unmodified and only scale the alpha channel by coverage.
    // Scaling all four channels by alpha (as in premultiplied blending) would
    // cause the GPU to apply coverage again during blending, darkening edges.
    return vec4<f32>(in.color.rgb, in.color.a * alpha);
}
"#;

/// WGSL shader for SDF rounded rectangle rendering in overlay / premultiplied-alpha mode.
///
/// Identical SDF geometry to `ROUNDED_RECT_SHADER`, but the fragment output
/// scales **all four channels** by the coverage alpha.  This is required when
/// the pipeline uses `BlendState::PREMULTIPLIED_ALPHA_BLENDING`, whose blend
/// equation is:
///
/// ```text
/// result.rgb = src.rgb           + dst.rgb * (1 - src.a)
/// result.a   = src.a * 1         + dst.a   * (1 - src.a)
/// ```
///
/// In overlay mode the vertex colors are premultiplied by `gpu_color`
/// (`src.rgb = actual.rgb * actual.a`).  Scaling everything by coverage gives:
///
/// ```text
/// out = vec4(premul_rgb * cov, premul_a * cov)
/// ```
///
/// which the premultiplied blend equation composites correctly:
///
/// ```text
/// result.rgb = premul_rgb * cov + dst.rgb * (1 - premul_a * cov)
/// ```
///
/// DWM then composites the framebuffer (already premultiplied) with the desktop.
pub const ROUNDED_RECT_OVERLAY_SHADER: &str = r#"
struct VertexInput {
    @location(0) position:       vec2<f32>,
    @location(1) frag_pos:       vec2<f32>,
    @location(2) rect_center:    vec2<f32>,
    @location(3) rect_half_size: vec2<f32>,
    @location(4) radius:         f32,
    @location(5) color:          vec4<f32>,
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) frag_pos:       vec2<f32>,
    @location(1) rect_center:    vec2<f32>,
    @location(2) rect_half_size: vec2<f32>,
    @location(3) radius:         f32,
    @location(4) color:          vec4<f32>,
};

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.clip_position = vec4<f32>(in.position, 0.0, 1.0);
    out.frag_pos       = in.frag_pos;
    out.rect_center    = in.rect_center;
    out.rect_half_size = in.rect_half_size;
    out.radius         = in.radius;
    out.color          = in.color;
    return out;
}

fn sdf_rounded_box(p: vec2<f32>, b: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - b + vec2<f32>(r, r);
    return length(max(q, vec2<f32>(0.0, 0.0))) + min(max(q.x, q.y), 0.0) - r;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let p = in.frag_pos - in.rect_center;
    let d = sdf_rounded_box(p, in.rect_half_size, in.radius);
    let alpha = smoothstep(0.5, -0.5, d);

    // Overlay mode: vertex color is already premultiplied (rgb = actual.rgb * actual.a).
    // Scale all four channels by coverage so the premultiplied blend equation
    // composites the anti-aliased edge correctly.
    return vec4<f32>(in.color.rgb * alpha, in.color.a * alpha);
}
"#;

/// The shader source for rendering colored rectangles.
pub const RECT_SHADER: &str = r#"
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
