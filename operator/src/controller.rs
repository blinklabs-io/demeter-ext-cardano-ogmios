use futures::StreamExt;
use kube::{
    api::ListParams,
    runtime::{controller::Action, watcher::Config as WatcherConfig, Controller},
    Api, Client, CustomResource, CustomResourceExt, ResourceExt,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};
use tracing::{error, info, instrument};

use crate::{build_api_key, build_hostname, patch_resource_status, Error, Metrics, Result, State};

pub static OGMIOS_PORT_FINALIZER: &str = "ogmiosports.demeter.run";

#[derive(CustomResource, Deserialize, Serialize, Clone, Debug, JsonSchema)]
#[kube(
    kind = "OgmiosPort",
    group = "demeter.run",
    version = "v1alpha1",
    category = "demeter-port",
    shortname = "opt",
    namespaced
)]
#[kube(status = "OgmiosPortStatus")]
#[kube(printcolumn = r#"
        {"name": "Network", "jsonPath": ".spec.network", "type": "string"},
        {"name": "Version", "jsonPath": ".spec.version", "type": "number"},
        {"name": "Endpoint URL", "jsonPath": ".status.endpointUrl",  "type": "string"},
        {"name": "Authenticated Endpoint URL", "jsonPath": ".status.authenticatedEndpointUrl", "type": "string"},
        {"name": "Auth Token", "jsonPath": ".status.authToken", "type": "string"},
        {"name": "Throughput Tier", "jsonPath":".spec.throughputTier", "type": "string"}
    "#)]
#[serde(rename_all = "camelCase")]
pub struct OgmiosPortSpec {
    pub network: String,
    pub version: u8,
    // Self-serve tiers are "0" through "3"; "4" is the internal enterprise
    // tier, assigned only through the backoffice and never offered as a
    // customer choice. Kept a free-form string on purpose: the proxy resolves
    // it against its own tier configuration, so adding a tier is a config
    // change and not a CRD schema change.
    pub throughput_tier: String,
    pub auth_token: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Default, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OgmiosPortStatus {
    pub endpoint_url: String,
    pub authenticated_endpoint_url: String,
    pub auth_token: String,
}

struct Context {
    pub client: Client,
    pub metrics: Metrics,
}
impl Context {
    pub fn new(client: Client, metrics: Metrics) -> Self {
        Self { client, metrics }
    }
}

async fn reconcile(crd: Arc<OgmiosPort>, ctx: Arc<Context>) -> Result<Action> {
    let key = match &crd.spec.auth_token {
        Some(api_key) => api_key.clone(),
        None => build_api_key(&crd).await?,
    };

    let (hostname, hostname_key) = build_hostname(&crd.spec.network, &crd.spec.version, &key);

    let status = OgmiosPortStatus {
        endpoint_url: format!("https://{hostname}",),
        authenticated_endpoint_url: format!("https://{hostname_key}"),
        auth_token: key,
    };

    let namespace = crd.namespace().unwrap();
    let ogmios_port = OgmiosPort::api_resource();

    patch_resource_status(
        ctx.client.clone(),
        &namespace,
        ogmios_port,
        &crd.name_any(),
        serde_json::to_value(status)?,
    )
    .await?;

    info!(resource = crd.name_any(), "Reconcile completed");

    Ok(Action::await_change())
}

fn error_policy(crd: Arc<OgmiosPort>, err: &Error, ctx: Arc<Context>) -> Action {
    error!(error = err.to_string(), "reconcile failed");
    ctx.metrics.reconcile_failure(&crd, err);
    Action::requeue(Duration::from_secs(5))
}

#[instrument("controller run", skip_all)]
pub async fn run(state: Arc<State>) {
    info!("listening crds running");

    let client = Client::try_default()
        .await
        .expect("failed to create kube client");

    let crds = Api::<OgmiosPort>::all(client.clone());
    if let Err(e) = crds.list(&ListParams::default().limit(1)).await {
        error!("CRD is not queryable; {e:?}. Is the CRD installed?");
        std::process::exit(1);
    }

    let ctx = Context::new(client, state.metrics.clone());

    Controller::new(crds, WatcherConfig::default().any_semantic())
        .shutdown_on_signal()
        .run(reconcile, error_policy, Arc::new(ctx))
        .filter_map(|x| async move { std::result::Result::ok(x) })
        .for_each(|_| futures::future::ready(()))
        .await;
}
