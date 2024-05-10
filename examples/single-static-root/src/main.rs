// Copyright 2024 Wladimir Palant
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! # Single static root example
//!
//! This is a simple web server using `static-files-module` crate. It combines the usual
//! [Pingora command line options](Opt) with the
//! [command line options of `static-files-module`](static_files_module::StaticFilesOpt)
//! and the usual [Pingora config file settings](ServerConf) with the
//! [config file settings of `static-files-module`](static_files_module::StaticFilesConf).
//! In addition, it provides the following settings:
//!
//! * `listen` (`--listen` as command line flag): A list of IP address/port combinations the server
//!   should listen on, e.g. `0.0.0.0:8080`.
//! * `compression_level` (`--compression-level` as command line flag): If present, dynamic
//!   compression will be enabled and compression level set to the value provided for all
//!   algorithms (see [Pingora issue #228](https://github.com/cloudflare/pingora/issues/228)).
//!
//! An example config file is provided in this directory. You can run this example with the
//! following command:
//!
//! ```sh
//! cargo run --package example-single-static-root -- -c config.yaml
//! ```
//!
//! To enable debugging output you can use the `RUST_LOG` environment variable:
//!
//! ```sh
//! RUST_LOG=debug cargo run --package example-single-static-root -- -c config.yaml
//! ```

use async_trait::async_trait;
use log::error;
use pingora_core::server::configuration::{Opt as ServerOpt, ServerConf};
use pingora_core::server::Server;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::{Error, ErrorType};
use pingora_proxy::{http_proxy_service, ProxyHttp, Session};
use pingora_utils_core::{merge_conf, merge_opt, FromYaml, RequestFilter};
use serde::Deserialize;
use static_files_module::{StaticFilesHandler, StaticFilesOpt};
use structopt::StructOpt;

/// The application implementing the Pingora Proxy interface
struct StaticRootApp {
    handler: StaticFilesHandler,
    compression_level: Option<u32>,
}

impl StaticRootApp {
    /// Creates a new application instance with the given static files handler.
    fn new(handler: StaticFilesHandler, compression_level: Option<u32>) -> Self {
        Self {
            handler,
            compression_level,
        }
    }
}

/// Command line options of this application
#[derive(Debug, StructOpt)]
struct StaticRootAppOpt {
    /// Address and port to listen on, e.g. "127.0.0.1:8080". This command line flag can be
    /// specified multiple times.
    #[structopt(short, long)]
    listen: Option<Vec<String>>,

    /// Compression level to be used for dynamic compression (omit to disable compression).
    #[structopt(long)]
    compression_level: Option<u32>,
}

merge_opt! {
    /// Run a web server exposing a single directory with static content.
    ///
    /// This application is based on pingora-proxy and static-files-module.
    struct Opt {
        app: StaticRootAppOpt,
        server: ServerOpt,
        static_files: StaticFilesOpt,
    }
}

/// Application-specific configuration settings
#[derive(Debug, Deserialize)]
struct StaticRootAppConf {
    /// List of address/port combinations to listen on, e.g. "127.0.0.1:8080".
    listen: Vec<String>,

    /// Compression level to be used for dynamic compression (omit to disable compression).
    compression_level: Option<u32>,
}

impl Default for StaticRootAppConf {
    fn default() -> Self {
        Self {
            listen: vec!["127.0.0.1:8080".to_owned(), "[::1]:8080".to_owned()],
            compression_level: None,
        }
    }
}

merge_conf! {
    /// The combined configuration of Pingora server and [`StaticFilesHandler`].
    struct Conf {
        app: StaticRootAppConf,
        server: ServerConf,
        static_files: <StaticFilesHandler as RequestFilter>::Conf,
    }
}

#[async_trait]
impl ProxyHttp for StaticRootApp {
    type CTX = <StaticFilesHandler as RequestFilter>::CTX;

    fn new_ctx(&self) -> Self::CTX {
        StaticFilesHandler::new_ctx()
    }

    async fn request_filter(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<bool, Box<Error>> {
        if let Some(level) = self.compression_level {
            session.downstream_compression.adjust_level(level);
        }
        self.handler.handle(session, ctx).await
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>, Box<Error>> {
        Err(Error::new(ErrorType::HTTPStatus(404)))
    }
}

fn main() {
    env_logger::init();

    let opt = Opt::from_args();
    let conf = opt
        .server
        .conf
        .as_ref()
        .and_then(|path| match Conf::load_from_yaml(path) {
            Ok(conf) => Some(conf),
            Err(err) => {
                error!("{err}");
                None
            }
        })
        .unwrap_or_else(Conf::default);

    let mut server = Server::new_with_opt_and_conf(opt.server, conf.server);
    server.bootstrap();

    let mut static_files_conf = conf.static_files;
    static_files_conf.merge_with_opt(opt.static_files);
    let handler = match StaticFilesHandler::new(static_files_conf) {
        Ok(handler) => handler,
        Err(err) => {
            error!("{err}");
            return;
        }
    };
    let compression_level = opt.app.compression_level.or(conf.app.compression_level);

    let mut proxy = http_proxy_service(
        &server.configuration,
        StaticRootApp::new(handler, compression_level),
    );
    for addr in opt.app.listen.unwrap_or(conf.app.listen) {
        proxy.add_tcp(&addr);
    }
    server.add_service(proxy);

    server.run_forever();
}
