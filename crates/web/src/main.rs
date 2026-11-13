#[cfg(target_arch = "wasm32")]
mod app;

fn main() {
    #[cfg(target_arch = "wasm32")]
    {
        console_error_panic_hook::set_once();
        leptos::mount::mount_to_body(app::App);
    }
    #[cfg(not(target_arch = "wasm32"))]
    eprintln!("Build wetube-web for wasm32-unknown-unknown and serve it through wetube-server.");
}
