use crate::cli::{Cli, CompletionsArgs};
use anyhow::Result;
use clap::CommandFactory;
use std::io;

pub fn run(args: &CompletionsArgs) -> Result<i32> {
    let mut command = Cli::command();
    clap_complete::generate(args.shell, &mut command, "pkdw", &mut io::stdout());
    Ok(0)
}
