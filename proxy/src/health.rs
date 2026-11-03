use std::sync::Arc;
use tracing::{error, info, warn};

use crate::State;

async fn get_health(state: &State) -> bool {
    let client = match reqwest::Client::builder().build() {
        Ok(client) => client,
        Err(err) => {
            warn!(error = err.to_string(), "Failed to build reqwest client");
            return false;
        }
    };

    let response = match client
        .get(format!(
            "http://{}/health",
            state.config.instance(&state.config.health_network, "6")
        ))
        .send()
        .await
    {
        Ok(response) => response,
        Err(err) => {
            warn!(error = err.to_string(), "Failed to perform health request");
            return false;
        }
    };

    let status = response.status();
    if status.is_client_error() || status.is_server_error() {
        error!(status = status.to_string(), "Health request failed");
        return false;
    }

    status == 200
}

async fn update_health(state: &State) {
    let current_health = *state.upstream_health.read().await;

    let new_health = get_health(state).await;

    match (current_health, new_health) {
        (false, true) => info!("Upstream is now healthy, ready to proxy requests."),
        (true, false) => warn!("Upstream is now deamed unhealthy."),
        _ => {}
    }

    *state.upstream_health.write().await = new_health;
}

pub async fn start(state: Arc<State>) {
    loop {
        update_health(&state).await;
        tokio::time::sleep(state.config.health_poll_interval).await;
    }
}
