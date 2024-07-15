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

mod derive_deserialize_map;
mod derive_request_filter;
mod merge_conf;
mod merge_opt;
#[cfg(test)]
mod tests;
mod utils;

use proc_macro::TokenStream;

/// This attribute macro merges the command-line arguments from all structs identified as field of
/// the current struct. The result will implement `clap::Parser` and `Debug` automatically.
/// All field types are required to implement `clap::Parser` and `Debug`.
///
/// ```rust
/// use pandora_module_utils::merge_opt;
/// use startup_module::StartupOpt;
/// use static_files_module::StaticFilesOpt;
/// use clap::Parser;
///
/// #[derive(Debug, Parser)]
/// struct MyAppOpt {
///     /// Use to make the server roll over
///     #[clap(long)]
///     roll_over: bool,
/// }
///
/// /// Starts my great application.
/// #[merge_opt]
/// struct Opt {
///     app: MyAppOpt,
///     startup: StartupOpt,
///     static_files: StaticFilesOpt,
/// }
///
/// let opt = Opt::parse();
/// println!("Application options: {:?}", opt.app);
/// println!("Startup module options: {:?}", opt.startup);
/// println!("Static files options: {:?}", opt.static_files);
/// ```
#[proc_macro_attribute]
pub fn merge_opt(_args: TokenStream, input: TokenStream) -> TokenStream {
    merge_opt::merge_opt(input).unwrap_or_else(|err| err.into_compile_error().into())
}

/// This attribute macro merges the configuration settings from all structs identified as field of
/// the current struct. It’s essentially a shortcut for deriving `Debug`, `Default` and
/// `DeserializeMap` traits, the latter with all fields flattened. All field types are required to
/// implement `Debug`, `Default` and `DeserializeMap`.
///
/// ```rust
/// use pandora_module_utils::{merge_conf, DeserializeMap, FromYaml};
/// use startup_module::StartupConf;
/// use static_files_module::StaticFilesConf;
/// use std::path::PathBuf;
///
/// #[derive(Debug, Default, Clone, PartialEq, Eq, DeserializeMap)]
/// struct MyAppConf {
///     /// If `true`, the server will roll over
///     roll_over: bool,
/// }
///
/// #[merge_conf]
/// struct Conf {
///     app: MyAppConf,
///     startup: StartupConf,
///     static_files: StaticFilesConf,
/// }
///
/// let conf = Conf::from_yaml(r#"
///     roll_over: true
///     listen: 127.0.0.1:8080
///     root: .
/// "#).unwrap();
/// assert!(conf.app.roll_over);
/// assert_eq!(conf.startup.listen, vec!["127.0.0.1:8080".into()].into());
/// assert_eq!(conf.static_files.root, Some(PathBuf::from(".")));
/// ```
///
/// Unknown fields will cause an error during deserialization:
///
/// ```rust
/// use compression_module::CompressionConf;
/// use pandora_module_utils::{merge_conf, FromYaml};
/// use static_files_module::StaticFilesConf;
///
/// #[merge_conf]
/// struct Conf {
///     compression: CompressionConf,
///     static_files: StaticFilesConf,
/// }
///
/// assert!(Conf::from_yaml(r#"
///     root: .
///     compression_level_gzip: 3
///     unknown_field: flagged
/// "#).is_err());
/// ```
#[proc_macro_attribute]
pub fn merge_conf(_attr: TokenStream, input: TokenStream) -> TokenStream {
    merge_conf::merge_conf(input).unwrap_or_else(|err| err.into_compile_error().into())
}

/// This macro will automatically implement `RequestFilter` by chaining the handlers identified
/// in the struct’s fields.
///
/// Each handler has to implement `RequestFilter` trait. The handlers will be called in the order
/// in which they are listed. Each handler can prevent the subsequent handlers from being called by
/// returning `RequestFilterResult::ResponseSent` or `RequestFilterResult::Handled`.
///
/// The configuration and context for the struct will be implemented implicitly. These will have
/// the configuration/context of the respective handler in a field with the same name as the
/// handler in this struct.
///
/// ```rust
/// use pandora_module_utils::{FromYaml, RequestFilter};
/// use compression_module::CompressionHandler;
/// use static_files_module::StaticFilesHandler;
///
/// #[derive(Debug, Clone, PartialEq, Eq, RequestFilter)]
/// struct Handler {
///     compression: CompressionHandler,
///     static_files: StaticFilesHandler,
/// }
///
/// type Conf = <Handler as RequestFilter>::Conf;
///
/// let conf = Conf::from_yaml(r#"
///     root: .
///     compression_level_gzip: 3
///     compression_level_brotli: 3
///     compression_level_zstd: 3
/// "#).unwrap();
/// let handler: Handler = conf.try_into().unwrap();
/// ```
///
/// As this derives `DeserializeMap` trait for configurations internally, unknown fields in
/// configuration will cause an error during deserialization:
///
/// ```rust
/// use pandora_module_utils::{FromYaml, RequestFilter};
/// use compression_module::CompressionHandler;
/// use static_files_module::StaticFilesHandler;
///
/// #[derive(Debug, Clone, PartialEq, Eq, RequestFilter)]
/// struct Handler {
///     compression: CompressionHandler,
///     static_files: StaticFilesHandler,
/// }
///
/// type Conf = <Handler as RequestFilter>::Conf;
///
/// assert!(Conf::from_yaml(r#"
///     root: .
///     compression_level_gzip: 3
///     unknown_field: flagged
/// "#).is_err());
/// ```
#[proc_macro_derive(RequestFilter)]
pub fn derive_request_filter(input: TokenStream) -> TokenStream {
    derive_request_filter::derive_request_filter(input)
        .unwrap_or_else(|err| err.into_compile_error().into())
}

/// This macro will automatically implement `DeserializeMap`, `serde::Deserialize` and
/// `serde::DeserializeSeed` traits for a structure.
///
/// Unlike Serde’s usual deserialization, this approach is optimized for configuration files. It
/// allows an efficient implementation of the `flatten` attribute without intermediate storage.
/// Unknown fields are flagged automatically, effectively implying `deny_unknown_fields` attribute
/// which Serde does not support in combination with `flatten`. Merging multiple configurations
/// into a single data structure on the fly is also supported.
///
/// The structure has to implement `Default` which will be used as initial value for
/// `serde::Deserialize`. Individual fields usually need to implement `serde::Deserialize`. The
/// following field attributes are supported, striving for compatibility with the corresponding
/// [Serde field attributes](https://serde.rs/field-attrs.html):
///
/// * `#[pandora(rename = "name")]` or `#[pandora(rename(deserialize = "name"))]`
///
///   Deserialize this field with the given name instead of its Rust name.
/// * `#[pandora(alias = "name")]`
///
///   Deserialize this field from the given name or from its Rust name. May be repeated to specify
///   multiple possible names for the same field.
/// * `#[pandora(flatten)]`
///
///   Flatten the contents of this field into the container it is defined in. This removes one
///   level of structure between the configuration file and the Rust data structure representation.
///
///   Unlike regular fields, flattened fields have to implement `DeserializeMap` trait.
/// * `#[pandora(skip)]` or `#[serde(skip_deserializing)]`
///
///   Skip this field when deserializing, always use the default value instead.
/// * `#[pandora(deserialize_with = "path")]`
///
///   Deserialize this field using a function that is different from its implementation of
///   `serde::Deserialize`. The given function must be callable as
///   `fn<'de, D>(D) -> Result<T, D::Error> where D: serde::Deserializer<'de>`, although it may
///   also be generic over `T`. Fields used with `deserialize_with` are not required to implement
///   `serde::Deserialize`.
/// * `#[pandora(deserialize_with_seed = "path")]`
///
///   This is similar to `deserialize_with` but meant for fields that support merging of values.
///   The function receives an additional parameter before the deserializer, the previous value of
///   this field. It can then proceed to deserialize the new value and to merge the two as desired.
/// * `#[serde(with = "module")]`
///
///   Same as `deserialize_with` but `$module::deserialize` will be used as the `deserialize_with`
///   function.
///
/// In addition, the following analogs of [Serde’s container
/// attributes](https://serde.rs/container-attrs.html) are currently supported:
///
/// * `#[pandora(rename_all = "convention")]` or
///   `#[pandora(rename_all(deserialize = "convention"))]`
///
///   Rename all the fields according to the given case convention. The possible values are
///   `"lowercase"`, `"UPPERCASE"`, `"PascalCase"`, `"camelCase"`, `"snake_case"`,
///   `"SCREAMING_SNAKE_CASE"`, `"kebab-case"`, `"SCREAMING-KEBAB-CASE"`. The field’s individual
///   `rename` attribute takes precedence.
/// * `#[pandora(crate = "path")]`
///
///   Specify a path to the `pandora_module_utils` crate instance to use when referring to APIs
///   from generated code. This is normally only applicable when `pandora_module_utils` isn’t
///   accessible under its usual name but only as a re-exported name from a different crate.
///
/// Unknown fields will cause a deserialization error, missing fields will be left at their initial
/// value. This is similar to the behavior of
/// [Serde container attributes](https://serde.rs/container-attrs.html)
/// `#[serde(deny_unknown_fields)]` and `#[serde(default)]`.
///
/// Example:
///
/// ```rust
/// use pandora_module_utils::{DeserializeMap, FromYaml, merge_conf};
///
/// #[derive(Debug, Default, Clone, PartialEq, Eq, DeserializeMap)]
/// struct Conf1 {
///     value1: u32,
/// }
///
/// #[derive(Debug, Default, Clone, PartialEq, Eq, DeserializeMap)]
/// struct Conf2 {
///     #[pandora(rename = "Value2")]
///     value2: String,
///     #[pandora(skip)]
///     value3: Option<bool>,
/// }
///
/// #[merge_conf]
/// struct Conf {
///     conf1: Conf1,
///     conf2: Conf2,
/// }
///
/// let conf = Conf::from_yaml(r#"
///     value1: 12
///     Value2: "Hi!"
/// "#).unwrap();
///
/// assert_eq!(conf.conf1.value1, 12);
/// assert_eq!(conf.conf2.value2, String::from("Hi!"));
/// assert!(conf.conf2.value3.is_none());
/// ```
#[proc_macro_derive(DeserializeMap, attributes(pandora))]
pub fn derive_deserialize_map(input: TokenStream) -> TokenStream {
    derive_deserialize_map::derive_deserialize_map(input)
        .unwrap_or_else(|err| err.into_compile_error().into())
}
