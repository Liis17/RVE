#version 450

layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform texture2D current_hdr_image;
layout(set = 0, binding = 1) uniform texture2D velocity_image;
layout(set = 0, binding = 2) uniform texture2D depth_image;
layout(set = 0, binding = 3) uniform texture2D history_image;
layout(set = 0, binding = 4) uniform sampler post_sampler;

layout(set = 1, binding = 0) uniform FrameUniforms {
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

layout(push_constant) uniform PushConstants {
    // z is one when previous history may be sampled.
    vec4 params;
} push_constants;

vec3 to_ycocg(vec3 color) {
    return vec3(
        color.r * 0.25 + color.g * 0.5 + color.b * 0.25,
        color.r * 0.5 - color.b * 0.5,
        -color.r * 0.25 + color.g * 0.5 - color.b * 0.25
    );
}

vec3 from_ycocg(vec3 color) {
    return vec3(color.x + color.y - color.z, color.x + color.z, color.x - color.y - color.z);
}

float catmull_rom_weight(float value) {
    float x = abs(value);
    if (x < 1.0) {
        return 1.5 * x * x * x - 2.5 * x * x + 1.0;
    }
    if (x < 2.0) {
        return -0.5 * x * x * x + 2.5 * x * x - 4.0 * x + 2.0;
    }
    return 0.0;
}

vec4 sample_history_catmull(vec2 uv, ivec2 size) {
    vec2 position = uv * vec2(size) - 0.5;
    ivec2 base = ivec2(floor(position));
    vec2 fraction = position - vec2(base);
    vec4 sum = vec4(0.0);
    float total = 0.0;
    for (int y = -1; y <= 2; ++y) {
        float weight_y = catmull_rom_weight(float(y) - fraction.y);
        for (int x = -1; x <= 2; ++x) {
            float weight = weight_y * catmull_rom_weight(float(x) - fraction.x);
            ivec2 pixel = clamp(base + ivec2(x, y), ivec2(0), size - 1);
            sum += texelFetch(history_image, pixel, 0) * weight;
            total += weight;
        }
    }
    return sum / max(total, 1e-6);
}

void main() {
    ivec2 render_size_i = textureSize(current_hdr_image, 0);
    ivec2 output_size_i = textureSize(history_image, 0);
    vec2 render_size = vec2(render_size_i);
    vec2 output_size = vec2(output_size_i);
    vec2 jitter_render = push_constants.params.xy;
    vec2 render_position = gl_FragCoord.xy * render_size / output_size + jitter_render;
    ivec2 center = ivec2(floor(render_position));

    float closest_depth = 1.0;
    ivec2 closest_pixel = clamp(center, ivec2(0), render_size_i - 1);
    for (int y = -1; y <= 1; ++y) {
        for (int x = -1; x <= 1; ++x) {
            ivec2 pixel = clamp(center + ivec2(x, y), ivec2(0), render_size_i - 1);
            float depth = texelFetch(depth_image, pixel, 0).r;
            if (depth < closest_depth) {
                closest_depth = depth;
                closest_pixel = pixel;
            }
        }
    }

    vec2 current_uv = (vec2(closest_pixel) + 0.5 - jitter_render) / render_size;
    vec2 velocity = texelFetch(velocity_image, closest_pixel, 0).xy;
    vec2 history_uv = current_uv + velocity;

    vec3 current_sum = vec3(0.0);
    float current_weight = 0.0;
    vec3 neighborhood_min = vec3(1e30);
    vec3 neighborhood_max = vec3(-1e30);
    vec3 neighborhood_sum = vec3(0.0);
    vec3 neighborhood_square_sum = vec3(0.0);
    float neighborhood_weight = 0.0;
    for (int y = -1; y <= 1; ++y) {
        float py = float(y);
        float gaussian_y = exp(-2.0 * py * py);
        float blackman_harris_y = y == 0 ? 1.0 : 0.06;
        for (int x = -1; x <= 1; ++x) {
            float px = float(x);
            float gaussian = gaussian_y * exp(-2.0 * px * px);
            float blackman_harris = blackman_harris_y * (x == 0 ? 1.0 : 0.06);
            ivec2 pixel = clamp(closest_pixel + ivec2(x, y), ivec2(0), render_size_i - 1);
            vec3 color = texelFetch(current_hdr_image, pixel, 0).rgb;
            float karis = 1.0 / (1.0 + dot(color, vec3(0.2126, 0.7152, 0.0722)));
            float weight = gaussian * blackman_harris * karis;
            current_sum += color * weight;
            current_weight += weight;

            vec3 ycocg = to_ycocg(color);
            float box_weight = gaussian * blackman_harris;
            neighborhood_min = min(neighborhood_min, ycocg);
            neighborhood_max = max(neighborhood_max, ycocg);
            neighborhood_sum += ycocg * box_weight;
            neighborhood_square_sum += ycocg * ycocg * box_weight;
            neighborhood_weight += box_weight;
        }
    }
    vec3 current_color = current_sum / max(current_weight, 1e-6);
    vec3 neighborhood_mean = neighborhood_sum / max(neighborhood_weight, 1e-6);
    vec3 neighborhood_variance = max(
        neighborhood_square_sum / max(neighborhood_weight, 1e-6)
            - neighborhood_mean * neighborhood_mean,
        vec3(0.0)
    );
    vec3 variance_extent = 1.25 * sqrt(neighborhood_variance);
    vec3 clip_min = max(neighborhood_min, neighborhood_mean - variance_extent);
    vec3 clip_max = min(neighborhood_max, neighborhood_mean + variance_extent);

    vec3 result = current_color;
    bool history_inside = all(greaterThanEqual(history_uv, vec2(0.0)))
        && all(lessThanEqual(history_uv, vec2(1.0)));
    if (push_constants.params.z > 0.5
        && push_constants.params.w < 0.5
        && history_inside) {
        vec4 world_h = frame.inverse_view_projection * vec4(
            current_uv.x * 2.0 - 1.0,
            1.0 - current_uv.y * 2.0,
            closest_depth,
            1.0
        );
        vec3 world_position = world_h.xyz / max(abs(world_h.w), 1e-8) * sign(world_h.w);
        vec4 previous_clip = frame.previous_view_projection * vec4(world_position, 1.0);
        bool projected = previous_clip.w > 1e-6;
        float expected_previous_depth = previous_clip.z / max(previous_clip.w, 1e-6);
        float history_depth = textureLod(
            sampler2D(history_image, post_sampler), history_uv, 0.0
        ).a;
        bool disoccluded = closest_depth < 0.9999
            && (!projected
                || expected_previous_depth < 0.0
                || expected_previous_depth > 1.0
                || abs(history_depth - expected_previous_depth)
                    > max(0.002, abs(expected_previous_depth) * 0.015));
        if (!disoccluded) {
            vec4 history_sample = sample_history_catmull(history_uv, output_size_i);
            vec3 clipped_history = clamp(to_ycocg(history_sample.rgb), clip_min, clip_max);
            float motion_pixels = length(velocity * output_size);
            float motion_weight = clamp(1.0 - motion_pixels / 32.0, 0.35, 1.0);
            result = mix(current_color, from_ycocg(clipped_history), 0.9 * motion_weight);
        }
    }

    out_color = vec4(result, closest_depth);
}
