#version 450

// Tent-фильтр 3x3 для upsample. Результат умножается на scatter и добавляется к уровню
// через аддитивное смешивание (blend ONE/ONE).

layout(location = 0) in vec2 in_uv;
layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform texture2D source_image;
layout(set = 0, binding = 1) uniform sampler source_sampler;

layout(push_constant) uniform PushConstants {
    // xy — обратный размер источника; z — scatter.
    vec4 texel_scatter;
} push_constants;

vec3 sample_source(vec2 uv) {
    return textureLod(sampler2D(source_image, source_sampler), uv, 0.0).rgb;
}

void main() {
    vec2 t = push_constants.texel_scatter.xy;
    vec2 uv = in_uv;
    vec3 center = sample_source(uv) * 4.0;
    vec3 edges = (sample_source(uv + t * vec2(-1.0, 0.0))
        + sample_source(uv + t * vec2(1.0, 0.0))
        + sample_source(uv + t * vec2(0.0, -1.0))
        + sample_source(uv + t * vec2(0.0, 1.0))) * 2.0;
    vec3 corners = sample_source(uv + t * vec2(-1.0, -1.0))
        + sample_source(uv + t * vec2(1.0, -1.0))
        + sample_source(uv + t * vec2(-1.0, 1.0))
        + sample_source(uv + t * vec2(1.0, 1.0));
    vec3 tent = (center + edges + corners) / 16.0;
    out_color = vec4(tent * push_constants.texel_scatter.z, 1.0);
}
