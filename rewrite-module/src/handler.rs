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

//! Handler for the `request_filter` phase.

use async_trait::async_trait;
use http::{HeaderValue, StatusCode};
use log::{debug, error, trace};
use pandora_module_utils::merger::Merger;
use pandora_module_utils::pingora::{Error, SessionWrapper};
use pandora_module_utils::router::{Path, Router};
use pandora_module_utils::standard_response::redirect_response;
use pandora_module_utils::{RequestFilter, RequestFilterResult};

use crate::configuration::{RegexMatch, RewriteConf, RewriteType, VariableInterpolation};

#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    from_regex: Option<RegexMatch>,
    query_regex: Option<RegexMatch>,
    to: VariableInterpolation,
    r#type: RewriteType,
}

/// Handler for Pingora’s `request_filter` phase
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewriteHandler {
    router: Router<Vec<(Path, Rule)>>,
}

impl TryFrom<RewriteConf> for RewriteHandler {
    type Error = Box<Error>;

    fn try_from(mut conf: RewriteConf) -> Result<Self, Self::Error> {
        debug!("Rewrite configuration received: {conf:#?}");

        let mut merger = Merger::new();

        // Add in reverse order, so that the first rule listed in configuration takes precedence.
        conf.rewrite_rules.reverse();

        // Sort by prefix so that exact rules get priority.
        conf.rewrite_rules.sort_by(|a, b| a.from.cmp(&b.from));

        for rule in conf.rewrite_rules {
            let path = rule.from.path.clone();
            let from = rule.from;
            let rule = Rule {
                from_regex: rule.from_regex,
                query_regex: rule.query_regex,
                to: rule.to,
                r#type: rule.r#type,
            };

            merger.push(from, (path, rule));
        }

        Ok(Self {
            router: merger.merge(|rules| rules.cloned().collect::<Vec<_>>()),
        })
    }
}

#[async_trait]
impl RequestFilter for RewriteHandler {
    type Conf = RewriteConf;

    type CTX = ();

    fn new_ctx() -> Self::CTX {}

    async fn request_filter(
        &self,
        session: &mut impl SessionWrapper,
        _ctx: &mut Self::CTX,
    ) -> Result<RequestFilterResult, Box<Error>> {
        let path = session.uri().path();
        trace!("Determining rewrite rules for path {path}");

        let list = if let Some(list) = self.router.lookup("", path) {
            list
        } else {
            trace!("No match for the path");
            return Ok(RequestFilterResult::Unhandled);
        };

        trace!("Applying rewrite rules: {list:?}");

        // Iterate in reverse order, merging puts rules in reverse order of precedence.
        for (rule_path, rule) in list.iter().rev() {
            if let Some(from_regex) = &rule.from_regex {
                if !from_regex.matches(session.uri().path()) {
                    continue;
                }
            }

            if let Some(query_regex) = &rule.query_regex {
                if !query_regex.matches(session.uri().query().unwrap_or("")) {
                    continue;
                }
            }

            let tail = rule_path
                .remove_prefix_from(path)
                .unwrap_or(path.as_bytes().to_owned());
            trace!(
                "Matched rule for path `{}`, tail is: {tail:?}",
                String::from_utf8_lossy(rule_path)
            );

            let target = rule.to.interpolate(|name| match name {
                "tail" => Some(&tail),
                "query" => Some(session.uri().query().unwrap_or("").as_bytes()),
                name => {
                    if let Some(name) = name.strip_prefix("http_") {
                        Some(
                            session
                                .req_header()
                                .headers
                                .get(name.replace('_', "-"))
                                .map(HeaderValue::as_bytes)
                                .unwrap_or(b""),
                        )
                    } else {
                        None
                    }
                }
            });

            match rule.r#type {
                RewriteType::Internal => {
                    let uri = match target.as_slice().try_into() {
                        Ok(uri) => uri,
                        Err(err) => {
                            error!("Could not parse {target:?} as URI: {err}");
                            return Ok(RequestFilterResult::Unhandled);
                        }
                    };
                    session.set_uri(uri);
                    break;
                }
                RewriteType::Redirect | RewriteType::Permanent => {
                    let location = match String::from_utf8(target) {
                        Ok(location) => location,
                        Err(err) => {
                            error!("Failed converting redirect target to UTF-8: {err}");
                            return Ok(RequestFilterResult::Unhandled);
                        }
                    };
                    let status = if rule.r#type == RewriteType::Redirect {
                        StatusCode::TEMPORARY_REDIRECT
                    } else {
                        StatusCode::PERMANENT_REDIRECT
                    };
                    redirect_response(session, status, &location).await?;
                    return Ok(RequestFilterResult::ResponseSent);
                }
            }
        }

        Ok(RequestFilterResult::Unhandled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use pandora_module_utils::pingora::{RequestHeader, TestSession};
    use pandora_module_utils::FromYaml;
    use test_log::test;

    fn make_handler(conf: &str) -> RewriteHandler {
        <RewriteHandler as RequestFilter>::Conf::from_yaml(conf)
            .unwrap()
            .try_into()
            .unwrap()
    }

    async fn make_session(path: &str) -> TestSession {
        let header = RequestHeader::build("GET", path.as_bytes(), None).unwrap();

        TestSession::from(header).await
    }

    #[test(tokio::test)]
    async fn internal_redirect() -> Result<(), Box<Error>> {
        let handler = make_handler(
            r#"
                rewrite_rules:
                    from: /path/*
                    to: /another${tail}
            "#,
        );

        let mut session = make_session("/").await;
        assert_eq!(
            handler
                .request_filter(&mut session, &mut RewriteHandler::new_ctx())
                .await?,
            RequestFilterResult::Unhandled
        );
        assert_eq!(session.uri(), "/");
        assert_eq!(session.original_uri(), "/");

        let mut session = make_session("/path").await;
        assert_eq!(
            handler
                .request_filter(&mut session, &mut RewriteHandler::new_ctx())
                .await?,
            RequestFilterResult::Unhandled
        );
        assert_eq!(session.uri(), "/another/");
        assert_eq!(session.original_uri(), "/path");

        let mut session = make_session("/path/").await;
        assert_eq!(
            handler
                .request_filter(&mut session, &mut RewriteHandler::new_ctx())
                .await?,
            RequestFilterResult::Unhandled
        );
        assert_eq!(session.uri(), "/another/");
        assert_eq!(session.original_uri(), "/path/");

        let mut session = make_session("/path/file.txt").await;
        assert_eq!(
            handler
                .request_filter(&mut session, &mut RewriteHandler::new_ctx())
                .await?,
            RequestFilterResult::Unhandled
        );
        assert_eq!(session.uri(), "/another/file.txt");
        assert_eq!(session.original_uri(), "/path/file.txt");

        Ok(())
    }

    #[test(tokio::test)]
    async fn conditions() -> Result<(), Box<Error>> {
        let handler = make_handler(
            r#"
                rewrite_rules:
                -
                    from: /path/*
                    from_regex: "\\.jpg$"
                    to: /another${tail}
                -
                    from: /path/image.jpg
                    query_regex: "query"
                    to: /nowhere
                -
                    from: /path/*
                    query_regex: "!^file="
                    to: /different?${query}
                -
                    from: /*
                    from_regex: ^/file\.txt$
                    query_regex: "!no_redirect"
                    to: /other.txt
            "#,
        );

        let mut session = make_session("/path/image.jpg").await;
        assert_eq!(
            handler
                .request_filter(&mut session, &mut RewriteHandler::new_ctx())
                .await?,
            RequestFilterResult::Unhandled
        );
        assert_eq!(session.uri(), "/another/image.jpg");

        let mut session = make_session("/path/?a=b").await;
        assert_eq!(
            handler
                .request_filter(&mut session, &mut RewriteHandler::new_ctx())
                .await?,
            RequestFilterResult::Unhandled
        );
        assert_eq!(session.uri(), "/different?a=b");

        let mut session = make_session("/path/image.png?a=b&file=c").await;
        assert_eq!(
            handler
                .request_filter(&mut session, &mut RewriteHandler::new_ctx())
                .await?,
            RequestFilterResult::Unhandled
        );
        assert_eq!(session.uri(), "/different?a=b&file=c");

        let mut session = make_session("/path/image.png?file=c").await;
        assert_eq!(
            handler
                .request_filter(&mut session, &mut RewriteHandler::new_ctx())
                .await?,
            RequestFilterResult::Unhandled
        );
        assert_eq!(session.uri(), "/path/image.png?file=c");

        let mut session = make_session("/file.txt").await;
        assert_eq!(
            handler
                .request_filter(&mut session, &mut RewriteHandler::new_ctx())
                .await?,
            RequestFilterResult::Unhandled
        );
        assert_eq!(session.uri(), "/other.txt");

        let mut session = make_session("/file.txt?no_redirect").await;
        assert_eq!(
            handler
                .request_filter(&mut session, &mut RewriteHandler::new_ctx())
                .await?,
            RequestFilterResult::Unhandled
        );
        assert_eq!(session.uri(), "/file.txt?no_redirect");

        Ok(())
    }

    #[test(tokio::test)]
    async fn interpolation() -> Result<(), Box<Error>> {
        let handler = make_handler(
            r#"
                rewrite_rules:
                    from: /path/*
                    to: /another${tail}?${query}&host=${http_host}&test=${http_test_header}
            "#,
        );

        let mut session = make_session("/path/file.txt").await;
        assert_eq!(
            handler
                .request_filter(&mut session, &mut RewriteHandler::new_ctx())
                .await?,
            RequestFilterResult::Unhandled
        );
        assert_eq!(session.uri(), "/another/file.txt?&host=&test=");

        let mut session = make_session("/path/file.txt?a=b").await;
        assert_eq!(
            handler
                .request_filter(&mut session, &mut RewriteHandler::new_ctx())
                .await?,
            RequestFilterResult::Unhandled
        );
        assert_eq!(session.uri(), "/another/file.txt?a=b&host=&test=");

        let mut session = make_session("/path/file.txt?a=b").await;
        session
            .req_header_mut()
            .insert_header("Host", "localhost")?;
        session
            .req_header_mut()
            .insert_header("Test-Header", "successful")?;
        assert_eq!(
            handler
                .request_filter(&mut session, &mut RewriteHandler::new_ctx())
                .await?,
            RequestFilterResult::Unhandled
        );
        assert_eq!(
            session.uri(),
            "/another/file.txt?a=b&host=localhost&test=successful"
        );

        Ok(())
    }

    #[test(tokio::test)]
    async fn external_redirect() -> Result<(), Box<Error>> {
        let handler = make_handler(
            r#"
                rewrite_rules:
                -
                    from: /path/*
                    to: /another${tail}
                    type: permanent
                -
                    from: /file.txt
                    to: https://example.com/?${query}
                    type: redirect
            "#,
        );

        let mut session = make_session("/path/file.txt").await;
        assert_eq!(
            handler
                .request_filter(&mut session, &mut RewriteHandler::new_ctx())
                .await?,
            RequestFilterResult::ResponseSent
        );
        assert_eq!(
            session.response_written().map(|r| r.status),
            Some(StatusCode::PERMANENT_REDIRECT)
        );
        assert_eq!(
            session
                .response_written()
                .and_then(|r| r.headers.get("Location"))
                .map(|h| h.to_str().unwrap()),
            Some("/another/file.txt")
        );

        let mut session = make_session("/file.txt?a=b").await;
        assert_eq!(
            handler
                .request_filter(&mut session, &mut RewriteHandler::new_ctx())
                .await?,
            RequestFilterResult::ResponseSent
        );
        assert_eq!(
            session.response_written().map(|r| r.status),
            Some(StatusCode::TEMPORARY_REDIRECT)
        );
        assert_eq!(
            session
                .response_written()
                .and_then(|r| r.headers.get("Location"))
                .map(|h| h.to_str().unwrap()),
            Some("https://example.com/?a=b")
        );

        Ok(())
    }

    #[test(tokio::test)]
    async fn rule_order() -> Result<(), Box<Error>> {
        let handler = make_handler(
            r#"
                rewrite_rules:
                -
                    from: /*
                    query_regex: "1"
                    to: /1
                -
                    from: /path/*
                    query_regex: "2"
                    to: /2
                -
                    from: /path/*
                    query_regex: "3"
                    to: /3
                -
                    from: /path
                    query_regex: "4"
                    to: /4
                -
                    from: /path
                    query_regex: "5"
                    to: /5
            "#,
        );

        let mut session = make_session("/path?12345").await;
        assert_eq!(
            handler
                .request_filter(&mut session, &mut RewriteHandler::new_ctx())
                .await?,
            RequestFilterResult::Unhandled
        );
        assert_eq!(session.uri(), "/4");

        let mut session = make_session("/path?1235").await;
        assert_eq!(
            handler
                .request_filter(&mut session, &mut RewriteHandler::new_ctx())
                .await?,
            RequestFilterResult::Unhandled
        );
        assert_eq!(session.uri(), "/5");

        let mut session = make_session("/path?123").await;
        assert_eq!(
            handler
                .request_filter(&mut session, &mut RewriteHandler::new_ctx())
                .await?,
            RequestFilterResult::Unhandled
        );
        assert_eq!(session.uri(), "/2");

        let mut session = make_session("/path?13").await;
        assert_eq!(
            handler
                .request_filter(&mut session, &mut RewriteHandler::new_ctx())
                .await?,
            RequestFilterResult::Unhandled
        );
        assert_eq!(session.uri(), "/3");

        let mut session = make_session("/path?1").await;
        assert_eq!(
            handler
                .request_filter(&mut session, &mut RewriteHandler::new_ctx())
                .await?,
            RequestFilterResult::Unhandled
        );
        assert_eq!(session.uri(), "/1");

        Ok(())
    }
}
