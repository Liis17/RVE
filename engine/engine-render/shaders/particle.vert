#version 450

// Billboard на каждую частицу: 6 вершин на instance, без vertex buffer.

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

// Параметры эмиттера; совпадает с EmitterParamsGpu в particles.rs.
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

// Совпадает с ParticleGpu в particles.rs.
struct Particle {
    vec4 position_age;
    vec4 velocity_life;
    vec4 params;
};

layout(set = 1, binding = 1, std430) readonly buffer ParticleBuffer {
    Particle particles[];
} particle_buffer;

// Ключи после сортировки: `y` — относительный индекс в диапазоне отрисовки.
layout(set = 1, binding = 2, std430) readonly buffer SortBuffer {
    uvec2 keys[];
} sort_buffer;

// Совпадает с VertexPushGpu в particles.rs (32 байта): диапазон слотов, порядок и размер кадра.
layout(push_constant) uniform VertexPush {
    uint draw_first;
    uint capacity;
    uint draw_count;
    uint sorted;
    float render_width;
    float render_height;
    uint padding0;
    uint padding1;
} push;

layout(location = 0) out vec2 out_atlas_uv;
layout(location = 1) out vec4 out_color;
layout(location = 2) out vec2 out_quad_uv;
layout(location = 3) out float out_view_depth;

const vec2 CORNERS[6] = vec2[](
    vec2(-1.0, -1.0), vec2(1.0, -1.0), vec2(1.0, 1.0),
    vec2(-1.0, -1.0), vec2(1.0, 1.0), vec2(-1.0, 1.0));

// Цвет по кривой жизни: кусочно-линейная интерполяция между ключами (до четырёх).
vec4 sample_color(float t) {
    uint count = emitter.counts.x;
    if (count == 0u) {
        return vec4(1.0);
    }
    if (count == 1u || t <= emitter.color_times.x) {
        return emitter.color_values[0];
    }
    for (uint i = 1u; i < count; ++i) {
        float next_time = emitter.color_times[i];
        if (t <= next_time) {
            float previous_time = emitter.color_times[i - 1u];
            float weight = (t - previous_time) / max(next_time - previous_time, 1e-6);
            return mix(emitter.color_values[i - 1u], emitter.color_values[i], weight);
        }
    }
    return emitter.color_values[count - 1u];
}

// Множитель размера по кривой жизни; без ключей размер не меняется.
float sample_size(float t) {
    uint count = emitter.counts.y;
    if (count == 0u) {
        return 1.0;
    }
    if (count == 1u || t <= emitter.size_times.x) {
        return emitter.size_values.x;
    }
    for (uint i = 1u; i < count; ++i) {
        float next_time = emitter.size_times[i];
        if (t <= next_time) {
            float previous_time = emitter.size_times[i - 1u];
            float weight = (t - previous_time) / max(next_time - previous_time, 1e-6);
            return mix(emitter.size_values[i - 1u], emitter.size_values[i], weight);
        }
    }
    return emitter.size_values[count - 1u];
}

void main() {
    uint instance = push.sorted == 1u ? sort_buffer.keys[gl_InstanceIndex].y : gl_InstanceIndex;
    uint slot = (push.draw_first + instance) % push.capacity;
    Particle particle = particle_buffer.particles[slot];
    float age = particle.position_age.w;
    float lifetime = particle.velocity_life.w;
    if (age < 0.0 || age >= lifetime) {
        // Мёртвая или ещё не рождённая частица: вершина вне clip volume.
        gl_Position = vec4(0.0, 0.0, -2.0, 1.0);
        out_atlas_uv = vec2(0.0);
        out_color = vec4(0.0);
        out_quad_uv = vec2(0.0);
        out_view_depth = 1.0;
        return;
    }
    float life = clamp(age / lifetime, 0.0, 1.0);
    vec2 corner = CORNERS[gl_VertexIndex];
    vec2 quad_uv = corner * 0.5 + 0.5;

    float columns = emitter.flipbook.x;
    float rows = emitter.flipbook.y;
    float frames = columns * rows;
    float frame_index = emitter.flipbook.z > 0.0
        ? floor(age * emitter.flipbook.z)
        : floor(life * frames);
    frame_index = min(max(frame_index, 0.0), frames - 1.0);
    vec2 cell = vec2(mod(frame_index, columns), floor(frame_index / columns));

    float rotation = particle.params.y;
    float cos_r = cos(rotation);
    float sin_r = sin(rotation);
    vec2 rotated = vec2(cos_r * corner.x - sin_r * corner.y, sin_r * corner.x + cos_r * corner.y);
    vec3 right = vec3(frame.view[0][0], frame.view[1][0], frame.view[2][0]);
    vec3 up = vec3(frame.view[0][1], frame.view[1][1], frame.view[2][1]);
    float half_size = particle.params.x * sample_size(life) * 0.5;
    vec3 world = particle.position_age.xyz + (right * rotated.x + up * rotated.y) * half_size;
    vec4 clip = frame.view_projection * vec4(world, 1.0);
    clip.xy += frame.render_params.xy * clip.w;
    gl_Position = clip;
    out_atlas_uv = (cell + quad_uv) / vec2(columns, rows);
    out_color = sample_color(life);
    out_quad_uv = quad_uv;
    out_view_depth = clip.w;
}
