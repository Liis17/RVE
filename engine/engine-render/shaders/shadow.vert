#version 450

layout(location = 0) in vec3 in_position;
layout(location = 1) in vec2 in_uv;
// Столбцы model экземпляра: binding 1 с шагом на instance.
layout(location = 2) in vec4 in_model_0;
layout(location = 3) in vec4 in_model_1;
layout(location = 4) in vec4 in_model_2;
layout(location = 5) in vec4 in_model_3;
layout(location = 0) out vec2 out_uv;

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

#ifdef LOCAL_SHADOW
layout(set = 0, binding = 6, std430) readonly buffer LocalShadowMatrixBuffer {
    mat4 matrices[MAX_LOCAL_SHADOW_LAYERS];
} local_shadow_matrix_buffer;
#endif

// params.y — каскад directional shadow или слой local shadow.
layout(push_constant) uniform PushConstants {
    uvec4 params;
} push_constants;

void main() {
    out_uv = in_uv;
    vec4 world_position = mat4(in_model_0, in_model_1, in_model_2, in_model_3)
        * vec4(in_position, 1.0);
#ifdef LOCAL_SHADOW
    gl_Position = local_shadow_matrix_buffer.matrices[push_constants.params.y] * world_position;
#else
    gl_Position = frame.shadow_view_projections[push_constants.params.y] * world_position;
#endif
}
