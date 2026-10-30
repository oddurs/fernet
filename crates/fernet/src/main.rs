//! The `fernet` command-line interface.

use clap::Parser;

/// every file has depth, every moment is kept.
#[derive(Parser)]
#[command(name = "fernet", version, about)]
struct Cli {}

fn main() {
    let Cli {} = Cli::parse();
}
