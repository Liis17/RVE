#version 450

// 13-точечный downsample (Jimenez, "Next-Gen Post Processing"): центральный квадрат и четыре
// угловых блока по 2x2 отсчёта с коэффициентами, сумма которых равна 1.

layout(location = 0) in vec2 in_uv;
layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform texture2D source_image;
layout(set = 0, binding = 1) uniform sampler source_sampler;

layout(push_constant) uniform PushConstants {
    // xy — обратный размер источника.
    vec4 texel;
} push_constants;

vec3 sample_source(vec2 uv) {
    return textureLod(sampler2D(source_image, source_sampler), uv, 0.0).rgb;
}

void main() {
    vec2 t = push_constants.texel.xy;
    vec2 uv = in_uv;
    vec3 a = sample_source(uv + t * vec2(-2.0, -2.0));
    vec3 b = sample_source(uv + t * vec2(0.0, -2.0));
    vec3 c = sample_source(uv + t * vec2(2.0, -2.0));
    vec3 d = sample_source(uv + t * vec2(-1.0, -1.0));
    vec3 e = sample_source(uv + t * vec2(1.0, -1.0));
    vec3 f = sample_source(uv + t * vec2(-2.0, 0.0));
    vec3 g = sample_source(uv);
    vec3 h = sample_source(uv + t * vec2(2.0, 0.0));
    vec3 i = sample_source(uv + t * vec2(-1.0, 1.0));
    vec3 j = sample_source(uv + t * vec2(1.0, 1.0));
    vec3 k = sample_source(uv + t * vec2(-2.0, 2.0));
    vec3 l = sample_source(uv + t * vec2(0.0, 2.0));
    vec3 m = sample_source(uv + t * vec2(2.0, 2.0));
    vec3 result = (a + b + f + g) * 0.03125
        + (b + c + g + h) * 0.03125
        + (f + g + k + l) * 0.03125
        + (g + h + l + m) * 0.03125
        + (d + e + i + j) * 0.125;
    out_color = vec4(result, 1.0);
}
