#[cfg(windows)]
mod cli;
#[cfg(windows)]
mod profile;
#[cfg(windows)]
mod windows;

fn main() {
    #[cfg(windows)]
    if let Err(error) = windows::run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
    #[cfg(not(windows))]
    {
        eprintln!("This diagnostic entry point requires Windows.");
        std::process::exit(1);
    }
}
