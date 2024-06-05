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

//! Standard responses for various conditions

use http::{header, method::Method, status::StatusCode};
use maud::{html, DOCTYPE};

use crate::pingora::{Error, ResponseHeader, SessionWrapper};

/// Produces the text of a standard response page for the given status code.
pub fn response_text(status: StatusCode) -> String {
    let status_str = status.as_str();
    let reason = status.canonical_reason().unwrap_or("");
    html! {
        (DOCTYPE)
        html {
            head {
                title {
                    (status_str) " " (reason)
                }
            }

            body {
                center {
                    h1 {
                        (status_str) " " (reason)
                    }
                }
            }
        }
    }
    .into()
}

async fn response(
    session: &mut impl SessionWrapper,
    status: StatusCode,
    location: Option<&str>,
) -> Result<(), Box<Error>> {
    let text = response_text(status);

    let num_headers = if location.is_some() { 3 } else { 2 };
    let mut header = ResponseHeader::build(status, Some(num_headers))?;
    header.append_header(header::CONTENT_LENGTH, text.len().to_string())?;
    header.append_header(header::CONTENT_TYPE, "text/html")?;
    if let Some(location) = location {
        header.append_header(header::LOCATION, location)?;
    }
    session.write_response_header(Box::new(header)).await?;

    if session.req_header().method != Method::HEAD {
        session.write_response_body(text.into()).await?;
    }

    Ok(())
}

/// Responds with a standard error page for the given status code.
pub async fn error_response(
    session: &mut impl SessionWrapper,
    status: StatusCode,
) -> Result<(), Box<Error>> {
    response(session, status, None).await
}

/// Responds with a redirect to the given location.
pub async fn redirect_response(
    session: &mut impl SessionWrapper,
    status: StatusCode,
    location: &str,
) -> Result<(), Box<Error>> {
    response(session, status, Some(location)).await
}
