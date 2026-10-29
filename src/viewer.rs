//! The image viewer behind "View Image": the full-size original in its own
//! window (an `adw::Dialog` can't be resized by the user).
//! Fitted to the window at first; the wheel zooms about the pointer, a
//! click toggles fit ⇄ actual size, a middle click goes back to fit, and
//! dragging pans.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{gdk, glib, graphene, gsk};

const ZOOM_MIN: f64 = 0.02;
const ZOOM_MAX: f64 = 32.0;
/// Zoom factor per wheel notch (and per +/- key).
const ZOOM_STEP: f64 = 1.2;
/// The size the viewer opens at least at, and the smallest it can be made.
const OPEN_MIN: (i32, i32) = (640, 480);
const WINDOW_MIN: (i32, i32) = (360, 280);

mod imp {
    use super::*;

    /// A texture drawn at an explicit scale. `gtk::Picture` can't do this:
    /// it only fits or shows at intrinsic size, and its natural size is
    /// always the intrinsic one.
    #[derive(Default)]
    pub struct ZoomPicture {
        pub texture: RefCell<Option<gdk::Texture>>,
        /// None = fit to the viewport.
        pub zoom: Cell<Option<f64>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ZoomPicture {
        const NAME: &'static str = "BindleZoomPicture";
        type Type = super::ZoomPicture;
        type ParentType = gtk4::Widget;
    }

    impl ObjectImpl for ZoomPicture {}

    impl WidgetImpl for ZoomPicture {
        // Fitted, it asks for nothing and expands into the viewport; zoomed,
        // it asks for exactly the scaled size, so the viewport scrolls.
        fn measure(&self, orientation: gtk4::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            let size = match (self.zoom.get(), self.texture.borrow().as_ref()) {
                (Some(zoom), Some(texture)) => {
                    let dim = match orientation {
                        gtk4::Orientation::Horizontal => texture.width(),
                        _ => texture.height(),
                    };
                    (dim as f64 * zoom).ceil() as i32
                }
                _ => 0,
            };
            (size, size, -1, -1)
        }

        // Drawn as a render node, so the GPU scales it and only the visible
        // part costs anything — a cairo surface at 8× would be gigabytes.
        fn snapshot(&self, snapshot: &gtk4::Snapshot) {
            let Some(texture) = self.texture.borrow().clone() else { return };
            let widget = self.obj();
            let scale = widget.scale() as f32;
            let (w, h) = (texture.width() as f32 * scale, texture.height() as f32 * scale);
            let x = ((widget.width() as f32 - w) / 2.0).max(0.0).round();
            let y = ((widget.height() as f32 - h) / 2.0).max(0.0).round();
            // Trilinear (mipmapped) when shrinking, or fine detail aliases.
            let filter = if scale < 1.0 { gsk::ScalingFilter::Trilinear } else { gsk::ScalingFilter::Linear };
            snapshot.append_scaled_texture(&texture, filter, &graphene::Rect::new(x, y, w, h));
        }
    }
}

glib::wrapper! {
    pub struct ZoomPicture(ObjectSubclass<imp::ZoomPicture>) @extends gtk4::Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl ZoomPicture {
    fn new(texture: &gdk::Texture) -> Self {
        let picture: Self = glib::Object::new();
        picture.imp().texture.replace(Some(texture.clone()));
        picture.set_hexpand(true);
        picture.set_vexpand(true);
        picture
    }

    fn size(&self) -> (f64, f64) {
        let texture = self.imp().texture.borrow();
        let texture = texture.as_ref().expect("set in new");
        (texture.width().max(1) as f64, texture.height().max(1) as f64)
    }

    fn zoom(&self) -> Option<f64> {
        self.imp().zoom.get()
    }

    /// The scale that fits the viewport (`page` sizes), never enlarging.
    fn fit_scale(&self, page: (f64, f64)) -> f64 {
        let (w, h) = self.size();
        if page.0 <= 0.0 || page.1 <= 0.0 {
            return 1.0;
        }
        (page.0 / w).min(page.1 / h).min(1.0)
    }

    /// The scale it is drawn at now.
    fn scale(&self) -> f64 {
        self.zoom().unwrap_or_else(|| {
            let page = self.parent().map_or((0.0, 0.0), |p| (p.width() as f64, p.height() as f64));
            self.fit_scale(page)
        })
    }

    fn set_zoom(&self, zoom: Option<f64>) {
        self.imp().zoom.set(zoom.map(|z| z.clamp(ZOOM_MIN, ZOOM_MAX)));
        self.queue_resize();
    }
}

/// Zoom to `target` (None = fit) keeping the image point under (`x`, `y`)
/// — viewport coordinates — where it is.
fn zoom_at(picture: &ZoomPicture, scroller: &gtk4::ScrolledWindow, target: Option<f64>, x: f64, y: f64) {
    let (hadj, vadj) = (scroller.hadjustment(), scroller.vadjustment());
    let page = (hadj.page_size(), vadj.page_size());
    let (w, h) = picture.size();
    let old = picture.zoom().unwrap_or_else(|| picture.fit_scale(page));
    // Smaller than the viewport, the image is centred in it.
    let inset = |page: f64, content: f64| ((page - content) / 2.0).max(0.0);
    let ix = (hadj.value() + x - inset(page.0, w * old)) / old;
    let iy = (vadj.value() + y - inset(page.1, h * old)) / old;
    picture.set_zoom(target);
    let Some(new) = picture.zoom() else { return };
    // The adjustments learn the new size only at the next allocation, which
    // would clamp the value against the old one; so configure them now.
    for (adj, pos, image, size, page) in [(&hadj, x, ix, w, page.0), (&vadj, y, iy, h, page.1)] {
        let content = size * new;
        let value = (image * new + inset(page, content) - pos).clamp(0.0, (content - page).max(0.0));
        adj.configure(value, 0.0, content.max(page), adj.step_increment(), adj.page_increment(), page);
    }
}

/// The open viewer, if any: there is only ever one. Its key controller is
/// kept so it can be swapped along with the content, and `dest` is the
/// image reference it shows.
struct Viewer {
    window: adw::Window,
    keys: gtk4::EventControllerKey,
    dest: String,
}

thread_local! {
    static VIEWER: RefCell<Option<Viewer>> = const { RefCell::new(None) };
}

/// Bring the viewer forward if it already shows `dest`; false when the
/// image has to be loaded first.
pub fn raise(dest: &str) -> bool {
    VIEWER.with_borrow(|viewer| match viewer {
        Some(viewer) if viewer.dest == dest => {
            viewer.window.present();
            true
        }
        _ => false,
    })
}

/// Show an image in the viewer, opening at the image's own size up to most
/// of `parent` (and at least `OPEN_MIN`). Escape closes it.
///
/// `separate`: a free, resizable window kept above `parent` — there is only
/// one, and an open one takes the new image, keeping its size and place.
/// Otherwise an `adw::Dialog` inside `parent`, which the user can't resize
/// and which blocks the window under it, so there can't be two either.
pub fn show(texture: &gdk::Texture, title: &str, dest: &str, parent: &impl IsA<gtk4::Window>, separate: bool) {
    let (toolbar, keys, (w, h)) = content(texture);
    let parent = parent.as_ref();
    let max = (parent.width() * 9 / 10, parent.height() * 9 / 10);
    let scale = (max.0 as f64 / w).min((max.1 - 48) as f64 / h).min(1.0);
    // A small image still opens at a comfortable size (`OPEN_MIN`, within
    // the parent); a window can't be dragged below `WINDOW_MIN`.
    let open = |image: f64, min: i32, max: i32| (image as i32).max(min.min(max));
    let size = (open(w * scale, OPEN_MIN.0, max.0), open(h * scale + 48.0, OPEN_MIN.1, max.1));
    if !separate {
        let dialog = adw::Dialog::builder()
            .title(title)
            .content_width(size.0)
            .content_height(size.1)
            .child(&toolbar)
            .build();
        dialog.add_controller(keys);
        dialog.present(Some(parent));
        return;
    }
    let reused = VIEWER.with_borrow_mut(|viewer| {
        let viewer = viewer.as_mut()?;
        viewer.window.remove_controller(&viewer.keys);
        viewer.window.add_controller(keys.clone());
        viewer.window.set_content(Some(&toolbar));
        viewer.window.set_title(Some(title));
        viewer.window.present();
        viewer.keys = keys.clone();
        viewer.dest = dest.to_string();
        Some(())
    });
    if reused.is_some() {
        return;
    }
    let window = adw::Window::builder()
        .title(title)
        .transient_for(parent)
        .destroy_with_parent(true)
        .default_width(size.0)
        .default_height(size.1)
        .width_request(WINDOW_MIN.0)
        .height_request(WINDOW_MIN.1)
        .content(&toolbar)
        .build();
    window.add_controller(keys.clone());
    // On destroy rather than close-request: closing the parent destroys it
    // too, without asking.
    window.connect_destroy(|_| VIEWER.set(None));
    window.present();
    VIEWER.set(Some(Viewer { window, keys, dest: dest.to_string() }));
}

/// The viewer's content for one image: toolbar view, the key controller to
/// attach to the window, and the image size.
fn content(texture: &gdk::Texture) -> (adw::ToolbarView, gtk4::EventControllerKey, (f64, f64)) {
    let picture = ZoomPicture::new(texture);
    let scroller = gtk4::ScrolledWindow::new();
    scroller.set_child(Some(&picture));

    let zoom_button = gtk4::Button::new();
    let percent = gtk4::Label::new(None);
    percent.add_css_class("dim-label");
    percent.add_css_class("numeric");
    let header = adw::HeaderBar::new();
    header.pack_start(&zoom_button);
    header.pack_start(&percent);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&scroller));

    // Header button, label and cursor follow the zoom.
    let refresh = {
        let (picture, zoom_button, percent) = (picture.clone(), zoom_button.clone(), percent.clone());
        Rc::new(move || {
            let fit = picture.zoom().is_none();
            picture.set_cursor_from_name(Some(if fit { "zoom-in" } else { "grab" }));
            zoom_button.set_icon_name(if fit { "zoom-original-symbolic" } else { "zoom-fit-best-symbolic" });
            zoom_button.set_tooltip_text(Some(if fit { "Actual Size" } else { "Fit to Window" }));
            percent.set_label(&match picture.zoom() {
                Some(zoom) => format!("{:.0} %", zoom * 100.0),
                None => "Fit".to_string(),
            });
        })
    };
    refresh();
    let zoom_to = {
        let (picture, scroller, refresh) = (picture.clone(), scroller.clone(), refresh.clone());
        Rc::new(move |target: Option<f64>, x: f64, y: f64| {
            zoom_at(&picture, &scroller, target, x, y);
            refresh();
        })
    };
    let centre = {
        let scroller = scroller.clone();
        move || (scroller.width() as f64 / 2.0, scroller.height() as f64 / 2.0)
    };
    // Click and the header button: fit ⇄ actual size.
    let toggle = {
        let (picture, zoom_to) = (picture.clone(), zoom_to.clone());
        Rc::new(move |x: f64, y: f64| zoom_to(if picture.zoom().is_none() { Some(1.0) } else { None }, x, y))
    };
    zoom_button.connect_clicked({
        let (toggle, centre) = (toggle.clone(), centre.clone());
        move |_| {
            let (x, y) = centre();
            toggle(x, y)
        }
    });

    // The wheel zooms about the pointer instead of scrolling (captured
    // before the scroller's own handler). A touchpad reports pixels, a
    // wheel notches.
    let pointer = Rc::new(Cell::new((0.0, 0.0)));
    let motion = gtk4::EventControllerMotion::new();
    motion.connect_motion({
        let pointer = pointer.clone();
        move |_, x, y| pointer.set((x, y))
    });
    scroller.add_controller(motion);
    let wheel = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::VERTICAL);
    wheel.set_propagation_phase(gtk4::PropagationPhase::Capture);
    wheel.connect_scroll({
        let (picture, zoom_to, pointer) = (picture.clone(), zoom_to.clone(), pointer.clone());
        move |wheel, _, dy| {
            let notches = match wheel.unit() {
                gdk::ScrollUnit::Surface => dy / 40.0,
                _ => dy,
            };
            let (x, y) = pointer.get();
            zoom_to(Some(picture.scale() * ZOOM_STEP.powf(-notches)), x, y);
            glib::Propagation::Stop
        }
    });
    scroller.add_controller(wheel);

    // Drag pans; a press that barely moves is a click. On the scroller, not
    // the picture: the picture moves while panning, so offsets measured in
    // its coordinates would shift under the pointer and judder.
    let drag = gtk4::GestureDrag::new();
    let origin = Rc::new(Cell::new((0.0, 0.0)));
    drag.connect_drag_begin({
        let (scroller, origin, picture) = (scroller.clone(), origin.clone(), picture.clone());
        move |_, _, _| {
            origin.set((scroller.hadjustment().value(), scroller.vadjustment().value()));
            if picture.zoom().is_some() {
                picture.set_cursor_from_name(Some("grabbing"));
            }
        }
    });
    drag.connect_drag_update({
        let (scroller, origin) = (scroller.clone(), origin.clone());
        move |_, dx, dy| {
            let (x, y) = origin.get();
            scroller.hadjustment().set_value(x - dx);
            scroller.vadjustment().set_value(y - dy);
        }
    });
    drag.connect_drag_end({
        let (toggle, refresh) = (toggle.clone(), refresh.clone());
        move |drag, dx, dy| {
            if dx.hypot(dy) < 4.0 {
                if let Some((x, y)) = drag.start_point() {
                    return toggle(x + dx, y + dy);
                }
            }
            refresh();
        }
    });
    scroller.add_controller(drag);

    // Middle click: back to fit.
    let middle = gtk4::GestureClick::new();
    middle.set_button(gdk::BUTTON_MIDDLE);
    middle.connect_pressed({
        let zoom_to = zoom_to.clone();
        move |_, _, x, y| zoom_to(None, x, y)
    });
    scroller.add_controller(middle);

    // Keys, as in GNOME's image viewer: +/- zoom, 0 fits, 1 actual size.
    let keys = gtk4::EventControllerKey::new();
    keys.connect_key_pressed({
        let (picture, zoom_to) = (picture.clone(), zoom_to.clone());
        move |keys, key, _, _| {
            let (x, y) = centre();
            let target = match key {
                gdk::Key::plus | gdk::Key::equal | gdk::Key::KP_Add => Some(picture.scale() * ZOOM_STEP),
                gdk::Key::minus | gdk::Key::KP_Subtract => Some(picture.scale() / ZOOM_STEP),
                gdk::Key::_1 | gdk::Key::KP_1 => Some(1.0),
                gdk::Key::_0 | gdk::Key::KP_0 => None,
                // A dialog closes on Escape by itself; the window doesn't.
                gdk::Key::Escape => match keys.widget().and_downcast::<gtk4::Window>() {
                    Some(window) => {
                        window.close();
                        return glib::Propagation::Stop;
                    }
                    None => return glib::Propagation::Proceed,
                },
                _ => return glib::Propagation::Proceed,
            };
            zoom_to(target, x, y);
            glib::Propagation::Stop
        }
    });

    let size = picture.size();
    (toolbar, keys, size)
}
