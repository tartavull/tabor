in vec2 v_tex_coord;
out vec4 color;

uniform sampler2DRect tex;

void main() {
    color = texture(tex, v_tex_coord);
}
