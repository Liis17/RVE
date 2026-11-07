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

// Совпадает с ParticleGpu в particles.rs.
struct Particle {
    vec4 position_age;
    vec4 velocity_life;
    vec4 params;
};

layout(set = 1, binding = 1, std430) readonly buffer ParticleBuffer {
    Particle particles[];
} particle_buffer;

// Совпадает с VertexPushGpu: диапазон слотов и порядок (сортировка добавляется в следующих стадиях).
layout(push_constant) uniform VertexPush {
    uint draw_first;
    uint capacity;
    uint draw_count;
    uint sorted;
} push;

layout(location = 0) out vec2 out_uv;
layout(location = 1) out float out_life_fraction;

const vec2 CORNERS[6] = vec2[](
    vec2(-1.0, -1.0), vec2(1.0, -1.0), vec2(1.0, 1.0),
    vec2(-1.0, -1.0), vec2(1.0, 1.0), vec2(-1.0, 1.0));

void main() {
    uint slot = (push.draw_first + gl_InstanceIndex) % push.capacity;
    Particle particle = particle_buffer.particles[slot];
    float age = particle.position_age.w;
    float lifetime = particle.velocity_life.w;
    if (age < 0.0 || age >= lifetime) {
        // Мёртвая или ещё не рождённая частица: вершина вне clip volume.
        gl_Position = vec4(0.0, 0.0, -2.0, 1.0);
        out_uv = vec2(0.0);
        out_life_fraction = 1.0;
        return;
    }
    vec2 corner = CORNERS[gl_VertexIndex];
    float rotation = particle.params.y;
    float cos_r = cos(rotation);
    float sin_r = sin(rotation);
    vec2 rotated = vec2(cos_r * corner.x - sin_r * corner.y, sin_r * corner.x + cos_r * corner.y);
    vec3 right = vec3(frame.view[0][0], frame.view[1][0], frame.view[2][0]);
    vec3 up = vec3(frame.view[0][1], frame.view[1][1], frame.view[2][1]);
    float half_size = particle.params.x * 0.5;
    vec3 world = particle.position_age.xyz + (right * rotated.x + up * rotated.y) * half_size;
    vec4 clip = frame.view_projection * vec4(world, 1.0);
    clip.xy += frame.render_params.xy * clip.w;
    gl_Position = clip;
    out_uv = corner * 0.5 + 0.5;
    out_life_fraction = clamp(age / lifetime, 0.0, 1.0);
}
