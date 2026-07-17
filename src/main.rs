use anyhow::Result;
use chainz::{
    chain::ChainDefinition, config::Chainz, doctor, init, listing, opt, opt::Opt, ui,
    variables::ChainVariables,
};
use clap::{CommandFactory, Parser};
use dialoguer::FuzzySelect;
use std::process::Command as ProcessCommand;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        if ui::is_cancelled(&error) {
            eprintln!("Cancelled");
            return;
        }
        eprintln!("Error: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let opts = Opt::parse();

    // These commands run before the config is loaded: completions needs no
    // config, and init must be able to recover from a corrupt config
    // (which Chainz::load rejects) by recreating it.
    match opts.cmd {
        opt::Command::Completions { shell } => {
            clap_complete::generate(shell, &mut Opt::command(), "chainz", &mut std::io::stdout());
            return Ok(());
        }
        opt::Command::Init {} => return init::handle_init().await,
        opt::Command::Doctor { fix } => {
            let mut chainz = Chainz::load_for_doctor().await?;
            if !fix {
                chainz.release_config_lock();
            }
            let report = doctor::run(&mut chainz, fix).await?;
            if report.failures > 0 {
                std::process::exit(1);
            }
            return Ok(());
        }
        _ => {}
    }

    let mut chainz = Chainz::load().await?;

    match opts.cmd {
        opt::Command::Init {} | opt::Command::Completions { .. } | opt::Command::Doctor { .. } => {
            unreachable!("handled above")
        }
        opt::Command::Key { cmd } => {
            cmd.handle(&mut chainz).await?;
        }
        opt::Command::Var { cmd } => {
            cmd.handle(&mut chainz).await?;
        }
        opt::Command::Add { args } => {
            let chain = args.handle(&mut chainz).await?;
            println!("Added chain {}", chain.name);
        }
        opt::Command::Update { args } => {
            args.handle(&mut chainz).await?;
        }
        opt::Command::Remove { name_or_id } => {
            let removed = chainz.remove_chain_exact(&name_or_id)?;
            chainz.save().await?;
            println!("Removed chain '{}'", removed.name);
        }
        opt::Command::Use { name_or_id } => {
            let target = match name_or_id {
                Some(id) => id,
                None => select_chain(&chainz)?,
            };
            let definition = chainz.config.get_chain(&target)?;
            chainz.config.default_chain = Some(definition.name.clone());
            chainz.save().await?;
            println!("Default chain set to '{}'", definition.name);
        }
        opt::Command::List {
            json,
            show_secrets,
            verbose,
        } => {
            let chains = chainz.list_chains();
            if json {
                let entries: Vec<_> = chains
                    .iter()
                    .map(|c| ChainListing {
                        name: &c.name,
                        aliases: &c.aliases,
                        chain_id: c.chain_id,
                        selected_rpc: if show_secrets {
                            c.selected_rpc.clone()
                        } else {
                            chainz::variables::redact_url(&c.selected_rpc)
                        },
                        rpc_urls: c
                            .rpc_urls
                            .iter()
                            .map(|url| {
                                if show_secrets {
                                    url.clone()
                                } else {
                                    chainz::variables::redact_url(url)
                                }
                            })
                            .collect(),
                        key_name: c.key_name.as_deref(),
                        verification_url: c.verification_url.as_ref().map(|url| {
                            if show_secrets {
                                url.clone()
                            } else {
                                chainz::variables::redact_url(url)
                            }
                        }),
                        verification_api_key: if show_secrets {
                            c.verification_api_key.as_deref()
                        } else {
                            None
                        },
                        is_default: chainz.config.default_chain.as_deref() == Some(c.name.as_str()),
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else if show_secrets {
                for chain_def in chains {
                    println!("{}", chain_def.display_with_secrets(true));
                }
            } else if verbose {
                print!(
                    "{}",
                    listing::verbose(chains, chainz.config.default_chain.as_deref())
                );
            } else {
                print!(
                    "{}",
                    listing::compact(chains, chainz.config.default_chain.as_deref())
                );
            }
        }
        opt::Command::Show {
            name_or_id,
            json,
            show_secrets,
        } => {
            let chain = chainz.config.get_chain(&name_or_id)?;
            let entry = ChainListing::new(&chain, &chainz.config.default_chain, show_secrets);
            if json {
                println!("{}", serde_json::to_string_pretty(&entry)?);
            } else {
                println!("{}", chain.display_with_secrets(show_secrets));
                println!("Default: {}", if entry.is_default { "Yes" } else { "No" });
            }
        }
        opt::Command::Shell { name_or_id } => {
            let name_or_id = match name_or_id.or_else(|| chainz.config.default_chain.clone()) {
                Some(id) => id,
                None => select_chain(&chainz)?,
            };
            let chain = chainz.get_chain(&name_or_id)?;
            // Empty command args → lazy rule: key backends are never touched
            let variables = ChainVariables::new(&chain, &[], false)?;
            let chain_name = chain.definition.name.clone();
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());

            eprintln!(
                "{}",
                ui::item(&format!("entering {} shell — ctrl-d to exit", chain_name))
            );
            let ps1 = format!(
                "(⛓ {}) {}",
                chain_name,
                std::env::var("PS1").unwrap_or_default()
            );
            chainz.release_config_lock();
            let status = ProcessCommand::new(&shell)
                .envs(variables.as_map())
                .env("CHAINZ_CHAIN", &chain_name)
                .env("PS1", ps1)
                .status()?;
            eprintln!("{}", ui::dim(&format!("left {} shell", chain_name)));
            if !status.success() {
                std::process::exit(status.code().unwrap_or(1));
            }
        }
        opt::Command::Exec {
            name_or_id,
            command,
            key,
            expose_key,
        } => {
            // Explicit chain > configured default > interactive picker
            let name_or_id = match name_or_id.or_else(|| chainz.config.default_chain.clone()) {
                Some(id) => id,
                None => select_chain(&chainz)?,
            };
            let mut chain = chainz.get_chain(&name_or_id)?;
            if let Some(key_name) = key {
                chain = chain.with_key(chainz.get_key(&key_name)?);
            }
            let variables = ChainVariables::new(&chain, &command, expose_key)?;
            let expanded_command = variables.expand(command);

            chainz.release_config_lock();
            let status = ProcessCommand::new(&expanded_command[0])
                .args(&expanded_command[1..])
                .envs(variables.as_map())
                .status()?;

            if !status.success() {
                std::process::exit(status.code().unwrap_or(1));
            }
        }
    }
    Ok(())
}

/// The `list --json` scripting contract: a stable shape decoupled from the
/// storage schema. Credentials are omitted unless the caller explicitly uses
/// `--show-secrets`.
#[derive(serde::Serialize)]
struct ChainListing<'a> {
    name: &'a str,
    aliases: &'a [String],
    chain_id: u64,
    selected_rpc: String,
    rpc_urls: Vec<String>,
    key_name: Option<&'a str>,
    verification_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verification_api_key: Option<&'a str>,
    is_default: bool,
}

impl<'a> ChainListing<'a> {
    fn new(chain: &'a ChainDefinition, default: &Option<String>, show_secrets: bool) -> Self {
        Self {
            name: &chain.name,
            aliases: &chain.aliases,
            chain_id: chain.chain_id,
            selected_rpc: if show_secrets {
                chain.selected_rpc.clone()
            } else {
                chainz::variables::redact_url(&chain.selected_rpc)
            },
            rpc_urls: chain
                .rpc_urls
                .iter()
                .map(|url| {
                    if show_secrets {
                        url.clone()
                    } else {
                        chainz::variables::redact_url(url)
                    }
                })
                .collect(),
            key_name: chain.key_name.as_deref(),
            verification_url: chain.verification_url.as_ref().map(|url| {
                if show_secrets {
                    url.clone()
                } else {
                    chainz::variables::redact_url(url)
                }
            }),
            verification_api_key: show_secrets
                .then_some(chain.verification_api_key.as_deref())
                .flatten(),
            is_default: default.as_deref() == Some(chain.name.as_str()),
        }
    }
}

fn select_chain(chainz: &Chainz) -> Result<String> {
    let chains = chainz.list_chains();
    if chains.is_empty() {
        anyhow::bail!("No chains configured. Use 'chainz add' to add a chain first.");
    }
    let items: Vec<String> = chains
        .iter()
        .map(|c| format!("{} ({})", c.name, c.chain_id))
        .collect();
    let selection = FuzzySelect::new()
        .with_prompt("Select a chain")
        .items(&items)
        .default(0)
        .interact_opt()?
        .ok_or_else(ui::cancelled)?;
    Ok(chains[selection].name.clone())
}
