#version 450

layout(location = 0) in vec3 in_position;
layout(location = 1) in vec2 in_uv;
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

layout(push_constant) uniform PushConstants {
    mat4 model;
    mat4 normal_from_model;
    mat4 previous_model;
    uvec4 params;
} push_constants;

void main() {
    out_uv = in_uv;
#ifdef LOCAL_SHADOW
    gl_Position = local_shadow_matrix_buffer.matrices[gl_InstanceIndex]
        * push_constants.model * vec4(in_position, 1.0);
#else
    gl_Position = frame.shadow_view_projections[gl_InstanceIndex]
        * push_constants.model * vec4(in_position, 1.0);
#endif
}
