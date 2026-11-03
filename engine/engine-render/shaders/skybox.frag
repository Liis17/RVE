#version 450

layout(location = 0) in vec2 in_ndc;
layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform FrameUniforms {
    mat4 view_projection;
    mat4 inverse_view_projection;
    mat4 shadow_view_projections[4];
    vec4 light_direction;
    vec4 light_color;
    vec4 ambient_color;
    vec4 shadow_params;
    vec4 camera_position;
    mat4 environment_rotation;
    vec4 environment_params;
    vec4 local_light_params;
    mat4 view;
    vec4 cluster_params;
} frame;

layout(set = 2, binding = 0) uniform textureCube environment_map;
layout(set = 2, binding = 1) uniform sampler environment_sampler;

void main() {
    vec4 world_far = frame.inverse_view_projection * vec4(in_ndc, 1.0, 1.0);
    vec3 world_position = world_far.xyz / max(abs(world_far.w), 1e-8) * sign(world_far.w);
    vec3 world_direction = normalize(world_position - frame.camera_position.xyz);
    vec3 local_direction = transpose(mat3(frame.environment_rotation)) * world_direction;
    vec3 radiance = textureLod(
        samplerCube(environment_map, environment_sampler), local_direction, 0.0
    ).rgb;
    out_color = vec4(radiance * frame.environment_params.x, 1.0);
}
