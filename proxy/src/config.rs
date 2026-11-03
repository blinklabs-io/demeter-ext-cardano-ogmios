use std::{env, path::PathBuf, time::Duration};

#[derive(Debug, Clone)]
pub struct Config {
    pub proxy_addr: String,
    pub proxy_namespace: String,
    pub proxy_tiers_path: PathBuf,
    pub proxy_tiers_poll_interval: Duration,
    pub prometheus_addr: String,
    pub ogmios_port: u16,
    pub ogmios_dns: String,
    pub ssl_crt_path: PathBuf,
    pub ssl_key_path: PathBuf,
    pub health_network: String,

    // Health endpoint
    pub health_poll_interval: std::time::Duration,

    // CORS configuration
    pub cors_allow_origin: String,
    pub cors_allow_methods: String,
    pub cors_allow_headers: String,
    pub cors_max_age: String,

    // Request classification telemetry
    pub rpc_telemetry_networks: RpcTelemetryNetworks,
}

impl Config {
    pub fn new() -> Self {
        Self {
            proxy_addr: env::var("PROXY_ADDR").expect("PROXY_ADDR must be set"),
            proxy_namespace: env::var("PROXY_NAMESPACE").unwrap_or("ftr-ogmios-v1".into()),
            proxy_tiers_path: env::var("PROXY_TIERS_PATH")
                .map(|v| v.into())
                .expect("PROXY_TIERS_PATH must be set"),
            proxy_tiers_poll_interval: env::var("PROXY_TIERS_POLL_INTERVAL")
                .map(|v| {
                    Duration::from_secs(
                        v.parse::<u64>()
                            .expect("PROXY_TIERS_POLL_INTERVAL must be a number in seconds. eg: 2"),
                    )
                })
                .unwrap_or(Duration::from_secs(2)),
            prometheus_addr: env::var("PROMETHEUS_ADDR").expect("PROMETHEUS_ADDR must be set"),
            ssl_crt_path: env::var("SSL_CRT_PATH")
                .map(|e| e.into())
                .expect("SSL_CRT_PATH must be set"),
            ssl_key_path: env::var("SSL_KEY_PATH")
                .map(|e| e.into())
                .expect("SSL_KEY_PATH must be set"),
            ogmios_port: env::var("OGMIOS_PORT")
                .expect("OGMIOS_PORT must be set")
                .parse()
                .expect("OGMIOS_PORT must a number"),
            ogmios_dns: env::var("OGMIOS_DNS").expect("OGMIOS_DNS must be set"),
            health_network: env::var("HEALTH_NETWORK").unwrap_or("cardano-mainnet".into()),
            health_poll_interval: env::var("HEALTH_POLL_INTERVAL")
                .map(|v| {
                    Duration::from_secs(
                        v.parse::<u64>()
                            .expect("HEALTH_POLL_INTERVAL must be a number in seconds. eg: 2"),
                    )
                })
                .unwrap_or(Duration::from_secs(10)),
            cors_allow_origin: env::var("CORS_ALLOW_ORIGIN").unwrap_or("*".into()),
            cors_allow_methods: env::var("CORS_ALLOW_METHODS")
                .unwrap_or("GET, POST, PUT, DELETE, OPTIONS".into()),
            cors_allow_headers: env::var("CORS_ALLOW_HEADERS")
                .unwrap_or("Content-Type, Authorization, X-Requested-With, dmtr-api-key".into()),
            cors_max_age: env::var("CORS_MAX_AGE").unwrap_or("86400".into()),
            rpc_telemetry_networks: RpcTelemetryNetworks::parse(
                env::var("RPC_TELEMETRY_NETWORKS").ok().as_deref(),
            ),
        }
    }

    pub fn instance(&self, network: &str, version: &str) -> String {
        format!(
            "ogmios-{}-{}.{}:{}",
            network, version, self.ogmios_dns, self.ogmios_port
        )
    }
}

/// The networks whose client messages are classified and counted, from
/// `RPC_TELEMETRY_NETWORKS`: unset or empty disables telemetry, `*` enables
/// every network, and otherwise it is a comma list of exact network names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcTelemetryNetworks {
    Disabled,
    All,
    Only(Vec<String>),
}

impl RpcTelemetryNetworks {
    pub fn parse(value: Option<&str>) -> Self {
        let Some(value) = value.map(str::trim) else {
            return Self::Disabled;
        };
        if value == "*" {
            return Self::All;
        }
        let networks: Vec<String> = value
            .split(',')
            .map(str::trim)
            .filter(|network| !network.is_empty())
            .map(String::from)
            .collect();
        if networks.is_empty() {
            Self::Disabled
        } else {
            Self::Only(networks)
        }
    }

    pub fn enabled_for(&self, network: &str) -> bool {
        match self {
            Self::Disabled => false,
            Self::All => true,
            Self::Only(networks) => networks.iter().any(|enabled| enabled == network),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RpcTelemetryNetworks;

    #[test]
    fn rpc_telemetry_unset_is_disabled() {
        let networks = RpcTelemetryNetworks::parse(None);
        assert_eq!(networks, RpcTelemetryNetworks::Disabled);
        assert!(!networks.enabled_for("cardano-mainnet"));
    }

    #[test]
    fn rpc_telemetry_empty_is_disabled() {
        for value in ["", "  ", ",", " , "] {
            let networks = RpcTelemetryNetworks::parse(Some(value));
            assert_eq!(networks, RpcTelemetryNetworks::Disabled, "{value:?}");
            assert!(!networks.enabled_for(""));
        }
    }

    #[test]
    fn rpc_telemetry_star_enables_every_network() {
        let networks = RpcTelemetryNetworks::parse(Some("*"));
        assert_eq!(networks, RpcTelemetryNetworks::All);
        assert!(networks.enabled_for("cardano-mainnet"));
        assert!(networks.enabled_for("prime-testnet"));
    }

    #[test]
    fn rpc_telemetry_comma_list_matches_exactly() {
        let networks = RpcTelemetryNetworks::parse(Some("cardano-preprod,vector-testnet"));
        assert_eq!(
            networks,
            RpcTelemetryNetworks::Only(vec!["cardano-preprod".into(), "vector-testnet".into()])
        );
        assert!(networks.enabled_for("cardano-preprod"));
        assert!(networks.enabled_for("vector-testnet"));
        assert!(!networks.enabled_for("cardano-mainnet"));
        assert!(!networks.enabled_for("preprod"));
        assert!(!networks.enabled_for("cardano-preprod-v6"));
    }
}
