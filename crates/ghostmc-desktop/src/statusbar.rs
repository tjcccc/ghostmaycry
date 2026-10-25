//! macOS menu-bar item: the ghost icon that opens the panel.
//!
//! A left click reports where the icon is so the panel can drop down under
//! it; a right click (or control-click) shows a small menu with Quit. The
//! icon is a template image, so macOS tints it for light and dark menu bars,
//! and gains a dot while an agent waits for the user.

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{
    AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel,
};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSEventMask, NSEventModifierFlags, NSEventType,
    NSImage, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength, NSView,
    NSWindow,
};
use objc2_foundation::{NSData, NSNumber, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};

const GHOST_SVG: &[u8] = include_bytes!("../assets/ghost.svg");
/// Icon size in points; rendered at 2x for Retina.
const ICON_W: f32 = 18.;
const ICON_H: f32 = 16.;

/// Where the icon sits, relative to the top-left of its display.
#[derive(Debug, Clone, Copy)]
pub struct Anchor {
    /// `CGDirectDisplayID` of the icon's screen (GPUI's `DisplayId`).
    pub display: u32,
    pub left: f32,
    pub screen_width: f32,
    /// Bottom edge of the menu bar.
    pub bottom: f32,
}

struct Ivars {
    on_click: Box<dyn Fn(Anchor)>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "GhostmcStatusTarget"]
    #[ivars = Ivars]
    struct Target;

    impl Target {
        #[unsafe(method(clicked:))]
        fn clicked(&self, _sender: Option<&AnyObject>) {
            let mtm = self.mtm();
            let app = NSApplication::sharedApplication(mtm);
            let right = app.currentEvent().is_some_and(|e| {
                e.r#type() == NSEventType::RightMouseUp
                    || e.modifierFlags().contains(NSEventModifierFlags::Control)
            });
            if right {
                STATUS.with(|s| {
                    if let Some(s) = s.get() {
                        s.show_menu(mtm);
                    }
                });
            } else if let Some(a) = STATUS.with(|s| s.get().and_then(|s| s.anchor(mtm))) {
                (self.ivars().on_click)(a);
            }
        }
    }

    unsafe impl NSObjectProtocol for Target {}
);

pub struct StatusItem {
    item: Retained<NSStatusItem>,
    _target: Retained<Target>,
    normal: Retained<NSImage>,
    attention: Retained<NSImage>,
}

thread_local! {
    static STATUS: std::cell::OnceCell<StatusItem> = const { std::cell::OnceCell::new() };
}

/// Hide the Dock icon: the app lives in the menu bar.
pub fn become_accessory() {
    if let Some(mtm) = MainThreadMarker::new() {
        NSApplication::sharedApplication(mtm)
            .setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    }
}

/// Create the menu-bar item (once, on the main thread).
pub fn install(on_click: impl Fn(Anchor) + 'static) -> Result<(), String> {
    let mtm = MainThreadMarker::new().ok_or("status item must be created on the main thread")?;
    let normal = icon(false)?;
    let attention = icon(true)?;
    let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    let target: Retained<Target> = {
        let this = Target::alloc(mtm).set_ivars(Ivars {
            on_click: Box::new(on_click),
        });
        unsafe { msg_send![super(this), init] }
    };
    let button = item.button(mtm).ok_or("status item has no button")?;
    button.setImage(Some(&normal));
    unsafe {
        button.setTarget(Some(&target));
        button.setAction(Some(sel!(clicked:)));
    }
    button.sendActionOn(NSEventMask::LeftMouseUp | NSEventMask::RightMouseUp);
    STATUS.with(|s| {
        s.set(StatusItem {
            item,
            _target: target,
            normal,
            attention,
        })
        .map_err(|_| "status item already installed".to_owned())
    })
}

/// Show or clear the attention dot.
pub fn set_attention(on: bool) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    STATUS.with(|s| {
        if let Some(s) = s.get()
            && let Some(b) = s.item.button(mtm)
        {
            b.setImage(Some(if on { &s.attention } else { &s.normal }));
        }
    });
}

impl StatusItem {
    fn anchor(&self, mtm: MainThreadMarker) -> Option<Anchor> {
        let button = self.item.button(mtm)?;
        let win = button.window()?;
        let screen = win.screen()?;
        let f = win.frame();
        let s = screen.frame();
        let number = screen
            .deviceDescription()
            .objectForKey(&NSString::from_str("NSScreenNumber"))?
            .downcast::<NSNumber>()
            .ok()?;
        Some(Anchor {
            display: number.unsignedIntValue(),
            left: (f.origin.x - s.origin.x) as f32,
            screen_width: s.size.width as f32,
            bottom: (s.origin.y + s.size.height - f.origin.y) as f32,
        })
    }

    fn show_menu(&self, mtm: MainThreadMarker) {
        let menu = NSMenu::new(mtm);
        let quit = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("Quit Ghost May Cry"),
                Some(sel!(terminate:)),
                &NSString::from_str("q"),
            )
        };
        menu.addItem(&quit);
        // Attach the menu only for this click, so a left click still opens
        // the panel.
        self.item.setMenu(Some(&menu));
        if let Some(b) = self.item.button(mtm) {
            unsafe { b.performClick(None) };
        }
        self.item.setMenu(None);
    }
}

/// Render the ghost as a template image, optionally with a dot at the top
/// right cut out of the ghost's head.
fn icon(dot: bool) -> Result<Retained<NSImage>, String> {
    use resvg::tiny_skia::{BlendMode, Color, FillRule, Paint, PathBuilder, Pixmap, Transform};
    use resvg::usvg;
    let tree = usvg::Tree::from_data(GHOST_SVG, &usvg::Options::default())
        .map_err(|e| format!("icon: {e}"))?;
    let scale = 2.;
    let (w, h) = ((ICON_W * scale) as u32, (ICON_H * scale) as u32);
    let mut pixmap = Pixmap::new(w, h).ok_or("icon: empty pixmap")?;
    let size = tree.size();
    let s = h as f32 / size.height();
    let ghost_w = size.width() * s;
    let dx = (w as f32 - ghost_w) / 2.;
    resvg::render(
        &tree,
        Transform::from_row(s, 0., 0., s, dx, 0.),
        &mut pixmap.as_mut(),
    );
    if dot {
        let (cx, cy) = (w as f32 - 6., 6.);
        let clear = Paint {
            blend_mode: BlendMode::Clear,
            ..Default::default()
        };
        let mut fill = Paint::default();
        fill.set_color(Color::BLACK);
        for (r, paint) in [(8.5, &clear), (5.5, &fill)] {
            let c = PathBuilder::from_circle(cx, cy, r).ok_or("icon: circle")?;
            pixmap.fill_path(&c, paint, FillRule::Winding, Transform::identity(), None);
        }
    }
    let png = pixmap.encode_png().map_err(|e| format!("icon: {e}"))?;
    let image = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(&png))
        .ok_or("icon: NSImage rejected the PNG")?;
    image.setSize(NSSize::new(ICON_W.into(), ICON_H.into()));
    image.setTemplate(true);
    Ok(image)
}

/// Hide the (greyed-out) traffic lights GPUI leaves on a titlebar-less
/// window, and the window shadow: the panel draws its cards' own shadows on
/// a transparent window.
pub fn style_panel_windows() {
    use objc2_app_kit::NSWindowButton;
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    for w in NSApplication::sharedApplication(mtm).windows() {
        w.setHasShadow(false);
        for b in [
            NSWindowButton::CloseButton,
            NSWindowButton::MiniaturizeButton,
            NSWindowButton::ZoomButton,
        ] {
            if let Some(button) = w.standardWindowButton(b) {
                button.setHidden(true);
            }
        }
    }
}

/// The AppKit window behind a GPUI window.
pub fn ns_window(window: &gpui::Window) -> Option<Retained<NSWindow>> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let RawWindowHandle::AppKit(h) = HasWindowHandle::window_handle(window).ok()?.as_raw() else {
        return None;
    };
    // SAFETY: an AppKit handle's `ns_view` is the window's live NSView, and
    // this runs on the main thread that owns it.
    let view: &NSView = unsafe { h.ns_view.cast::<NSView>().as_ref() };
    view.window()
}

/// Move and size `w` in one step; `left` and `top` are measured from the
/// top-left of its screen, in points. GPUI's own `resize` keeps the bottom
/// edge and the left edge, but the panel must keep its top edge and stay on screen.
pub fn set_frame(w: &NSWindow, left: f32, top: f32, width: f32, height: f32) {
    let Some(screen) = w.screen() else {
        return;
    };
    let s = screen.frame();
    let rect = NSRect::new(
        NSPoint::new(
            s.origin.x + f64::from(left),
            s.origin.y + s.size.height - f64::from(top + height),
        ),
        NSSize::new(f64::from(width), f64::from(height)),
    );
    w.setFrame_display(rect, true);
}
