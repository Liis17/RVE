#version 450

// Выделение ярких областей до экспозиции: порог и мягкое колено сравниваются с яркостью в той же
// экспонированной шкале, что и composite (ручной EV плюс adapted EV в auto), а сохраняется цвет
// без экспозиции, умноженный на intensity и tint.

layout(location = 0) in vec2 in_uv;
layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform texture2D source_image;
layout(set = 0, binding = 1) uniform sampler source_sampler;

// Буфер экспозиции вида: adapted EV лежит после гистограммы (см. exposure.rs).
layout(std430, set = 0, binding = 2) readonly buffer ExposureBuffer {
    uint histogram[256];
    float adapted_ev;
    float pad0;
    float pad1;
    float pad2;
} exposure;

layout(push_constant) uniform PushConstants {
    // x — порог, y — колено, z — ручной EV вида, w — 1, если кадр адаптирует экспозицию.
    vec4 threshold_knee_exposure;
    // rgb — intensity * tint.
    vec4 tint_intensity;
} push_constants;

void main() {
    vec3 color = textureLod(sampler2D(source_image, source_sampler), in_uv, 0.0).rgb;
    float ev = push_constants.threshold_knee_exposure.z;
    if (push_constants.threshold_knee_exposure.w > 0.5) {
        ev += exposure.adapted_ev;
    }
    float exposure_scale = exp2(ev);
    float luminance = dot(color * exposure_scale, vec3(0.2126, 0.7152, 0.0722));
    float threshold = push_constants.threshold_knee_exposure.x;
    float knee = max(push_constants.threshold_knee_exposure.y, 1e-4);
    float soft = clamp(luminance - (threshold - knee), 0.0, 2.0 * knee);
    soft = soft * soft * (0.25 / knee);
    float contribution = max(soft, luminance - threshold) / max(luminance, 1e-4);
    vec3 bloom = color * max(contribution, 0.0) * push_constants.tint_intensity.rgb;
    out_color = vec4(bloom, 1.0);
}
