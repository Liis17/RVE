#version 450

layout(location = 0) in vec2 in_ndc;
layout(location = 0) out vec4 out_color;
layout(location = 1) out vec2 out_velocity;

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
    mat4 previous_view_projection;
    vec4 render_params;
} frame;

layout(set = 2, binding = 0) uniform textureCube environment_map;
layout(set = 2, binding = 1) uniform sampler environment_sampler;

void main() {
    vec2 current_ndc = in_ndc - frame.render_params.xy;
    vec4 world_far = frame.inverse_view_projection * vec4(current_ndc, 1.0, 1.0);
    vec3 world_position = world_far.xyz / max(abs(world_far.w), 1e-8) * sign(world_far.w);
    vec3 world_direction = normalize(world_position - frame.camera_position.xyz);
    vec3 local_direction = transpose(mat3(frame.environment_rotation)) * world_direction;
    vec3 radiance = textureLod(
        samplerCube(environment_map, environment_sampler), local_direction, 0.0
    ).rgb;
    out_color = vec4(radiance * frame.environment_params.x, 1.0);
    vec4 previous_clip = frame.previous_view_projection * vec4(world_direction, 0.0);
    vec2 previous_ndc = previous_clip.xy / previous_clip.w;
    vec2 current_uv = vec2(current_ndc.x * 0.5 + 0.5, 0.5 - current_ndc.y * 0.5);
    vec2 previous_uv = vec2(previous_ndc.x * 0.5 + 0.5, 0.5 - previous_ndc.y * 0.5);
    out_velocity = previous_uv - current_uv;
}
