mod auth;
mod error;
mod network;
mod persistence;
mod preview;
mod state;

use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::info;
use walloftext_shared::FontAtlasFile;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let atlas_bytes = std::fs::read("static/unifont.wtfont")
        .map_err(|e| anyhow::anyhow!("failed to read unifont.wtfont: {e}"))?;
    let atlas_bytes = zstd::decode_all(atlas_bytes.as_slice())
        .map_err(|e| anyhow::anyhow!("failed to decompress font atlas: {e}"))?;
    let font_atlas: FontAtlasFile = bitcode::decode(&atlas_bytes)
        .map_err(|e| anyhow::anyhow!("failed to decode font atlas: {e}"))?;
    let font_atlas = Arc::new(font_atlas);

    let index_html = Arc::new(
        std::fs::read_to_string("static/index.html")
            .map_err(|e| anyhow::anyhow!("failed to read index.html: {e}"))?,
    );

    let (segment_tx, segment_rx) = mpsc::unbounded_channel();

    let state = state::AppState::new(segment_tx, font_atlas, index_html);
    let last_segment_id = state.hydrate().await?;
    persistence::start_segment_worker(
        segment_rx,
        last_segment_id,
        state.inner.last_flushed_segment_id.clone(),
    );
    state.start_snapshot_worker();
    state.start_cell_update_worker();

    let app = network::build_router(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], 3000));
    info!("Listening on {}", addr);
    axum::serve(tokio::net::TcpListener::bind(addr).await?, app).await?;
    Ok(())
}
