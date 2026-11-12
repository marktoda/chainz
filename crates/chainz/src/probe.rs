//! Probing endpoints for the commands that judge them: `rpc bench`,
//! `rpc discover`, `doctor` and `chain add`. What "healthy" means and how
//! endpoints rank is `chainz_rpc::health`; this module turns config into
//! probes, records them, renders ranked lists and proposes repairs.

use std::future::Future;
use std::time::Duration;

use anyhow::{Result, anyhow};
use chainz_core::model::{Chain, Endpoint};
use chainz_core::redact;
use chainz_core::{Chainz, Tx, names};
use chainz_rpc::health::{self, EndpointProbe, Health, Ranking};
use chainz_rpc::{HealthStore, ProbeResult, RpcClient, Target};

use crate::{net, ui};

/// Check that `endpoint` answers with chain ID `chain_id`.
pub fn check(
    chainz: &Chainz,
    chain: &str,
    chain_id: u64,
    ep: &str,
    endpoint: &Endpoint,
) -> Result<()> {
    let target = net::target(chainz, chain, chain_id, ep, endpoint)?;
    net::block_on(RpcClient::new().check_chain_id(&target, chain_id)).map_err(|e| {
        anyhow!(
            "RPC check failed for {}: {e}",
            redact::url(&endpoint.url, false)
        )
    })
}

/// Probe `urls` for chain `chain_id`. A URL that is not a valid endpoint is
/// dropped (with a warning), so each result is paired with the URL it
/// probed.
pub fn probe_urls(chain_id: u64, urls: &[String]) -> Vec<(String, ProbeResult)> {
    let mut probed: Vec<String> = Vec::with_capacity(urls.len());
    let mut targets = Vec::with_capacity(urls.len());
    for url in urls {
        match Target::new(chain_id, names::endpoint_name_for_url(url), url, &[]) {
            Ok(target) => {
                probed.push(url.clone());
                targets.push(target);
            }
            Err(e) => eprintln!(
                "{}",
                ui::warn(&format!("skipping {}: {e}", redact::url(url, false)))
            ),
        }
    }
    let mut results = net::block_on(async {
        let mut rx = chainz_rpc::probe_all(&RpcClient::new(), targets, chain_id);
        let mut out: Vec<Option<ProbeResult>> = vec![None; probed.len()];
        while let Some((i, r)) = rx.recv().await {
            out[i] = Some(r);
        }
        out
    });
    probed
        .into_iter()
        .zip(results.iter_mut())
        .filter_map(|(url, result)| Some((url, result.take()?)))
        .collect()
}

/// `probed` ranked, with one list item per URL in rank order: the URL
/// (redacted) and its latency, or what went wrong.
pub fn ranked_urls(probed: &[(String, ProbeResult)]) -> (Ranking, Vec<String>) {
    let results: Vec<&ProbeResult> = probed.iter().map(|(_, r)| r).collect();
    let ranking = Ranking::new(&results);
    let items = ranking
        .order
        .iter()
        .map(|&i| {
            let (url, result) = &probed[i];
            match &result.error {
                None => format!(
                    "{}  {}ms",
                    redact::url(url, false),
                    result.latency.as_millis()
                ),
                Some(e) => format!("{}  {}", redact::url(url, false), e.class().phrase()),
            }
        })
        .collect();
    (ranking, items)
}

/// How much to ask each endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    /// The health probe only.
    Health,
    /// The health probe, then (when it answered) its capabilities.
    Capabilities,
}

/// One chain's probed HTTP endpoints, in config order.
pub struct ChainProbes {
    pub chain: String,
    pub probes: Vec<EndpointProbe>,
}

impl ChainProbes {
    /// The probe of endpoint `name`, if it was probed.
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.probes.iter().position(|p| p.endpoint == name)
    }

    /// The probes ranked, a recorded median latency beating one probe.
    pub fn ranking(&self, store: &HealthStore) -> Ranking {
        let p50: Vec<Option<Duration>> = self
            .probes
            .iter()
            .map(|p| store.p50(p.chain_id, &p.endpoint))
            .collect();
        Ranking::with_p50(&self.probes, &p50)
    }
}

/// Probe every HTTP endpoint of `chains` concurrently, and record the
/// results in `store` (a failed save is a warning).
pub async fn probe_chains(
    chainz: &Chainz,
    chains: &[String],
    depth: Depth,
    store: &mut HealthStore,
) -> Result<Vec<ChainProbes>> {
    let client = RpcClient::new();
    let mut out = Vec::new();
    for chain in chains {
        let c = &chainz.config().chains[chain];
        let mut futures = Vec::new();
        for (ep, e) in &c.rpcs {
            if e.is_websocket() {
                continue;
            }
            let target = match net::target(chainz, chain, c.id, ep, e) {
                Ok(t) => t,
                Err(err) => {
                    eprintln!("{}", ui::warn(&format!("{err:#}")));
                    continue;
                }
            };
            futures.push(probe_one(client.clone(), target, c.id, depth));
        }
        out.push(ChainProbes {
            chain: chain.clone(),
            probes: join_indexed(futures).await?,
        });
    }
    if let Err(e) = health::record(store, out.iter().flat_map(|c| &c.probes)) {
        eprintln!(
            "{}",
            ui::warn(&format!("could not save health history: {e}"))
        );
    }
    Ok(out)
}

async fn probe_one(
    client: RpcClient,
    target: Target,
    chain_id: u64,
    depth: Depth,
) -> EndpointProbe {
    let result = chainz_rpc::probe(&client, &target, chain_id).await;
    let capabilities = if depth == Depth::Capabilities && result.is_ok() {
        Some(chainz_rpc::probe_capabilities(&client, &target).await)
    } else {
        None
    };
    EndpointProbe {
        chain_id,
        endpoint: target.name().to_string(),
        result,
        capabilities,
    }
}

/// Run `futures` concurrently; their outputs in input order. A task that
/// panicked is an error.
pub async fn join_indexed<F>(futures: Vec<F>) -> Result<Vec<F::Output>>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let mut set = tokio::task::JoinSet::new();
    for (i, f) in futures.into_iter().enumerate() {
        set.spawn(async move { (i, f.await) });
    }
    let mut out: Vec<Option<F::Output>> = (0..set.len()).map(|_| None).collect();
    while let Some(joined) = set.join_next().await {
        let (i, v) = joined.map_err(|e| anyhow!("a probe task failed: {e}"))?;
        out[i] = Some(v);
    }
    Ok(out.into_iter().flatten().collect())
}

/// The marker for an endpoint's role: `●` active, `◐` failover.
pub fn marker(chain: &Chain, endpoint: &str) -> &'static str {
    if chain.rpc == endpoint {
        "●"
    } else if chain.failover.iter().any(|f| f == endpoint) {
        "◐"
    } else {
        " "
    }
}

/// The short health word of a ranked list: `ok`, `lag N`, or the error
/// class.
pub fn health_label(h: &Health) -> String {
    match h {
        Health::Healthy => "ok".into(),
        Health::Lagging { behind } => format!("lag {behind}"),
        Health::Failing(class) => class.to_string(),
    }
}

/// What is wrong with an endpoint, to end a sentence ("… 'x' is failing
/// (connection failed)"); `None` when it is healthy.
pub fn health_problem(h: &Health) -> Option<String> {
    match h {
        Health::Healthy => None,
        Health::Lagging { behind } => Some(format!("is lagging ({behind} blocks behind)")),
        Health::Failing(class) => Some(format!("is failing ({})", class.phrase())),
    }
}

/// Print `probes` ranked best-first, one line each: role marker, name,
/// probe latency, health, capability tags.
pub fn print_ranked(chain: &Chain, probes: &ChainProbes, ranking: &Ranking, store: &HealthStore) {
    for &i in &ranking.order {
        let p = &probes.probes[i];
        let tags = store
            .capabilities(p.chain_id, &p.endpoint)
            .map(|c| c.tags().join(" "))
            .unwrap_or_default();
        println!(
            "  {} {:<18} {:>6}ms  {:<10} {}",
            marker(chain, &p.endpoint),
            p.endpoint,
            p.result.latency.as_millis(),
            health_label(&ranking.health[i]),
            ui::dim(&tags)
        );
    }
}

/// A change to a chain's endpoints, proposed from a ranking.
///
/// Rules: the best endpoint becomes active when it is healthy; the failover
/// list is the next healthy endpoints that send no headers (a private,
/// header-authenticated request is never routed elsewhere), at most
/// [`Repair::MAX_FAILOVER`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repair {
    pub chain: String,
    /// The endpoint to make active.
    pub active: String,
    /// The proposed failover list.
    pub failover: Vec<String>,
}

impl Repair {
    /// Longest failover list proposed.
    pub const MAX_FAILOVER: usize = 2;

    /// The repair for `chain` (named `name`) from its ranked probes; `None`
    /// when no endpoint is healthy.
    pub fn propose(
        name: &str,
        chain: &Chain,
        probes: &ChainProbes,
        ranking: &Ranking,
    ) -> Option<Repair> {
        let best = ranking.best_healthy()?;
        let failover = ranking
            .order
            .iter()
            .filter(|&&i| i != best && ranking.health[i] == Health::Healthy)
            .map(|&i| probes.probes[i].endpoint.clone())
            .filter(|n| chain.rpcs.get(n).is_some_and(|e| e.headers.is_empty()))
            .take(Self::MAX_FAILOVER)
            .collect();
        Some(Repair {
            chain: name.to_string(),
            active: probes.probes[best].endpoint.clone(),
            failover,
        })
    }

    /// Make the endpoint active and, when `with_failover`, write the
    /// failover list.
    pub fn apply(&self, tx: &mut Tx, with_failover: bool) -> Result<()> {
        tx.set_active(&self.chain, &self.active)?;
        if with_failover {
            tx.set_failover(&self.chain, &self.failover)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chainz_rpc::RpcError;
    use indexmap::IndexMap;

    fn probe(endpoint: &str, ms: u64, head: Option<u64>) -> EndpointProbe {
        EndpointProbe {
            chain_id: 1,
            endpoint: endpoint.into(),
            result: ProbeResult {
                latency: Duration::from_millis(ms),
                head,
                error: head.is_none().then_some(RpcError::Timeout),
            },
            capabilities: None,
        }
    }

    fn chain(rpcs: &[(&str, bool)]) -> Chain {
        let mut map = IndexMap::new();
        for (name, header) in rpcs {
            let mut e = Endpoint::new(format!("https://{name}.example"));
            if *header {
                e.headers.insert("x-api-key".into(), "${KEY}".into());
            }
            map.insert(name.to_string(), e);
        }
        Chain {
            id: 1,
            aliases: vec![],
            signer: None,
            explorer: None,
            rpc: rpcs[0].0.to_string(),
            failover: vec![],
            rpcs: map,
        }
    }

    #[test]
    fn repair_activates_the_best_and_proposes_headerless_failover() {
        let c = chain(&[
            ("slow", false),
            ("keyed", true),
            ("fast", false),
            ("mid", false),
            ("spare", false),
            ("down", false),
        ]);
        let probes = ChainProbes {
            chain: "eth".into(),
            probes: vec![
                probe("slow", 90, Some(100)),
                probe("keyed", 20, Some(100)),
                probe("fast", 10, Some(100)),
                probe("mid", 30, Some(100)),
                probe("spare", 40, Some(100)),
                probe("down", 1, None),
            ],
        };
        let ranking = probes.ranking(&HealthStore::empty());
        let repair = Repair::propose("eth", &c, &probes, &ranking).unwrap();
        assert_eq!(repair.active, "fast");
        // `keyed` sends a header; at most two.
        assert_eq!(repair.failover, ["mid", "spare"]);

        let none = ChainProbes {
            chain: "eth".into(),
            probes: vec![probe("slow", 1, None)],
        };
        let ranking = none.ranking(&HealthStore::empty());
        assert_eq!(Repair::propose("eth", &c, &none, &ranking), None);
    }

    #[test]
    fn health_words() {
        assert_eq!(health_label(&Health::Lagging { behind: 30 }), "lag 30");
        assert_eq!(
            health_label(&Health::Failing(chainz_rpc::ErrorClass::Connect)),
            "connect"
        );
        assert_eq!(health_problem(&Health::Healthy), None);
        assert_eq!(
            health_problem(&Health::Failing(chainz_rpc::ErrorClass::Connect)).unwrap(),
            "is failing (connection failed)"
        );
    }

    #[test]
    fn ranked_urls_are_redacted() {
        let probed = vec![
            (
                "https://rpc.example/v2/abcdef0123456789abcdef0123456789".to_string(),
                ProbeResult {
                    latency: Duration::from_millis(12),
                    head: Some(1),
                    error: None,
                },
            ),
            (
                "https://down.example".to_string(),
                ProbeResult {
                    latency: Duration::from_millis(1),
                    head: None,
                    error: Some(RpcError::Connect),
                },
            ),
        ];
        let (ranking, items) = ranked_urls(&probed);
        assert_eq!(ranking.order, [0, 1]);
        assert!(!items[0].contains("abcdef0123456789"), "{}", items[0]);
        assert!(items[0].ends_with("  12ms"), "{}", items[0]);
        assert!(items[1].starts_with("https://down.example"), "{}", items[1]);
        assert!(items[1].ends_with("  connection failed"), "{}", items[1]);
    }
}
