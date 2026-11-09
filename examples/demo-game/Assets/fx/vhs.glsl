// Видеокассета: сдвиг цветовых каналов, строки развёртки, зернистость.
// p0 — сила искажения [0, 1], p1 — сила шума [0, 1].
float rve_noise(vec2 point) {
    return fract(sin(dot(point, vec2(12.9898, 78.233))) * 43758.5453);
}

vec3 rve_shade(vec2 uv) {
    float strength = clamp(rve_param(0), 0.0, 1.0);
    float noise_amount = clamp(rve_param(1), 0.0, 1.0);
    float shift = strength * 0.004;
    vec3 color = vec3(
        rve_sample(uv + vec2(shift, 0.0)).r,
        rve_sample(uv).g,
        rve_sample(uv - vec2(shift, 0.0)).b
    );
    float lines = 0.5 + 0.5 * sin(uv.y * rve.resolution.y * 3.14159265);
    color *= mix(1.0, 0.8 + 0.2 * lines, strength);
    float grain = rve_noise(uv * rve.resolution.xy + vec2(rve.time.x * 37.0, rve.time.y));
    color += (grain - 0.5) * noise_amount * 0.2;
    return clamp(color, 0.0, 1.0);
}
