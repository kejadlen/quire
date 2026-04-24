use miette::Result;

use quire::Config;

pub async fn run(config: &Config) -> Result<()> {
    tracing::info!("quire serve starting");
    let _ = config; // used once HTTP routing lands
    // TODO: bind HTTP server
    Ok(())
}
