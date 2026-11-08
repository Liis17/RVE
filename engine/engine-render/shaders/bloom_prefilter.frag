#version 450

// Выделение ярких областей до экспозиции: порог и мягкое колено сравниваются с экспонированной
// яркостью, а сохраняется цвет без экспозиции, умноженный на intensity и tint.

layout(location = 0) in vec2 in_uv;
layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform texture2D source_image;
layout(set = 0, binding = 1) uniform sampler source_sampler;

layout(push_constant) uniform PushConstants {
    // x — порог, y — колено, z — экспозиция EV, w — резерв.
    vec4 threshold_knee_exposure;
    // rgb — intensity * tint.
    vec4 tint_intensity;
} push_constants;

void main() {
    vec3 color = textureLod(sampler2D(source_image, source_sampler), in_uv, 0.0).rgb;
    float exposure = exp2(push_constants.threshold_knee_exposure.z);
    float luminance = dot(color * exposure, vec3(0.2126, 0.7152, 0.0722));
    float threshold = push_constants.threshold_knee_exposure.x;
    float knee = max(push_constants.threshold_knee_exposure.y, 1e-4);
    float soft = clamp(luminance - (threshold - knee), 0.0, 2.0 * knee);
    soft = soft * soft * (0.25 / knee);
    float contribution = max(soft, luminance - threshold) / max(luminance, 1e-4);
    vec3 bloom = color * max(contribution, 0.0) * push_constants.tint_intensity.rgb;
    out_color = vec4(bloom, 1.0);
}
