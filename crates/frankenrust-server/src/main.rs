//! Binary entry point: reads a small env-var configuration (no Caddyfile
//! parsing -- out of scope, `docs/PORTING-NOTES.md`), binds the listener,
//! and runs [`frankenrust_server::run`] until `SIGINT`/`SIGTERM`-equivalent
//! (`ctrl_c`) triggers a graceful shutdown.
use std::collections::HashMap;
use std::path::PathBuf;

use frankenrust_server::ServerConfig;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let document_root = match std::env::var_os("FRANKENRUST_DOCUMENT_ROOT") {
        Some(value) => PathBuf::from(value),
        None => std::env::current_dir()?,
    };
    let listen_addr =
        std::env::var("FRANKENRUST_LISTEN").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    let num_threads: usize = std::env::var("FRANKENRUST_THREADS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| {
            std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get)
        });

    let addr = listen_addr.parse().map_err(|error| {
        std::io::Error::other(format!(
            "invalid FRANKENRUST_LISTEN {listen_addr:?}: {error}"
        ))
    })?;
    let listener = frankenrust_server::bind(addr).await?;
    eprintln!(
        "frankenrust-server listening on {listen_addr}, document root {}, {num_threads} PHP thread(s)",
        document_root.display()
    );

    let config = ServerConfig {
        document_root,
        num_threads,
        php_ini: HashMap::new(),
    };

    frankenrust_server::run(listener, config, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
}
