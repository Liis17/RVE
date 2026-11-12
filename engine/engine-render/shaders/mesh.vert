#version 450

layout(location = 0) in vec3 in_position;
layout(location = 1) in vec3 in_normal;
layout(location = 2) in vec2 in_uv;
layout(location = 3) in vec3 in_color;
// Позиция прошлого кадра: binding 1. Для статического меша это тот же буфер.
layout(location = 4) in vec3 in_previous_position;
// Данные экземпляра: binding 2 с шагом на instance. Столбцы model, матрицы нормалей
// (верхний блок 3x3 переводит нормали модели в мировое пространство) и model прошлого кадра.
layout(location = 5) in vec4 in_model_0;
layout(location = 6) in vec4 in_model_1;
layout(location = 7) in vec4 in_model_2;
layout(location = 8) in vec4 in_model_3;
layout(location = 9) in vec4 in_normal_from_model_0;
layout(location = 10) in vec4 in_normal_from_model_1;
layout(location = 11) in vec4 in_normal_from_model_2;
layout(location = 12) in vec4 in_previous_model_0;
layout(location = 13) in vec4 in_previous_model_1;
layout(location = 14) in vec4 in_previous_model_2;
layout(location = 15) in vec4 in_previous_model_3;

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

// Одинаковая позиция в prepass и в основном проходе: глубина совпадает бит-в-бит для LESS_OR_EQUAL.
invariant gl_Position;

void main() {
    mat4 model = mat4(in_model_0, in_model_1, in_model_2, in_model_3);
    mat3 normal_from_model = mat3(
        in_normal_from_model_0.xyz,
        in_normal_from_model_1.xyz,
        in_normal_from_model_2.xyz
    );
    mat4 previous_model = mat4(
        in_previous_model_0,
        in_previous_model_1,
        in_previous_model_2,
        in_previous_model_3
    );
    vec4 world_position = model * vec4(in_position, 1.0);
    vec4 current_clip = frame.view_projection * world_position;
    gl_Position = current_clip;
    gl_Position.xy += frame.render_params.xy * current_clip.w;
    out_current_clip = current_clip;
    out_previous_clip = frame.previous_view_projection
        * previous_model * vec4(in_previous_position, 1.0);
    for (int cascade = 0; cascade < 4; ++cascade) {
        out_shadow_clip[cascade] = frame.shadow_view_projections[cascade] * world_position;
    }
    out_world_position = world_position.xyz;
    out_normal = normal_from_model * in_normal;
    out_uv = in_uv;
    out_color = in_color;
}
