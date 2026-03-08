layout (location = 0) in vec2 position;
layout (location = 1) in vec2 tex_coord;
out vec2 v_tex_coord;

uniform vec2 size;

void main() {
    vec2 normalized = vec2((position.x / size.x) * 2.0 - 1.0, 1.0 - (position.y / size.y) * 2.0);
    gl_Position = vec4(normalized, 0.0, 1.0);
    v_tex_coord = tex_coord;
}
