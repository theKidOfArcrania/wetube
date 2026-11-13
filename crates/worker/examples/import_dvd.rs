use anyhow::Result;
use clap::Parser;
use wetube_worker::experimental::{Options, run};

#[tokio::main]
async fn main() -> Result<()> {
    let report = run(Options::parse()).await?;
    println!("Experimental import verified. Report: {}", report.display());
    Ok(())
}
