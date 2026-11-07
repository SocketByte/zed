use crate::{
    AnyElement, AnyImageCache, App, Asset, AssetLogger, Bounds, DefiniteLength, Element, ElementId,
    Entity, GlobalElementId, Hitbox, Image, ImageCache, InspectorElementId, InteractiveElement,
    Interactivity, IntoElement, LayoutId, Length, ObjectFit, Pixels, RenderImage, Resource,
    SharedString, SharedUri, StyleRefinement, Styled, Task, Window, decode_static_image,
    decode_static_image_from_decoder, px,
};
use anyhow::Result;

use futures::Future;
use gpui_util::ResultExt;
use image::{
    AnimationDecoder, ImageError, ImageFormat, Rgba,
    codecs::{gif::GifDecoder, webp::WebPDecoder},
};
use scheduler::Instant;
use smallvec::SmallVec;
use std::{
    fs,
    io::{self, Cursor},
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::Duration,
};
use thiserror::Error;

use super::{Stateful, StatefulInteractiveElement};

/// The delay before showing the loading state.
pub const LOADING_DELAY: Duration = Duration::from_millis(200);

/// A type alias to the resource loader that the `img()` element uses.
///
/// Note: that this is only for Resources, like URLs or file paths.
/// Custom loaders, or external images will not use this asset loader
pub type ImgResourceLoader = AssetLogger<ImageAssetLoader>;

/// A source of image content.
#[derive(Clone)]
pub enum ImageSource {
    /// The image content will be loaded from some resource location
    Resource(Resource),
    /// Cached image data
    Render(Arc<RenderImage>),
    /// Cached image data
    Image(Arc<Image>),
    /// A custom loading function to use
    Custom(Arc<dyn Fn(&mut Window, &mut App) -> Option<Result<Arc<RenderImage>, ImageCacheError>>>),
}

fn is_uri(uri: &str) -> bool {
    url::Url::from_str(uri).is_ok()
}

impl From<SharedUri> for ImageSource {
    fn from(value: SharedUri) -> Self {
        Self::Resource(Resource::Uri(value))
    }
}

impl<'a> From<&'a str> for ImageSource {
    fn from(s: &'a str) -> Self {
        if is_uri(s) {
            Self::Resource(Resource::Uri(s.to_string().into()))
        } else {
            Self::Resource(Resource::Embedded(s.to_string().into()))
        }
    }
}

impl From<String> for ImageSource {
    fn from(s: String) -> Self {
        if is_uri(&s) {
            Self::Resource(Resource::Uri(s.into()))
        } else {
            Self::Resource(Resource::Embedded(s.into()))
        }
    }
}

impl From<SharedString> for ImageSource {
    fn from(s: SharedString) -> Self {
        s.as_ref().into()
    }
}

impl From<&Path> for ImageSource {
    fn from(value: &Path) -> Self {
        Self::Resource(value.to_path_buf().into())
    }
}

impl From<Arc<Path>> for ImageSource {
    fn from(value: Arc<Path>) -> Self {
        Self::Resource(value.into())
    }
}

impl From<PathBuf> for ImageSource {
    fn from(value: PathBuf) -> Self {
        Self::Resource(value.into())
    }
}

impl From<Arc<RenderImage>> for ImageSource {
    fn from(value: Arc<RenderImage>) -> Self {
        Self::Render(value)
    }
}

impl From<Arc<Image>> for ImageSource {
    fn from(value: Arc<Image>) -> Self {
        Self::Image(value)
    }
}

impl<F> From<F> for ImageSource
where
    F: Fn(&mut Window, &mut App) -> Option<Result<Arc<RenderImage>, ImageCacheError>> + 'static,
{
    fn from(value: F) -> Self {
        Self::Custom(Arc::new(value))
    }
}

/// The style of an image element.
pub struct ImageStyle {
    grayscale: bool,
    object_fit: ObjectFit,
    loading: Option<Box<dyn Fn() -> AnyElement>>,
    fallback: Option<Box<dyn Fn() -> AnyElement>>,
}

impl Default for ImageStyle {
    fn default() -> Self {
        Self {
            grayscale: false,
            object_fit: ObjectFit::Contain,
            loading: None,
            fallback: None,
        }
    }
}

/// Style an image element.
pub trait StyledImage: Sized {
    /// Get a mutable [ImageStyle] from the element.
    fn image_style(&mut self) -> &mut ImageStyle;

    /// Set the image to be displayed in grayscale.
    fn grayscale(mut self, grayscale: bool) -> Self {
        self.image_style().grayscale = grayscale;
        self
    }

    /// Set the object fit for the image.
    fn object_fit(mut self, object_fit: ObjectFit) -> Self {
        self.image_style().object_fit = object_fit;
        self
    }

    /// Set a fallback function that will be invoked to render an error view should
    /// the image fail to load.
    fn with_fallback(mut self, fallback: impl Fn() -> AnyElement + 'static) -> Self {
        self.image_style().fallback = Some(Box::new(fallback));
        self
    }

    /// Set a fallback function that will be invoked to render a view while the image
    /// is still being loaded.
    fn with_loading(mut self, loading: impl Fn() -> AnyElement + 'static) -> Self {
        self.image_style().loading = Some(Box::new(loading));
        self
    }
}

impl StyledImage for Img {
    fn image_style(&mut self) -> &mut ImageStyle {
        &mut self.style
    }
}

impl StyledImage for Stateful<Img> {
    fn image_style(&mut self) -> &mut ImageStyle {
        &mut self.element.style
    }
}

/// An image element.
pub struct Img {
    interactivity: Interactivity,
    source: ImageSource,
    style: ImageStyle,
    image_cache: Option<AnyImageCache>,
}

/// Create a new image element.
#[track_caller]
pub fn img(source: impl Into<ImageSource>) -> Img {
    Img {
        interactivity: Interactivity::new(),
        source: source.into(),
        style: ImageStyle::default(),
        image_cache: None,
    }
}

impl Img {
    /// A list of all format extensions currently supported by this img element
    pub fn extensions() -> &'static [&'static str] {
        // This is the list in [image::ImageFormat::from_extension] + `svg`
        &[
            "avif", "jpg", "jpeg", "png", "gif", "webp", "tif", "tiff", "tga", "dds", "bmp", "ico",
            "hdr", "exr", "pbm", "pam", "ppm", "pgm", "ff", "farbfeld", "qoi", "svg",
        ]
    }

    /// Sets the image cache for the current node.
    ///
    /// If the `image_cache` is not explicitly provided, the function will determine the image cache by:
    ///
    /// 1. Checking if any ancestor node of the current node contains an `ImageCacheElement`, If such a node exists, the image cache specified by that ancestor will be used.
    /// 2. If no ancestor node contains an `ImageCacheElement`, the global image cache will be used as a fallback.
    ///
    /// This mechanism provides a flexible way to manage image caching, allowing precise control when needed,
    /// while ensuring a default behavior when no cache is explicitly specified.
    #[inline]
    pub fn image_cache<I: ImageCache>(self, image_cache: &Entity<I>) -> Self {
        Self {
            image_cache: Some(image_cache.clone().into()),
            ..self
        }
    }
}

impl Deref for Stateful<Img> {
    type Target = Img;

    fn deref(&self) -> &Self::Target {
        &self.element
    }
}

impl DerefMut for Stateful<Img> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.element
    }
}

/// The image state between frames
struct ImgState {
    started_loading: Option<(Instant, Task<()>)>,
    /// When the view is to be drawn again for an animation's next frame. Which frame to draw is
    /// the image's own clock's ([`RenderImage::frame_at`]), never kept here.
    next_frame: Option<(Instant, Task<()>)>,
}

/// How late a timer may fire: Windows's thread pool timers fire on its 15.6 ms tick.
const TIMER_LATENESS: Duration = Duration::from_millis(20);

/// Draws the current view again on the first frame after `due`. A timer wakes a little before,
/// as a timer alone can fire up to a tick late, and from then each frame checks the time, which
/// draws nothing until the frame is due.
fn redraw_after(due: Instant, window: &mut Window, cx: &mut App) -> Task<()> {
    let Some(view) = window.current_view_if_any() else {
        return Task::ready(());
    };
    window.spawn(cx, async move |cx| {
        let early = due
            .saturating_duration_since(Instant::now())
            .saturating_sub(TIMER_LATENESS);
        if !early.is_zero() {
            cx.background_executor().timer(early).await;
        }
        loop {
            let (sent, arrived) = futures::channel::oneshot::channel();
            let asked = cx.update(|window, _| {
                window.on_next_frame(move |_, cx| {
                    let ready = Instant::now() >= due;
                    if ready {
                        cx.notify(view);
                    }
                    sent.send(ready).ok();
                })
            });
            if asked.is_err() || !matches!(arrived.await, Ok(false)) {
                break;
            }
        }
    })
}

/// The image layout state between frames
pub struct ImgLayoutState {
    frame_index: usize,
    replacement: Option<AnyElement>,
}

impl Element for Img {
    type RequestLayoutState = ImgLayoutState;
    type PrepaintState = Option<Hitbox>;

    fn id(&self) -> Option<ElementId> {
        self.interactivity.element_id.clone()
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        self.interactivity.source_location()
    }

    fn request_layout(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut layout_state = ImgLayoutState {
            frame_index: 0,
            replacement: None,
        };

        window.with_optional_element_state(global_id, |state, window| {
            let mut state = state.map(|state| {
                state.unwrap_or(ImgState {
                    started_loading: None,
                    next_frame: None,
                })
            });

            let mut frame_index = 0;

            let layout_id = self.interactivity.request_layout(
                global_id,
                inspector_id,
                window,
                cx,
                |mut style, window, cx| {
                    let mut replacement_id = None;

                    match self.source.use_data(
                        self.image_cache
                            .clone()
                            .or_else(|| window.image_cache_stack.last().cloned()),
                        window,
                        cx,
                    ) {
                        Some(Ok(data)) => {
                            // The frame the image's own clock is at. An element that stepped its
                            // own frame along drew frames late and unevenly, started over when it
                            // came back into view, and never moved with other copies of the image.
                            let (frame, next_frame_due) = if cx.reduce_motion() {
                                (0, None)
                            } else {
                                data.frame_at(Instant::now())
                            };
                            frame_index = frame;
                            if let Some(state) = &mut state {
                                state.started_loading = None;
                            }

                            let image_size = data.render_size(frame_index);

                            if style.aspect_ratio.is_none() {
                                style.aspect_ratio = Some(image_size.width / image_size.height);
                            }

                            if let Length::Auto = style.size.width {
                                style.size.width = match style.size.height {
                                    Length::Definite(DefiniteLength::Absolute(abs_length)) => {
                                        let height_px = abs_length.to_pixels(window.rem_size());
                                        Length::Definite(
                                            px(image_size.width.0 * height_px.0
                                                / image_size.height.0)
                                            .into(),
                                        )
                                    }
                                    _ => Length::Definite(image_size.width.into()),
                                };
                            }

                            if let Length::Auto = style.size.height {
                                style.size.height = match style.size.width {
                                    Length::Definite(DefiniteLength::Absolute(abs_length)) => {
                                        let width_px = abs_length.to_pixels(window.rem_size());
                                        Length::Definite(
                                            px(image_size.height.0 * width_px.0
                                                / image_size.width.0)
                                            .into(),
                                        )
                                    }
                                    _ => Length::Definite(image_size.height.into()),
                                };
                            }

                            // Drawn again on the first frame after the next one is due, rather
                            // than on every vsync, which drew the whole view 240 times a second
                            // on a 240 Hz display for an animation of 20 frames a second. Every
                            // copy waits for the same moment, so they're drawn together. As in a
                            // browser, it keeps moving while the window is visible but not
                            // focused.
                            if let Some(due) = next_frame_due {
                                match &mut state {
                                    Some(state) => {
                                        if !state
                                            .next_frame
                                            .as_ref()
                                            .is_some_and(|(at, _)| *at == due)
                                        {
                                            state.next_frame =
                                                Some((due, redraw_after(due, window, cx)));
                                        }
                                    }
                                    None => redraw_after(due, window, cx).detach(),
                                }
                            }
                        }
                        Some(_err) => {
                            if let Some(fallback) = self.style.fallback.as_ref() {
                                let mut element = fallback();
                                replacement_id = Some(element.request_layout(window, cx));
                                layout_state.replacement = Some(element);
                            }
                            if let Some(state) = &mut state {
                                state.started_loading = None;
                            }
                        }
                        None => {
                            if let Some(state) = &mut state {
                                if let Some((started_loading, _)) = state.started_loading {
                                    if started_loading.elapsed() > LOADING_DELAY
                                        && let Some(loading) = self.style.loading.as_ref()
                                    {
                                        let mut element = loading();
                                        replacement_id = Some(element.request_layout(window, cx));
                                        layout_state.replacement = Some(element);
                                    }
                                } else {
                                    let current_view = window.current_view();
                                    let task = window.spawn(cx, async move |cx| {
                                        cx.background_executor().timer(LOADING_DELAY).await;
                                        cx.update(move |_, cx| {
                                            cx.notify(current_view);
                                        })
                                        .ok();
                                    });
                                    state.started_loading = Some((Instant::now(), task));
                                }
                            }
                        }
                    }

                    window.request_layout(style, replacement_id, cx)
                },
            );

            layout_state.frame_index = frame_index;

            ((layout_id, layout_state), state)
        })
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.interactivity.prepaint(
            global_id,
            inspector_id,
            bounds,
            bounds.size,
            window,
            cx,
            |_, _, hitbox, window, cx| {
                if let Some(replacement) = &mut request_layout.replacement {
                    replacement.prepaint(window, cx);
                }

                hitbox
            },
        )
    }

    fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout_state: &mut Self::RequestLayoutState,
        hitbox: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let source = self.source.clone();
        self.interactivity.paint(
            global_id,
            inspector_id,
            bounds,
            hitbox.as_ref(),
            window,
            cx,
            |style, window, cx| {
                if let Some(Ok(data)) = source.use_data(
                    self.image_cache
                        .clone()
                        .or_else(|| window.image_cache_stack.last().cloned()),
                    window,
                    cx,
                ) {
                    if data.frame_count() == 0 {
                        return;
                    }
                    // Held to the image painted, should it differ from the one laid out.
                    let frame_index = if layout_state.frame_index < data.frame_count() {
                        layout_state.frame_index
                    } else {
                        0
                    };
                    let new_bounds = self
                        .style
                        .object_fit
                        .get_bounds(bounds, data.size(frame_index));
                    let corner_radii = style.corner_radii.to_pixels(window.rem_size());
                    window
                        .paint_image(
                            bounds,
                            new_bounds,
                            corner_radii,
                            data,
                            frame_index,
                            self.style.grayscale,
                        )
                        .log_err();
                } else if let Some(replacement) = &mut layout_state.replacement {
                    replacement.paint(window, cx);
                }
            },
        )
    }
}

impl Styled for Img {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.interactivity.base_style
    }
}

impl InteractiveElement for Img {
    fn interactivity(&mut self) -> &mut Interactivity {
        &mut self.interactivity
    }
}

impl IntoElement for Img {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl StatefulInteractiveElement for Img {}

impl ImageSource {
    pub(crate) fn use_data(
        &self,
        cache: Option<AnyImageCache>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Result<Arc<RenderImage>, ImageCacheError>> {
        match self {
            ImageSource::Resource(resource) => {
                if let Some(cache) = cache {
                    cache.load(resource, window, cx)
                } else {
                    window.use_asset::<ImgResourceLoader>(resource, cx)
                }
            }
            ImageSource::Custom(loading_fn) => loading_fn(window, cx),
            ImageSource::Render(data) => Some(Ok(data.to_owned())),
            ImageSource::Image(data) => window.use_asset::<AssetLogger<ImageDecoder>>(data, cx),
        }
    }

    pub(crate) fn get_data(
        &self,
        cache: Option<AnyImageCache>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Result<Arc<RenderImage>, ImageCacheError>> {
        match self {
            ImageSource::Resource(resource) => {
                if let Some(cache) = cache {
                    cache.load(resource, window, cx)
                } else {
                    window.get_asset::<ImgResourceLoader>(resource, cx)
                }
            }
            ImageSource::Custom(loading_fn) => loading_fn(window, cx),
            ImageSource::Render(data) => Some(Ok(data.to_owned())),
            ImageSource::Image(data) => window.get_asset::<AssetLogger<ImageDecoder>>(data, cx),
        }
    }

    /// Remove this image source from the asset system
    pub fn remove_asset(&self, cx: &mut App) {
        match self {
            ImageSource::Resource(resource) => {
                cx.remove_asset::<ImgResourceLoader>(resource);
            }
            ImageSource::Custom(_) | ImageSource::Render(_) => {}
            ImageSource::Image(data) => cx.remove_asset::<AssetLogger<ImageDecoder>>(data),
        }
    }

    /// Check whether this image source is present in the asset system (loading
    /// or loaded), without fetching it.
    #[cfg(any(test, feature = "test-support"))]
    pub fn is_asset_cached(&self, cx: &App) -> bool {
        match self {
            ImageSource::Resource(resource) => cx.has_asset::<ImgResourceLoader>(resource),
            ImageSource::Custom(_) | ImageSource::Render(_) => false,
            ImageSource::Image(data) => cx.has_asset::<AssetLogger<ImageDecoder>>(data),
        }
    }
}

#[derive(Clone)]
enum ImageDecoder {}

impl Asset for ImageDecoder {
    type Source = Arc<Image>;
    type Output = Result<Arc<RenderImage>, ImageCacheError>;

    fn load(
        source: Self::Source,
        cx: &mut App,
    ) -> impl Future<Output = Self::Output> + Send + 'static {
        let renderer = cx.svg_renderer();
        async move { source.to_image_data(renderer).map_err(Into::into) }
    }
}

/// An image loader for the GPUI asset system
#[derive(Clone)]
pub enum ImageAssetLoader {}

impl Asset for ImageAssetLoader {
    type Source = Resource;
    type Output = Result<Arc<RenderImage>, ImageCacheError>;

    fn load(
        source: Self::Source,
        cx: &mut App,
    ) -> impl Future<Output = Self::Output> + Send + 'static {
        let client = cx.http_client();
        // TODO: Can we make SVGs always rescale?
        // let scale_factor = cx.scale_factor();
        let svg_renderer = cx.svg_renderer();
        let asset_source = cx.asset_source().clone();
        async move {
            let bytes = match source.clone() {
                Resource::Path(uri) => fs::read(uri.as_ref())?,
                Resource::Uri(uri) => {
                    use anyhow::Context as _;
                    use futures::AsyncReadExt as _;

                    let mut response = client
                        .get(uri.as_ref(), ().into(), true)
                        .await
                        .with_context(|| format!("loading image asset from {uri:?}"))?;
                    let mut body = Vec::new();
                    response.body_mut().read_to_end(&mut body).await?;
                    if !response.status().is_success() {
                        let mut body = String::from_utf8_lossy(&body).into_owned();
                        let first_line = body.lines().next().unwrap_or("").trim_end();
                        body.truncate(first_line.len());
                        return Err(ImageCacheError::BadStatus {
                            uri,
                            status: response.status(),
                            body,
                        });
                    }
                    body
                }
                Resource::Embedded(path) => {
                    let data = asset_source.load(&path).ok().flatten();
                    if let Some(data) = data {
                        data.to_vec()
                    } else {
                        return Err(ImageCacheError::Asset(
                            format!("Embedded resource not found: {}", path).into(),
                        ));
                    }
                }
            };

            if let Ok(format) = image::guess_format(&bytes) {
                let data = match format {
                    ImageFormat::Gif => {
                        let decoder = GifDecoder::new(Cursor::new(&bytes))?;
                        let mut frames = SmallVec::new();

                        for frame in decoder.into_frames() {
                            match frame {
                                Ok(mut frame) => {
                                    // Convert from RGBA to BGRA.
                                    for pixel in frame.buffer_mut().chunks_exact_mut(4) {
                                        pixel.swap(0, 2);
                                    }
                                    frames.push(frame);
                                }
                                Err(err) => {
                                    log::debug!(
                                        "Skipping GIF frame in {source:?} due to decode error: {err}"
                                    );
                                }
                            }
                        }

                        if frames.is_empty() {
                            return Err(ImageCacheError::Other(Arc::new(anyhow::anyhow!(
                                "GIF could not be decoded: all frames failed ({source:?})"
                            ))));
                        }

                        frames
                    }
                    ImageFormat::WebP => {
                        let mut decoder = WebPDecoder::new(Cursor::new(&bytes))?;

                        if decoder.has_animation() {
                            let _ = decoder.set_background_color(Rgba([0, 0, 0, 0]));
                            let mut frames = SmallVec::new();

                            for frame in decoder.into_frames() {
                                match frame {
                                    Ok(mut frame) => {
                                        // Convert from RGBA to BGRA.
                                        for pixel in frame.buffer_mut().chunks_exact_mut(4) {
                                            pixel.swap(0, 2);
                                        }
                                        frames.push(frame);
                                    }
                                    Err(err) => {
                                        log::debug!(
                                            "Skipping WebP frame in {source:?} due to decode error: {err}"
                                        );
                                    }
                                }
                            }

                            if frames.is_empty() {
                                return Err(ImageCacheError::Other(Arc::new(anyhow::anyhow!(
                                    "WebP could not be decoded: all frames failed ({source:?})"
                                ))));
                            }

                            frames
                        } else {
                            decode_static_image_from_decoder(decoder)?
                        }
                    }
                    _ => decode_static_image(&bytes, format)?,
                };

                Ok(Arc::new(RenderImage::new(data)))
            } else {
                svg_renderer
                    .render_single_frame(&bytes, 1.0)
                    .map_err(Into::into)
            }
        }
    }
}

/// An error that can occur when interacting with the image cache.
#[derive(Debug, Error, Clone)]
pub enum ImageCacheError {
    /// Some other kind of error occurred
    #[error("error: {0}")]
    Other(#[from] Arc<anyhow::Error>),
    /// An error that occurred while reading the image from disk.
    #[error("IO error: {0}")]
    Io(Arc<std::io::Error>),
    /// An error that occurred while processing an image.
    #[error("unexpected http status for {uri}: {status}, body: {body}")]
    BadStatus {
        /// The URI of the image.
        uri: SharedUri,
        /// The HTTP status code.
        status: http_client::StatusCode,
        /// The HTTP response body.
        body: String,
    },
    /// An error that occurred while processing an asset.
    #[error("asset error: {0}")]
    Asset(SharedString),
    /// An error that occurred while processing an image.
    #[error("image error: {0}")]
    Image(Arc<ImageError>),
    /// An error that occurred while processing an SVG.
    #[error("svg error: {0}")]
    Usvg(Arc<usvg::Error>),
}

impl From<anyhow::Error> for ImageCacheError {
    fn from(value: anyhow::Error) -> Self {
        Self::Other(Arc::new(value))
    }
}

impl From<io::Error> for ImageCacheError {
    fn from(value: io::Error) -> Self {
        Self::Io(Arc::new(value))
    }
}

impl From<usvg::Error> for ImageCacheError {
    fn from(value: usvg::Error) -> Self {
        Self::Usvg(Arc::new(value))
    }
}

impl From<image::ImageError> for ImageCacheError {
    fn from(value: image::ImageError) -> Self {
        Self::Image(Arc::new(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ParentElement as _, TestAppContext, div, point, px, size};
    use image::{Frame, ImageBuffer, Rgba};

    const TEST_IMG_ID: &str = "test-img";

    fn test_image(frame_count: usize) -> Arc<RenderImage> {
        let frame = Frame::new(ImageBuffer::from_pixel(1, 1, Rgba([0, 0, 0, 0])));
        Arc::new(RenderImage::new(SmallVec::from_iter(
            (0..frame_count).map(|_| frame.clone()),
        )))
    }

    fn test_image_with_size(width: u32, height: u32) -> Arc<RenderImage> {
        let frame = Frame::new(ImageBuffer::from_pixel(width, height, Rgba([0, 0, 0, 0])));
        Arc::new(RenderImage::new(SmallVec::from_elem(frame, 1)))
    }

    fn animation(delays_ms: &[u32]) -> RenderImage {
        RenderImage::new(
            delays_ms
                .iter()
                .map(|&ms| {
                    Frame::from_parts(
                        ImageBuffer::from_pixel(2, 2, Rgba([0, 0, 0, 0])),
                        0,
                        0,
                        image::Delay::from_numer_denom_ms(ms, 1),
                    )
                })
                .collect::<Vec<_>>(),
        )
    }

    #[gpui::test]
    fn zero_frame_image_does_not_panic_on_paint(cx: &mut TestAppContext) {
        cx.add_empty_window()
            .draw(point(px(0.), px(0.)), size(px(100.), px(100.)), |_, _| {
                img(ImageSource::Render(test_image(0))).into_any_element()
            });
    }

    #[gpui::test]
    fn image_object_fit_cover_crops_to_element_bounds(cx: &mut TestAppContext) {
        let window = cx.add_empty_window();
        let image = test_image_with_size(200, 100);
        window.draw(point(px(0.), px(0.)), size(px(100.), px(100.)), |_, _| {
            img(ImageSource::Render(image.clone()))
                .size_full()
                .object_fit(ObjectFit::Fill)
                .into_any_element()
        });
        let full_tile_bounds = window.update(|window, _| {
            window
                .rendered_frame
                .scene
                .polychrome_sprites
                .last()
                .expect("fill image should paint a sprite")
                .tile
                .bounds
        });

        window.draw(point(px(10.), px(20.)), size(px(100.), px(100.)), |_, _| {
            img(ImageSource::Render(image))
                .size_full()
                .object_fit(ObjectFit::Cover)
                .into_any_element()
        });

        let (rendered_bounds, rendered_tile_bounds, scale_factor) = window.update(|window, _| {
            let sprite = window
                .rendered_frame
                .scene
                .polychrome_sprites
                .last()
                .expect("cover image should paint a sprite");
            (sprite.bounds, sprite.tile.bounds, window.scale_factor())
        });
        assert_eq!(
            rendered_bounds,
            Bounds {
                origin: point(px(10.).scale(scale_factor), px(20.).scale(scale_factor)),
                size: size(px(100.).scale(scale_factor), px(100.).scale(scale_factor)),
            }
        );
        assert_eq!(
            (
                rendered_tile_bounds.origin.x.0 - full_tile_bounds.origin.x.0,
                rendered_tile_bounds.origin.y.0 - full_tile_bounds.origin.y.0,
                rendered_tile_bounds.size.width.0,
                rendered_tile_bounds.size.height.0,
            ),
            (50, 0, 100, 100),
        );
    }

    #[gpui::test]
    fn explicit_aspect_ratio_is_not_overridden_by_intrinsic_ratio(cx: &mut TestAppContext) {
        let window = cx.add_empty_window();

        // A portrait image in a square container
        window.draw(point(px(0.), px(0.)), size(px(100.), px(100.)), |_, _| {
            div()
                .size(px(100.))
                .overflow_hidden()
                .child(
                    img(ImageSource::Render(test_image_with_size(100, 200)))
                        .size_full()
                        .aspect_square()
                        .object_fit(ObjectFit::Contain),
                )
                .into_any_element()
        });

        let (rendered_bounds, scale_factor) = window.update(|window, _| {
            let sprite = window
                .rendered_frame
                .scene
                .polychrome_sprites
                .last()
                .expect("contained image should paint a sprite");
            (sprite.bounds, window.scale_factor())
        });

        // The element stays 100x100, so the image is letterboxed horizontally
        assert_eq!(
            rendered_bounds,
            Bounds {
                origin: point(px(25.).scale(scale_factor), px(0.).scale(scale_factor)),
                size: size(px(50.).scale(scale_factor), px(100.).scale(scale_factor)),
            }
        );
    }

    #[gpui::test]
    fn image_object_fit_cover_clamps_corner_radii_to_visible_bounds(cx: &mut TestAppContext) {
        let window = cx.add_empty_window();
        window.draw(point(px(0.), px(0.)), size(px(100.), px(100.)), |_, _| {
            img(ImageSource::Render(test_image_with_size(200, 100)))
                .size_full()
                .rounded(px(100.))
                .object_fit(ObjectFit::Cover)
                .into_any_element()
        });

        let (corner_radius, expected_corner_radius) = window.update(|window, _| {
            (
                window
                    .rendered_frame
                    .scene
                    .polychrome_sprites
                    .last()
                    .map(|sprite| sprite.corner_radii.top_left),
                px(50.).scale(window.scale_factor()),
            )
        });
        assert_eq!(corner_radius, Some(expected_corner_radius));
    }

    #[gpui::test]
    fn replacing_an_animation_with_fewer_frames_does_not_panic(cx: &mut TestAppContext) {
        let window = cx.add_empty_window();
        let animated = Arc::new(animation(&[40, 40, 40, 40, 40]));
        // Its clock starts at the first draw and is at the last frame by the second.
        animated.frame_at(Instant::now() - Duration::from_millis(190));
        window.draw(point(px(0.), px(0.)), size(px(100.), px(100.)), |_, _| {
            img(ImageSource::Render(animated.clone()))
                .id(TEST_IMG_ID)
                .into_any_element()
        });
        window.draw(point(px(0.), px(0.)), size(px(100.), px(100.)), |_, _| {
            img(ImageSource::Render(test_image(1)))
                .id(TEST_IMG_ID)
                .into_any_element()
        });
    }

    #[test]
    fn copies_of_an_animation_show_the_frame_its_clock_is_at() {
        let image = animation(&[40, 60, 100]);
        let start = Instant::now();
        // Its clock starts when it's first asked, and every copy asking later reads the same one.
        assert_eq!(
            image.frame_at(start),
            (0, Some(start + Duration::from_millis(40)))
        );
        let later = start + Duration::from_millis(50);
        assert_eq!(
            image.frame_at(later),
            (1, Some(start + Duration::from_millis(100)))
        );
        assert_eq!(image.frame_at(later), image.frame_at(later));
        // However late a redraw comes, it's the frame that's due, and it goes round.
        assert_eq!(image.frame_at(start + Duration::from_millis(150)).0, 2);
        assert_eq!(
            image.frame_at(start + Duration::from_millis(210)),
            (0, Some(start + Duration::from_millis(240)))
        );
    }

    #[test]
    fn frames_of_10_ms_or_less_last_100_ms_as_in_browsers() {
        let image = animation(&[0, 10, 20]);
        let start = Instant::now();
        assert_eq!(image.frame_at(start).0, 0);
        assert_eq!(image.frame_at(start + Duration::from_millis(99)).0, 0);
        assert_eq!(image.frame_at(start + Duration::from_millis(150)).0, 1);
        assert_eq!(image.frame_at(start + Duration::from_millis(205)).0, 2);
    }

    #[test]
    fn a_still_image_has_one_frame_and_nothing_due() {
        assert_eq!(animation(&[50]).frame_at(Instant::now()), (0, None));
    }
}
