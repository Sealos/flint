use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, Context, DismissEvent, Entity, IntoElement,
    MouseMoveEvent, ParentElement as _, PlatformInput, Point, Render, ScrollDelta, ScrollEvent,
    Styled as _, TestAppContext, Window, WindowBounds, WindowOptions, div, point, px, size,
};

use super::{
    MENU_NOTE, MENU_WIDTH, dismiss_when_shrunk, fit_menu, fit_picker, note_width, picker_item,
};

/// A window of `width` by `height` showing `menu`.
struct Host(Entity<PopupMenu>);

impl Render for Host {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.0.clone())
    }
}

fn open(
    cx: &mut TestAppContext,
    width: f32,
    height: f32,
    build: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
) -> AnyWindowHandle {
    cx.update(gpui_kit::init);
    let window = cx
        .update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Point::default(),
                        size: size(px(width), px(height)),
                    })),
                    ..Default::default()
                },
                cx,
                |window, cx| {
                    let menu =
                        PopupMenu::build(window, cx, |menu, window, cx| build(menu, window, cx));
                    cx.new(|_| Host(menu))
                },
            )
        })
        .unwrap()
        .0;
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    window
}

const LONG_NAME: &str = "mesh-production-app-http-instrumentation-proxy-with-a-long-name-x";

// covers UP-47: in a narrow window a picker's long name wraps, inside the window.
#[gpui_kit::test]
async fn a_picker_stays_inside_a_narrow_window(cx: &mut TestAppContext) {
    let window = open(cx, 240., 500., |menu, window, cx| {
        fit_picker(menu, window, cx).item(picker_item(LONG_NAME.to_owned(), None))
    });
    cx.update_window(window, |_, window, _| {
        let menu = window.find("popup-menu").bounds();
        let text = window.find(MENU_NOTE).bounds();
        let viewport = window.viewport_size();
        assert!(menu.right() <= viewport.width, "{menu:?}");
        assert!(text.right() <= menu.right(), "{text:?} in {menu:?}");
        assert!(text.size.height > px(30.), "it wraps: {text:?}");
    })
    .unwrap();
}

// covers UP-47: a menu taller than a short window is capped to it, and scrolls.
#[gpui_kit::test]
async fn a_tall_menu_stays_inside_a_short_window(cx: &mut TestAppContext) {
    let window = open(cx, 800., 200., |menu, window, _| {
        (0..40).fold(fit_menu(menu, window), |menu, n| {
            menu.item(PopupMenuItem::new(format!("entry {n}")))
        })
    });
    let (center, top) = cx
        .update_window(window, |_, window, _| {
            let menu = window.find("popup-menu").bounds();
            assert!(menu.bottom() <= window.viewport_size().height, "{menu:?}");
            let last = window.within("popup-menu").find(39usize).bounds();
            assert!(last.top() > menu.bottom(), "starts out of view: {last:?}");
            (
                menu.center(),
                window.within("popup-menu").find(0usize).bounds().top(),
            )
        })
        .unwrap();
    // The wheel reaches the last entry.
    cx.update_window(window, |_, window, cx| {
        window.dispatch_event(
            PlatformInput::MouseMove(MouseMoveEvent {
                position: center,
                ..Default::default()
            }),
            cx,
        );
        window.dispatch_event(
            PlatformInput::Scroll(ScrollEvent {
                position: center,
                delta: ScrollDelta::Pixels(point(px(0.), px(-5000.))),
                ..Default::default()
            }),
            cx,
        );
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let menu = window.find("popup-menu").bounds();
        let first = window.within("popup-menu").find(0usize).bounds();
        let last = window.within("popup-menu").find(39usize).bounds();
        assert!(first.top() < top, "it scrolled: {first:?}");
        assert!(last.bottom() <= menu.bottom(), "{last:?} in {menu:?}");
    })
    .unwrap();
}

// covers UP-47: a note's width follows the window and the rem, not a fixed value.
#[gpui_kit::test]
async fn a_notes_width_follows_the_window(cx: &mut TestAppContext) {
    let width_in = |cx: &mut TestAppContext, width: f32| {
        let window = open(cx, width, 500., |menu, window, _| fit_menu(menu, window));
        cx.update_window(window, |_, window, _| {
            (note_width(window), MENU_WIDTH.to_pixels(window.rem_size()))
        })
        .unwrap()
    };
    let (narrow, _) = width_in(cx, 240.);
    assert!(narrow < px(240.), "{narrow:?}");
    let (wide, default) = width_in(cx, 1200.);
    assert!(wide > narrow, "{wide:?}");
    assert!(wide < default, "{wide:?} inside {default:?}");
}

// covers UP-47: even narrower than a menu's minimum width, it stays inside the window.
#[gpui_kit::test]
async fn a_menu_is_never_wider_than_a_very_narrow_window(cx: &mut TestAppContext) {
    let window = open(cx, 100., 300., |menu, window, _| {
        fit_menu(menu, window).item(PopupMenuItem::new("x"))
    });
    cx.update_window(window, |_, window, _| {
        let menu = window.find("popup-menu").bounds();
        assert!(menu.size.width <= window.viewport_size().width, "{menu:?}");
    })
    .unwrap();
}

// covers UP-47: an open menu goes when the window shrinks under the limits it was built for, and
// stays when the window grows.
#[gpui_kit::test]
async fn a_menu_is_dismissed_when_the_window_shrinks(cx: &mut TestAppContext) {
    let dismissed = Rc::new(Cell::new(0));
    let seen = dismissed.clone();
    let window = open(cx, 800., 600., move |menu, window, cx| {
        let seen = seen.clone();
        cx.subscribe_self(move |_, _: &DismissEvent, _| seen.set(seen.get() + 1))
            .detach();
        dismiss_when_shrunk(window, cx);
        fit_menu(menu, window)
    });
    cx.simulate_window_resize(window, size(px(1000.), px(700.)));
    assert_eq!(dismissed.get(), 0, "it grew");
    cx.simulate_window_resize(window, size(px(400.), px(700.)));
    assert_eq!(dismissed.get(), 1, "it shrank");
}
