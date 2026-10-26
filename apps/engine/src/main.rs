//! Formal headless entry. Diagnostic sources remain separate development tools.

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|value| value == "participant") {
        args.remove(0);
    }
    if let Err(error) = rustlanbus_engine::participant::cli::run(&args) {
        eprintln!("{error}");
        std::process::exit(error.exit_code);
    }
}
