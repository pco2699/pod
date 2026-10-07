#[cfg(target_arch = "wasm32")]
mod app;
#[cfg(target_arch = "wasm32")]
mod audio;
#[cfg(target_arch = "wasm32")]
mod net;
#[cfg(target_arch = "wasm32")]
mod store;
#[cfg(target_arch = "wasm32")]
mod transfer;

#[cfg(target_arch = "wasm32")]
fn main() {
    use wasm_bindgen::{JsCast, closure::Closure};

    let window = web_sys::window().unwrap();
    // Warn before closing the tab while edits are still being saved.
    let on_unload = Closure::<dyn FnMut(web_sys::BeforeUnloadEvent)>::new(|e: web_sys::BeforeUnloadEvent| {
        if app::UNSAVED.with(|u| u.get()) {
            e.prevent_default();
            e.set_return_value("Edits are still being saved.");
        }
    });
    window.set_onbeforeunload(Some(on_unload.as_ref().unchecked_ref()));
    on_unload.forget();
    audio::install_unlock();

    wasm_bindgen_futures::spawn_local(async {
        let canvas = web_sys::window()
            .unwrap()
            .document()
            .unwrap()
            .get_element_by_id("pod")
            .unwrap()
            .dyn_into::<web_sys::HtmlCanvasElement>()
            .unwrap();
        let r = eframe::WebRunner::new()
            .start(canvas, eframe::WebOptions::default(), Box::new(|cc| Ok(Box::new(app::PodApp::new(cc)))))
            .await;
        if let Err(e) = r {
            web_sys::console::error_1(&e);
        }
    });
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!("The pod client only runs in the browser: build it with `trunk build`.");
}
