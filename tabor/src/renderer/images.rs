use glutin::context::PossiblyCurrentContext;

use crate::display::SizeInfo;
#[cfg(target_os = "macos")]
use crate::macos::image_view::ImageRenderQuad;
use crate::renderer::shader::ShaderVersion;
use crate::renderer::{self};

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct ImageSlice {
    pub dest_x_px: usize,
    pub dest_y_px: usize,
    pub dest_width_px: usize,
    pub dest_height_px: usize,
    pub src_x_px: usize,
    pub src_y_px: usize,
    pub src_width_px: usize,
    pub src_height_px: usize,
}

#[cfg(target_os = "macos")]
mod macos {
    use std::collections::{HashMap, VecDeque};
    use std::ffi::c_void;
    use std::mem;

    use cef::ColorType;
    use glutin::context::{AsRawContext, RawContext};
    use log::warn;
    use objc2::encode::{Encoding, RefEncode};
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    use super::*;
    use crate::gl;
    use crate::gl::types::*;
    use crate::renderer::shader::ShaderProgram;

    const IMAGE_SHADER_F: &str = include_str!("../../res/image.f.glsl");
    const IMAGE_SHADER_V: &str = include_str!("../../res/image.v.glsl");

    #[derive(Debug, Copy, Clone, PartialEq, Eq)]
    pub enum SurfaceSlot {
        Main,
        Popup,
    }

    #[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
    pub struct BitmapCacheKey {
        pub namespace: u64,
        pub entry: u64,
    }

    const MAX_BITMAP_CACHE_BYTES: usize = 256 * 1024 * 1024;

    #[repr(C)]
    #[derive(Debug, Copy, Clone)]
    struct Vertex {
        x: f32,
        y: f32,
        u: f32,
        v: f32,
    }

    #[repr(C)]
    struct CGLContextObject {
        __inner: [u8; 0],
    }

    unsafe impl RefEncode for CGLContextObject {
        const ENCODING_REF: Encoding =
            Encoding::Pointer(&Encoding::Struct("_CGLContextObject", &[]));
    }

    type CGLContextObj = *mut CGLContextObject;
    type CGLError = i32;

    #[link(name = "OpenGL", kind = "framework")]
    unsafe extern "C" {
        fn CGLTexImageIOSurface2D(
            ctx: CGLContextObj,
            target: GLenum,
            internal_format: GLenum,
            width: GLsizei,
            height: GLsizei,
            format: GLenum,
            type_: GLenum,
            io_surface: *mut c_void,
            plane: GLuint,
        ) -> CGLError;
    }

    #[derive(Debug, Copy, Clone, PartialEq, Eq)]
    struct SurfaceKey {
        io_surface: *mut c_void,
        width: usize,
        height: usize,
        format: ColorType,
    }

    #[derive(Debug)]
    struct SurfaceTexture {
        texture: GLuint,
        bound_surface: Option<SurfaceKey>,
    }

    impl SurfaceTexture {
        fn new() -> Self {
            let mut texture = 0;
            unsafe {
                gl::GenTextures(1, &mut texture);
                gl::BindTexture(gl::TEXTURE_RECTANGLE, texture);
                gl::TexParameteri(
                    gl::TEXTURE_RECTANGLE,
                    gl::TEXTURE_WRAP_S,
                    gl::CLAMP_TO_EDGE as i32,
                );
                gl::TexParameteri(
                    gl::TEXTURE_RECTANGLE,
                    gl::TEXTURE_WRAP_T,
                    gl::CLAMP_TO_EDGE as i32,
                );
                gl::TexParameteri(gl::TEXTURE_RECTANGLE, gl::TEXTURE_MIN_FILTER, gl::LINEAR as i32);
                gl::TexParameteri(gl::TEXTURE_RECTANGLE, gl::TEXTURE_MAG_FILTER, gl::LINEAR as i32);
                gl::BindTexture(gl::TEXTURE_RECTANGLE, 0);
            }

            Self { texture, bound_surface: None }
        }

        fn bind_surface(
            &mut self,
            cgl_context: CGLContextObj,
            io_surface: *mut c_void,
            width: usize,
            height: usize,
            format: ColorType,
        ) -> Result<(), String> {
            let key = SurfaceKey { io_surface, width, height, format };
            let (internal_format, gl_format, gl_type) = gl_surface_format(format)?;

            unsafe {
                gl::BindTexture(gl::TEXTURE_RECTANGLE, self.texture);
                if self.bound_surface != Some(key) {
                    let error = CGLTexImageIOSurface2D(
                        cgl_context,
                        gl::TEXTURE_RECTANGLE,
                        internal_format,
                        width as i32,
                        height as i32,
                        gl_format,
                        gl_type,
                        io_surface,
                        0,
                    );
                    if error != 0 {
                        gl::BindTexture(gl::TEXTURE_RECTANGLE, 0);
                        return Err(format!(
                            "CGLTexImageIOSurface2D failed with CGLError {}",
                            error
                        ));
                    }
                    self.bound_surface = Some(key);
                }
            }

            Ok(())
        }
    }

    impl Drop for SurfaceTexture {
        fn drop(&mut self) {
            unsafe {
                gl::DeleteTextures(1, &self.texture);
            }
        }
    }

    #[derive(Debug)]
    struct BitmapTexture {
        texture: GLuint,
        dimensions: Option<(usize, usize)>,
        byte_size: usize,
    }

    impl BitmapTexture {
        fn new() -> Self {
            let mut texture = 0;
            unsafe {
                gl::GenTextures(1, &mut texture);
                gl::BindTexture(gl::TEXTURE_RECTANGLE, texture);
                gl::TexParameteri(
                    gl::TEXTURE_RECTANGLE,
                    gl::TEXTURE_WRAP_S,
                    gl::CLAMP_TO_EDGE as i32,
                );
                gl::TexParameteri(
                    gl::TEXTURE_RECTANGLE,
                    gl::TEXTURE_WRAP_T,
                    gl::CLAMP_TO_EDGE as i32,
                );
                gl::TexParameteri(gl::TEXTURE_RECTANGLE, gl::TEXTURE_MIN_FILTER, gl::LINEAR as i32);
                gl::TexParameteri(gl::TEXTURE_RECTANGLE, gl::TEXTURE_MAG_FILTER, gl::LINEAR as i32);
                gl::BindTexture(gl::TEXTURE_RECTANGLE, 0);
            }

            Self { texture, dimensions: None, byte_size: 0 }
        }

        fn upload(&mut self, width: usize, height: usize, rgba: &[u8]) {
            unsafe {
                gl::BindTexture(gl::TEXTURE_RECTANGLE, self.texture);
                if self.dimensions == Some((width, height)) {
                    gl::TexSubImage2D(
                        gl::TEXTURE_RECTANGLE,
                        0,
                        0,
                        0,
                        width as i32,
                        height as i32,
                        gl::RGBA,
                        gl::UNSIGNED_BYTE,
                        rgba.as_ptr().cast(),
                    );
                } else {
                    gl::PixelStorei(gl::UNPACK_ALIGNMENT, 1);
                    gl::TexImage2D(
                        gl::TEXTURE_RECTANGLE,
                        0,
                        gl::RGBA8 as i32,
                        width as i32,
                        height as i32,
                        0,
                        gl::RGBA,
                        gl::UNSIGNED_BYTE,
                        rgba.as_ptr().cast(),
                    );
                    self.dimensions = Some((width, height));
                }
            }
            self.byte_size = width.saturating_mul(height).saturating_mul(4);
        }
    }

    impl Drop for BitmapTexture {
        fn drop(&mut self) {
            unsafe {
                gl::DeleteTextures(1, &self.texture);
            }
        }
    }

    #[derive(Debug)]
    pub struct ImageRenderer {
        vao: GLuint,
        vbo: GLuint,
        main_texture: SurfaceTexture,
        popup_texture: SurfaceTexture,
        bitmap_texture: BitmapTexture,
        bitmap_texture_cache: HashMap<BitmapCacheKey, BitmapTexture>,
        bitmap_cache_order: VecDeque<BitmapCacheKey>,
        bitmap_cache_bytes: usize,
        program: ImageShaderProgram,
        vertices: Vec<Vertex>,
        cgl_context: CGLContextObj,
    }

    impl ImageRenderer {
        pub fn new(
            context: &PossiblyCurrentContext,
            _shader_version: ShaderVersion,
        ) -> Result<Self, renderer::Error> {
            let mut vao = 0;
            let mut vbo = 0;
            let program = ImageShaderProgram::new(ShaderVersion::Glsl3)?;
            let cgl_context = current_cgl_context(context)?;

            unsafe {
                gl::GenVertexArrays(1, &mut vao);
                gl::GenBuffers(1, &mut vbo);

                gl::BindVertexArray(vao);
                gl::BindBuffer(gl::ARRAY_BUFFER, vbo);

                let mut attribute_offset = 0;
                gl::VertexAttribPointer(
                    0,
                    2,
                    gl::FLOAT,
                    gl::FALSE,
                    mem::size_of::<Vertex>() as i32,
                    attribute_offset as *const _,
                );
                gl::EnableVertexAttribArray(0);
                attribute_offset += mem::size_of::<f32>() * 2;

                gl::VertexAttribPointer(
                    1,
                    2,
                    gl::FLOAT,
                    gl::FALSE,
                    mem::size_of::<Vertex>() as i32,
                    attribute_offset as *const _,
                );
                gl::EnableVertexAttribArray(1);

                gl::BindVertexArray(0);
                gl::BindBuffer(gl::ARRAY_BUFFER, 0);
            }

            Ok(Self {
                vao,
                vbo,
                main_texture: SurfaceTexture::new(),
                popup_texture: SurfaceTexture::new(),
                bitmap_texture: BitmapTexture::new(),
                bitmap_texture_cache: HashMap::new(),
                bitmap_cache_order: VecDeque::new(),
                bitmap_cache_bytes: 0,
                program,
                vertices: Vec::new(),
                cgl_context,
            })
        }

        #[allow(clippy::too_many_arguments)]
        pub fn draw_iosurface(
            &mut self,
            size_info: &SizeInfo,
            slot: SurfaceSlot,
            io_surface: *mut c_void,
            surface_width_px: usize,
            surface_height_px: usize,
            format: ColorType,
            slices: &[ImageSlice],
        ) {
            if io_surface.is_null()
                || surface_width_px == 0
                || surface_height_px == 0
                || slices.is_empty()
            {
                return;
            }

            let texture = match slot {
                SurfaceSlot::Main => &mut self.main_texture,
                SurfaceSlot::Popup => &mut self.popup_texture,
            };
            let texture_id = texture.texture;

            if let Err(err) = texture.bind_surface(
                self.cgl_context,
                io_surface,
                surface_width_px,
                surface_height_px,
                format,
            ) {
                warn!("Failed to bind accelerated browser IOSurface: {err}");
                return;
            }

            self.vertices.clear();
            self.vertices.reserve(slices.len() * 6);

            for slice in slices {
                if slice.dest_width_px == 0
                    || slice.dest_height_px == 0
                    || slice.src_width_px == 0
                    || slice.src_height_px == 0
                {
                    continue;
                }

                let u0 = slice.src_x_px as f32;
                let v0 = slice.src_y_px as f32;
                let u1 = (slice.src_x_px + slice.src_width_px) as f32;
                let v1 = (slice.src_y_px + slice.src_height_px) as f32;

                let x0 = slice.dest_x_px as f32;
                let y0 = slice.dest_y_px as f32;
                let x1 = (slice.dest_x_px + slice.dest_width_px) as f32;
                let y1 = (slice.dest_y_px + slice.dest_height_px) as f32;

                self.vertices.extend_from_slice(&[
                    Vertex { x: x0, y: y0, u: u0, v: v0 },
                    Vertex { x: x1, y: y0, u: u1, v: v0 },
                    Vertex { x: x0, y: y1, u: u0, v: v1 },
                    Vertex { x: x0, y: y1, u: u0, v: v1 },
                    Vertex { x: x1, y: y0, u: u1, v: v0 },
                    Vertex { x: x1, y: y1, u: u1, v: v1 },
                ]);
            }

            if self.vertices.is_empty() {
                unsafe {
                    gl::BindTexture(gl::TEXTURE_RECTANGLE, 0);
                }
                return;
            }

            self.draw_vertices(size_info, texture_id);
        }

        pub fn draw_bitmap(
            &mut self,
            size_info: &SizeInfo,
            width: usize,
            height: usize,
            rgba: &[u8],
            quad: ImageRenderQuad,
        ) {
            if width == 0
                || height == 0
                || rgba.is_empty()
                || quad.dest_width_px <= 0.0
                || quad.dest_height_px <= 0.0
            {
                return;
            }

            self.bitmap_texture.upload(width, height, rgba);
            self.vertices.clear();
            self.vertices.extend_from_slice(&[
                Vertex {
                    x: quad.dest_x_px,
                    y: quad.dest_y_px,
                    u: quad.uv_top_left.0,
                    v: quad.uv_top_left.1,
                },
                Vertex {
                    x: quad.dest_x_px + quad.dest_width_px,
                    y: quad.dest_y_px,
                    u: quad.uv_top_right.0,
                    v: quad.uv_top_right.1,
                },
                Vertex {
                    x: quad.dest_x_px,
                    y: quad.dest_y_px + quad.dest_height_px,
                    u: quad.uv_bottom_left.0,
                    v: quad.uv_bottom_left.1,
                },
                Vertex {
                    x: quad.dest_x_px,
                    y: quad.dest_y_px + quad.dest_height_px,
                    u: quad.uv_bottom_left.0,
                    v: quad.uv_bottom_left.1,
                },
                Vertex {
                    x: quad.dest_x_px + quad.dest_width_px,
                    y: quad.dest_y_px,
                    u: quad.uv_top_right.0,
                    v: quad.uv_top_right.1,
                },
                Vertex {
                    x: quad.dest_x_px + quad.dest_width_px,
                    y: quad.dest_y_px + quad.dest_height_px,
                    u: quad.uv_bottom_right.0,
                    v: quad.uv_bottom_right.1,
                },
            ]);

            self.draw_vertices(size_info, self.bitmap_texture.texture);
        }

        pub fn draw_cached_bitmap(
            &mut self,
            size_info: &SizeInfo,
            cache_key: BitmapCacheKey,
            width: usize,
            height: usize,
            rgba: &[u8],
            quad: ImageRenderQuad,
        ) {
            if width == 0
                || height == 0
                || rgba.is_empty()
                || quad.dest_width_px <= 0.0
                || quad.dest_height_px <= 0.0
            {
                return;
            }

            let texture = self.ensure_cached_bitmap_texture(cache_key, width, height, rgba);
            self.vertices.clear();
            self.vertices.extend_from_slice(&[
                Vertex {
                    x: quad.dest_x_px,
                    y: quad.dest_y_px,
                    u: quad.uv_top_left.0,
                    v: quad.uv_top_left.1,
                },
                Vertex {
                    x: quad.dest_x_px + quad.dest_width_px,
                    y: quad.dest_y_px,
                    u: quad.uv_top_right.0,
                    v: quad.uv_top_right.1,
                },
                Vertex {
                    x: quad.dest_x_px,
                    y: quad.dest_y_px + quad.dest_height_px,
                    u: quad.uv_bottom_left.0,
                    v: quad.uv_bottom_left.1,
                },
                Vertex {
                    x: quad.dest_x_px,
                    y: quad.dest_y_px + quad.dest_height_px,
                    u: quad.uv_bottom_left.0,
                    v: quad.uv_bottom_left.1,
                },
                Vertex {
                    x: quad.dest_x_px + quad.dest_width_px,
                    y: quad.dest_y_px,
                    u: quad.uv_top_right.0,
                    v: quad.uv_top_right.1,
                },
                Vertex {
                    x: quad.dest_x_px + quad.dest_width_px,
                    y: quad.dest_y_px + quad.dest_height_px,
                    u: quad.uv_bottom_right.0,
                    v: quad.uv_bottom_right.1,
                },
            ]);

            self.draw_vertices(size_info, texture);
        }

        fn ensure_cached_bitmap_texture(
            &mut self,
            cache_key: BitmapCacheKey,
            width: usize,
            height: usize,
            rgba: &[u8],
        ) -> GLuint {
            let texture = {
                let entry =
                    self.bitmap_texture_cache.entry(cache_key).or_insert_with(BitmapTexture::new);
                let old_size = entry.byte_size;
                if entry.dimensions != Some((width, height)) || old_size == 0 {
                    entry.upload(width, height, rgba);
                    self.bitmap_cache_bytes = self
                        .bitmap_cache_bytes
                        .saturating_sub(old_size)
                        .saturating_add(entry.byte_size);
                }
                entry.texture
            };

            self.bitmap_cache_order.retain(|existing| existing != &cache_key);
            self.bitmap_cache_order.push_back(cache_key);
            while self.bitmap_cache_bytes > MAX_BITMAP_CACHE_BYTES {
                let Some(evicted_key) = self.bitmap_cache_order.pop_front() else {
                    break;
                };
                if evicted_key == cache_key {
                    self.bitmap_cache_order.push_back(evicted_key);
                    break;
                }
                if let Some(evicted) = self.bitmap_texture_cache.remove(&evicted_key) {
                    self.bitmap_cache_bytes =
                        self.bitmap_cache_bytes.saturating_sub(evicted.byte_size);
                }
            }

            texture
        }

        fn draw_vertices(&mut self, size_info: &SizeInfo, texture: GLuint) {
            if self.vertices.is_empty() {
                unsafe {
                    gl::BindTexture(gl::TEXTURE_RECTANGLE, 0);
                }
                return;
            }

            unsafe {
                gl::Viewport(0, 0, size_info.width() as i32, size_info.height() as i32);
                gl::BlendFunc(gl::ONE, gl::ONE_MINUS_SRC_ALPHA);

                gl::UseProgram(self.program.program.id());
                gl::Uniform2f(self.program.size_uniform, size_info.width(), size_info.height());
                gl::Uniform1i(self.program.texture_uniform, 0);

                gl::ActiveTexture(gl::TEXTURE0);
                gl::BindTexture(gl::TEXTURE_RECTANGLE, texture);
                gl::BindVertexArray(self.vao);
                gl::BindBuffer(gl::ARRAY_BUFFER, self.vbo);
                gl::BufferData(
                    gl::ARRAY_BUFFER,
                    (self.vertices.len() * mem::size_of::<Vertex>()) as isize,
                    self.vertices.as_ptr().cast(),
                    gl::STREAM_DRAW,
                );

                gl::DrawArrays(gl::TRIANGLES, 0, self.vertices.len() as i32);

                gl::BindVertexArray(0);
                gl::BindBuffer(gl::ARRAY_BUFFER, 0);
                gl::BindTexture(gl::TEXTURE_RECTANGLE, 0);
                gl::UseProgram(0);

                gl::BlendFunc(gl::SRC1_COLOR, gl::ONE_MINUS_SRC1_COLOR);
                gl::Viewport(
                    size_info.padding_x() as i32,
                    size_info.padding_bottom() as i32,
                    size_info.width() as i32
                        - size_info.padding_x() as i32
                        - size_info.padding_right() as i32,
                    size_info.viewport_height() as i32,
                );
            }
        }
    }

    impl Drop for ImageRenderer {
        fn drop(&mut self) {
            unsafe {
                gl::DeleteBuffers(1, &self.vbo);
                gl::DeleteVertexArrays(1, &self.vao);
            }
        }
    }

    #[derive(Debug)]
    struct ImageShaderProgram {
        program: ShaderProgram,
        size_uniform: GLint,
        texture_uniform: GLint,
    }

    impl ImageShaderProgram {
        fn new(shader_version: ShaderVersion) -> Result<Self, renderer::Error> {
            let program = ShaderProgram::new(shader_version, None, IMAGE_SHADER_V, IMAGE_SHADER_F)?;
            let size_uniform =
                program.get_uniform_location(c"size").map_err(renderer::Error::Shader)?;
            let texture_uniform =
                program.get_uniform_location(c"tex").map_err(renderer::Error::Shader)?;
            Ok(Self { program, size_uniform, texture_uniform })
        }
    }

    fn current_cgl_context(
        context: &PossiblyCurrentContext,
    ) -> Result<CGLContextObj, renderer::Error> {
        let RawContext::Cgl(ns_context) = context.raw_context();

        let ns_context = ns_context.cast::<AnyObject>();
        let cgl_context: CGLContextObj = unsafe { msg_send![ns_context, CGLContextObj] };
        if cgl_context.is_null() {
            return Err(renderer::Error::Other(String::from(
                "NSOpenGLContext returned a null CGLContextObj",
            )));
        }

        Ok(cgl_context)
    }

    fn gl_surface_format(format: ColorType) -> Result<(GLenum, GLenum, GLenum), String> {
        match format {
            t if t == ColorType::BGRA_8888 => {
                Ok((gl::RGBA8, gl::BGRA, gl::UNSIGNED_INT_8_8_8_8_REV))
            },
            t if t == ColorType::RGBA_8888 => Ok((gl::RGBA8, gl::RGBA, gl::UNSIGNED_BYTE)),
            other => Err(format!("Unsupported accelerated surface color format: {:?}", other)),
        }
    }
}

#[cfg(target_os = "macos")]
pub use macos::{BitmapCacheKey, ImageRenderer, SurfaceSlot};
