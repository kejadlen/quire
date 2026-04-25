use miette::Result;

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum HookName {
    PostReceive,
}

impl std::fmt::Display for HookName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            HookName::PostReceive => "post-receive",
        };
        f.write_str(name)
    }
}

pub async fn run(hook_name: HookName) -> Result<()> {
    tracing::info!(hook = %hook_name, "hook invoked");
    Ok(())
}
