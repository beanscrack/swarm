//! Prints a complete managed-provider TOML config for testing prompt transport.
//! The configured command echoes the prompt; replace it with a real agent CLI.

use swarm_core::config::{ProviderConfig, RunConfig};
use swarm_core::provider::ProviderKind;

fn main() {
    let mut config = RunConfig::mock("managed example", 1);
    let mut provider = ProviderConfig::mock("managed", "echo-agent");
    provider.kind = ProviderKind::ManagedAgent;
    provider.command = if cfg!(windows) {
        vec!["cmd".into(), "/C".into(), "more".into()]
    } else {
        vec!["cat".into()]
    };
    config.provider = provider.name.clone();
    config.model = provider.model.clone();
    config.providers = vec![provider];
    print!("{}", toml::to_string_pretty(&config).expect("serialize config"));
}
