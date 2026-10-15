#version 450

layout(location = 0) in vec2 in_position;
layout(location = 1) in vec4 in_color;

layout(location = 0) out vec4 out_color;

layout(push_constant) uniform PushConstants {
    // Размер swapchain-изображения в физических пикселях.
    vec2 target_size;
} push_constants;

void main() {
    // Пиксели от левого верхнего угла переводятся в NDC Vulkan, где ось Y направлена вниз.
    gl_Position = vec4(in_position / push_constants.target_size * 2.0 - 1.0, 0.0, 1.0);
    out_color = in_color;
}
