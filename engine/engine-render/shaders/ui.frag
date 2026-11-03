#version 450

layout(location = 0) in vec2 in_uv;
layout(location = 1) in vec4 in_color;

layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform texture2D image_texture;
layout(set = 0, binding = 1) uniform sampler image_sampler;

layout(push_constant) uniform PushConstants {
    // xy — размер swapchain-изображения, z — запись в sRGB-формат, w — изображение в линейном RGB.
    vec4 params;
} push_constants;

vec3 gamma_from_linear(vec3 rgb) {
    vec3 low = rgb * 12.92;
    vec3 high = 1.055 * pow(rgb, vec3(1.0 / 2.4)) - 0.055;
    return mix(high, low, step(rgb, vec3(0.0031308)));
}

vec3 linear_from_gamma(vec3 rgb) {
    vec3 low = rgb / 12.92;
    vec3 high = pow((rgb + 0.055) / 1.055, vec3(2.4));
    return mix(high, low, step(rgb, vec3(0.04045)));
}

void main() {
    vec4 texel = texture(sampler2D(image_texture, image_sampler), in_uv);
    // Viewport хранит линейный HDR-цвет сцены; альфа сцены не участвует в смешивании с UI.
    if (push_constants.params.w > 0.5) {
        texel = vec4(gamma_from_linear(clamp(texel.rgb, vec3(0.0), vec3(1.0))), 1.0);
    }
    // egui смешивает цвета в гамма-пространстве, предумноженная альфа сохраняется.
    vec4 color = in_color * texel;
    if (push_constants.params.z > 0.5) {
        color = vec4(linear_from_gamma(color.rgb), color.a);
    }
    out_color = color;
}
