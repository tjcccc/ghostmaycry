//! Native right-click menus (`NSMenu`). macOS draws them as their own
//! windows, so they get the system look, keyboard handling and, on macOS
//! 26+, Liquid Glass, which a menu drawn inside the panel can't have.

use gpui::{Pixels, Point};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{NSMenu, NSMenuItem, NSWindow};
use objc2_foundation::{NSObjectProtocol, NSPoint, NSString};
use std::cell::Cell;

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "GhostmcContextMenuTarget"]
    #[ivars = Cell<Option<isize>>]
    struct Target;

    impl Target {
        #[unsafe(method(picked:))]
        fn picked(&self, sender: Option<&NSMenuItem>) {
            self.ivars().set(sender.map(|i| i.tag()));
        }
    }

    unsafe impl NSObjectProtocol for Target {}
);

/// Show `items` at `at` (window coordinates, top-left origin) and wait for
/// the user; returns the chosen index, or `None` if the menu was dismissed.
///
/// Runs the menu's own event loop, so call it from a spawned task, never
/// while GPUI is handling an event.
pub fn pop_up(window: &NSWindow, items: &[&str], at: Point<Pixels>) -> Option<usize> {
    let mtm = MainThreadMarker::new()?;
    let view = window.contentView()?;
    let target: Retained<Target> = {
        let this = Target::alloc(mtm).set_ivars(Cell::new(None));
        unsafe { msg_send![super(this), init] }
    };
    let menu = NSMenu::new(mtm);
    for (i, label) in items.iter().enumerate() {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(label),
                Some(sel!(picked:)),
                &NSString::from_str(""),
            )
        };
        item.setTag(i as isize);
        // SAFETY: `target` outlives the menu's tracking below; items hold
        // their target weakly.
        unsafe { item.setTarget(Some(&*target as &AnyObject)) };
        menu.addItem(&item);
    }
    // The content view's origin is its bottom-left corner.
    let height = view.bounds().size.height;
    let location = NSPoint::new(f64::from(at.x), height - f64::from(at.y));
    menu.popUpMenuPositioningItem_atLocation_inView(None, location, Some(&view));
    target.ivars().get().and_then(|i| usize::try_from(i).ok())
}
