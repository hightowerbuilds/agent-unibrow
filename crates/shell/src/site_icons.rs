//! Site icons for the History and Bookmarks lists.
//!
//! The browser pane saves each site's favicon in the browsing store
//! (`browsing::favicons`); this keeps them decoded for drawing, one per site,
//! so a long history list costs a hash lookup per row. Sites without an icon
//! get their first letter.

use crate::theme;
use browsing::{Browsing, Favicon};
use gpui::{AnyElement, RenderImage, Window, div, img, prelude::*, px, rgb};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::Arc;

const SIZE: f32 = 16.;

#[derive(Default)]
pub struct SiteIcons {
    /// By site: the icon's pixels (to notice changes) and its image.
    cache: RefCell<HashMap<String, Option<(Favicon, Arc<RenderImage>)>>>,
    /// The store's `favicon_version` the cache matches.
    version: Cell<u64>,
    /// Replaced images, to release from the GPU atlas.
    retired: RefCell<Vec<Arc<RenderImage>>>,
}

fn to_image(icon: &Favicon) -> Option<Arc<RenderImage>> {
    let buf = image::RgbaImage::from_raw(icon.width, icon.height, icon.bgra.clone())?;
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(buf)])))
}

impl SiteIcons {
    /// The image for `site`, if the store has one.
    fn image(&self, store: &Browsing, site: &str) -> Option<Arc<RenderImage>> {
        let version = store.favicon_version();
        if version != self.version.get() {
            // Something was saved: look again, keeping images that didn't change.
            self.version.set(version);
            let mut cache = self.cache.borrow_mut();
            let mut retired = self.retired.borrow_mut();
            for (site, entry) in cache.iter_mut() {
                let now = store.favicon(site);
                if now.as_ref() != entry.as_ref().map(|(f, _)| f) {
                    if let Some((_, old)) = entry.take() {
                        retired.push(old);
                    }
                    *entry = now.and_then(|f| to_image(&f).map(|i| (f, i)));
                }
            }
        }
        let mut cache = self.cache.borrow_mut();
        cache
            .entry(site.to_string())
            .or_insert_with(|| store.favicon(site).and_then(|f| to_image(&f).map(|i| (f, i))))
            .as_ref()
            .map(|(_, i)| i.clone())
    }

    /// The icon to draw next to a `site` entry.
    pub fn view(&self, store: &Browsing, site: &str) -> AnyElement {
        match self.image(store, site) {
            Some(image) => img(image).size(px(SIZE)).flex_none().into_any_element(),
            None => {
                let letter = site.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_ascii_uppercase()).unwrap_or('•');
                div()
                    .size(px(SIZE))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_sm()
                    .bg(rgb(theme::RULE))
                    .text_color(rgb(theme::MUTED))
                    .text_size(px(10.))
                    .child(letter.to_string())
                    .into_any_element()
            }
        }
    }

    /// Whether `site` has an icon to draw (test logs).
    pub fn has_icon(&self, store: &Browsing, site: &str) -> bool {
        self.image(store, site).is_some()
    }

    /// Frees replaced images' GPU memory.
    pub fn release(&self, window: &mut Window) {
        for old in self.retired.borrow_mut().drain(..) {
            let _ = window.drop_image(old);
        }
    }
}
