#version 450

layout(location = 0) in vec3 in_position;
layout(location = 1) in vec3 in_normal;
layout(location = 2) in vec2 in_uv;
layout(location = 3) in vec3 in_color;

layout(location = 0) out vec3 out_normal;
layout(location = 1) out vec2 out_uv;
layout(location = 2) out vec3 out_color;

layout(push_constant) uniform PushConstants {
    mat4 clip_from_model;
    // Верхний блок 3x3 переводит нормали модели в мировое пространство.
    mat4 normal_from_model;
} push_constants;

void main() {
    gl_Position = push_constants.clip_from_model * vec4(in_position, 1.0);
    out_normal = (push_constants.normal_from_model * vec4(in_normal, 0.0)).xyz;
    out_uv = in_uv;
    out_color = in_color;
}
