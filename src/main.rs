#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![feature(addr_parse_ascii)]

extern crate core;

use crate::config::ip_source::GetIpError;
use crate::config::Config;
use crate::network_listener::has_internet;
use crate::retrying_client::RetryingClient;
use crate::updaters::{UpdaterEvent, UpdaterExitStatus, UpdatersManager};
use crate::util::{new_skip_interval, num_cpus, EscapeExt};
use anyhow::{anyhow, Context};
use futures::StreamExt;
use serde::Deserialize;
use std::borrow::Cow;
use std::cell::Cell;
use std::net::Ipv4Addr;
use std::pin::pin;
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tokio::runtime::Runtime;
use tokio::sync::Semaphore;
use tokio::try_join;

mod config;
mod console_listener;
mod err;
mod network_listener;
mod retrying_client;
mod updaters;
mod util;

struct DdnsContext {
    client: RetryingClient,
    message_boxes: MessageBoxes,
}

#[derive(Debug)]
struct Record {
    id: Box<str>,
    ip: Ipv4Addr,
}

impl DdnsContext {
    async fn get_ip(&self, cfg: &Config) -> anyhow::Result<Ipv4Addr> {
        let last_err = Cell::new(None);

        let iter = cfg.ip_sources().map(|x| x.resolve_ip(&self.client, cfg));
        let stream = futures::stream::iter(iter)
            .buffer_unordered(cfg.concurrent_resolve().get() as usize)
            .filter_map(|x| {
                std::future::ready({
                    match x {
                        Ok(x) => Some(x),
                        Err(err) => {
                            last_err.set(Some(err));
                            None
                        }
                    }
                })
            });

        pin!(stream)
            .next()
            .await
            .ok_or_else(|| last_err.take().unwrap_or(GetIpError::NoIpSources).into())
    }

    async fn get_record(&self, cfg: &Config) -> anyhow::Result<Record> {
        let url = format!(
            "https://api.cloudflare.com/client/v4/zones/{zone_id}/dns_records?type=A&name={record}",
            zone_id = include_str!("./secret/zone-id"),
            record = include_str!("./secret/record")
        );

        #[derive(Debug, Deserialize)]
        struct FullRecord {
            id: Box<str>,
            name: Box<str>,
            #[serde(rename = "content")]
            ip: Ipv4Addr,
        }

        #[derive(Debug, Deserialize)]
        pub struct GetResponse {
            result: Vec<FullRecord>,
        }

        let records = cfg
            .authorize_request(self.client.get(url))
            .send()
            .await?
            .json::<GetResponse>()
            .await?
            .result;

        let [FullRecord { id, ip, name }] = <[FullRecord; 1]>::try_from(records)
            .map_err(|vec| anyhow!("expected 1 record got {} records: {vec:?}", vec.len()))?;

        anyhow::ensure!(
            &*name == include_str!("secret/record"),
            "Expected {} found {name}",
            include_str!("secret/record")
        );

        Ok(Record { id, ip })
    }

    async fn update_record(&self, id: &str, ip: Ipv4Addr, cfg: &Config) -> anyhow::Result<()> {
        let request_json = format! {
            r###"{{"type":"A","name":"{record}","content":"{ip}","proxied":{proxied}}}"###,
            record = include_str!("./secret/record").escape_json(),
            proxied = false
        };

        let url = format! {
            "https://api.cloudflare.com/client/v4/zones/{zone_id}/dns_records/{record_id}",
            zone_id = include_str!("./secret/zone-id"),
            record_id = id
        };

        let response = cfg
            .authorize_request(self.client.patch(url))
            .json(request_json)
            .send()
            .await?;

        let failure = !response.status().is_success();

        let bytes = response
            .bytes()
            .await
            .with_context(|| "unable to retrieve bytes")?;

        #[derive(Debug, Deserialize)]
        pub struct PatchResponse {
            success: bool,
        }

        let response = serde_json::from_slice::<PatchResponse>(&bytes)
            .with_context(|| "unable to deserialize patch response json")?;

        if failure || !response.success {
            anyhow::bail!("Bad response: {}", String::from_utf8_lossy(&bytes))
        }

        Ok(())
    }

    pub async fn run_ddns(&self, cfg: Config) -> anyhow::Result<()> {
        let (record, current_ip) = try_join!(self.get_record(&cfg), self.get_ip(&cfg))?;

        let Record { id, ip } = record;
        if ip == current_ip {
            dbg_println!("IP didn't change skipping record update");
            return Ok(());
        }

        self.update_record(&id, current_ip, &cfg).await
    }
}

#[derive(Clone)]
struct MessageBoxes {
    errors_semaphore: Arc<Semaphore>,
    warning_semaphore: Arc<Semaphore>,
}

impl MessageBoxes {
    async fn custom_error(&self, fun: impl FnOnce() + Send + 'static) {
        err::spawn_message_box(Arc::clone(&self.errors_semaphore), fun).await
    }

    async fn custom_warning(&self, fun: impl FnOnce() + Send + 'static) {
        err::spawn_message_box(Arc::clone(&self.warning_semaphore), fun).await
    }

    async fn error(&self, msg: impl Into<Cow<'static, str>>) {
        let msg = msg.into();
        self.custom_error(move || err::err(&msg)).await
    }

    async fn warning(&self, msg: impl Into<Cow<'static, str>>) {
        let msg = msg.into();
        self.custom_warning(move || err::warn(&msg)).await
    }
}

enum Action {
    Restart,
    Exit(u8),
}

async fn real_main() -> Action {
    let ctx = DdnsContext {
        client: RetryingClient::new(),
        message_boxes: MessageBoxes {
            errors_semaphore: Arc::new(Semaphore::new(5)),
            warning_semaphore: Arc::new(Semaphore::new(5)),
        },
    };

    let mut updaters_manager = UpdatersManager::new(ctx.message_boxes.clone());

    err::exit::subscribe(&mut updaters_manager);
    network_listener::subscribe(&mut updaters_manager);
    console_listener::subscribe(&mut updaters_manager);
    let cfg_store = config::listener::subscribe(&mut updaters_manager).await;

    // 1 hour
    // this will be controlled by config
    let mut interval = new_skip_interval(Duration::from_secs(60 * 60));

    loop {
        tokio::select! {
            _ = interval.tick() => {
                if !has_internet().await {
                    dbg_println!("no internet available skipping update");
                    continue;
                }

                dbg_println!("updating");
                match ctx.run_ddns(cfg_store.load_config()).await {
                    Err(panic) => ctx.message_boxes.error(panic.to_string()).await,
                    Ok(()) => dbg_println!("successfully updated")
                }
            },
            res = updaters_manager.watch() => match res {
                UpdaterEvent::Update => interval.reset_immediately(),
                UpdaterEvent::ServiceEvent(exit) => {
                    match exit.status {
                        UpdaterExitStatus::Success => {},
                        UpdaterExitStatus::Panic | UpdaterExitStatus::Error(_) => {
                            ctx.message_boxes.error(format!("Updater abruptly exited: {exit}")).await
                        }
                        UpdaterExitStatus::TriggerExit(code) => {
                            updaters_manager.shutdown().await;
                            return Action::Exit(code);
                        },
                        UpdaterExitStatus::TriggerRestart => return Action::Restart,
                    }
                }
            }
        }
    }
}

#[cfg(feature = "trace")]
fn make_runtime() -> Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build runtime")
}

#[cfg(not(feature = "trace"))]
fn make_runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(num_cpus().get())
        .enable_all()
        .build()
        .expect("failed to build runtime")
}

fn main() -> ExitCode {
    err::set_hook();
    #[cfg(feature = "trace")]
    console_subscriber::init();

    let runtime = make_runtime();
    loop {
        let exit = std::panic::catch_unwind(|| runtime.block_on(real_main()));

        match exit {
            Ok(Action::Exit(exit)) => {
                dbg_println!("Shutting down the runtime...");
                drop(runtime);
                dbg_println!("Exiting...");
                return ExitCode::from(exit);
            }
            Ok(Action::Restart) => dbg_println!("Restarting..."),
            Err(_) => {
                dbg_println!("Panicked!!");
                dbg_println!("Retrying in 15s...");
                thread::sleep(Duration::from_secs(15));
                dbg_println!("Retrying")
            }
        }
    }
}
