use clap::{Parser, clap_derive::*};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use mineshare::{client::client, server::server};

#[tokio::main]
async fn main() {
    async_main().await;
}

const DEFAULT_CONN_ALPN: &str = "mineshare-conn";
const DEFAULT_PING_ALPN: &str = "mineshare-ping";
const DEFAULT_PING_MSG: &str = "PING";
const DEFAULT_PONG_MSG: &str = "PONG";

async fn async_main() {
    let args = Cli::parse();
    match args.action {
        Actions::Server {
            server_ip,
            conn_alpn,
            ping_alpn,
            ping_msg,
            pong_msg,
        } => {
            let (errsend, mut errrecv) = tokio::sync::mpsc::channel(100);
            let (evsend, mut evrecv) = tokio::sync::mpsc::channel(100);
            tokio::task::spawn(async move {
                loop {
                    while let Some(recv) = evrecv.recv().await {
                        println!("{}", recv);
                    }
                }
            });
            tokio::task::spawn(async move {
                loop {
                    while let Some(recv) = errrecv.recv().await {
                        println!("{}", recv);
                    }
                }
            });
            server(
                server_ip,
                conn_alpn.as_ref().map_or(DEFAULT_CONN_ALPN, |alpn| alpn),
                ping_alpn.as_ref().map_or(DEFAULT_PING_ALPN, |alpn| alpn),
                ping_msg.as_ref().map_or(DEFAULT_PING_MSG, |msg| msg),
                pong_msg.as_ref().map_or(DEFAULT_PONG_MSG, |msg| msg),
                evsend,
                errsend,
            )
            .await
        }
        Actions::Client {
            server_nodeid,
            listener_ip,
            conn_alpn,
            ping_alpn,
            ping_msg,
            pong_msg,
        } => {
            let (errsend, mut errrecv) = tokio::sync::mpsc::channel(100);
            let (evsend, mut evrecv) = tokio::sync::mpsc::channel(100);
            tokio::task::spawn(async move {
                loop {
                    while let Some(recv) = evrecv.recv().await {
                        println!("{}", recv);
                    }
                }
            });
            tokio::task::spawn(async move {
                loop {
                    while let Some(recv) = errrecv.recv().await {
                        println!("{}", recv);
                    }
                }
            });
            let errsend2 = errsend.clone();
            match client(
                listener_ip.unwrap_or(SocketAddr::V4(SocketAddrV4::new(
                    Ipv4Addr::new(127, 0, 0, 1),
                    0,
                ))),
                &server_nodeid,
                conn_alpn.as_ref().map_or(DEFAULT_CONN_ALPN, |alpn| alpn),
                ping_alpn.as_ref().map_or(DEFAULT_PING_ALPN, |alpn| alpn),
                ping_msg.as_ref().map_or(DEFAULT_PING_MSG, |msg| msg),
                pong_msg.as_ref().map_or(DEFAULT_PONG_MSG, |msg| msg),
                evsend,
                errsend,
            )
            .await
            {
                Ok(()) => (),
                Err(e) => {
                    errsend2.send(e).await.unwrap();
                }
            }
        }
        Actions::Licenses => {
            println!(
                "This project is licensed under the Apache-2.0 license, and includes code licensed under the"
            );
            println!(
                "MIT, Apache-2.0, ISC, Unicode-3.0, Unlicense, CDLA-Permissive-2.0, BSD-3-Clause, ZLib, and MPL-2.0 licenses"
            );
            println!(
                "If you wish to look at the exact list of libraries, please head to where this project is hosted, at"
            );
            println!("`https://github.com/gabeperson/mineshare` and look at the dependency list.");
        }
    }
}

#[derive(Parser, Debug)]
struct Cli {
    #[command(subcommand)]
    action: Actions,
}

#[derive(Subcommand, Debug)]
enum Actions {
    /// Host the `server` side of the reverse proxy connection.
    /// This is the option you want if you want to share your TLS server (ex: minecraft, http(s), etc) with others
    Server {
        /// The IP of the server you wish to proxy requests to.
        /// If you're running this on the same machine as a minecraft server for example, you probably want to use `127.0.0.1:25565`
        server_ip: SocketAddr,
        /// The ALPN used for non-ping requests.
        /// Leave this as default if you are unsure
        #[arg(long)]
        conn_alpn: Option<String>,
        /// The ALPN used for ping requests.
        /// Leave this as default if you are unsure
        #[arg(long)]
        ping_alpn: Option<String>,
        /// The PING message used for pings
        /// Leave this as default if you are unsure
        #[arg(long)]
        ping_msg: Option<String>,
        /// The PONG message used for pings
        /// Leave this as default if you are unsure
        #[arg(long)]
        pong_msg: Option<String>,
    },
    /// Host the `client` side of the reverse proxy connection.
    /// This is the option you want if you want to use or access someone else's TLS server (ex: minecraft, http(s), etc)
    Client {
        /// The Node ID of the server you wish to connect to.
        /// When you start a proxy server with the `server` subcommand, the nodeid will be displayed
        server_nodeid: String,
        /// The IP address that you would use to connect to the proxy (to then connect to the server).
        /// Leaving it blank will automatically choose an address for you
        listener_ip: Option<SocketAddr>,
        /// The ALPN used for non-ping requests.
        /// Leave this as default if you are unsure
        #[arg(long)]
        conn_alpn: Option<String>,
        /// The ALPN used for ping requests.
        /// Leave this as default if you are unsure
        #[arg(long)]
        ping_alpn: Option<String>,
        /// The PING message used for pings
        /// Leave this as default if you are unsure
        #[arg(long)]
        ping_msg: Option<String>,
        /// The PONG message used for pings
        /// Leave this as default if you are unsure
        #[arg(long)]
        pong_msg: Option<String>,
    },
    /// List licensing information
    Licenses,
}
