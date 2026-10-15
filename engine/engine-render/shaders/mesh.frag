#version 450

layout(location = 0) in vec3 in_normal;
layout(location = 1) in vec2 in_uv;
layout(location = 2) in vec3 in_color;
layout(location = 3) in vec4 in_light_clip;

layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform FrameUniforms {
    mat4 view_projection;
    mat4 light_view_projection;
    // xyz — направление лучей направленного света в мировом пространстве.
    vec4 light_direction;
    vec4 light_color;
    vec4 ambient_color;
    // x — флаг теней, y — размер текселя карты теней.
    vec4 shadow_params;
} frame;

layout(set = 0, binding = 1) uniform texture2D shadow_map;
layout(set = 0, binding = 2) uniform sampler shadow_sampler;
layout(set = 1, binding = 0) uniform texture2D albedo_texture;
layout(set = 1, binding = 1) uniform sampler albedo_sampler;

void main() {
    vec3 normal = normalize(in_normal);
    float light_dot = dot(normal, -normalize(frame.light_direction.xyz));
    float diffuse = max(light_dot, 0.0);
    vec3 albedo = texture(sampler2D(albedo_texture, albedo_sampler), in_uv).rgb * in_color;
    float shadow = 1.0;
    if (frame.shadow_params.x > 0.5 && in_light_clip.w != 0.0) {
        vec3 light_ndc = in_light_clip.xyz / in_light_clip.w;
        vec2 shadow_uv = vec2(0.5 + 0.5 * light_ndc.x, 0.5 - 0.5 * light_ndc.y);
        if (all(greaterThanEqual(shadow_uv, vec2(0.0)))
            && all(lessThanEqual(shadow_uv, vec2(1.0)))
            && light_ndc.z >= 0.0 && light_ndc.z <= 1.0) {
            float bias = max(0.005 * (1.0 - light_dot), 0.0005);
            shadow = 0.0;
            for (int y = -1; y <= 1; ++y) {
                for (int x = -1; x <= 1; ++x) {
                    vec2 offset = vec2(float(x), float(y)) * frame.shadow_params.y;
                    float shadow_depth = textureLod(
                        sampler2D(shadow_map, shadow_sampler), shadow_uv + offset, 0.0
                    ).r;
                    if (light_ndc.z - bias <= shadow_depth) {
                        shadow += 1.0;
                    }
                }
            }
            shadow /= 9.0;
        }
    }
    vec3 lighting = frame.ambient_color.rgb + frame.light_color.rgb * diffuse * shadow;
    out_color = vec4(albedo * lighting, 1.0);
}
