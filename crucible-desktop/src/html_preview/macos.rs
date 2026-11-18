use std::cell::RefCell;
use std::rc::Rc;

use anyhow::Context;
use objc2::rc::Retained;
use objc2::runtime::AnyClass;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::NSView;
use objc2_foundation::{NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::CALayer;
use objc2_web_kit::{WKWebView, WKWebViewConfiguration, WKWebsiteDataStore};
use raw_window_handle::RawWindowHandle;
use thiserror::Error;

use crate::captured::Body;
use crate::html_preview::{ELEMENT, OBSCURED, document};

#[derive(Debug, Error)]
enum PreviewError {
    #[error("HTML preview requires an AppKit window")]
    NotAppKit,
    #[error("HTML preview window is unavailable: {0}")]
    Window(String),
}

#[derive(Default)]
struct Host {
    window: Option<rune::WindowHandle>,
    surface: Option<Surface>,
    source: Option<String>,
    error: Option<String>,
}

struct Surface {
    parent: Retained<NSView>,
    view: Retained<WKWebView>,
    metal: Retained<CALayer>,
}

pub fn install(app: rune::App<crate::App>) -> rune::App<crate::App> {
    let host = Rc::new(RefCell::new(Host::default()));
    let window_host = Rc::clone(&host);
    let handle = app.handle();
    app.on_window(move |window| {
        let mut host = window_host.borrow_mut();
        host.clear();
        host.window = Some(window.clone());
    })
    .on_frame_drawn(move |runtime| {
        let mut host = host.borrow_mut();
        let error = host.sync(runtime).err().map(|error| {
            host.hide();
            format!("{error:#}")
        });
        if host.error != error {
            host.error.clone_from(&error);
            handle.update(move |app, _| app.html_preview_error = error);
        }
    })
}

impl Host {
    fn clear(&mut self) {
        if let Some(surface) = self.surface.take() {
            surface.view.removeFromSuperview();
            surface.restore_metal();
        }
        self.source = None;
    }

    fn hide(&self) {
        if let Some(surface) = &self.surface {
            if surface.attached() {
                surface.view.removeFromSuperview();
            }
            surface.restore_metal();
        }
    }

    fn sync(&mut self, runtime: &rune::Runtime<crate::App>) -> anyhow::Result<()> {
        let app = &runtime.state;
        if app.files_html_source
            || runtime.presented(ELEMENT).is_none()
            || runtime.presented(OBSCURED).is_some()
        {
            self.hide();
            return Ok(());
        }
        let Some(opened) = &app.remote.opened_file else {
            self.hide();
            return Ok(());
        };
        if app.wanted_file().as_ref().map(|(run, key, _)| (run, key))
            != Some((&opened.run, &opened.key))
        {
            self.hide();
            return Ok(());
        }
        let Ok(Body::Text(source)) = &opened.body else {
            self.hide();
            return Ok(());
        };
        let slot = runtime.elements().into_iter().find(|element| {
            element.key.as_deref() == Some(ELEMENT) && !element.exiting && element.opacity > 0.99
        });
        let Some(slot) = slot
            .filter(|slot| slot.visible == slot.rect && slot.rect[2] > 0.0 && slot.rect[3] > 0.0)
        else {
            self.hide();
            return Ok(());
        };
        if self.surface.is_none() {
            self.create().context("create captured HTML preview")?;
        }
        let surface = self
            .surface
            .as_ref()
            .context("locate HTML preview surface")?;
        let (parent, view) = (&surface.parent, &surface.view);
        if !surface.attached() {
            parent.addSubview(view);
        }
        let [x, y, width, height] = slot.rect.map(f64::from);
        let y = if parent.isFlipped() {
            y
        } else {
            parent.bounds().size.height - y - height
        };
        view.setFrame(NSRect::new(NSPoint::new(x, y), NSSize::new(width, height)));
        if self.source.as_deref() != Some(source.as_str()) {
            let page = NSString::from_str(&document(source));
            // The report has no base URL, persistent store, bridge or navigation authority.
            unsafe { view.loadHTMLString_baseURL(&page, None) }
                .context("load captured HTML document")?;
            self.source = Some(source.clone());
        }
        surface.restore_metal();
        Ok(())
    }

    fn create(&mut self) -> anyhow::Result<()> {
        let mtm = MainThreadMarker::new().context("locate UI thread for HTML preview")?;
        let window = self.window.as_ref().context("locate HTML preview window")?;
        let parent = window
            .with_raw_handle(|handle| {
                let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
                    return Err(PreviewError::NotAppKit.into());
                };
                // The window owns this NSView and this hook runs on its UI thread.
                unsafe { Retained::retain(handle.ns_view.as_ptr().cast::<NSView>()) }
                    .context("retain HTML preview parent")
            })
            .map_err(PreviewError::Window)
            .context("read HTML preview window handle")??;
        // WebKit configuration and view creation are confined to the UI thread.
        let layer = parent.layer().context("locate Rune window layer")?;
        let class = AnyClass::get(c"CAMetalLayer").context("locate Metal layer class")?;
        let metal = unsafe { layer.sublayers() }
            .and_then(|layers| layers.iter().find(|layer| layer.isKindOfClass(class)))
            .context("locate Rune Metal layer before attaching HTML preview")?;
        let view = unsafe {
            let configuration = WKWebViewConfiguration::new(mtm);
            configuration.setWebsiteDataStore(&WKWebsiteDataStore::nonPersistentDataStore(mtm));
            WKWebView::initWithFrame_configuration(
                WKWebView::alloc(mtm),
                NSRect::ZERO,
                &configuration,
            )
        };
        unsafe {
            view.setAllowsLinkPreview(false);
        }
        parent.addSubview(&view);
        self.surface = Some(Surface {
            parent,
            view,
            metal,
        });
        Ok(())
    }
}

impl Surface {
    fn attached(&self) -> bool {
        // The retained view is queried only on the UI thread.
        unsafe { self.view.superview().is_some() }
    }

    fn restore_metal(&self) {
        if let Some(root) = self.parent.layer() {
            // AppKit reorders unmanaged sublayers when a native subview changes.
            if self.metal.superlayer().as_ref() != Some(&root) {
                root.addSublayer(&self.metal);
            }
            self.metal.setZPosition(1.0);
            if let Some(layer) = self.view.layer() {
                layer.setZPosition(2.0);
            }
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.clear();
    }
}
