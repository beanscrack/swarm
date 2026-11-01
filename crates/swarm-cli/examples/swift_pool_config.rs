//! Print a complete config; replace model/URLs with the actual deployment.
use swarm_core::config::{ProviderConfig, ProviderEndpoint, RunConfig, ToolProtocolSelection};

fn main() {
    let mut config = RunConfig::mock("Swift worker", 4);
    let mut provider = ProviderConfig::openai_compatible("swift", "swift");
    provider.zero_cost = true;
    provider.billing = swarm_core::budget::BillingMode::ZeroCost;
    provider.api_key_env = None;
    provider.max_concurrency = Some(4);
    provider.max_output_tokens = 8192;
    provider.openai.min_output_tokens = Some(4096);
    provider
        .openai
        .chat_template_kwargs
        .insert("enable_thinking".into(), serde_json::json!(false));
    provider.endpoints = vec![
        ProviderEndpoint {
            base_url: "http://127.0.0.1:8080/v1".into(),
            max_concurrency: 2,
        },
        ProviderEndpoint {
            base_url: "http://127.0.0.1:8081/v1".into(),
            max_concurrency: 2,
        },
    ];
    config.provider = provider.name.clone();
    config.model = provider.model.clone();
    config.providers = vec![provider];
    config.tool_protocol = ToolProtocolSelection::Text;
    print!(
        "{}",
        toml::to_string_pretty(&config).expect("serialize config")
    );
}
