pub async fn run(command: Vec<String>) -> color_eyre::Result<()> {
    tracing::info!(?command, "quire exec dispatching");
    // TODO: parse command, validate against allowlist, exec
    Ok(())
}
