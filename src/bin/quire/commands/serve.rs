use miette::Result;

use quire::Quire;

pub async fn run(quire: &Quire) -> Result<()> {
    quire::server::run(quire).await
}
