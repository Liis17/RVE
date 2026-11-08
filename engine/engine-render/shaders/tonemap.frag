#version 450

// Composite камеры: chromatic aberration, экспозиция, цветокоррекция и tonemapping в linear HDR,
// затем lift/gamma/gain и vignette в LDR. Выключенные эффекты приходят тождественными значениями
// (см. post::composite_params), поэтому шейдер не ветвится по набору эффектов.

layout(location = 0) in vec2 in_uv;
layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform texture2D source_image;
layout(set = 0, binding = 1) uniform sampler source_sampler;
layout(set = 0, binding = 2) uniform texture2D bloom_image;

layout(push_constant) uniform PushConstants {
    // v0: x — экспозиция EV, y — код tonemapper, z — код debug view, w — интенсивность CA.
    vec4 exposure_tonemap_debug_aberration;
    // v1: x — температура, y — tint, z — контраст, w — насыщенность.
    vec4 white_balance_contrast_saturation;
    // v2: rgb — lift, w — bloom включён (1) или выключен (0).
    vec4 lift_bloom_enabled;
    // v3: rgb — gamma, w — интенсивность vignette.
    vec4 gamma_vignette_intensity;
    // v4: rgb — gain, w — smoothness vignette.
    vec4 gain_vignette_smoothness;
    // v5: rgb — цвет vignette, w — roundness vignette.
    vec4 vignette_color_roundness;
} push_constants;

vec3 linear_to_srgb(vec3 value) {
    vec3 low = value * 12.92;
    vec3 high = 1.055 * pow(value, vec3(1.0 / 2.4)) - 0.055;
    return mix(high, low, step(value, vec3(0.0031308)));
}

vec3 aces(vec3 color) {
    const float a = 2.51;
    const float b = 0.03;
    const float c = 2.43;
    const float d = 0.59;
    const float e = 0.14;
    return clamp((color * (a * color + b)) / (color * (c * color + d) + e), 0.0, 1.0);
}

vec3 pbr_neutral(vec3 color) {
    const float start_compression = 0.76;
    const float desaturation = 0.15;
    float x = min(color.r, min(color.g, color.b));
    float offset = x < 0.08 ? x - 6.25 * x * x : 0.04;
    color -= offset;
    float peak = max(color.r, max(color.g, color.b));
    if (peak < start_compression) {
        return color;
    }
    const float d = 1.0 - start_compression;
    float new_peak = 1.0 - d * d / (peak + d - start_compression);
    color *= new_peak / peak;
    float gray = 1.0 - 1.0 / (desaturation * (peak - new_peak) + 1.0);
    return mix(color, vec3(new_peak), gray);
}

vec3 overdraw_heat(float value) {
    if (value < 0.25) return mix(vec3(0.0, 0.0, 1.0), vec3(0.0, 1.0, 1.0), value * 4.0);
    if (value < 0.5) return mix(vec3(0.0, 1.0, 1.0), vec3(0.0, 1.0, 0.0), (value - 0.25) * 4.0);
    if (value < 0.75) return mix(vec3(0.0, 1.0, 0.0), vec3(1.0, 1.0, 0.0), (value - 0.5) * 4.0);
    return mix(vec3(1.0, 1.0, 0.0), vec3(1.0, 0.0, 0.0), (value - 0.75) * 4.0);
}

vec3 sample_hdr(vec2 uv) {
    return textureLod(sampler2D(source_image, source_sampler), uv, 0.0).rgb;
}

// Радиальное смещение каналов: красный наружу, синий внутрь.
vec3 chromatic_aberration(vec2 uv, float intensity) {
    vec2 offset = (uv - 0.5) * intensity * 0.02;
    return vec3(sample_hdr(uv + offset).r, sample_hdr(uv).g, sample_hdr(uv - offset).b);
}

vec3 grade_linear(vec3 color) {
    vec4 wb = push_constants.white_balance_contrast_saturation;
    float temperature = wb.x;
    float tint = wb.y;
    vec3 gain_balance = vec3(1.0 + 0.25 * temperature, 1.0 - 0.25 * tint, 1.0 - 0.25 * temperature);
    color *= gain_balance;
    const float pivot = 0.18;
    color = max((color - pivot) * wb.z + pivot, vec3(0.0));
    float luma = dot(color, vec3(0.2126, 0.7152, 0.0722));
    return mix(vec3(luma), color, wb.w);
}

vec3 lift_gamma_gain(vec3 color) {
    vec3 gamma = push_constants.gamma_vignette_intensity.rgb;
    vec3 gain = push_constants.gain_vignette_smoothness.rgb;
    vec3 lift = push_constants.lift_bloom_enabled.rgb;
    return pow(clamp(color, 0.0, 1.0), 1.0 / gamma) * gain + lift;
}

vec3 vignette(vec3 color, vec2 uv) {
    float intensity = push_constants.gamma_vignette_intensity.w;
    if (intensity <= 0.0) {
        return color;
    }
    vec2 size = vec2(textureSize(sampler2D(source_image, source_sampler), 0));
    float aspect = size.x / max(size.y, 1.0);
    vec2 p = (uv - 0.5) * 2.0;
    p.x *= aspect;
    float corner = length(vec2(aspect, 1.0));
    float box_distance = max(abs(p.x), abs(p.y));
    float round_distance = length(p);
    float roundness = push_constants.vignette_color_roundness.w;
    float normalized = mix(box_distance, round_distance, roundness) / corner;
    float smoothness = push_constants.gain_vignette_smoothness.w;
    float mask = smoothstep(1.0 - smoothness, 1.0, normalized);
    return mix(color, push_constants.vignette_color_roundness.rgb, mask * intensity);
}

void main() {
    vec4 controls = push_constants.exposure_tonemap_debug_aberration;
    int debug_view = int(controls.z + 0.5);
    vec3 color;
    if (controls.w > 0.0) {
        color = chromatic_aberration(in_uv, controls.w);
    } else {
        color = sample_hdr(in_uv);
    }
    // Bloom хранится без экспозиции и складывается с HDR до неё.
    if (push_constants.lift_bloom_enabled.w > 0.5) {
        color += textureLod(sampler2D(bloom_image, source_sampler), in_uv, 0.0).rgb;
    }
    if (debug_view == 3) {
        float normalized_count = clamp(log2(color.r + 1.0) / log2(9.0), 0.0, 1.0);
        color = overdraw_heat(normalized_count);
    } else {
        if (debug_view == 0 || debug_view == 1) {
            color *= exp2(controls.x);
            color = grade_linear(color);
            int tonemapper = int(controls.y + 0.5);
            if (tonemapper == 0) {
                color = aces(color);
            } else if (tonemapper == 1) {
                color = pbr_neutral(color);
            } else {
                color = clamp(color, 0.0, 1.0);
            }
            color = lift_gamma_gain(color);
            color = vignette(color, in_uv);
        } else {
            color = clamp(color, 0.0, 1.0);
        }
    }
    vec3 perceptual_color = linear_to_srgb(color);
    float luma = dot(perceptual_color, vec3(0.2126, 0.7152, 0.0722));
    out_color = vec4(color, luma);
}
