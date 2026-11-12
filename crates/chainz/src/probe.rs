//! Probing endpoints for the commands that judge them: `doctor` (with
//! `--discover`) and `add`. What "healthy" means and how
//! endpoints rank is `chainz_rpc::health`; this module turns config into
//! probes, records them, renders ranked lists and proposes repairs.

use std::time::Duration;

use anyhow::{Result, anyhow};
use chainz_core::model::{Chain, Endpoint};
use chainz_core::redact;
use chainz_core::{Chainz, Tx, names};
use chainz_rpc::health;
use chainz_rpc::{EndpointProbe, Health, HealthStore, ProbeResult, Ranking, RpcClient, Target};

use crate::{net, ui, view};

/// Check that `endpoint` answers with chain ID `chain_id`.
pub fn check(
    chainz: &Chainz,
    chain: &str,
    chain_id: u64,
    ep: &str,
    endpoint: &Endpoint,
) -> Result<()> {
    let target = net::target(chainz, chain, chain_id, ep, endpoint)?;
    net::block_on(RpcClient::new().check_chain_id(&target, chain_id))
        .map_err(|e| anyhow!("RPC check failed for {}: {e}", redact::url(&endpoint.url)))
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
                ui::warn(&format!("skipping {}: {e}", redact::url(url)))
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
            let health = view::health_cell(Some(ranking.health[i]), Some(result.latency));
            format!("{}  {health}", redact::url(url))
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
    /// HTTP endpoints that could not be probed (a `${VAR}` that is set
    /// nowhere, say): the endpoint name and why.
    pub unprobed: Vec<(String, String)>,
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
            .map(|p| health::p50(&store.samples(p.chain_id, &p.endpoint)))
            .collect();
        Ranking::with_p50(&self.probes, &p50)
    }
}

/// Probe every HTTP endpoint of `chains` concurrently, and record the
/// results in `store` (a failed save is a warning). An endpoint that
/// can't be turned into a target is listed in `unprobed`, for the caller
/// to report.
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
        let mut unprobed = Vec::new();
        for (ep, e) in &c.rpcs {
            if e.is_websocket() {
                continue;
            }
            match net::target(chainz, chain, c.id, ep, e) {
                Ok(target) => futures.push(probe_one(client.clone(), target, c.id, depth)),
                Err(err) => unprobed.push((ep.clone(), format!("{err:#}"))),
            }
        }
        out.push(ChainProbes {
            chain: chain.clone(),
            probes: net::join_indexed(futures).await?,
            unprobed,
        });
    }
    if let Some(warning) = health::record(store, out.iter().flat_map(|c| &c.probes)) {
        eprintln!("{}", ui::warn(&warning));
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

/// What is wrong with an endpoint, to end a sentence ("… 'x' is failing
/// (connection failed)"); `None` when it is healthy.
pub fn health_problem(h: &Health) -> Option<String> {
    match h {
        Health::Healthy => None,
        Health::Lagging { behind } => Some(format!("is lagging ({behind} blocks behind)")),
        Health::Failing(class) => Some(format!("is failing ({})", class.phrase())),
    }
}

/// How many of `probes` answered, and of how many.
pub fn answered(probes: &ChainProbes) -> (usize, usize) {
    let ok = probes.probes.iter().filter(|p| p.result.is_ok()).count();
    (ok, probes.probes.len())
}

/// Print `probes` best-first, one line each: role marker, name, health
/// ([`view::health_cell`]), capability tags. Endpoints that failed come
/// last, in config order, with what went wrong.
pub fn print_ranked(chain: &Chain, probes: &ChainProbes, ranking: &Ranking, store: &HealthStore) {
    for line in ranked_lines(chain, probes, ranking, store) {
        println!("{line}");
    }
}

fn ranked_lines(
    chain: &Chain,
    probes: &ChainProbes,
    ranking: &Ranking,
    store: &HealthStore,
) -> Vec<String> {
    let rows: Vec<[String; 4]> = ranking
        .order
        .iter()
        .map(|&i| {
            let p = &probes.probes[i];
            let marker = view::marker(
                chain.rpc == p.endpoint,
                chain.failover.contains(&p.endpoint),
            )
            .to_string();
            let tags = match ranking.health[i] {
                Health::Failing(_) => String::new(),
                _ => store
                    .capabilities(p.chain_id, &p.endpoint)
                    .map(|c| c.tags().join(" "))
                    .unwrap_or_default(),
            };
            [
                marker,
                p.endpoint.clone(),
                view::health_cell(Some(ranking.health[i]), Some(p.result.latency)),
                ui::dim(&tags),
            ]
        })
        .collect();
    // No header row: the chain's section title heads the list.
    view::table_lines(&["", "", "", ""], &rows)[1..]
        .iter()
        .map(|line| format!("  {line}"))
        .collect()
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
            unprobed: vec![],
        };
        let ranking = probes.ranking(&HealthStore::empty());
        let repair = Repair::propose("eth", &c, &probes, &ranking).unwrap();
        assert_eq!(repair.active, "fast");
        // `keyed` sends a header; at most two.
        assert_eq!(repair.failover, ["mid", "spare"]);

        let none = ChainProbes {
            chain: "eth".into(),
            probes: vec![probe("slow", 1, None)],
            unprobed: vec![],
        };
        let ranking = none.ranking(&HealthStore::empty());
        assert_eq!(Repair::propose("eth", &c, &none, &ranking), None);
    }

    /// Dead endpoints show no latency (a refusal in 0ms read as the
    /// fastest) and come after every endpoint that answered.
    #[test]
    fn dead_endpoints_are_listed_last_without_a_latency() {
        let c = chain(&[("slow", false), ("down", false), ("fast", false)]);
        let probes = ChainProbes {
            chain: "eth".into(),
            probes: vec![
                probe("slow", 90, Some(100)),
                probe("down", 0, None),
                probe("fast", 10, Some(100)),
            ],
            unprobed: vec![],
        };
        let ranking = probes.ranking(&HealthStore::empty());
        let lines = ranked_lines(&c, &probes, &ranking, &HealthStore::empty());
        assert!(
            lines[0].contains("fast") && lines[0].contains("10ms"),
            "{lines:?}"
        );
        assert!(lines[1].contains("slow"), "{lines:?}");
        assert!(lines[2].contains("down"), "{lines:?}");
        assert!(lines[2].contains("✗ timed out"), "{lines:?}");
        assert!(!lines[2].contains("ms"), "{lines:?}");
        assert_eq!(answered(&probes), (2, 3));
    }

    #[test]
    fn health_words() {
        let ms = Some(Duration::from_millis(42));
        let cell = view::health_cell;
        assert_eq!(cell(Some(Health::Healthy), ms), "42ms");
        assert_eq!(
            cell(Some(Health::Lagging { behind: 30 }), ms),
            "42ms lag 30"
        );
        assert_eq!(
            cell(Some(Health::Failing(chainz_rpc::ErrorClass::Timeout)), ms),
            "✗ timed out"
        );
        assert_eq!(cell(None, None), "-");
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
        assert!(items[1].ends_with("  ✗ connection failed"), "{}", items[1]);
    }
}
