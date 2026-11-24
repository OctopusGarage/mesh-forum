#[cfg(target_os = "macos")]
mod localization;
#[cfg(target_os = "macos")]
mod window;

#[cfg(target_os = "macos")]
fn main() -> std::io::Result<()> {
    window::run()
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("Mesh Forum Desktop targets macOS.");
    std::process::exit(1);
}
