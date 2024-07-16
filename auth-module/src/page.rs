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

use bytes::BytesMut;
use hmac::{Hmac, Mac};
use http::{header, Method, StatusCode};
use jwt::{SignWithKey, VerifyWithKey};
use log::{error, trace, warn};
use maud::{html, DOCTYPE};
use pandora_module_utils::pingora::{Error, ErrorType, ResponseHeader, SessionWrapper};
use pandora_module_utils::standard_response::{error_response, redirect_response_with_cookie};
use pandora_module_utils::RequestFilterResult;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::time::{Duration, SystemTime};

use crate::common::{is_rate_limited, validate_login};
use crate::AuthConf;

#[derive(Debug, Deserialize)]
struct AuthRequest {
    username: String,
    password: String,
    r#type: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct JwtClaim {
    sub: String,
    iat: i64,
}

async fn login_response(
    session: &mut impl SessionWrapper,
    conf: &AuthConf,
    login_failure: bool,
    suggestion: Option<String>,
) -> Result<RequestFilterResult, Box<Error>> {
    if let Some(login_page) = &conf.auth_page_session.login_page {
        session.set_uri(login_page.clone());
        if session.req_header().method != Method::HEAD {
            session.req_header_mut().set_method(Method::GET);
        }
        return Ok(RequestFilterResult::Unhandled);
    }

    let strings = &conf.auth_page_strings;
    let text = html! {
        (DOCTYPE)
        html {
            head {
                title {
                    (strings.title)
                }
                meta name="viewport" content="width=device-width, initial-scale=1.0";
                style {
                    "body{width:40%;margin:0 auto;padding:.5em;background-color:#fff;color:#000;}"
                    "@media(max-width:75em){body{width:30em;}}"
                    "@media(max-width:30em){body{width:100%;}}"
                    "@media(prefers-color-scheme:dark){body{background-color:#0d1117;color:#e6edf3;}}"
                    "*{box-sizing:border-box;}"
                    "input{width:100%;}"
                    ".error{color:#f00}"
                }
            }
            body {
                h1 {
                    (strings.heading)
                }
                @if login_failure {
                    p class="error" {
                        (strings.error)
                    }
                }
                @if let Some(suggestion) = suggestion {
                    p {
                        "If you are the administrator of this server, you might want to add the following to your configuration:"
                    }
                    pre {
                        (suggestion)
                    }
                }
                form method="POST" {
                    p {
                        (strings.username_label)
                        br;
                        input name="username" autofocus;
                    }
                    p {
                        (strings.password_label)
                        br;
                        input name="password" type="password";
                    }
                    p {
                        button type="submit" {
                            (strings.button_text)
                        }
                    }
                }
            }
        }
    }.into_string();

    let mut header = ResponseHeader::build(StatusCode::OK, Some(3))?;
    header.append_header(header::CONTENT_LENGTH, text.len().to_string())?;
    header.append_header(header::CONTENT_TYPE, "text/html;charset=utf-8")?;
    header.append_header(header::CACHE_CONTROL, "no-store")?;

    let send_body = session.req_header().method != Method::HEAD;
    session
        .write_response_header(Box::new(header), !send_body)
        .await?;

    if send_body {
        session.write_response_body(Some(text.into()), true).await?;
    }

    Ok(RequestFilterResult::ResponseSent)
}

async fn login_response_json(
    session: &mut impl SessionWrapper,
    suggestion: Option<String>,
    cookie: Option<String>,
) -> Result<RequestFilterResult, Box<Error>> {
    let mut text = String::from("{");
    if cookie.is_some() {
        text.push_str("\"success\":true");
    } else {
        text.push_str("\"success\":false");
    }
    if let Some(suggestion) = suggestion {
        text.push_str(&format!(
            ",\"suggestion\":\"{}\"",
            // String::escape_default() almost matches JSON escaping, it merely escapes single
            // quotation marks unnecessarily.
            suggestion
                .escape_default()
                .collect::<String>()
                .replace("\\'", "'")
        ));
    }
    text.push('}');

    let mut header = ResponseHeader::build(StatusCode::OK, Some(3))?;
    header.append_header(header::CONTENT_LENGTH, text.len().to_string())?;
    header.append_header(header::CONTENT_TYPE, "application/json;charset=utf-8")?;
    if let Some(cookie) = cookie {
        header.append_header(header::SET_COOKIE, cookie)?;
    }

    let send_body = session.req_header().method != Method::HEAD;
    session
        .write_response_header(Box::new(header), !send_body)
        .await?;

    if send_body {
        session.write_response_body(Some(text.into()), true).await?;
    }

    Ok(RequestFilterResult::ResponseSent)
}

fn to_unix_timestamp(time: SystemTime) -> i64 {
    match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(duration) => duration.as_secs() as i64,
        Err(err) => -(err.duration().as_secs() as i64),
    }
}

fn from_unix_timestamp(timestamp: i64) -> SystemTime {
    if timestamp > 0 {
        SystemTime::UNIX_EPOCH + Duration::new(timestamp as u64, 0)
    } else {
        SystemTime::UNIX_EPOCH - Duration::new((-timestamp) as u64, 0)
    }
}

pub(crate) async fn page_auth(
    conf: &AuthConf,
    session: &mut impl SessionWrapper,
) -> Result<RequestFilterResult, Box<Error>> {
    let key = if let Some(secret) = &conf.auth_page_session.token_secret {
        Hmac::<Sha256>::new_from_slice(secret).map_err(|err| {
            Error::because(ErrorType::InternalError, "failed creating HMAC key", err)
        })?
    } else {
        error!("Unexpected: page_auth entered without a secret token, rejecting request");
        return Err(Error::explain(
            ErrorType::InternalError,
            "cannot proceed without a secret token",
        ));
    };

    for value in session.req_header().headers.get_all(header::COOKIE) {
        let value = value.to_str().unwrap_or("");
        for pair in value.split(';') {
            if let Some((name, value)) = pair.split_once('=') {
                if name.trim() == conf.auth_page_session.cookie_name {
                    let claim: JwtClaim = match value.trim().verify_with_key(&key) {
                        Ok(claim) => claim,
                        Err(_) => continue,
                    };

                    let now = SystemTime::now();
                    let issued_at = from_unix_timestamp(claim.iat);
                    if now >= issued_at
                        && now < issued_at + conf.auth_page_session.session_expiration
                    {
                        trace!("Found cookie with valid JWT token, allowing request");
                        session.set_remote_user(claim.sub);
                        return Ok(RequestFilterResult::Unhandled);
                    }
                }
            }
        }
    }
    trace!("Found no valid JWT token in cookies, trying to authorize request");

    if session.req_header().method != Method::POST {
        trace!("Requiring login, not a POST request");
        return login_response(session, conf, false, None).await;
    }

    let content_type = session
        .req_header()
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .map(|h| h.split_once(';').map_or(h, |(h, _)| h))
        .map(str::trim)
        .unwrap_or_default();
    if content_type != "application/x-www-form-urlencoded" {
        trace!("Requiring login, MIME type is not application/x-www-form-urlencoded");
        return login_response(session, conf, false, None).await;
    }

    const MAX_BODY_SIZE: usize = 4096;
    let mut data = BytesMut::with_capacity(MAX_BODY_SIZE);
    loop {
        match session.read_request_body().await {
            Ok(None) => break,
            Ok(Some(bytes)) => {
                if data.len() >= MAX_BODY_SIZE {
                    trace!("Requiring login, request body too long");
                    return login_response(session, conf, false, None).await;
                }

                data.extend(std::iter::once(bytes));
            }
            Err(err) => {
                warn!("Failed reading request body, requiring login: {err}");
                return login_response(session, conf, false, None).await;
            }
        }
    }

    let request: AuthRequest = match serde_urlencoded::from_bytes(&data) {
        Ok(request) => request,
        Err(err) => {
            warn!("Failed reading auth request, requiring login: {err}");
            return login_response(session, conf, false, None).await;
        }
    };

    if is_rate_limited(session, &conf.auth_rate_limits, &request.username) {
        error_response(session, StatusCode::TOO_MANY_REQUESTS).await?;
        return Ok(RequestFilterResult::ResponseSent);
    }

    let (valid, suggestion) = validate_login(conf, &request.username, request.password.as_bytes());
    if !valid {
        return if request.r#type.is_some_and(|t| t == "json") {
            login_response_json(session, suggestion, None).await
        } else {
            login_response(session, conf, true, suggestion).await
        };
    }

    session.set_remote_user(request.username.clone());

    let claim = JwtClaim {
        sub: request.username,
        iat: to_unix_timestamp(SystemTime::now()),
    };
    let token = claim
        .sign_with_key(&key)
        .map_err(|err| Error::because(ErrorType::InternalError, "failed signing JTW token", err))?;

    let secure = conf.auth_page_session.secure_cookie.unwrap_or_else(|| {
        session
            .digest()
            .and_then(|digest| digest.ssl_digest.as_ref())
            .is_some()
    });

    let cookie = format!(
        "{}={token}; Max-Age={}; HttpOnly{}",
        conf.auth_page_session.cookie_name,
        conf.auth_page_session.session_expiration.as_secs(),
        if secure { "; Secure" } else { "" }
    );

    if request.r#type.is_some_and(|t| t == "json") {
        login_response_json(session, None, Some(cookie)).await?;
    } else {
        let redirect_target = session
            .original_uri()
            .path_and_query()
            .map(|path| path.as_str())
            .unwrap_or("/")
            .to_owned();
        trace!("Login successful, redirecting to {}", redirect_target);

        redirect_response_with_cookie(session, StatusCode::FOUND, &redirect_target, &cookie)
            .await?;
    };

    Ok(RequestFilterResult::ResponseSent)
}

#[cfg(test)]
mod tests {
    use super::*;

    use pandora_module_utils::pingora::{
        create_test_session, create_test_session_with_body, RequestHeader, Session,
    };
    use pandora_module_utils::{FromYaml, RequestFilter};
    use rewrite_module::RewriteHandler;
    use startup_module::{AppResult, DefaultApp};
    use test_log::test;

    use crate::AuthHandler;

    fn default_conf() -> &'static str {
        r#"
auth_mode: page
auth_credentials:
    # test
    me: $2y$04$V15kxj8/a7JsIb6lXkcK7ex.IiNSM3.nbLJaLbkAi10iVXUip/JoC
    # test2
    another: $2y$04$s/KAIlzQM8VfPsf9.YKAGOfZhMp44lcXHLB9avFGnON3D1QKG9clS
auth_page_strings:
    title: "%%title%%"
    heading: "%%heading%%"
    error: "%%error%%"
    username_label: "%%username_label%%"
    password_label: "%%password_label%%"
    button_text: "%%button_text%%"
auth_rate_limits:
    total: 0
    per_ip: 0
    per_user: 0
auth_page_session:
    token_secret: abcd
    cookie_name: auth_cookie
    session_expiration: 200000d
        "#
    }

    #[derive(Debug, RequestFilter)]
    struct Handler {
        rewrite: RewriteHandler,
        auth: AuthHandler,
    }

    fn make_app(conf: &str) -> DefaultApp<Handler> {
        DefaultApp::new(
            <Handler as RequestFilter>::Conf::from_yaml(conf)
                .unwrap()
                .try_into()
                .unwrap(),
        )
    }

    async fn make_session(path: &str) -> Session {
        let header = RequestHeader::build("GET", path.as_bytes(), None).unwrap();
        create_test_session(header).await
    }

    async fn make_session_with_body(path: &str, body: &str) -> Session {
        let header = RequestHeader::build("POST", path.as_bytes(), None).unwrap();
        create_test_session_with_body(header, body).await
    }

    fn check_login_page_response(
        result: &mut AppResult,
        expect_error: bool,
        expect_suggestion: bool,
    ) {
        {
            let session = result.session();
            assert_eq!(session.response_written().unwrap().status, 200);
            assert_eq!(
                session
                    .response_written()
                    .unwrap()
                    .headers
                    .get("Content-Type")
                    .unwrap()
                    .to_str()
                    .unwrap(),
                "text/html;charset=utf-8"
            );
        }

        let body = result.body_str();
        assert!(body.contains("%%title%%"));
        assert!(body.contains("%%heading%%"));
        assert_eq!(body.contains("%%error%%"), expect_error);
        assert_eq!(
            body.contains("&quot;'&lt;me&gt;'&quot;: $2b$"),
            expect_suggestion
        );
        assert!(body.contains("%%username_label%%"));
        assert!(body.contains("%%password_label%%"));
        assert!(body.contains("%%button_text%%"));
    }

    fn check_json_response(result: &mut AppResult, expect_error: bool, expect_suggestion: bool) {
        {
            let session = result.session();
            assert_eq!(session.response_written().unwrap().status, 200);
            assert_eq!(
                session
                    .response_written()
                    .unwrap()
                    .headers
                    .get("Content-Type")
                    .unwrap()
                    .to_str()
                    .unwrap(),
                "application/json;charset=utf-8"
            );
        }

        #[derive(Deserialize)]
        struct JsonResponse {
            success: bool,
            suggestion: Option<String>,
        }

        let response: JsonResponse = serde_json::from_slice(result.body()).unwrap();
        assert_eq!(response.success, !expect_error);
        assert_eq!(response.suggestion.is_some(), expect_suggestion);
    }

    #[test(tokio::test)]
    async fn unconfigured() {
        let mut app = make_app("auth_mode: page");
        let session = make_session("/").await;
        let mut result = app.handle_request(session).await;
        assert_eq!(
            result.err().as_ref().map(|err| &err.etype),
            Some(&ErrorType::HTTPStatus(404))
        );
        assert_eq!(result.session().remote_user(), None);
    }

    #[test(tokio::test)]
    async fn no_cookies() {
        let mut app = make_app(default_conf());
        let session = make_session("/").await;
        let mut result = app.handle_request(session).await;
        assert!(result.err().is_none());
        assert_eq!(result.session().remote_user(), None);
        check_login_page_response(&mut result, false, false);
    }

    #[test(tokio::test)]
    async fn unknown_cookie() {
        let mut app = make_app(default_conf());
        let mut session = make_session("/").await;
        session
            .req_header_mut()
            .insert_header("Cookie", "auth_cookie2=eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJtZSIsImlhdCI6MTIzNDV9.oo4uMH-cKddfcmh14kEyXGDUeWObNEXht3lBymUjWlw").unwrap();
        let mut result = app.handle_request(session).await;
        assert!(result.err().is_none());
        assert_eq!(result.session().remote_user(), None);
        check_login_page_response(&mut result, false, false);
    }

    #[test(tokio::test)]
    async fn cookie_invalid_token() {
        let mut app = make_app(default_conf());
        let mut session = make_session("/").await;
        session
            .req_header_mut()
            .insert_header("Cookie", "auth_cookie=fyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJtZSIsImlhdCI6MTIzNDV9.oo4uMH-cKddfcmh14kEyXGDUeWObNEXht3lBymUjWlw").unwrap();
        let mut result = app.handle_request(session).await;
        assert!(result.err().is_none());
        assert_eq!(result.session().remote_user(), None);
        check_login_page_response(&mut result, false, false);
    }

    #[test(tokio::test)]
    async fn cookie_invalid_signature() {
        let mut app = make_app(default_conf());
        let mut session = make_session("/").await;
        session
            .req_header_mut()
            .insert_header("Cookie", "auth_cookie=eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJtZSIsImlhdCI6MTIzNDV9.oo4uMH-cKddfcmh14kEyXGDUeWObNEXht3lBymUjWlv").unwrap();
        let mut result = app.handle_request(session).await;
        assert!(result.err().is_none());
        assert_eq!(result.session().remote_user(), None);
        check_login_page_response(&mut result, false, false);
    }

    #[test(tokio::test)]
    async fn cookie_expired_token() {
        let conf = default_conf().replace("200000d", "2h");
        let mut app = make_app(&conf);
        let mut session = make_session("/").await;
        session
            .req_header_mut()
            .insert_header("Cookie", "auth_cookie=eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJtZSIsImlhdCI6MTIzNDV9.oo4uMH-cKddfcmh14kEyXGDUeWObNEXht3lBymUjWlw").unwrap();
        let mut result = app.handle_request(session).await;
        assert!(result.err().is_none());
        assert_eq!(result.session().remote_user(), None);
        check_login_page_response(&mut result, false, false);
    }

    #[test(tokio::test)]
    async fn cookie_issued_in_future() {
        let mut app = make_app(default_conf());
        let mut session = make_session("/").await;
        session
            .req_header_mut()
            .insert_header("Cookie", "auth_cookie=eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJtZSIsImlhdCI6OTk5OTk5OTk5OX0.rHg--l9K83j5LUResMAa4lutm5Gz9jAk5zvWZAEARdM").unwrap();
        let mut result = app.handle_request(session).await;
        assert!(result.err().is_none());
        assert_eq!(result.session().remote_user(), None);
        check_login_page_response(&mut result, false, false);
    }

    #[test(tokio::test)]
    async fn valid_cookie() {
        let mut app = make_app(default_conf());
        let mut session = make_session("/").await;
        session
            .req_header_mut()
            .insert_header("Cookie", "auth_cookie=eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJtZSIsImlhdCI6MTIzNDV9.oo4uMH-cKddfcmh14kEyXGDUeWObNEXht3lBymUjWlw").unwrap();
        let mut result = app.handle_request(session).await;
        assert_eq!(
            result.err().as_ref().map(|err| &err.etype),
            Some(&ErrorType::HTTPStatus(404))
        );
        assert_eq!(result.session().remote_user(), Some("me"));
    }

    #[test(tokio::test)]
    async fn multiple_cookies() {
        let mut app = make_app(default_conf());
        let mut session = make_session("/").await;
        session
            .req_header_mut()
            .insert_header("Cookie", "auth=abcd; auth_cookie=eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJtZSIsImlhdCI6MTIzNDV9.oo4uMH-cKddfcmh14kEyXGDUeWObNEXht3lBymUjWlw; another=dcba").unwrap();
        let mut result = app.handle_request(session).await;
        assert_eq!(
            result.err().as_ref().map(|err| &err.etype),
            Some(&ErrorType::HTTPStatus(404))
        );
        assert_eq!(result.session().remote_user(), Some("me"));
    }

    #[test(tokio::test)]
    async fn post_without_body() {
        let mut app = make_app(default_conf());
        let mut session = make_session_with_body("/", "").await;
        session
            .req_header_mut()
            .insert_header("Content-Type", "application/x-www-form-urlencoded")
            .unwrap();
        session.req_header_mut().set_method(Method::POST);
        let mut result = app.handle_request(session).await;
        assert!(result.err().is_none());
        assert_eq!(result.session().remote_user(), None);
        check_login_page_response(&mut result, false, false);
    }

    #[test(tokio::test)]
    async fn wrong_content_type() {
        let mut app = make_app(default_conf());
        let mut session = make_session_with_body("/", "username=me&password=test").await;
        session
            .req_header_mut()
            .insert_header("Content-Type", "multipart/form-data")
            .unwrap();
        session.req_header_mut().set_method(Method::POST);
        let mut result = app.handle_request(session).await;
        assert!(result.err().is_none());
        assert_eq!(result.session().remote_user(), None);
        check_login_page_response(&mut result, false, false);
    }

    #[test(tokio::test)]
    async fn wrong_user_name() {
        let mut app = make_app(default_conf());
        let mut session = make_session_with_body("/", "username=notme&password=test").await;
        session
            .req_header_mut()
            .insert_header("Content-Type", "application/x-www-form-urlencoded")
            .unwrap();
        session.req_header_mut().set_method(Method::POST);
        let mut result = app.handle_request(session).await;
        assert!(result.err().is_none());
        assert_eq!(result.session().remote_user(), None);
        check_login_page_response(&mut result, true, false);
    }

    #[test(tokio::test)]
    async fn wrong_user_name_json() {
        let mut app = make_app(default_conf());
        let mut session =
            make_session_with_body("/", "username=notme&password=test&type=json").await;
        session
            .req_header_mut()
            .insert_header("Content-Type", "application/x-www-form-urlencoded")
            .unwrap();
        session.req_header_mut().set_method(Method::POST);
        let mut result = app.handle_request(session).await;
        assert!(result.err().is_none());
        assert_eq!(result.session().remote_user(), None);
        check_json_response(&mut result, true, false);
    }

    #[test(tokio::test)]
    async fn wrong_password() {
        let mut app = make_app(default_conf());
        let mut session = make_session_with_body("/", "username=me&password=nottest").await;
        session
            .req_header_mut()
            .insert_header("Content-Type", "application/x-www-form-urlencoded")
            .unwrap();
        session.req_header_mut().set_method(Method::POST);
        let mut result = app.handle_request(session).await;
        assert!(result.err().is_none());
        assert_eq!(result.session().remote_user(), None);
        check_login_page_response(&mut result, true, false);
    }

    #[test(tokio::test)]
    async fn wrong_password_json() {
        let mut app = make_app(default_conf());
        let mut session =
            make_session_with_body("/", "username=me&password=nottest&type=json").await;
        session
            .req_header_mut()
            .insert_header("Content-Type", "application/x-www-form-urlencoded")
            .unwrap();
        session.req_header_mut().set_method(Method::POST);
        let mut result = app.handle_request(session).await;
        assert!(result.err().is_none());
        assert_eq!(result.session().remote_user(), None);
        check_json_response(&mut result, true, false);
    }

    #[test(tokio::test)]
    async fn correct_credentials() {
        let mut app = make_app(default_conf());
        let mut session = make_session_with_body("/", "username=me&password=test").await;
        session
            .req_header_mut()
            .insert_header("Content-Type", "application/x-www-form-urlencoded")
            .unwrap();
        session.req_header_mut().set_method(Method::POST);
        let mut result = app.handle_request(session).await;
        assert!(result.err().is_none());
        assert_eq!(result.session().remote_user(), Some("me"));

        let session = result.session();
        let response = session.response_written().unwrap();
        assert_eq!(response.status, 302);
        assert_eq!(response.headers.get("Location").unwrap(), "/");

        let cookie = response
            .headers
            .get("Set-Cookie")
            .unwrap()
            .to_str()
            .unwrap();
        let mut token = None;
        let mut exp = None;
        let mut http_only = false;
        let mut secure = false;
        for param in cookie.split(';') {
            let param = param.trim();
            if param.to_ascii_lowercase() == "httponly" {
                http_only = true;
            } else if param.to_ascii_lowercase() == "secure" {
                secure = true;
            } else {
                let (param, value) = param.split_once('=').unwrap();
                match param.to_ascii_lowercase().as_str() {
                    "auth_cookie" => token = Some(value.to_owned()),
                    "max-age" => exp = Some(value.parse::<u64>().unwrap()),
                    other => panic!("unexpected cookie parameter {other}"),
                }
            }
        }
        assert_eq!(exp, Some(200000 * 24 * 60 * 60));
        assert!(http_only);
        assert!(!secure);

        if let Some(token) = token {
            // Test whether this cookie is valid
            let mut session = make_session("/").await;
            session
                .req_header_mut()
                .insert_header("Cookie", format!("auth_cookie={token}"))
                .unwrap();
            let mut result = app.handle_request(session).await;
            assert_eq!(
                result.err().as_ref().map(|err| &err.etype),
                Some(&ErrorType::HTTPStatus(404))
            );
            assert_eq!(result.session().remote_user(), Some("me"));
        } else {
            panic!("auth_cookie cookie wasn't set")
        }
    }

    #[test(tokio::test)]
    async fn correct_credentials_json() {
        let mut app = make_app(default_conf());
        let mut session = make_session_with_body("/", "username=me&password=test&type=json").await;
        session
            .req_header_mut()
            .insert_header("Content-Type", "application/x-www-form-urlencoded")
            .unwrap();
        session.req_header_mut().set_method(Method::POST);
        let mut result = app.handle_request(session).await;
        assert!(result.err().is_none());
        assert_eq!(result.session().remote_user(), Some("me"));

        check_json_response(&mut result, false, false);

        assert!(result
            .session()
            .response_written()
            .unwrap()
            .headers
            .get("Set-Cookie")
            .is_some());
    }

    #[test(tokio::test)]
    async fn display_hash() {
        let mut conf = default_conf().to_owned();
        conf.push_str("\nauth_display_hash: true");
        let mut app = make_app(&conf);
        let mut session = make_session_with_body("/", "username='<me>'&password=nottest").await;
        session
            .req_header_mut()
            .insert_header("Content-Type", "application/x-www-form-urlencoded")
            .unwrap();
        session.req_header_mut().set_method(Method::POST);
        let mut result = app.handle_request(session).await;
        assert!(result.err().is_none());
        assert_eq!(result.session().remote_user(), None);
        check_login_page_response(&mut result, true, true);
    }

    #[test(tokio::test)]
    async fn display_hash_json() {
        let mut conf = default_conf().to_owned();
        conf.push_str("\nauth_display_hash: true");
        let mut app = make_app(&conf);
        let mut session =
            make_session_with_body("/", "username='<me>'&password=nottest&type=json").await;
        session
            .req_header_mut()
            .insert_header("Content-Type", "application/x-www-form-urlencoded")
            .unwrap();
        session.req_header_mut().set_method(Method::POST);
        let mut result = app.handle_request(session).await;
        assert!(result.err().is_none());
        assert_eq!(result.session().remote_user(), None);
        check_json_response(&mut result, true, true);
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
            let mut session = make_session_with_body("/", "username=me&password=test").await;
            session
                .req_header_mut()
                .insert_header("Content-Type", "application/x-www-form-urlencoded")
                .unwrap();
            session.req_header_mut().set_method(Method::POST);
            app.handle_request(session).await;
        }

        let mut session = make_session_with_body("/", "username=me&password=test").await;
        session
            .req_header_mut()
            .insert_header("Content-Type", "application/x-www-form-urlencoded")
            .unwrap();
        session.req_header_mut().set_method(Method::POST);
        let mut result = app.handle_request(session).await;
        assert!(result.err().is_none());
        assert_eq!(result.session().remote_user(), None);
        assert_eq!(
            result.session().response_written().unwrap().status,
            StatusCode::TOO_MANY_REQUESTS
        );
    }

    #[test(tokio::test)]
    async fn redirect_after_uri_modified() {
        let mut conf = default_conf().to_owned();
        conf.push_str(
            r#"
rewrite_rules:
    from: /subdir/file
    to: /file
            "#,
        );
        let mut app = make_app(&conf);
        let mut session = make_session_with_body("/subdir/file", "username=me&password=test").await;
        session
            .req_header_mut()
            .insert_header("Content-Type", "application/x-www-form-urlencoded")
            .unwrap();
        session.req_header_mut().set_method(Method::POST);
        let mut result = app.handle_request(session).await;
        assert!(result.err().is_none());
        assert_eq!(result.session().remote_user(), Some("me"));

        let session = result.session();
        let response = session.response_written().unwrap();
        assert_eq!(response.status, 302);
        assert_eq!(response.headers.get("Location").unwrap(), "/subdir/file");
        assert!(response.headers.get("Set-Cookie").is_some());
    }

    #[test(tokio::test)]
    async fn login_page() {
        let mut conf = default_conf().to_owned();
        conf.push_str(
            r#"
auth_page_session:
    login_page: /login.html
            "#,
        );
        let mut app = make_app(&conf);
        let mut session = make_session("/file").await;
        session.req_header_mut().set_method(Method::POST);
        let mut result = app.handle_request(session).await;
        assert_eq!(
            result.err().as_ref().map(|err| &err.etype),
            Some(&ErrorType::HTTPStatus(404))
        );
        assert_eq!(result.session().remote_user(), None);

        assert_eq!(result.session().req_header().method, Method::GET);
        assert_eq!(result.session().uri(), "/login.html");
        assert_eq!(result.session().original_uri(), "/file");
    }

    #[test(tokio::test)]
    async fn login_page_head() {
        let mut conf = default_conf().to_owned();
        conf.push_str(
            r#"
auth_page_session:
    login_page: /login.html
            "#,
        );
        let mut app = make_app(&conf);
        let mut session = make_session("/file").await;
        session.req_header_mut().set_method(Method::HEAD);
        let mut result = app.handle_request(session).await;
        assert_eq!(
            result.err().as_ref().map(|err| &err.etype),
            Some(&ErrorType::HTTPStatus(404))
        );
        assert_eq!(result.session().remote_user(), None);

        assert_eq!(result.session().req_header().method, Method::HEAD);
        assert_eq!(result.session().uri().path(), "/login.html");
    }
}
