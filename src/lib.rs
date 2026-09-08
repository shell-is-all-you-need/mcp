//! A dependency-free MCP 2026-07-28 server for fixed process-invocation tools.

mod cli;
mod json;
mod path_policy;
mod process;
mod protocol;
mod stdio;
mod task;
mod tool;

/// Parses command-line arguments and runs the MCP server.
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    match cli::parse()? {
        cli::Action::Help => {
            println!("{}", cli::USAGE);
            Ok(())
        }
        cli::Action::Version => {
            println!("shell-is-all-you-need {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        cli::Action::Run(config) => stdio::serve(config),
    }
}

#[cfg(test)]
#[path = "../tests/unit/mod.rs"]
mod tests;
