#version 450

layout(location = 0) in vec2 in_uv;

layout(set = 1, binding = 0) uniform MaterialUniforms {
    vec4 base_color;
    vec4 parameters;
    vec4 emissive;
    vec4 alpha_parameters;
} material;
layout(set = 1, binding = 1) uniform texture2D base_color_texture;
layout(set = 1, binding = 6) uniform sampler material_sampler;

void main() {
    if (material.alpha_parameters.z > 0.5 && material.alpha_parameters.z < 1.5) {
        float alpha = texture(sampler2D(base_color_texture, material_sampler), in_uv).a
            * material.alpha_parameters.x;
        if (alpha < material.alpha_parameters.y) {
            discard;
        }
    }
}
