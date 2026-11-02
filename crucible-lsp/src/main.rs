use tower_lsp_server::{LspService, Server};

#[tokio::main]
async fn main() {
    if std::env::args().any(|arg| arg == "--version") {
        println!("crucible-lsp {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let (service, socket) = LspService::new(crucible_lsp::server::Backend::new);
    Server::new(tokio::io::stdin(), tokio::io::stdout(), socket)
        .serve(service)
        .await;
}
