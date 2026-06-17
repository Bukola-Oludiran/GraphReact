//! `graphreact-lsp` — stdio language-server entry point.
//!
//! Reads JSON-RPC frames off `stdin`, writes responses to `stdout`,
//! pumps log output through `tracing` (defaulting to `stderr` so it
//! never collides with the LSP wire). Editors point their `command`
//! setting at this binary.

use graphreact_lsp::GrxLanguageServer;
use tower_lsp_server::{LspService, Server};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // `tracing` goes to stderr (LSP clients want stdout clean).
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_ansi(false)
        .try_init();

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(GrxLanguageServer::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}
