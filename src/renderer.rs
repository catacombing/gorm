//! OpenGL renderer.

use std::ffi::CString;
use std::num::NonZeroU32;
use std::ptr::NonNull;
use std::{mem, ptr};

use glutin::config::{Api, ConfigTemplateBuilder};
use glutin::context::{ContextApi, ContextAttributesBuilder, PossiblyCurrentContext, Version};
use glutin::display::Display;
use glutin::prelude::*;
use glutin::surface::{Surface, SurfaceAttributesBuilder, SwapInterval, WindowSurface};
use raw_window_handle::{RawWindowHandle, WaylandWindowHandle};
use smithay_client_toolkit::reexports::client::Proxy;
use smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface;

use crate::geometry::{Position, Size};
use crate::gl;
use crate::gl::types::{GLfloat, GLint, GLuint};

// OpenGL shader programs.
const VERTEX_SHADER: &str = include_str!("../shaders/vertex.glsl");
const FRAGMENT_SHADER: &str = include_str!("../shaders/fragment.glsl");

/// OpenGL renderer.
#[derive(Debug)]
pub struct Renderer {
    sized: Option<SizedRenderer>,
    surface: WlSurface,
    display: Display,
}

impl Renderer {
    /// Initialize a new renderer.
    pub fn new(display: Display, surface: WlSurface) -> Self {
        // Setup OpenGL symbol loader.
        gl::load_with(|symbol| {
            let symbol = CString::new(symbol).unwrap();
            display.get_proc_address(symbol.as_c_str()).cast()
        });

        Renderer { surface, display, sized: Default::default() }
    }

    /// Perform drawing with this renderer mapped.
    pub fn draw<F: FnOnce(&Renderer)>(&mut self, size: Size, fun: F) {
        self.sized(size).make_current();

        // Resize OpenGL viewport.
        //
        // This isn't done in `Self::resize` since the renderer must be current.
        unsafe { gl::Viewport(0, 0, size.width as i32, size.height as i32) };

        fun(self);

        unsafe { gl::Flush() };

        self.sized(size).swap_buffers();
    }

    /// Render texture at a position in viewport-coordinates.
    ///
    /// Specifying a `size` will automatically scale the texture to render at
    /// the desired size. Otherwise the texture's size will be used instead.
    pub fn draw_texture_at(
        &self,
        texture: &Texture,
        mut position: Position<f32>,
        size: impl Into<Option<Size<f32>>>,
    ) {
        // Fail before renderer initialization.
        //
        // The sized state should always be initialized since it only makes sense to
        // call this function within `Self::draw`'s closure.
        let sized = match &self.sized {
            Some(sized) => sized,
            None => unreachable!(),
        };

        let (width, height) = match size.into() {
            Some(Size { width, height }) => (width, height),
            None => (texture.width as f32, texture.height as f32),
        };

        unsafe {
            // Matrix transforming vertex positions to desired size.
            let size: Size<f32> = sized.size.into();
            let x_scale = width / size.width;
            let y_scale = height / size.height;
            let matrix = [x_scale, 0., 0., y_scale];
            gl::UniformMatrix2fv(sized.uniform_matrix, 1, gl::FALSE, matrix.as_ptr());

            // Set texture position offset.
            position.x /= size.width / 2.;
            position.y /= size.height / 2.;
            gl::Uniform2fv(sized.uniform_position, 1, [position.x, -position.y].as_ptr());

            gl::BindTexture(gl::TEXTURE_2D, texture.id);

            gl::DrawArrays(gl::TRIANGLES, 0, 6);
        }
    }

    /// Get render state requiring a size.
    fn sized(&mut self, size: Size) -> &SizedRenderer {
        // Initialize or resize sized state.
        match &mut self.sized {
            // Resize renderer.
            Some(sized) => sized.resize(size),
            // Create sized state.
            None => {
                self.sized = Some(SizedRenderer::new(&self.display, &self.surface, size));
            },
        }

        self.sized.as_ref().unwrap()
    }
}

/// Render state requiring known size.
///
/// This state is initialized on-demand, to avoid Mesa's issue with resizing
/// before the first draw.
#[derive(Debug)]
struct SizedRenderer {
    uniform_position: GLint,
    uniform_matrix: GLint,

    egl_surface: Surface<WindowSurface>,
    egl_context: PossiblyCurrentContext,

    size: Size,
}

impl SizedRenderer {
    /// Create sized renderer state.
    fn new(display: &Display, surface: &WlSurface, size: Size) -> Self {
        // Create EGL surface and context and make it current.
        let (egl_surface, egl_context) = Self::create_surface(display, surface, size);

        // Setup OpenGL program.
        let (uniform_position, uniform_matrix) = Self::create_program();

        Self { uniform_position, uniform_matrix, egl_surface, egl_context, size }
    }

    /// Resize the renderer.
    fn resize(&mut self, size: Size) {
        if self.size == size {
            return;
        }

        // Resize EGL texture.
        self.egl_surface.resize(
            &self.egl_context,
            NonZeroU32::new(size.width).unwrap(),
            NonZeroU32::new(size.height).unwrap(),
        );

        self.size = size;
    }

    /// Make EGL surface current.
    fn make_current(&self) {
        self.egl_context.make_current(&self.egl_surface).unwrap();
    }

    /// Perform OpenGL buffer swap.
    fn swap_buffers(&self) {
        self.egl_surface.swap_buffers(&self.egl_context).unwrap();
    }

    /// Create a new EGL surface.
    fn create_surface(
        display: &Display,
        surface: &WlSurface,
        size: Size,
    ) -> (Surface<WindowSurface>, PossiblyCurrentContext) {
        assert!(size.width > 0 && size.height > 0);

        // Create EGL config.
        let config_template = ConfigTemplateBuilder::new().with_api(Api::GLES2).build();
        let egl_config = unsafe {
            display
                .find_configs(config_template)
                .ok()
                .and_then(|mut configs| configs.next())
                .unwrap()
        };

        // Create EGL context.
        let context_attributes = ContextAttributesBuilder::new()
            .with_context_api(ContextApi::Gles(Some(Version::new(2, 0))))
            .build(None);
        let egl_context =
            unsafe { display.create_context(&egl_config, &context_attributes).unwrap() };
        let egl_context = egl_context.treat_as_possibly_current();

        let surface = NonNull::new(surface.id().as_ptr().cast()).unwrap();
        let raw_window_handle = WaylandWindowHandle::new(surface);
        let raw_window_handle = RawWindowHandle::Wayland(raw_window_handle);
        let surface_attributes = SurfaceAttributesBuilder::<WindowSurface>::new().build(
            raw_window_handle,
            NonZeroU32::new(size.width).unwrap(),
            NonZeroU32::new(size.height).unwrap(),
        );

        let egl_surface =
            unsafe { display.create_window_surface(&egl_config, &surface_attributes).unwrap() };

        // Ensure rendering never blocks.
        egl_context.make_current(&egl_surface).unwrap();
        egl_surface.set_swap_interval(&egl_context, SwapInterval::DontWait).unwrap();

        (egl_surface, egl_context)
    }

    /// Create the OpenGL program.
    fn create_program() -> (GLint, GLint) {
        unsafe {
            // Create vertex shader.
            let vertex_shader = gl::CreateShader(gl::VERTEX_SHADER);
            gl::ShaderSource(
                vertex_shader,
                1,
                [VERTEX_SHADER.as_ptr()].as_ptr() as *const _,
                &(VERTEX_SHADER.len() as i32) as *const _,
            );
            gl::CompileShader(vertex_shader);

            // Create fragment shader.
            let fragment_shader = gl::CreateShader(gl::FRAGMENT_SHADER);
            gl::ShaderSource(
                fragment_shader,
                1,
                [FRAGMENT_SHADER.as_ptr()].as_ptr() as *const _,
                &(FRAGMENT_SHADER.len() as i32) as *const _,
            );
            gl::CompileShader(fragment_shader);

            // Create shader program.
            let program = gl::CreateProgram();
            gl::AttachShader(program, vertex_shader);
            gl::AttachShader(program, fragment_shader);
            gl::LinkProgram(program);
            gl::UseProgram(program);

            // Generate VBO.
            let mut vbo = 0;
            gl::GenBuffers(1, &mut vbo);
            gl::BindBuffer(gl::ARRAY_BUFFER, vbo);

            // Fill VBO with vertex positions.
            #[rustfmt::skip]
            let vertices: [GLfloat; 12] = [
                -1.0,  1.0, // Top-left
                -1.0, -1.0, // Bottom-left
                 1.0, -1.0, // Bottom-right

                -1.0,  1.0, // Top-left
                 1.0, -1.0, // Bottom-right
                 1.0,  1.0, // Top-right
            ];
            gl::BufferData(
                gl::ARRAY_BUFFER,
                (mem::size_of::<GLfloat>() * vertices.len()) as isize,
                vertices.as_ptr() as *const _,
                gl::STATIC_DRAW,
            );

            // Define VBO layout.
            let location = gl::GetAttribLocation(program, c"aVertexPosition".as_ptr()) as GLuint;
            gl::VertexAttribPointer(
                location,
                2,
                gl::FLOAT,
                gl::FALSE,
                2 * mem::size_of::<GLfloat>() as i32,
                ptr::null(),
            );
            gl::EnableVertexAttribArray(0);

            // Get uniform locations.
            let uniform_position = gl::GetUniformLocation(program, c"uPosition".as_ptr());
            let uniform_matrix = gl::GetUniformLocation(program, c"uMatrix".as_ptr());

            (uniform_position, uniform_matrix)
        }
    }
}

/// OpenGL texture.
#[derive(Debug)]
pub struct Texture {
    pub width: usize,
    pub height: usize,

    id: u32,
}

impl Texture {
    /// Load a buffer as texture into OpenGL.
    pub fn new(buffer: &[u8], width: usize, height: usize) -> Self {
        Self::new_with_format(buffer, width, height, gl::RGBA)
    }

    pub fn new_with_format(buffer: &[u8], width: usize, height: usize, color_format: u32) -> Self {
        assert!(buffer.len() == width * height * 4);

        unsafe {
            let mut id = 0;
            gl::GenTextures(1, &mut id);
            gl::BindTexture(gl::TEXTURE_2D, id);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as i32);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE as i32);
            gl::TexImage2D(
                gl::TEXTURE_2D,
                0,
                color_format as i32,
                width as i32,
                height as i32,
                0,
                color_format,
                gl::UNSIGNED_BYTE,
                buffer.as_ptr() as *const _,
            );
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as i32);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as i32);
            Self { id, width, height }
        }
    }

    /// Delete this texture.
    ///
    /// Since texture IDs are context-specific, the context must be bound when
    /// calling this function.
    pub fn delete(&self) {
        unsafe {
            gl::DeleteTextures(1, &self.id);
        }
    }
}
