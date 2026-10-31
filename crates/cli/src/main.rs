//! `cera-cli`: a command-line testing aid for `.cera` files. The commands arrive in M2.

use std::process::ExitCode;

const USAGE: &str = "\
usage: cera-cli <command> [args]

commands (from M2):
  import <pdf> <out.cera>   convert a PDF into a note
  info <file.cera>          print pages, assets and chunk stats
  compact <file.cera>       rewrite the file without dead chunks";

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some("-h" | "--help") => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some("-V" | "--version") => {
            println!("cera-cli {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}
