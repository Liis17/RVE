#version 450

layout(location = 0) in vec2 in_position;
layout(location = 1) in vec2 in_uv;
layout(location = 2) in vec4 in_color;

layout(location = 0) out vec2 out_uv;
layout(location = 1) out vec4 out_color;

layout(push_constant) uniform PushConstants {
    // xy — размер swapchain-изображения, z — запись в sRGB-формат, w — изображение в линейном RGB.
    vec4 params;
} push_constants;

void main() {
    // Пиксели от левого верхнего угла переводятся в NDC Vulkan, где ось Y направлена вниз.
    gl_Position = vec4(in_position / push_constants.params.xy * 2.0 - 1.0, 0.0, 1.0);
    out_uv = in_uv;
    out_color = in_color;
}
