//! `stego-flac` — encapsulate files in acoustic waveforms stored as FLAC.

mod cli;
mod commands;
mod cover;
mod flac_io;
mod flac_tags;
mod passphrase;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Command};

fn main() -> Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Command::Encode(args) => commands::encode::run(args),
        Command::Decode(args) => commands::decode::run(args),
        Command::Info(args) => commands::info::run(args),
        Command::Plan(args) => commands::plan::run(args),
        Command::Completions(args) => {
            let mut command = <Cli as clap::CommandFactory>::command();
            let name = command.get_name().to_string();
            clap_complete::generate(args.shell, &mut command, name, &mut std::io::stdout());
            Ok(())
        }
    }
}
