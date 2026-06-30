// SPDX-License-Identifier: BUSL-1.1
//! kube-rs glue: a client from the ambient `KUBECONFIG`, typed CR apply/delete, and
//! the poll/wait oracles (pod Running, Agent `.status.phase`, a metric reaching a
//! threshold) every scenario leans on.
//!
//! Typed CRs reuse the `agent-api` `Agent`/`AgentFleet`/`ModelPool` derives (P0:
//! agentctl drives the *contract* shapes, never an agent's internals).

use std::fmt::Debug;
use std::future::Future;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use k8s_openapi::api::core::v1::Pod;
use kube::api::{DeleteParams, Patch, PatchParams, PostParams};
use kube::core::{NamespaceResourceScope, Resource};
use kube::runtime::wait::{await_condition, conditions};
use kube::{Api, Client};
use serde::de::DeserializeOwned;
use serde::Serialize;

use agent_api::Agent;

/// The field-manager string for server-side apply (kube-rs requires one).
const FIELD_MANAGER: &str = "agentctl-e2e";

/// A kube [`Client`] from the ambient `KUBECONFIG` / in-cluster config. This is the
/// single knob that makes the whole suite portable: point `KUBECONFIG` at kind or a
/// real multi-node cluster and nothing else changes.
pub async fn client() -> Result<Client> {
    Client::try_default()
        .await
        .context("build kube client from KUBECONFIG / in-cluster config")
}

/// A namespaced typed API handle for a CR kind `K`.
pub fn api<K>(client: &Client, ns: &str) -> Api<K>
where
    K: Resource<Scope = NamespaceResourceScope>,
    K::DynamicType: Default,
{
    Api::namespaced(client.clone(), ns)
}

/// Server-side-apply a typed CR (create-or-update). `name` is the object name used
/// for the apply patch.
pub async fn apply<K>(client: &Client, ns: &str, name: &str, obj: &K) -> Result<K>
where
    K: Resource<Scope = NamespaceResourceScope> + Serialize + DeserializeOwned + Clone + Debug,
    K::DynamicType: Default,
{
    let api: Api<K> = api(client, ns);
    let pp = PatchParams::apply(FIELD_MANAGER).force();
    api.patch(name, &pp, &Patch::Apply(obj))
        .await
        .with_context(|| format!("apply {name} in {ns}"))
}

/// Create a typed CR (errors if it already exists).
pub async fn create<K>(client: &Client, ns: &str, obj: &K) -> Result<K>
where
    K: Resource<Scope = NamespaceResourceScope> + Serialize + DeserializeOwned + Clone + Debug,
    K::DynamicType: Default,
{
    let api: Api<K> = api(client, ns);
    api.create(&PostParams::default(), obj)
        .await
        .context("create CR")
}

/// Delete a typed CR by name and poll until it (and its garbage-collected children)
/// are gone, so the scenario "leaves the cluster clean" per the plan.
pub async fn delete_and_wait<K>(
    client: &Client,
    ns: &str,
    name: &str,
    timeout: Duration,
) -> Result<()>
where
    K: Resource<Scope = NamespaceResourceScope> + Clone + DeserializeOwned + Debug,
    K::DynamicType: Default,
{
    let api: Api<K> = api(client, ns);
    // Best-effort delete: a NotFound is already the desired end state.
    if let Err(e) = api.delete(name, &DeleteParams::foreground()).await {
        if !is_not_found(&e) {
            return Err(e).with_context(|| format!("delete {name} in {ns}"));
        }
    }
    poll_until(timeout, Duration::from_millis(500), || async {
        match api.get_opt(name).await {
            Ok(None) => Ok(true),
            Ok(Some(_)) => Ok(false),
            Err(e) => Err(anyhow!("get {name} during delete-wait: {e}")),
        }
    })
    .await
    .with_context(|| format!("await GC of {name} in {ns}"))
}

/// Whether a kube error is a 404 NotFound.
pub fn is_not_found(e: &kube::Error) -> bool {
    matches!(e, kube::Error::Api(ae) if ae.code == 404)
}

/// Wait for a pod to reach `Running` (uses the kube-runtime condition watcher).
pub async fn wait_pod_running(
    client: &Client,
    ns: &str,
    name: &str,
    timeout: Duration,
) -> Result<()> {
    let api: Api<Pod> = api(client, ns);
    let cond = await_condition(api, name, conditions::is_pod_running());
    tokio::time::timeout(timeout, cond)
        .await
        .with_context(|| format!("timed out waiting for pod {ns}/{name} Running"))?
        .with_context(|| format!("watch pod {ns}/{name}"))?;
    Ok(())
}

/// Wait until an `Agent`'s `.status.phase` equals `want` (e.g. `"Ready"`).
pub async fn wait_agent_phase(
    client: &Client,
    ns: &str,
    name: &str,
    want: &str,
    timeout: Duration,
) -> Result<()> {
    let api: Api<Agent> = api(client, ns);
    poll_until(timeout, Duration::from_millis(750), || async {
        let a = api
            .get(name)
            .await
            .with_context(|| format!("get Agent {ns}/{name}"))?;
        Ok(a.status.and_then(|s| s.phase).as_deref() == Some(want))
    })
    .await
    .with_context(|| format!("Agent {ns}/{name} reach phase {want}"))
}

/// Wait until an `Agent` reports a Ready condition `status == "True"`.
pub async fn wait_agent_ready(
    client: &Client,
    ns: &str,
    name: &str,
    timeout: Duration,
) -> Result<()> {
    let api: Api<Agent> = api(client, ns);
    poll_until(timeout, Duration::from_millis(750), || async {
        let a = api
            .get(name)
            .await
            .with_context(|| format!("get Agent {ns}/{name}"))?;
        let ready = a
            .status
            .map(|s| {
                s.conditions
                    .iter()
                    .any(|c| c.type_ == "Ready" && c.status == "True")
            })
            .unwrap_or(false);
        Ok(ready)
    })
    .await
    .with_context(|| format!("Agent {ns}/{name} Ready=True"))
}

/// Generic poller: call `f` every `interval` until it returns `Ok(true)` or
/// `timeout` elapses. An `Err` from `f` aborts immediately.
pub async fn poll_until<F, Fut>(timeout: Duration, interval: Duration, mut f: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<bool>>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if f().await? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("condition not met within {timeout:?}");
        }
        tokio::time::sleep(interval).await;
    }
}
