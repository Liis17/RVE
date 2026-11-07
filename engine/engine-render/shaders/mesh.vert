#version 450

layout(location = 0) in vec3 in_position;
layout(location = 1) in vec3 in_normal;
layout(location = 2) in vec2 in_uv;
layout(location = 3) in vec3 in_color;
// Позиция прошлого кадра: binding 1. Для статического меша это тот же буфер.
layout(location = 4) in vec3 in_previous_position;

layout(location = 0) out vec3 out_normal;
layout(location = 1) out vec2 out_uv;
layout(location = 2) out vec3 out_color;
layout(location = 3) out vec4 out_shadow_clip[4];
layout(location = 7) out vec3 out_world_position;
layout(location = 8) out vec4 out_current_clip;
layout(location = 9) out vec4 out_previous_clip;

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

layout(push_constant) uniform PushConstants {
    mat4 model;
    // Верхний блок 3x3 переводит нормали модели в мировое пространство.
    mat4 normal_from_model;
    mat4 previous_model;
    uvec4 params;
} push_constants;

void main() {
    vec4 world_position = push_constants.model * vec4(in_position, 1.0);
    vec4 current_clip = frame.view_projection * world_position;
    gl_Position = current_clip;
    gl_Position.xy += frame.render_params.xy * current_clip.w;
    out_current_clip = current_clip;
    out_previous_clip = frame.previous_view_projection
        * push_constants.previous_model * vec4(in_previous_position, 1.0);
    for (int cascade = 0; cascade < 4; ++cascade) {
        out_shadow_clip[cascade] = frame.shadow_view_projections[cascade] * world_position;
    }
    out_world_position = world_position.xyz;
    out_normal = (push_constants.normal_from_model * vec4(in_normal, 0.0)).xyz;
    out_uv = in_uv;
    out_color = in_color;
}
