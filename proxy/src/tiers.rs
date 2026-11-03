use notify::{Event, PollWatcher, RecursiveMode, Watcher};
use regex::Regex;
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::{error::Error, fs, sync::Arc, time::Duration};
use tokio::runtime::{Handle, Runtime};
use tracing::{error, info, instrument, warn};

use crate::State;

#[derive(Debug, Clone, Deserialize)]
pub struct Tier {
    pub name: String,
    pub rates: Vec<TierRate>,
    pub max_connections: usize,
}
#[derive(Debug, Clone, Deserialize)]
pub struct TierRate {
    pub limit: usize,
    #[serde(deserialize_with = "deserialize_duration")]
    pub interval: Duration,
}
pub fn deserialize_duration<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Duration, D::Error> {
    let value: String = Deserialize::deserialize(deserializer)?;
    let regex = Regex::new(r"([\d]+)([\w])").unwrap();
    let captures = regex.captures(&value);
    if captures.is_none() {
        return Err(<D::Error as serde::de::Error>::custom(
            "Invalid tier interval format",
        ));
    }

    let captures = captures.unwrap();
    let number = captures.get(1).unwrap().as_str().parse::<u64>().unwrap();
    let symbol = captures.get(2).unwrap().as_str();

    match symbol {
        "s" => Ok(Duration::from_secs(number)),
        "m" => Ok(Duration::from_secs(number * 60)),
        "h" => Ok(Duration::from_secs(number * 60 * 60)),
        "d" => Ok(Duration::from_secs(number * 60 * 60 * 24)),
        _ => Err(<D::Error as serde::de::Error>::custom(
            "Invalid symbol tier interval",
        )),
    }
}

#[instrument("tiers background service", skip_all)]
pub fn start(state: Arc<State>) {
    tokio::spawn(async move {
        if let Err(err) = update_tiers(state.clone()).await {
            error!(error = err.to_string(), "error to update tiers");
            return;
        }

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(1);

        let watcher_config = notify::Config::default()
            .with_compare_contents(true)
            .with_poll_interval(state.config.proxy_tiers_poll_interval);

        let watcher_result = PollWatcher::new(
            move |res| {
                if let Ok(event) = res {
                    runtime_handle()
                        .block_on(async { tx.send(event).await })
                        .unwrap();
                }
            },
            watcher_config,
        );
        if let Err(err) = watcher_result {
            error!(error = err.to_string(), "error to watcher tier");
            return;
        }

        let mut watcher = watcher_result.unwrap();
        let watcher_result =
            watcher.watch(&state.config.proxy_tiers_path, RecursiveMode::Recursive);
        if let Err(err) = watcher_result {
            error!(error = err.to_string(), "error to watcher tier");
            return;
        }

        loop {
            let result = rx.recv().await;
            if result.is_some() {
                if let Err(err) = update_tiers(state.clone()).await {
                    error!(error = err.to_string(), "error to update tiers");
                    continue;
                }

                info!("tiers modified");
            }
        }
    });
}

/// Parse a rendered `tiers.toml`.
///
/// `Ok(None)` means the file carried no `tiers` key — a warning, not an error.
/// Extracted from `update_tiers` so the exact deserialization path the proxy
/// uses at runtime can be tested against what Terraform renders.
pub(crate) fn parse_tiers(contents: &str) -> Result<Option<Vec<Tier>>, Box<dyn Error>> {
    let value: Value = toml::from_str(contents)?;

    match value.get("tiers") {
        None => Ok(None),
        Some(tiers) => Ok(Some(serde_json::from_value::<Vec<Tier>>(tiers.to_owned())?)),
    }
}

async fn update_tiers(state: Arc<State>) -> Result<(), Box<dyn Error>> {
    let contents = fs::read_to_string(&state.config.proxy_tiers_path)?;

    let tiers = match parse_tiers(&contents)? {
        Some(tiers) => tiers,
        None => {
            warn!("tiers not configured on toml");
            return Ok(());
        }
    };

    *state.tiers.write().await = tiers
        .into_iter()
        .map(|tier| (tier.name.clone(), tier))
        .collect();

    state.limiter.write().await.clear();

    Ok(())
}

fn runtime_handle() -> Handle {
    match Handle::try_current() {
        Ok(h) => h,
        Err(_) => {
            let rt = Runtime::new().unwrap();
            rt.handle().clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Byte-identical to what `bootstrap/proxy/proxy-config.toml.tftpl` renders
    /// for tier 3. This is the only thing standing between a Terraform template
    /// edit and a proxy that cannot load its tiers at runtime.
    const RENDERED_TIER_3: &str = "[[tiers]]\nname = \"3\"\nmax_connections = 450\n[[tiers.rates]]\ninterval = \"1m\"\nlimit = 18000\n";

    /// Byte-identical to what the same template renders for tiers 3 and 4
    /// together. Tier 4 is the internal enterprise tier: it takes tier 3's
    /// message rate and only its connection cap is its own, so the two are
    /// pinned here as a pair rather than separately.
    const RENDERED_TIERS_3_AND_4: &str = "[[tiers]]\nname = \"3\"\nmax_connections = 450\n[[tiers.rates]]\ninterval = \"1m\"\nlimit = 18000\n[[tiers]]\nname = \"4\"\nmax_connections = 450\n[[tiers.rates]]\ninterval = \"1m\"\nlimit = 18000\n";

    /// Byte-identical to what the template renders for the whole ladder:
    /// tiers 0/1/2/3 at the unified public throughput card's 5/20/100/300
    /// messages per second over the proxy's one-minute interval, and tier 4
    /// inheriting tier 3's rate. Connection caps are pinned alongside so a
    /// rate retune cannot silently move a cap.
    const RENDERED_LADDER: &str = "[[tiers]]\nname = \"0\"\nmax_connections = 2\n[[tiers.rates]]\ninterval = \"1m\"\nlimit = 300\n[[tiers]]\nname = \"1\"\nmax_connections = 5\n[[tiers.rates]]\ninterval = \"1m\"\nlimit = 1200\n[[tiers]]\nname = \"2\"\nmax_connections = 250\n[[tiers.rates]]\ninterval = \"1m\"\nlimit = 6000\n[[tiers]]\nname = \"3\"\nmax_connections = 450\n[[tiers.rates]]\ninterval = \"1m\"\nlimit = 18000\n[[tiers]]\nname = \"4\"\nmax_connections = 450\n[[tiers.rates]]\ninterval = \"1m\"\nlimit = 18000\n";

    fn one_rate(interval: &str) -> String {
        format!("[[tiers]]\nname = \"t\"\nmax_connections = 1\n[[tiers.rates]]\ninterval = \"{interval}\"\nlimit = 1\n")
    }

    #[test]
    fn the_rendered_tier_config_parses() {
        let tiers = parse_tiers(RENDERED_TIER_3)
            .expect("rendered config failed to parse")
            .expect("rendered config had no `tiers` key");

        assert_eq!(tiers.len(), 1);
        assert_eq!(tiers[0].name, "3");
        assert_eq!(tiers[0].max_connections, 450);
        assert_eq!(tiers[0].rates.len(), 1);
        assert_eq!(tiers[0].rates[0].limit, 18000);
        assert_eq!(tiers[0].rates[0].interval, Duration::from_secs(60));
    }

    #[test]
    fn the_enterprise_tier_parses_and_carries_tier_3s_message_rate() {
        let tiers = parse_tiers(RENDERED_TIERS_3_AND_4)
            .expect("rendered config failed to parse")
            .expect("rendered config had no `tiers` key");

        let by_name = |name: &str| {
            tiers
                .iter()
                .find(|tier| tier.name == name)
                .unwrap_or_else(|| panic!("tier {name} missing from the rendered config"))
        };
        let tier_3 = by_name("3");
        let tier_4 = by_name("4");

        assert_eq!(tier_4.max_connections, 450);
        assert_eq!(
            tier_4.rates.len(),
            tier_3.rates.len(),
            "tier 4 must carry tier 3's rates"
        );
        for (four, three) in tier_4.rates.iter().zip(&tier_3.rates) {
            assert_eq!(four.limit, three.limit);
            assert_eq!(four.interval, three.interval);
        }
    }

    #[test]
    fn every_documented_interval_unit_parses() {
        for (input, expected) in [
            ("30s", Duration::from_secs(30)),
            ("1m", Duration::from_secs(60)),
            ("1h", Duration::from_secs(60 * 60)),
            ("1d", Duration::from_secs(24 * 60 * 60)),
        ] {
            let tiers = parse_tiers(&one_rate(input)).unwrap().unwrap();
            assert_eq!(tiers[0].rates[0].interval, expected, "unit {input}");
        }
    }

    #[test]
    fn a_malformed_interval_is_an_error_not_a_panic() {
        assert!(parse_tiers(&one_rate("nope")).is_err());
    }

    #[test]
    fn a_file_without_a_tiers_key_is_not_an_error() {
        assert!(parse_tiers("something_else = 1\n").unwrap().is_none());
    }

    #[test]
    fn the_rendered_ladder_matches_the_unified_throughput_card() {
        let tiers = parse_tiers(RENDERED_LADDER)
            .expect("rendered ladder failed to parse")
            .expect("rendered ladder had no `tiers` key");

        // (name, max_connections per replica, messages per minute)
        let expected = [
            ("0", 2, 300),
            ("1", 5, 1200),
            ("2", 250, 6000),
            ("3", 450, 18000),
            ("4", 450, 18000),
        ];

        assert_eq!(tiers.len(), expected.len());
        for (tier, (name, max_connections, limit)) in tiers.iter().zip(expected) {
            assert_eq!(tier.name, name);
            assert_eq!(tier.max_connections, max_connections, "tier {name} cap");
            assert_eq!(tier.rates.len(), 1, "tier {name} rates");
            assert_eq!(tier.rates[0].limit, limit, "tier {name} limit");
            assert_eq!(tier.rates[0].interval, Duration::from_secs(60));
        }
    }
}
