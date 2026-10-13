use clap::Parser;
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    match swarm_cli::execute(swarm_cli::Cli::parse()).await {
        Ok(result) => {
            if !result.text.is_empty() {
                println!("{}", result.text);
            }
            if result.success {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}
