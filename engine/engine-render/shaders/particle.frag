#version 450

layout(location = 0) in vec2 in_uv;
layout(location = 1) in float in_life_fraction;

layout(location = 0) out vec4 out_color;

// Параметры эмиттера; совпадает с EmitterParamsGpu в particles.rs.
layout(set = 1, binding = 0, std140) uniform EmitterParams {
    vec4 shape;
    vec4 box_extents;
    vec4 lifetime_speed;
    vec4 size_rotation;
    vec4 angular_drag;
    vec4 gravity;
    vec4 color_times;
    vec4 color_values[4];
    vec4 size_times;
    vec4 size_values;
    vec4 flipbook;
    vec4 collision;
    uvec4 counts;
} emitter;

layout(set = 1, binding = 3) uniform texture2D particle_image;
layout(set = 1, binding = 4) uniform sampler particle_sampler;

void main() {
    vec2 centered = in_uv * 2.0 - 1.0;
    float radial = 1.0 - smoothstep(0.0, 1.0, length(centered));
    vec4 sample_value = vec4(1.0, 1.0, 1.0, radial);
    if (emitter.counts.w == 1u) {
        sample_value = texture(sampler2D(particle_image, particle_sampler), in_uv);
    }
    // Аддитивное смешивание: blend ONE, ONE; цвет предумножен на альфу.
    out_color = vec4(sample_value.rgb * sample_value.a, 0.0);
}
