#version 450

layout(location = 0) in vec2 in_atlas_uv;
layout(location = 1) in vec4 in_color;
layout(location = 2) in vec2 in_quad_uv;
layout(location = 3) in float in_view_depth;

layout(location = 0) out vec4 out_color;

// Параметры эмиттера; совпадает с EmitterParamsGpu в particles.rs.
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

layout(set = 1, binding = 0, std140) uniform EmitterParams {
    vec4 shape;
    vec4 box_extents;
    vec4 lifetime_speed;
    vec4 size_rotation;
    vec4 angular_drag;
    vec4 gravity;
    vec4 color_times;
    vec4 color_values[4];
    vec4 size_times;
    vec4 size_values;
    vec4 flipbook;
    vec4 collision;
    uvec4 counts;
} emitter;

layout(set = 1, binding = 3) uniform texture2D particle_image;
// Depth буфер вида-владельца в DEPTH_STENCIL_READ_ONLY_OPTIMAL для мягких частиц.
layout(set = 2, binding = 0) uniform texture2D scene_depth;

// Совпадает с VertexPushGpu в particles.rs: размер кадра нужен для чтения depth по пикселю.
layout(push_constant) uniform FragmentPush {
    uint draw_first;
    uint capacity;
    uint draw_count;
    uint sorted;
    float render_width;
    float render_height;
    uint padding0;
    uint padding1;
} push;

// Мягкое затухание у геометрии: разность линейных глубин сцены и частицы.
float soft_fade() {
    float soft_distance = emitter.angular_drag.w;
    if (soft_distance <= 0.0) {
        return 1.0;
    }
    ivec2 pixel = ivec2(gl_FragCoord.xy);
    float depth = texelFetch(scene_depth, pixel, 0).r;
    vec2 ndc = vec2((gl_FragCoord.x) / push.render_width * 2.0 - 1.0,
        1.0 - (gl_FragCoord.y) / push.render_height * 2.0);
    vec4 world = frame.inverse_view_projection * vec4(ndc, depth, 1.0);
    vec3 scene_position = world.xyz / world.w;
    float scene_linear = -(frame.view * vec4(scene_position, 1.0)).z;
    return clamp((scene_linear - in_view_depth) / soft_distance, 0.0, 1.0);
}
layout(set = 1, binding = 4) uniform sampler particle_sampler;

void main() {
    vec2 centered = in_quad_uv * 2.0 - 1.0;
    float radial = 1.0 - smoothstep(0.0, 1.0, length(centered));
    vec4 texel = vec4(1.0, 1.0, 1.0, radial);
    if (emitter.counts.w == 1u) {
        texel = texture(sampler2D(particle_image, particle_sampler), in_atlas_uv);
    }
    vec3 rgb = texel.rgb * in_color.rgb;
    float alpha = texel.a * in_color.a * soft_fade();
    if (emitter.counts.z == 1u) {
        // Альфа-смешивание: blend SRC_ALPHA, ONE_MINUS_SRC_ALPHA.
        out_color = vec4(rgb, alpha);
    } else {
        // Аддитивное смешивание: blend ONE, ONE; цвет предумножен на альфу.
        out_color = vec4(rgb * alpha, 0.0);
    }
}
