#version 450

layout(location = 0) out vec2 out_ndc;

void main() {
    vec2 position = vec2((gl_VertexIndex << 1) & 2, gl_VertexIndex & 2);
    out_ndc = position * 2.0 - 1.0;
    gl_Position = vec4(out_ndc, 1.0, 1.0);
}
