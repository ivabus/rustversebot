const MAX_STOPS: u32 = 8u;
const AA_EXPANSION: f32 = 1.0;

struct Viewport {
    size: vec2<f32>,
    _padding: vec2<f32>,
}

struct PrimitiveInstance {
    rect: vec4<f32>,
    radii: vec4<f32>,
    // stroke width, shape kind, style kind, paint kind
    control: vec4<f32>,
    gradient_geometry: vec4<f32>,
    colors: array<vec4<f32>, 8>,
    offsets: array<vec4<f32>, 2>,
    pattern_transform: array<vec4<f32>, 2>,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) logical_position: vec2<f32>,
    @location(1) @interpolate(flat) instance_index: u32,
}

@group(0) @binding(0)
var<uniform> viewport: Viewport;

@group(0) @binding(1)
var<storage, read> instances: array<PrimitiveInstance>;

@group(0) @binding(2)
var repeated_texture: texture_2d<f32>;

@group(0) @binding(3)
var repeated_sampler: sampler;

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
    var expansion = AA_EXPANSION;
    if instance.control.z > 0.5 {
        expansion += instance.control.x * 0.5;
    }
    let expanded_origin = instance.rect.xy - vec2<f32>(expansion);
    let expanded_size = instance.rect.zw + vec2<f32>(2.0 * expansion);
    let logical_position = expanded_origin + corners[vertex_index] * expanded_size;
    let normalized = logical_position / viewport.size;
    let clip_position = vec2<f32>(
        normalized.x * 2.0 - 1.0,
        1.0 - normalized.y * 2.0,
    );

    var output: VertexOutput;
    output.clip_position = vec4<f32>(clip_position, 0.0, 1.0);
    output.logical_position = logical_position;
    output.instance_index = instance_index;
    return output;
}

fn rounded_rect_distance(
    point: vec2<f32>,
    rect: vec4<f32>,
    radii: vec4<f32>,
) -> f32 {
    let center = rect.xy + rect.zw * 0.5;
    let centered = point - center;
    var radius = radii.x;
    if centered.x >= 0.0 && centered.y < 0.0 {
        radius = radii.y;
    } else if centered.x >= 0.0 && centered.y >= 0.0 {
        radius = radii.z;
    } else if centered.x < 0.0 && centered.y >= 0.0 {
        radius = radii.w;
    }
    let delta = abs(centered) - rect.zw * 0.5 + vec2<f32>(radius);
    return length(max(delta, vec2<f32>(0.0)))
        + min(max(delta.x, delta.y), 0.0)
        - radius;
}

fn shape_distance(instance: PrimitiveInstance, point: vec2<f32>) -> f32 {
    if instance.control.y > 0.5 {
        let center = instance.rect.xy + instance.rect.zw * 0.5;
        let radius = min(instance.rect.z, instance.rect.w) * 0.5;
        return length(point - center) - radius;
    }
    return rounded_rect_distance(point, instance.rect, instance.radii);
}

fn stop_offset(instance: PrimitiveInstance, index: u32) -> f32 {
    if index < 4u {
        return instance.offsets[0][index];
    }
    return instance.offsets[1][index - 4u];
}

fn quantize_unorm8(color: vec4<f32>) -> vec4<f32> {
    // Metal can evaluate an exact mathematical midpoint infinitesimally below
    // 0.5. Bias only the half-byte tie before matching resvg's byte rounding.
    return floor(color * 255.0 + vec4<f32>(0.50001)) / 255.0;
}

fn gradient_color(instance: PrimitiveInstance, value: f32) -> vec4<f32> {
    let stop_count = u32(instance.control.w) / 8u;
    let first_offset = stop_offset(instance, 0u);
    if value < first_offset {
        return instance.colors[0];
    }

    var index = 1u;
    while index < stop_count {
        let next_offset = stop_offset(instance, index);
        let previous_offset = stop_offset(instance, index - 1u);
        if next_offset > previous_offset && value < next_offset {
            let amount = clamp(
                (value - previous_offset) / (next_offset - previous_offset),
                0.0,
                1.0,
            );
            return quantize_unorm8(
                mix(instance.colors[index - 1u], instance.colors[index], amount),
            );
        }
        index += 1u;
    }
    return instance.colors[stop_count - 1u];
}

fn paint_color(instance: PrimitiveInstance, point: vec2<f32>) -> vec4<f32> {
    let paint_kind = u32(instance.control.w) % 8u;
    if paint_kind == 0u {
        return instance.colors[0];
    }
    if paint_kind == 1u {
        let start = instance.gradient_geometry.xy;
        let direction = instance.gradient_geometry.zw - start;
        let value = dot(point - start, direction) / dot(direction, direction);
        return gradient_color(instance, value);
    }
    if paint_kind == 2u {
        let center = instance.gradient_geometry.xy;
        let radii = instance.gradient_geometry.zw;
        return gradient_color(instance, length((point - center) / radii));
    }
    if paint_kind == 3u {
        let tile = instance.gradient_geometry.xy;
        let radius = instance.gradient_geometry.z;
        let local = fract(point / tile) * tile;
        // The reference SVG tile has a dot on every tile corner (shared by
        // adjacent tiles) and another at the tile center.
        let corner_delta = min(local, tile - local);
        let center_delta = local - tile * 0.5;
        let edge = min(length(corner_delta), length(center_delta)) - radius;
        let width = max(fwidth(edge), 0.0001);
        let dot_coverage = 1.0 - smoothstep(-0.5 * width, 0.5 * width, edge);
        return pattern_over(instance.colors[0], instance.colors[1], dot_coverage);
    }
    if paint_kind == 4u {
        let tile_size = instance.gradient_geometry.x;
        let line_width = instance.gradient_geometry.z;
        // Matches the SVG tile: mod(x + y, 4) is filled on [2, 4].
        let stripe_center = tile_size - line_width * 0.5;
        let centered_phase = (
            fract((point.x + point.y - stripe_center) / tile_size + 0.5) - 0.5
        ) * tile_size;
        let edge = abs(centered_phase) - line_width * 0.5;
        let width = max(fwidth(edge), 0.0001);
        let line_coverage = 1.0 - smoothstep(
            -0.5 * width,
            0.5 * width,
            edge,
        );
        return pattern_over(instance.colors[0], instance.colors[1], line_coverage);
    }
    let transformed = vec2<f32>(
        dot(instance.pattern_transform[0].xyz, vec3<f32>(point, 1.0)),
        dot(instance.pattern_transform[1].xyz, vec3<f32>(point, 1.0)),
    );
    let texture_color = textureSample(
        repeated_texture,
        repeated_sampler,
        transformed / instance.gradient_geometry.xy,
    );
    return texture_color * instance.colors[0];
}

fn pattern_over(foreground: vec4<f32>, background: vec4<f32>, coverage: f32) -> vec4<f32> {
    let source_alpha = foreground.a * coverage;
    let output_alpha = source_alpha + background.a * (1.0 - source_alpha);
    if output_alpha <= 0.0 {
        return vec4<f32>(0.0);
    }
    let premultiplied = foreground.rgb * source_alpha
        + background.rgb * background.a * (1.0 - source_alpha);
    return vec4<f32>(premultiplied / output_alpha, output_alpha);
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    let instance = instances[input.instance_index];
    let distance_to_edge = shape_distance(instance, input.logical_position);
    var signed_coverage_distance = distance_to_edge;
    if instance.control.z > 0.5 {
        let half_width = instance.control.x * 0.5;
        signed_coverage_distance = abs(distance_to_edge) - half_width;
    }
    let antialias_width = max(fwidth(signed_coverage_distance), 0.0001);
    let coverage = 1.0 - smoothstep(
        -0.5 * antialias_width,
        0.5 * antialias_width,
        signed_coverage_distance,
    );
    let color = paint_color(instance, input.logical_position);
    return vec4<f32>(color.rgb, color.a * coverage);
}
