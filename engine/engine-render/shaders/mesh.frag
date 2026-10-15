#version 450

layout(location = 0) in vec3 in_normal;
layout(location = 1) in vec2 in_uv;
layout(location = 2) in vec3 in_color;

layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform FrameUniforms {
    // xyz — направление лучей направленного света в мировом пространстве.
    vec4 light_direction;
    vec4 light_color;
    vec4 ambient_color;
} frame;

layout(set = 1, binding = 0) uniform texture2D albedo_texture;
layout(set = 1, binding = 1) uniform sampler albedo_sampler;

void main() {
    vec3 normal = normalize(in_normal);
    float diffuse = max(dot(normal, -normalize(frame.light_direction.xyz)), 0.0);
    vec3 albedo = texture(sampler2D(albedo_texture, albedo_sampler), in_uv).rgb * in_color;
    vec3 lighting = frame.ambient_color.rgb + frame.light_color.rgb * diffuse;
    out_color = vec4(albedo * lighting, 1.0);
}
