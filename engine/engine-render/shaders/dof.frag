#version 450

// Depth of field по модели тонкой линзы в один проход: радиус размытия (CoC) вычисляется по
// линейной глубине пикселя, а цвет собирается из диска golden-angle радиуса CoC.

layout(location = 0) in vec2 in_uv;
layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform texture2D color_image;
layout(set = 0, binding = 1) uniform sampler color_sampler;
layout(set = 0, binding = 2) uniform texture2D velocity_image;
layout(set = 0, binding = 3) uniform texture2D depth_image;

layout(push_constant) uniform PushConstants {
    // x, y — обратный размер источника; z — near; w — far.
    vec4 texel_near_far;
    // x — расстояние фокуса; y — масштаб CoC; z — максимум CoC в пикселях.
    vec4 focus_scale_max;
} push_constants;

const int TAP_COUNT = 16;
const float GOLDEN_ANGLE = 2.39996323;

float linear_depth(float depth) {
    float near = push_constants.texel_near_far.z;
    float far = push_constants.texel_near_far.w;
    return near * far / (far - depth * (far - near));
}

void main() {
    vec2 texel = push_constants.texel_near_far.xy;
    float depth = textureLod(sampler2D(depth_image, color_sampler), in_uv, 0.0).r;
    float z = max(linear_depth(depth), 1e-4);
    float focus = push_constants.focus_scale_max.x;
    float coc = min(push_constants.focus_scale_max.z,
        push_constants.focus_scale_max.y * abs(z - focus) / z);
    vec3 sum = textureLod(sampler2D(color_image, color_sampler), in_uv, 0.0).rgb;
    if (coc < 0.5) {
        out_color = vec4(sum, 1.0);
        return;
    }
    for (int i = 0; i < TAP_COUNT; i++) {
        float radius = sqrt((float(i) + 0.5) / float(TAP_COUNT)) * coc;
        float angle = float(i) * GOLDEN_ANGLE;
        vec2 offset = vec2(cos(angle), sin(angle)) * radius * texel;
        sum += textureLod(sampler2D(color_image, color_sampler), in_uv + offset, 0.0).rgb;
    }
    out_color = vec4(sum / float(TAP_COUNT + 1), 1.0);
}
