#version 450

// Размытие движения вдоль векторов скорости (UV, previous - current). Смещение делится на
// `shutter` от кадра и ограничивается `max_px`; выборки симметричны относительно пикселя.

layout(location = 0) in vec2 in_uv;
layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform texture2D color_image;
layout(set = 0, binding = 1) uniform sampler color_sampler;
layout(set = 0, binding = 2) uniform texture2D velocity_image;
layout(set = 0, binding = 3) uniform texture2D depth_image;

layout(push_constant) uniform PushConstants {
    // x, y — обратный размер источника; z — доля кадра (shutter / 360); w — максимум в пикселях.
    vec4 texel_shutter_max;
    // x — число выборок.
    vec4 samples;
} push_constants;

void main() {
    vec2 texel = push_constants.texel_shutter_max.xy;
    vec2 velocity = textureLod(sampler2D(velocity_image, color_sampler), in_uv, 0.0).xy;
    velocity *= push_constants.texel_shutter_max.z;
    vec2 pixels = velocity / texel;
    float length_px = length(pixels);
    float max_px = push_constants.texel_shutter_max.w;
    if (length_px > max_px && length_px > 0.0) {
        velocity *= max_px / length_px;
    }
    int count = max(int(push_constants.samples.x + 0.5), 1);
    vec3 sum = vec3(0.0);
    for (int i = 0; i < count; i++) {
        float t = (float(i) + 0.5) / float(count) - 0.5;
        sum += textureLod(sampler2D(color_image, color_sampler), in_uv + velocity * t, 0.0).rgb;
    }
    out_color = vec4(sum / float(count), 1.0);
}
