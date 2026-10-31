use std::{sync::LazyLock, time::Duration};

use hyper_util::client::proxy::matcher::Matcher;
use reqwest::{Client, Url, redirect::Policy};

pub(crate) const TOKEN_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) fn new() -> Client {
    builder()
        .build()
        .expect("HTTP client configuration is valid")
}

pub(crate) fn oauth() -> Client {
    builder()
        .timeout(TOKEN_REQUEST_TIMEOUT)
        .build()
        .expect("HTTP client configuration is valid")
}

// Read once, like reqwest, so both see the same proxy settings.
static PROXY: LazyLock<Matcher> = LazyLock::new(Matcher::from_system);

pub(crate) fn proxied(url: &Url) -> bool {
    url.as_str()
        .parse::<http::Uri>()
        .is_ok_and(|uri| PROXY.intercept(&uri).is_some())
}

fn builder() -> reqwest::ClientBuilder {
    LazyLock::force(&PROXY);
    Client::builder().redirect(Policy::custom(|attempt| {
        let target = attempt.url();
        let origin = attempt
            .previous()
            .first()
            .expect("a redirect follows a request");
        if target.scheme() != origin.scheme()
            || target.host_str() != origin.host_str()
            || target.port_or_known_default() != origin.port_or_known_default()
        {
            attempt.error("cross-origin redirect refused")
        } else if attempt.previous().len() >= 10 {
            attempt.error("redirect limit exceeded")
        } else {
            attempt.follow()
        }
    }))
}

#[cfg(test)]
mod tests;
