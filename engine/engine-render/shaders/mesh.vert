#version 450

layout(location = 0) in vec3 in_position;
layout(location = 1) in vec3 in_normal;
layout(location = 2) in vec2 in_uv;
layout(location = 3) in vec3 in_color;

layout(location = 0) out vec3 out_normal;
layout(location = 1) out vec2 out_uv;
layout(location = 2) out vec3 out_color;
layout(location = 3) out vec4 out_light_clip;

layout(set = 0, binding = 0) uniform FrameUniforms {
    mat4 view_projection;
    mat4 light_view_projection;
    vec4 light_direction;
    vec4 light_color;
    vec4 ambient_color;
    vec4 shadow_params;
} frame;

layout(push_constant) uniform PushConstants {
    mat4 model;
    // Верхний блок 3x3 переводит нормали модели в мировое пространство.
    mat4 normal_from_model;
} push_constants;

void main() {
    vec4 world_position = push_constants.model * vec4(in_position, 1.0);
    gl_Position = frame.view_projection * world_position;
    out_light_clip = frame.light_view_projection * world_position;
    out_normal = (push_constants.normal_from_model * vec4(in_normal, 0.0)).xyz;
    out_uv = in_uv;
    out_color = in_color;
}
