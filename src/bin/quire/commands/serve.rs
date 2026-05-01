use miette::Result;

use quire::Quire;

pub async fn run(quire: &Quire) -> Result<()> {
    crate::server::run(quire).await
}
