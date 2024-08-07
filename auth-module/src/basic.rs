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

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use http::{header, Method, StatusCode};
use log::{info, trace};
use maud::{html, DOCTYPE};
use pandora_module_utils::pingora::{Error, ResponseHeader, SessionWrapper};
use pandora_module_utils::standard_response::error_response;
use pandora_module_utils::RequestFilterResult;

use crate::{
    common::{is_rate_limited, validate_login},
    AuthConf,
};

async fn unauthorized_response(
    session: &mut impl SessionWrapper,
    realm: &str,
    suggestion: Option<String>,
) -> Result<(), Box<Error>> {
    let text = html! {
        (DOCTYPE)
        html {
            head {
                title {
                    "401 Unauthorized"
                }
            }

            body {
                center {
                    h1 {
                        "401 Unauthorized"
                    }
                }

                @if let Some(suggestion) = &suggestion {
                    p {
                        "If you are the administrator of this server, you might want to add the following to your configuration:"
                    }
                    pre {
                        (suggestion)
                    }
                }
            }
        }
    }.into_string();

    let mut header = ResponseHeader::build(StatusCode::UNAUTHORIZED, Some(3))?;
    header.append_header(header::CONTENT_LENGTH, text.len().to_string())?;
    header.append_header(header::CONTENT_TYPE, "text/html;charset=utf-8")?;
    header.append_header(header::WWW_AUTHENTICATE, format!("Basic realm=\"{realm}\""))?;

    let send_body = session.req_header().method != Method::HEAD;
    session
        .write_response_header(Box::new(header), !send_body)
        .await?;

    if send_body {
        session.write_response_body(Some(text.into()), true).await?;
    }

    Ok(())
}

pub(crate) async fn basic_auth(
    conf: &AuthConf,
    session: &mut impl SessionWrapper,
) -> Result<RequestFilterResult, Box<Error>> {
    let auth = match session.req_header().headers.get(header::AUTHORIZATION) {
        Some(auth) => auth,
        None => {
            trace!("Rejecting request, no Authorization header");
            unauthorized_response(session, &conf.auth_realm, None).await?;
            return Ok(RequestFilterResult::ResponseSent);
        }
    };

    let auth = match auth.to_str() {
        Ok(auth) => auth,
        Err(err) => {
            info!("Rejecting request, Authorization header cannot be converted to string: {err}");
            unauthorized_response(session, &conf.auth_realm, None).await?;
            return Ok(RequestFilterResult::ResponseSent);
        }
    };

    let (scheme, credentials) = auth.split_once(' ').unwrap_or(("", ""));
    if scheme != "Basic" {
        info!("Rejecting request, unsupported authorization scheme: {scheme}");
        unauthorized_response(session, &conf.auth_realm, None).await?;
        return Ok(RequestFilterResult::ResponseSent);
    }

    let credentials = match BASE64_STANDARD.decode(credentials) {
        Ok(credentials) => credentials,
        Err(err) => {
            info!("Rejecting request, failed decoding base64: {err}");
            unauthorized_response(session, &conf.auth_realm, None).await?;
            return Ok(RequestFilterResult::ResponseSent);
        }
    };

    // slice::split_once() is unstable
    let (user, password) = if let Some(index) = credentials.iter().position(|b| *b == b':') {
        (
            String::from_utf8(credentials[0..index].to_vec()).unwrap_or_default(),
            &credentials[index + 1..],
        )
    } else {
        ("".to_owned(), "".as_bytes())
    };

    if is_rate_limited(session, &conf.auth_rate_limits, &user) {
        error_response(session, StatusCode::TOO_MANY_REQUESTS).await?;
        return Ok(RequestFilterResult::ResponseSent);
    }

    let (valid, suggestion) = validate_login(conf, &user, password);
    if valid {
        session.set_remote_user(user);
        Ok(RequestFilterResult::Unhandled)
    } else {
        unauthorized_response(session, &conf.auth_realm, suggestion).await?;
        Ok(RequestFilterResult::ResponseSent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use pandora_module_utils::pingora::{create_test_session, ErrorType, RequestHeader, Session};
    use pandora_module_utils::standard_response::response_text;
    use pandora_module_utils::{FromYaml, RequestFilter};
    use startup_module::{AppResult, DefaultApp};
    use test_log::test;

    use crate::AuthHandler;

    fn default_conf() -> &'static str {
        r#"
auth_mode: http
auth_credentials:
    # test
    me: $2y$04$V15kxj8/a7JsIb6lXkcK7ex.IiNSM3.nbLJaLbkAi10iVXUip/JoC
    # test2
    another: $2y$04$s/KAIlzQM8VfPsf9.YKAGOfZhMp44lcXHLB9avFGnON3D1QKG9clS
auth_realm: "Protected area"
auth_rate_limits:
    total: 0
    per_ip: 0
    per_user: 0
        "#
    }

    fn make_app(conf: &str) -> DefaultApp<AuthHandler> {
        DefaultApp::new(
            <AuthHandler as RequestFilter>::Conf::from_yaml(conf)
                .unwrap()
                .try_into()
                .unwrap(),
        )
    }

    async fn make_session() -> Session {
        let header = RequestHeader::build("GET", b"/", None).unwrap();
        create_test_session(header).await
    }

    fn assert_headers(header: &ResponseHeader, expected: Vec<(&str, &str)>) {
        let mut headers: Vec<_> = header
            .headers
            .iter()
            .filter(|(name, _)| *name != header::CONNECTION && *name != header::DATE)
            .map(|(name, value)| {
                (
                    name.as_str().to_ascii_lowercase(),
                    value.to_str().unwrap().to_owned(),
                )
            })
            .collect();
        headers.sort();

        let mut expected: Vec<_> = expected
            .into_iter()
            .map(|(name, value)| (name.to_ascii_lowercase(), value.to_owned()))
            .collect();
        expected.sort();

        assert_eq!(headers, expected);
    }

    fn check_unauthorized_response(result: &mut AppResult) {
        let unauthorized_response = response_text(StatusCode::UNAUTHORIZED);
        assert_eq!(result.session().response_written().unwrap().status, 401);
        assert_headers(
            result.session().response_written().unwrap(),
            vec![
                ("Content-Type", "text/html;charset=utf-8"),
                ("Content-Length", &unauthorized_response.len().to_string()),
                ("WWW-Authenticate", "Basic realm=\"Protected area\""),
            ],
        );
        assert_eq!(result.body_str(), unauthorized_response);
    }

    #[test(tokio::test)]
    async fn unconfigured() {
        let mut app = make_app("auth_mode: http");
        let session = make_session().await;
        let mut result = app.handle_request(session).await;
        assert_eq!(
            result.err().as_ref().map(|err| &err.etype),
            Some(&ErrorType::HTTPStatus(404))
        );
        assert_eq!(result.session().remote_user(), None);
    }

    #[test(tokio::test)]
    async fn no_auth_header() {
        let mut app = make_app(default_conf());
        let session = make_session().await;
        let mut result = app.handle_request(session).await;
        assert!(result.err().is_none());
        assert_eq!(result.session().remote_user(), None);
        check_unauthorized_response(&mut result);
    }

    #[test(tokio::test)]
    async fn unknown_auth_scheme() {
        let mut app = make_app(default_conf());
        let mut session = make_session().await;
        session
            .req_header_mut()
            .insert_header("Authorization", "Unknown bWU6dGVzdA==")
            .unwrap();
        let mut result = app.handle_request(session).await;
        assert!(result.err().is_none());
        assert_eq!(result.session().remote_user(), None);
        check_unauthorized_response(&mut result);
    }

    #[test(tokio::test)]
    async fn missing_credentials() {
        let mut app = make_app(default_conf());
        let mut session = make_session().await;
        session
            .req_header_mut()
            .insert_header("Authorization", "Basic")
            .unwrap();
        let mut result = app.handle_request(session).await;
        assert!(result.err().is_none());
        assert_eq!(result.session().remote_user(), None);
        check_unauthorized_response(&mut result);
    }

    #[test(tokio::test)]
    async fn credentials_no_colon() {
        // Credentials without colon
        let mut app = make_app(default_conf());
        let mut session = make_session().await;
        session
            .req_header_mut()
            .insert_header("Authorization", "Basic bWV0ZXN0")
            .unwrap();
        let mut result = app.handle_request(session).await;
        assert!(result.err().is_none());
        assert_eq!(result.session().remote_user(), None);
        check_unauthorized_response(&mut result);
    }

    #[test(tokio::test)]
    async fn wrong_credentials() {
        let mut app = make_app(default_conf());
        let mut session = make_session().await;
        session
            .req_header_mut()
            .insert_header("Authorization", "Basic bWU6dGVzdDI=")
            .unwrap();
        let mut result = app.handle_request(session).await;
        assert!(result.err().is_none());
        assert_eq!(result.session().remote_user(), None);
        check_unauthorized_response(&mut result);
    }

    #[test(tokio::test)]
    async fn wrong_user_name() {
        let mut app = make_app(default_conf());
        let mut session = make_session().await;
        session
            .req_header_mut()
            .insert_header("Authorization", "Basic eW91OnRlc3Q=")
            .unwrap();
        let mut result = app.handle_request(session).await;
        assert!(result.err().is_none());
        assert_eq!(result.session().remote_user(), None);
        check_unauthorized_response(&mut result);
    }

    #[test(tokio::test)]
    async fn correct_credentials() {
        let mut app = make_app(default_conf());
        let mut session = make_session().await;
        session
            .req_header_mut()
            .insert_header("Authorization", "Basic bWU6dGVzdA==")
            .unwrap();
        let mut result = app.handle_request(session).await;
        assert_eq!(
            result.err().as_ref().map(|err| &err.etype),
            Some(&ErrorType::HTTPStatus(404))
        );
        assert_eq!(result.session().remote_user(), Some("me"));
    }

    #[test(tokio::test)]
    async fn display_hash() {
        let mut conf = default_conf().to_owned();
        conf.push_str("\nauth_display_hash: true");
        let mut app = make_app(&conf);
        let mut session = make_session().await;
        session
            .req_header_mut()
            .insert_header("Authorization", "Basic JzxtZT4nOnRlc3Q=")
            .unwrap();
        let mut result = app.handle_request(session).await;
        assert!(result.err().is_none());
        assert_eq!(result.session().remote_user(), None);
        assert!(result.body_str().contains("&quot;'&lt;me&gt;'&quot;: $2b$"));
    }

    #[test(tokio::test)]
    async fn rate_limiting() {
        let mut conf = default_conf().to_owned();
        conf.push_str(
            r#"
auth_rate_limits:
    total: 4
            "#,
        );
        let mut app = make_app(&conf);

        for _ in 0..4 {
            let mut session = make_session().await;
            session
                .req_header_mut()
                .insert_header("Authorization", "Basic bWU6dGVzdA==")
                .unwrap();
            app.handle_request(session).await;
        }

        let mut session = make_session().await;
        session
            .req_header_mut()
            .insert_header("Authorization", "Basic bWU6dGVzdA==")
            .unwrap();
        let mut result = app.handle_request(session).await;
        assert!(result.err().is_none());
        assert_eq!(result.session().remote_user(), None);
        assert_eq!(
            result.session().response_written().unwrap().status,
            StatusCode::TOO_MANY_REQUESTS
        );
    }
}
