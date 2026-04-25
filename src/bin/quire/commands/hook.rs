use miette::Result;

pub async fn run(hook_name: &str) -> Result<()> {
    tracing::info!(hook = %hook_name, "hook invoked");
    Ok(())
}
