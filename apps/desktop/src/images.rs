// SPDX-License-Identifier: EUPL-1.2
//! GPUI keeps every `RenderImage` it has drawn in the window's texture atlas
//! until `Window::drop_image`. Images Nuschl decodes itself (message images,
//! avatars, the viewer's large version) are tracked here and released once
//! nothing but the tracker holds them.
use gpui_kit::{RenderImage, Window};
use std::{cell::RefCell, sync::Arc};

thread_local! {
    static TRACKED: RefCell<Vec<Arc<RenderImage>>> = const { RefCell::new(Vec::new()) };
}

pub fn track(image: Arc<RenderImage>) -> Arc<RenderImage> {
    TRACKED.with_borrow_mut(|tracked| tracked.push(image.clone()));
    image
}

/// Keep a replacement video frame local until its view renders it.
pub struct VideoImage {
    image: Arc<RenderImage>,
    tracked: bool,
}

impl VideoImage {
    pub fn new(image: RenderImage) -> Self {
        Self {
            image: Arc::new(image),
            tracked: false,
        }
    }

    pub fn for_render(&mut self) -> Arc<RenderImage> {
        if self.tracked {
            self.image.clone()
        } else {
            self.tracked = true;
            track(self.image.clone())
        }
    }
}

/// Release the atlas space of images no message, avatar or viewer uses any more.
pub fn collect(window: &mut Window) {
    TRACKED.with_borrow_mut(|tracked| {
        tracked.retain(|image| {
            let used = Arc::strong_count(image) > 1;
            if !used {
                let _ = window.drop_image(image.clone());
            }
            used
        })
    });
}

#[cfg(test)]
mod tests {
    use super::{RenderImage, TRACKED, VideoImage};
    use std::sync::Arc;

    #[test]
    fn video_images_enter_the_tracker_once_when_rendered() {
        let image = VideoImage::new(RenderImage::new(Vec::new()));
        let skipped = Arc::downgrade(&image.image);
        drop(image);
        assert_eq!(skipped.strong_count(), 0);

        let mut image = VideoImage::new(RenderImage::new(Vec::new()));
        let rendered = Arc::downgrade(&image.image);
        drop(image.for_render());
        drop(image.for_render());
        drop(image);
        assert_eq!(rendered.strong_count(), 1);

        TRACKED.with_borrow_mut(|tracked| tracked.clear());
        assert_eq!(rendered.strong_count(), 0);
    }
}
