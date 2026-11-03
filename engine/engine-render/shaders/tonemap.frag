#version 450

layout(location = 0) in vec2 in_uv;
layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform texture2D source_image;
layout(set = 0, binding = 1) uniform sampler source_sampler;

layout(push_constant) uniform PushConstants {
    vec4 params;
} push_constants;

vec3 linear_to_srgb(vec3 value) {
    vec3 low = value * 12.92;
    vec3 high = 1.055 * pow(value, vec3(1.0 / 2.4)) - 0.055;
    return mix(high, low, step(value, vec3(0.0031308)));
}

vec3 aces(vec3 color) {
    const float a = 2.51;
    const float b = 0.03;
    const float c = 2.43;
    const float d = 0.59;
    const float e = 0.14;
    return clamp((color * (a * color + b)) / (color * (c * color + d) + e), 0.0, 1.0);
}

vec3 pbr_neutral(vec3 color) {
    const float start_compression = 0.76;
    const float desaturation = 0.15;
    float x = min(color.r, min(color.g, color.b));
    float offset = x < 0.08 ? x - 6.25 * x * x : 0.04;
    color -= offset;
    float peak = max(color.r, max(color.g, color.b));
    if (peak < start_compression) {
        return color;
    }
    const float d = 1.0 - start_compression;
    float new_peak = 1.0 - d * d / (peak + d - start_compression);
    color *= new_peak / peak;
    float gray = 1.0 - 1.0 / (desaturation * (peak - new_peak) + 1.0);
    return mix(color, vec3(new_peak), gray);
}

void main() {
    vec3 color = textureLod(sampler2D(source_image, source_sampler), in_uv, 0.0).rgb;
    color *= exp2(push_constants.params.x);
    int tonemapper = int(push_constants.params.y + 0.5);
    if (tonemapper == 0) {
        color = aces(color);
    } else if (tonemapper == 1) {
        color = pbr_neutral(color);
    } else {
        color = clamp(color, 0.0, 1.0);
    }
    vec3 perceptual_color = linear_to_srgb(color);
    float luma = dot(perceptual_color, vec3(0.2126, 0.7152, 0.0722));
    out_color = vec4(color, luma);
}
