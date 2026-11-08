// Старый электронно-лучевой экран: выпуклость, строки, маска люминофора и затемнение краёв.
// p0 — сила выпуклости [0, 1], p1 — глубина строк [0, 1].
vec3 rve_shade(vec2 uv) {
    float curvature = clamp(rve_param(0), 0.0, 1.0) * 0.12;
    float scanline_depth = clamp(rve_param(1), 0.0, 1.0);
    vec2 centered = uv * 2.0 - 1.0;
    centered *= 1.0 + curvature * dot(centered, centered);
    vec2 warped = centered * 0.5 + 0.5;
    if (warped.x < 0.0 || warped.x > 1.0 || warped.y < 0.0 || warped.y > 1.0) {
        return vec3(0.0);
    }
    vec3 color = rve_sample(warped).rgb;
    float scanline = 1.0 - scanline_depth * 0.5 * (1.0 + cos(warped.y * rve.resolution.y * 3.14159265));
    float mask = 0.92 + 0.08 * sin(warped.x * rve.resolution.x * 2.094395);
    float vignette = clamp(1.0 - 0.35 * dot(centered, centered), 0.0, 1.0);
    return color * scanline * mask * vignette;
}
