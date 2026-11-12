//! End-to-end tests of the `chainz` binary, one module per command. Every
//! run of the binary goes through [`common::Sandbox`]; RPCs are mocks on
//! loopback and nothing here touches the network.
//!
//! A test of one command goes in that command's module. `cli` holds what
//! every command shares (usage errors, removed spellings), `config` the
//! config files themselves, `hot_path` the structural check of `exec`'s
//! fast path, and `golden` the byte-exact snapshots.

mod add;
mod cli;
mod common;
mod complete;
mod config;
mod doctor;
mod edit;
mod exec;
mod fork;
mod golden;
mod hot_path;
mod init;
mod label;
mod label_import;
mod ls;
mod migrate;
mod rm;
mod serve;
mod signer;
mod top;
mod update;
mod r#use;
mod var;
