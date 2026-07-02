//! rdbg — IDE-grade Rust debugging for coding agents.
//!
//! One binary, three roles: the `rdbg` CLI (default), the per-project daemon
//! (`__daemon`, internal), and the MCP server (`mcp`).

mod client;
mod daemon;
mod dap;
mod lsp;
mod mcp;
mod render;
mod session;
mod util;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("__daemon") => {
            let ws = args.get(1).cloned().unwrap_or_else(|| ".".to_string());
            daemon::serve(&ws);
        }
        Some("mcp") => std::process::exit(mcp::main()),
        _ => std::process::exit(client::main(&args)),
    }
}
