// Оставляет цвет только рядом с заданным оттенком, остальное переводит в серое.
// p0 — оттенок в диапазоне [0, 1), p1 — допуск по оттенку, p2 — сила эффекта [0, 1].
float rve_grey(vec3 color) {
    return dot(color, vec3(0.2126, 0.7152, 0.0722));
}

float rve_hue(vec3 color) {
    float high = max(color.r, max(color.g, color.b));
    float low = min(color.r, min(color.g, color.b));
    float chroma = high - low;
    if (chroma < 1e-5) {
        return 0.0;
    }
    float hue;
    if (high == color.r) {
        hue = mod((color.g - color.b) / chroma, 6.0);
    } else if (high == color.g) {
        hue = (color.b - color.r) / chroma + 2.0;
    } else {
        hue = (color.r - color.g) / chroma + 4.0;
    }
    return hue / 6.0;
}

vec3 rve_shade(vec2 uv) {
    vec3 color = rve_sample(uv).rgb;
    float distance_to_target = abs(rve_hue(color) - rve_param(0));
    distance_to_target = min(distance_to_target, 1.0 - distance_to_target);
    float tolerance = max(rve_param(1), 0.001);
    float keep = 1.0 - smoothstep(tolerance * 0.5, tolerance, distance_to_target);
    float strength = clamp(rve_param(2), 0.0, 1.0);
    vec3 grey = vec3(rve_grey(color));
    return mix(color, mix(color, grey, keep), strength);
}
