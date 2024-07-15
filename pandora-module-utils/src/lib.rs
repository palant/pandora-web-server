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

#![doc = include_str!("../README.md")]
#![allow(non_ascii_idents)]

mod deserialize;
#[doc(hidden)]
pub mod jar;
pub mod merger;
pub mod pingora;
pub mod router;
pub mod standard_response;
mod trie;

use log::{error, info, trace};
use pingora::{Error, ErrorType, HttpModules, HttpPeer, ResponseHeader, SessionWrapper};
use serde::{de::DeserializeSeed, Deserialize};
use std::fmt::Debug;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

pub use deserialize::{DeserializeMap, MapVisitor, OneOrMany, _private};
pub use pandora_module_utils_macros::{merge_conf, merge_opt, DeserializeMap, RequestFilter};

// Required for macros
#[doc(hidden)]
pub use async_trait;
#[doc(hidden)]
pub use clap;
#[doc(hidden)]
pub use serde;
#[doc(hidden)]
pub use serde_yaml;

/// Request filter result indicating how the current request should be processed further
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default, Deserialize)]
pub enum RequestFilterResult {
    /// Response has been sent, no further processing should happen. Other Pingora phases should
    /// not be triggered.
    ResponseSent,

    /// Request has been handled and further request filters should not run. Response hasn’t been
    /// sent however, next Pingora phase should deal with that.
    Handled,

    /// Request filter could not handle this request, next request filter should run if it exists.
    #[default]
    Unhandled,
}

/// Trait to be implemented by request filters.
#[async_trait::async_trait]
pub trait RequestFilter: Sized {
    /// Configuration type of this handler.
    type Conf;

    /// Creates a new instance of the handler from its configuration.
    fn new(conf: Self::Conf) -> Result<Self, Box<Error>>
    where
        Self: Sized,
        Self::Conf: TryInto<Self, Error = Box<Error>>,
    {
        conf.try_into()
    }

    /// Per-request state of this handler, see [`pingora::ProxyHttp::CTX`]
    type CTX;

    /// Creates a new state object, see [`pingora::ProxyHttp::new_ctx`]
    ///
    /// Unlike Pingora’s method, this one is static. This is to accommodate the virtual hosts
    /// scenario: the session isn’t available at this point, so it isn’t yet known which one of the
    /// possible host-specific handlers will run.
    fn new_ctx() -> Self::CTX;

    /// Handler to run during Pingora’s `init_downstream_modules` phase, see
    /// [`pingora::ProxyHttp::init_downstream_modules`].
    ///
    /// Unlike Pingora’s method, this one is static. This is to accomodate the virtual hosts
    /// scenario: the session isn’t available at this point, so it isn’t yet known which one of the
    /// possible host-specific handlers will run.
    fn init_downstream_modules(_modules: &mut HttpModules) {}

    /// Handler to run during Pingora’s `early_request_filter` phase, see
    /// [`pingora::ProxyHttp::early_request_filter`].
    async fn early_request_filter(
        &self,
        _session: &mut impl SessionWrapper,
        _ctx: &mut Self::CTX,
    ) -> Result<(), Box<Error>> {
        Ok(())
    }

    /// Handler to run during Pingora’s `request_filter` phase, see
    /// [`pingora::ProxyHttp::request_filter`]. This uses a different return type to account
    /// for the existence of multiple chained handlers.
    async fn request_filter(
        &self,
        _session: &mut impl SessionWrapper,
        _ctx: &mut Self::CTX,
    ) -> Result<RequestFilterResult, Box<Error>> {
        Ok(RequestFilterResult::Unhandled)
    }

    /// Handler to run during Pingora’s `upstream_peer` phase, see
    /// [`pingora::ProxyHttp::upstream_peer`]. Unlike Pingora’s method, here returning a result is
    /// optional. If `None` is returned, other handlers in the chain will be called. If all of them
    /// return `None`, an error will be returned to Pingora.
    async fn upstream_peer(
        &self,
        _session: &mut impl SessionWrapper,
        _ctx: &mut Self::CTX,
    ) -> Result<Option<Box<HttpPeer>>, Box<Error>> {
        Ok(None)
    }

    /// Called when a response header is about to be sent, either from a request filter or an
    /// upstream response.
    ///
    /// *Note*: A context will only be available for the latter call.
    fn response_filter(
        &self,
        _session: &mut impl SessionWrapper,
        _response: &mut ResponseHeader,
        _ctx: Option<&mut Self::CTX>,
    ) {
    }

    /// Handler to run during Pingora’s `logging` phase, see [`pingora::ProxyHttp::logging`].
    async fn logging(
        &self,
        _session: &mut impl SessionWrapper,
        _e: Option<&Error>,
        _ctx: &mut Self::CTX,
    ) {
    }
}

/// Trait for configuration structures that can be loaded from YAML files. This trait has a blanket
/// implementation for any structure implementing [`serde::Deserialize`].
pub trait FromYaml {
    /// Loads and merges configuration from a number of YAML files. Glob patterns in file names
    /// will be resolved and file names will be sorted before further processing.
    fn load_from_files<I>(files: I) -> Result<Self, Box<Error>>
    where
        Self: Sized,
        I: IntoIterator,
        I::Item: AsRef<str>;

    /// Loads configuration from a YAML file.
    fn load_from_yaml(path: impl AsRef<Path>) -> Result<Self, Box<Error>>
    where
        Self: Sized;

    /// Loads configuration from a YAML file, using existing data for missing fields.
    fn merge_load_from_yaml(self, path: impl AsRef<Path>) -> Result<Self, Box<Error>>
    where
        Self: Sized;

    /// Loads configuration from a YAML string.
    fn from_yaml(yaml_conf: impl AsRef<str>) -> Result<Self, Box<Error>>
    where
        Self: Sized;

    /// Loads configuration from a YAML string, using existing data for missing fields.
    fn merge_from_yaml(self, yaml_conf: impl AsRef<str>) -> Result<Self, Box<Error>>
    where
        Self: Sized;
}

impl<D> FromYaml for D
where
    D: Debug + Default,
    for<'de> D: DeserializeSeed<'de, Value = D>,
{
    fn load_from_files<I>(files: I) -> Result<Self, Box<Error>>
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
    {
        let mut files = files
            .into_iter()
            .filter_map(|path| match glob::glob(path.as_ref()) {
                Ok(iter) => Some(iter),
                Err(err) => {
                    error!("Ignoring invalid glob pattern `{}`: {err}", path.as_ref());
                    None
                }
            })
            .flatten()
            .filter_map(|path| match path {
                Ok(path) => Some(path),
                Err(err) => {
                    error!("Failed resolving glob pattern: {err}");
                    None
                }
            })
            .collect::<Vec<_>>();
        files.sort();
        files.into_iter().try_fold(Self::default(), |conf, path| {
            info!("Loading configuration file `{}`", path.display());
            conf.merge_load_from_yaml(path)
        })
    }

    fn load_from_yaml(path: impl AsRef<Path>) -> Result<Self, Box<Error>> {
        Self::default().merge_load_from_yaml(path)
    }

    fn merge_load_from_yaml(self, path: impl AsRef<Path>) -> Result<Self, Box<Error>> {
        let path = path.as_ref();
        let file = File::open(path).map_err(|err| {
            Error::because(
                ErrorType::FileOpenError,
                format!("failed opening configuration file `{}`", path.display()),
                err,
            )
        })?;
        let reader = BufReader::new(file);

        let conf = self
            .deserialize(serde_yaml::Deserializer::from_reader(reader))
            .map_err(|err| {
                Error::because(
                    ErrorType::FileReadError,
                    format!("failed reading configuration file `{}`", path.display()),
                    err,
                )
            })?;
        trace!("Loaded configuration file: {conf:#?}");

        Ok(conf)
    }

    fn from_yaml(yaml_conf: impl AsRef<str>) -> Result<Self, Box<Error>> {
        Self::default().merge_from_yaml(yaml_conf)
    }

    fn merge_from_yaml(self, yaml_conf: impl AsRef<str>) -> Result<Self, Box<Error>> {
        let conf = self
            .deserialize(serde_yaml::Deserializer::from_str(yaml_conf.as_ref()))
            .map_err(|err| {
                Error::because(ErrorType::ReadError, "failed reading configuration", err)
            })?;
        trace!("Loaded configuration: {conf:#?}");

        Ok(conf)
    }
}
