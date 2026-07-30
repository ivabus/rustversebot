struct Viewport {
    size: vec2<f32>,
    _padding: vec2<f32>,
}

struct ImageInstance {
    // Logical destination x, y, width, height.
    destination: vec4<f32>,
    // Atlas-page min u, min v, max u, max v.
    uv: vec4<f32>,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@group(0) @binding(0)
var<uniform> viewport: Viewport;

@group(0) @binding(1)
var<storage, read> instances: array<ImageInstance>;

@group(0) @binding(2)
var atlas_page: texture_2d<f32>;

@group(0) @binding(3)
var atlas_sampler: sampler;

@vertex
fn vs_main(
    @builtin(vertex_index) vertex_index: u32,
    @builtin(instance_index) instance_index: u32,
) -> VertexOutput {
    let corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0),
    );
    let instance = instances[instance_index];
    let corner = corners[vertex_index];
    let logical = instance.destination.xy + corner * instance.destination.zw;
    let normalized = logical / viewport.size;

    var output: VertexOutput;
    output.clip_position = vec4<f32>(
        normalized.x * 2.0 - 1.0,
        1.0 - normalized.y * 2.0,
        0.0,
        1.0,
    );
    output.uv = mix(instance.uv.xy, instance.uv.zw, corner);
    return output;
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    let straight = textureSample(atlas_page, atlas_sampler, input.uv);
    return vec4<f32>(straight.rgb * straight.a, straight.a);
}
