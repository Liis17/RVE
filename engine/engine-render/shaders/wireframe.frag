#version 450

layout(location = 0) out vec4 out_color;
layout(location = 1) out vec2 out_velocity;

void main() {
    out_color = vec4(0.08, 0.72, 0.95, 1.0);
    out_velocity = vec2(0.0);
}
