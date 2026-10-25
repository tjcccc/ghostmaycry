//! macOS Liquid Glass behind the panel's cards.
//!
//! GPUI draws into one view inside the window's content view. Each card gets
//! an `NSGlassEffectView` placed below that view and moved to the card's
//! bounds after every layout; the card itself paints no fill, so the glass
//! shows through. Glass needs macOS 26: elsewhere [`Glass::new`] returns
//! `None` and the cards keep their opaque fill.

use gpui::{Bounds, Pixels};
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2::runtime::AnyClass;
use objc2_app_kit::{NSGlassEffectView, NSWindow, NSWindowOrderingMode};
use objc2_foundation::{NSPoint, NSRect, NSSize};

/// Glass views for one window, one per card slot.
pub struct Glass {
    window: Retained<NSWindow>,
    views: Vec<Retained<NSGlassEffectView>>,
    radius: f64,
}

impl Glass {
    pub fn new(window: Retained<NSWindow>, radius: f32) -> Option<Self> {
        AnyClass::get(c"NSGlassEffectView")?;
        Some(Self {
            window,
            views: Vec::new(),
            radius: f64::from(radius),
        })
    }

    /// Show glass at `cards` (window coordinates, top-left origin, in
    /// order) and hide the rest.
    pub fn place(&mut self, cards: &[Bounds<Pixels>]) {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let Some(content) = self.window.contentView() else {
            return;
        };
        while self.views.len() < cards.len() {
            let v = NSGlassEffectView::new(mtm);
            v.setCornerRadius(self.radius);
            // Below GPUI's view, as GPUI places its own blur view.
            content.addSubview_positioned_relativeTo(&v, NSWindowOrderingMode::Below, None);
            self.views.push(v);
        }
        let height = content.bounds().size.height;
        for (i, v) in self.views.iter().enumerate() {
            match cards.get(i) {
                Some(b) => {
                    let (x, y) = (f64::from(b.origin.x), f64::from(b.origin.y));
                    let (w, h) = (f64::from(b.size.width), f64::from(b.size.height));
                    // The content view's origin is its bottom-left corner.
                    let frame = NSRect::new(NSPoint::new(x, height - y - h), NSSize::new(w, h));
                    if v.frame() != frame {
                        v.setFrame(frame);
                    }
                    v.setHidden(false);
                }
                None => v.setHidden(true),
            }
        }
    }
}

impl Drop for Glass {
    fn drop(&mut self) {
        for v in &self.views {
            v.removeFromSuperview();
        }
    }
}
