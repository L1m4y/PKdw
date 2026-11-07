mod auth;
mod completions;
mod config;
mod get;
mod info;
mod ls;
mod whoami;

use crate::api::PikPak;
use crate::cli::{Cli, Command};
use crate::paths::Paths;
use anyhow::Result;

pub async fn run(cli: Cli) -> Result<i32> {
    let paths = Paths::discover()?;

    match &cli.command {
        Command::Completions(args) => completions::run(args),
        Command::Config => config::run(&paths, &cli.output),
        Command::Auth(args) => {
            let client = PikPak::new(paths).await?;
            auth::run(&client, args, &cli.output).await
        }
        Command::Whoami => {
            let client = PikPak::new(paths).await?;
            whoami::run(&client, &cli.output).await
        }
        Command::Ls(args) => {
            let client = PikPak::new(paths).await?;
            ls::run(&client, args, &cli.output).await
        }
        Command::Info(args) => {
            let client = PikPak::new(paths).await?;
            info::run(&client, args, &cli.output).await
        }
        Command::Get(args) => {
            let client = PikPak::new(paths).await?;
            get::run(&client, args, &cli.output).await
        }
    }
}
