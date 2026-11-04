#version 450

layout(location = 0) in vec2 in_uv;
layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform texture2D source_image;
layout(set = 0, binding = 1) uniform sampler source_sampler;

layout(push_constant) uniform PushConstants {
    // xy — обратный размер источника; z — bilinear вместо FXAA; w — sRGB target.
    vec4 params;
} push_constants;

vec4 sample_source(vec2 uv) {
    vec2 image_size = vec2(textureSize(sampler2D(source_image, source_sampler), 0));
    vec2 content_extent = 1.0 / push_constants.params.xy;
    vec2 minimum_uv = 0.5 / image_size;
    vec2 maximum_uv = max((content_extent - 0.5) / image_size, minimum_uv);
    return textureLod(
        sampler2D(source_image, source_sampler), clamp(uv, minimum_uv, maximum_uv), 0.0
    );
}

vec3 linear_to_srgb(vec3 value) {
    vec3 low = value * 12.92;
    vec3 high = 1.055 * pow(value, vec3(1.0 / 2.4)) - 0.055;
    return mix(high, low, step(value, vec3(0.0031308)));
}

vec3 srgb_to_linear(vec3 value) {
    vec3 low = value / 12.92;
    vec3 high = pow((value + 0.055) / 1.055, vec3(2.4));
    return mix(high, low, step(value, vec3(0.04045)));
}

float luma_at(vec2 uv) {
    return sample_source(uv).a;
}

vec3 perceptual_rgb_at(vec2 uv) {
    return linear_to_srgb(sample_source(uv).rgb);
}

vec3 fxaa(vec2 uv) {
    vec2 pixel = push_constants.params.xy;
    vec2 north_west = uv + pixel * vec2(-1.0, -1.0);
    vec2 north = uv + pixel * vec2(0.0, -1.0);
    vec2 north_east = uv + pixel * vec2(1.0, -1.0);
    vec2 west = uv + pixel * vec2(-1.0, 0.0);
    vec2 east = uv + pixel * vec2(1.0, 0.0);
    vec2 south_west = uv + pixel * vec2(-1.0, 1.0);
    vec2 south = uv + pixel * vec2(0.0, 1.0);
    vec2 south_east = uv + pixel * vec2(1.0, 1.0);

    float luma_m = luma_at(uv);
    float luma_n = luma_at(north);
    float luma_w = luma_at(west);
    float luma_e = luma_at(east);
    float luma_s = luma_at(south);
    float luma_nw = luma_at(north_west);
    float luma_ne = luma_at(north_east);
    float luma_sw = luma_at(south_west);
    float luma_se = luma_at(south_east);
    float range_min = min(luma_m, min(min(luma_n, luma_w), min(luma_s, luma_e)));
    float range_max = max(luma_m, max(max(luma_n, luma_w), max(luma_s, luma_e)));
    float range = range_max - range_min;
    if (range < max(1.0 / 16.0, range_max / 8.0)) {
        return perceptual_rgb_at(uv);
    }

    float luma_ns = luma_n + luma_s;
    float luma_we = luma_w + luma_e;
    float edge_horizontal_1 = -2.0 * luma_m + luma_ns;
    float edge_vertical_1 = -2.0 * luma_m + luma_we;
    float edge_horizontal_2 = -2.0 * luma_e + luma_ne + luma_se;
    float edge_vertical_2 = -2.0 * luma_n + luma_nw + luma_ne;
    float edge_horizontal_3 = -2.0 * luma_w + luma_nw + luma_sw;
    float edge_vertical_3 = -2.0 * luma_s + luma_sw + luma_se;
    float edge_horizontal = abs(edge_horizontal_3) + 2.0 * abs(edge_horizontal_1)
        + abs(edge_horizontal_2);
    float edge_vertical = abs(edge_vertical_3) + 2.0 * abs(edge_vertical_1)
        + abs(edge_vertical_2);
    bool horizontal = edge_horizontal >= edge_vertical;
    float luma_a = horizontal ? luma_n : luma_w;
    float luma_b = horizontal ? luma_s : luma_e;
    float gradient_a = abs(luma_a - luma_m);
    float gradient_b = abs(luma_b - luma_m);
    bool use_a = gradient_a >= gradient_b;
    float edge_luma = 0.5 * (luma_m + (use_a ? luma_a : luma_b));
    float gradient = max(gradient_a, gradient_b);
    vec2 normal = horizontal ? vec2(0.0, pixel.y) : vec2(pixel.x, 0.0);
    vec2 along_edge = horizontal ? vec2(pixel.x, 0.0) : vec2(0.0, pixel.y);
    vec2 edge_start = uv + normal * (use_a ? -0.5 : 0.5);

    vec2 end_a = edge_start - along_edge;
    vec2 end_b = edge_start + along_edge;
    float end_luma_a = luma_at(end_a);
    float end_luma_b = luma_at(end_b);
    bool done_a = abs(end_luma_a - edge_luma) >= gradient * 0.25;
    bool done_b = abs(end_luma_b - edge_luma) >= gradient * 0.25;
    const float quality_steps[11] = float[11](1.0, 1.0, 1.0, 1.0, 1.5, 2.0,
                                               2.0, 2.0, 2.0, 4.0, 8.0);
    for (int i = 0; i < 11; i++) {
        if (done_a && done_b) {
            break;
        }
        if (!done_a) {
            end_a -= along_edge * quality_steps[i];
            end_luma_a = luma_at(end_a);
            done_a = abs(end_luma_a - edge_luma) >= gradient * 0.25;
        }
        if (!done_b) {
            end_b += along_edge * quality_steps[i];
            end_luma_b = luma_at(end_b);
            done_b = abs(end_luma_b - edge_luma) >= gradient * 0.25;
        }
    }

    float distance_a = horizontal ? abs(uv.x - end_a.x) : abs(uv.y - end_a.y);
    float distance_b = horizontal ? abs(end_b.x - uv.x) : abs(end_b.y - uv.y);
    float edge_offset = 0.5 - min(distance_a, distance_b) / max(distance_a + distance_b, 1e-6);
    float end_luma = distance_a < distance_b ? end_luma_a : end_luma_b;
    bool center_is_below = luma_m < edge_luma;
    if ((end_luma < edge_luma) == center_is_below) {
        edge_offset = 0.0;
    }

    float average_luma = (2.0 * (luma_ns + luma_we) + luma_nw + luma_ne
        + luma_sw + luma_se) / 12.0;
    float subpixel = clamp(abs(average_luma - luma_m) / range, 0.0, 1.0);
    subpixel = subpixel * subpixel * (3.0 - 2.0 * subpixel);
    subpixel = subpixel * subpixel * 0.75;
    float offset = max(edge_offset, subpixel);
    return perceptual_rgb_at(uv + normal * offset * (use_a ? -1.0 : 1.0));
}

void main() {
    vec2 source_extent = 1.0 / push_constants.params.xy;
    vec2 source_scale = source_extent / vec2(textureSize(
        sampler2D(source_image, source_sampler), 0
    ));
    vec2 source_uv = in_uv * source_scale;
    vec3 linear_color;
    if (push_constants.params.z > 0.5) {
        linear_color = sample_source(source_uv).rgb;
    } else {
        vec3 filtered_srgb = clamp(fxaa(source_uv), 0.0, 1.0);
        linear_color = srgb_to_linear(filtered_srgb);
    }
    if (push_constants.params.w < 0.5) {
        linear_color = linear_to_srgb(clamp(linear_color, 0.0, 1.0));
    }
    out_color = vec4(linear_color, 1.0);
}
