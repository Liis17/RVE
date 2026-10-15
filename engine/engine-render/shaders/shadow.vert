#version 450

layout(location = 0) in vec3 in_position;

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
    mat4 normal_from_model;
} push_constants;

void main() {
    gl_Position = frame.light_view_projection * push_constants.model * vec4(in_position, 1.0);
}
