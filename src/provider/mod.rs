use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderValue, RETRY_AFTER};
use reqwest::{Method, RequestBuilder, Response, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::{Duration, sleep};
use url::Url;

use crate::config::{AuthConfig, ResolvedProfile};
use crate::model::{
    AttachmentInfo, CommentInfo, ContentItem, ContentKind, ContentProperty, CreateContentRequest,
    ProviderKind, SearchResult, SpaceSummary, UpdateContentRequest,
};

pub mod cloud;
pub mod datacenter;

#[async_trait]
pub trait ConfluenceProvider: Send + Sync {
    fn kind(&self) -> ProviderKind;
    fn web_path_prefix(&self) -> String;

    async fn ping(&self) -> Result<()>;
    async fn resolve_page_ref(&self, reference: &str) -> Result<String>;
    async fn list_spaces(&self, limit: usize) -> Result<Vec<SpaceSummary>>;
    async fn get_space(&self, key_or_id: &str) -> Result<SpaceSummary>;
    async fn search(&self, query: &str, cql: bool, limit: usize) -> Result<Vec<SearchResult>>;
    async fn get_content(
        &self,
        kind: ContentKind,
        id: &str,
        include_body: bool,
    ) -> Result<ContentItem>;
    async fn list_children(&self, parent_id: &str, recursive: bool) -> Result<Vec<ContentItem>>;
    async fn list_space_content(
        &self,
        kind: ContentKind,
        space_key_or_id: &str,
        recursive: bool,
    ) -> Result<Vec<ContentItem>>;
    async fn create_content(&self, request: &CreateContentRequest) -> Result<ContentItem>;
    async fn update_content(&self, request: &UpdateContentRequest) -> Result<ContentItem>;
    async fn delete_content(&self, kind: ContentKind, id: &str) -> Result<()>;
    async fn list_attachments(&self, content_id: &str) -> Result<Vec<AttachmentInfo>>;
    async fn download_attachment(&self, content_id: &str, attachment_id: &str) -> Result<Bytes>;
    async fn upload_attachment(
        &self,
        content_id: &str,
        path: &Path,
        comment: Option<&str>,
        replace: bool,
        minor_edit: bool,
    ) -> Result<AttachmentInfo>;
    async fn delete_attachment(&self, content_id: &str, attachment_id: &str) -> Result<()>;
    async fn list_labels(&self, content_id: &str) -> Result<Vec<String>>;
    async fn add_label(&self, content_id: &str, label: &str) -> Result<()>;
    async fn remove_label(&self, content_id: &str, label: &str) -> Result<()>;
    async fn list_comments(&self, content_id: &str) -> Result<Vec<CommentInfo>>;
    async fn add_comment(&self, content_id: &str, text: &str) -> Result<CommentInfo>;
    async fn delete_comment(&self, comment_id: &str) -> Result<()>;
    async fn list_properties(&self, content_id: &str) -> Result<Vec<ContentProperty>>;
    async fn get_property(&self, content_id: &str, key: &str) -> Result<Option<ContentProperty>>;
    async fn set_property(
        &self,
        content_id: &str,
        key: &str,
        value: Value,
    ) -> Result<ContentProperty>;
    async fn delete_property(&self, content_id: &str, key: &str) -> Result<()>;
}

pub fn build_provider(profile: ResolvedProfile) -> Box<dyn ConfluenceProvider> {
    match profile.provider {
        ProviderKind::Cloud => Box::new(cloud::CloudProvider::new(profile)),
        ProviderKind::DataCenter => Box::new(datacenter::DataCenterProvider::new(profile)),
    }
}

#[derive(Clone)]
pub struct HttpClient {
    pub profile: ResolvedProfile,
    client: reqwest::Client,
}

impl HttpClient {
    pub fn new(profile: ResolvedProfile) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        let client = reqwest::Client::builder()
            .user_agent(format!("confluence-cli/{}", env!("CARGO_PKG_VERSION")))
            .default_headers(headers)
            .build()?;
        Ok(Self { profile, client })
    }

    pub fn v1_url(&self, path: &str) -> String {
        format!(
            "{}{}{}",
            self.profile.base_url.trim_end_matches('/'),
            self.profile.api_path.trim_end_matches('/'),
            if path.starts_with('/') {
                path.to_string()
            } else {
                format!("/{path}")
            }
        )
    }

    pub fn v2_url(&self, path: &str) -> String {
        let v2_path = if self.profile.api_path.contains("/rest/api") {
            self.profile.api_path.replace("/rest/api", "/api/v2")
        } else {
            self.profile.api_path.replace("rest/api", "api/v2")
        };
        format!(
            "{}{}{}",
            self.profile.base_url.trim_end_matches('/'),
            v2_path.trim_end_matches('/'),
            if path.starts_with('/') {
                path.to_string()
            } else {
                format!("/{path}")
            }
        )
    }

    pub fn auth(&self, request: RequestBuilder) -> RequestBuilder {
        match &self.profile.auth {
            AuthConfig::Basic { username, token } => request.basic_auth(username, Some(token)),
            AuthConfig::Bearer { token } => request.bearer_auth(token),
        }
    }

    pub async fn json<T: for<'de> Deserialize<'de>>(
        &self,
        method: Method,
        url: String,
        body: Option<Value>,
    ) -> Result<T> {
        let mut request = self.auth(self.client.request(method, &url));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = self.send_with_retry(request, &url).await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("request to {url} failed with {status}: {body}");
        }
        Ok(response.json::<T>().await?)
    }

    pub async fn empty(&self, method: Method, url: String, body: Option<Value>) -> Result<()> {
        let mut request = self.auth(self.client.request(method, &url));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = self.send_with_retry(request, &url).await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("request to {url} failed with {status}: {body}");
        }
        Ok(())
    }

    pub async fn bytes(&self, method: Method, url: String) -> Result<Bytes> {
        let response = self
            .send_with_retry(self.auth(self.client.request(method, &url)), &url)
            .await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("request to {url} failed with {status}: {body}");
        }
        Ok(response.bytes().await?)
    }

    pub async fn send(&self, url: &str, request: RequestBuilder) -> Result<Response> {
        self.send_with_retry(request, url)
            .await
            .with_context(|| format!("request failed for {url}"))
            .map_err(|err| {
                if err.downcast_ref::<reqwest::Error>().is_some() {
                    anyhow!("request failed for {url}: {err}")
                } else {
                    err
                }
            })
    }

    pub fn raw_client(&self) -> &reqwest::Client {
        &self.client
    }

    async fn send_with_retry(&self, request: RequestBuilder, url: &str) -> Result<Response> {
        let retry_template = request.try_clone();
        let method = request
            .try_clone()
            .and_then(|builder| builder.build().ok())
            .map(|request| request.method().clone())
            .unwrap_or(Method::GET);
        let max_attempts = if retry_template.is_some() && request_supports_retry(&method) {
            4usize
        } else {
            1usize
        };
        let mut next_request = Some(request);

        for attempt in 0..max_attempts {
            let builder = if attempt == 0 {
                next_request
                    .take()
                    .ok_or_else(|| anyhow!("missing request builder for {url}"))?
            } else {
                retry_template
                    .as_ref()
                    .and_then(|builder| builder.try_clone())
                    .ok_or_else(|| anyhow!("request for {url} cannot be retried safely"))?
            };

            match builder.send().await {
                Ok(response) => {
                    if attempt + 1 < max_attempts && should_retry_status(response.status()) {
                        sleep(retry_delay(attempt, response.headers().get(RETRY_AFTER))).await;
                        continue;
                    }
                    return Ok(response);
                }
                Err(err) => {
                    if attempt + 1 < max_attempts && should_retry_error(&err) {
                        sleep(retry_delay(attempt, None)).await;
                        continue;
                    }
                    return Err(err).with_context(|| format!("request failed for {url}"));
                }
            }
        }

        unreachable!("retry loop should always return")
    }
}

fn request_supports_retry(method: &Method) -> bool {
    matches!(
        *method,
        Method::GET | Method::HEAD | Method::OPTIONS | Method::PUT | Method::DELETE
    )
}

fn should_retry_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS
            | StatusCode::REQUEST_TIMEOUT
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    )
}

fn should_retry_error(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect() || error.is_request()
}

fn retry_delay(attempt: usize, retry_after: Option<&HeaderValue>) -> Duration {
    if let Some(header) = retry_after
        && let Ok(value) = header.to_str()
        && let Ok(seconds) = value.trim().parse::<u64>()
    {
        return Duration::from_secs(seconds.min(30));
    }

    let base = 250u64;
    let factor = 2u64.saturating_pow(attempt as u32);
    Duration::from_millis((base * factor).min(5_000))
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct Results<T> {
    pub results: Vec<T>,
    pub size: Option<usize>,
    pub limit: Option<usize>,
    pub start: Option<usize>,
    #[serde(default)]
    pub _links: Links,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, Default)]
pub struct Links {
    pub base: Option<String>,
    pub webui: Option<String>,
    pub download: Option<String>,
    pub next: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct V1Space {
    pub id: Option<Value>,
    pub key: String,
    pub name: String,
    #[serde(rename = "type")]
    pub space_type: Option<String>,
    pub homepage: Option<SimpleId>,
    #[serde(default)]
    pub _links: Links,
}

#[derive(Debug, Deserialize)]
pub struct SimpleId {
    pub id: Value,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct V1Content {
    pub id: String,
    #[serde(rename = "type")]
    pub content_type: String,
    pub title: String,
    #[serde(default)]
    pub status: String,
    pub space: Option<V1SpaceRef>,
    pub version: Option<V1Version>,
    #[serde(default)]
    pub ancestors: Vec<V1Ancestor>,
    pub body: Option<V1Body>,
    pub metadata: Option<V1Metadata>,
    #[serde(default)]
    pub _links: Links,
    pub history: Option<V1History>,
}

#[derive(Debug, Deserialize)]
pub struct V1History {
    #[serde(default)]
    pub created_date: Option<String>,
    #[serde(default)]
    pub last_updated: Option<V1LastUpdated>,
}

#[derive(Debug, Deserialize)]
pub struct V1LastUpdated {
    #[serde(default)]
    pub when: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct V1SpaceRef {
    pub id: Option<Value>,
    pub key: String,
    pub name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct V1Version {
    pub number: u64,
    #[serde(default)]
    pub when: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct V1Ancestor {
    pub id: String,
}

#[derive(Debug, Deserialize)]
pub struct V1Body {
    pub storage: Option<V1BodyStorage>,
}

#[derive(Debug, Deserialize)]
pub struct V1BodyStorage {
    pub value: String,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct V1Metadata {
    pub labels: Option<Results<V1Label>>,
}

#[derive(Debug, Deserialize)]
pub struct V1Label {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct V2Page {
    pub id: Value,
    pub status: String,
    pub title: String,
    #[serde(rename = "spaceId")]
    pub space_id: Option<Value>,
    #[serde(rename = "parentId")]
    pub parent_id: Option<Value>,
    pub version: Option<V2Version>,
    pub body: Option<V2Body>,
    #[serde(default)]
    pub _links: Links,
}

#[derive(Debug, Deserialize)]
pub struct V2Version {
    pub number: u64,
    #[serde(default, rename = "createdAt")]
    pub created_at: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct V2Body {
    pub storage: Option<V1BodyStorage>,
}

#[derive(Debug, Deserialize)]
pub struct V1Attachment {
    pub id: String,
    pub title: String,
    pub metadata: Option<V1AttachmentMetadata>,
    pub extensions: Option<V1AttachmentExtensions>,
    #[serde(default)]
    pub _links: Links,
}

#[derive(Debug, Deserialize)]
pub struct V1AttachmentMetadata {
    #[serde(default, rename = "mediaType")]
    pub media_type: Option<String>,
    #[serde(default, rename = "comment")]
    pub comment: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct V1AttachmentExtensions {
    #[serde(default, rename = "fileSize")]
    pub file_size: Option<u64>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct V1Comment {
    pub id: String,
    pub body: Option<V1Body>,
    pub version: Option<V1Version>,
    pub history: Option<V1HistoryComment>,
}

#[derive(Debug, Deserialize)]
pub struct V1HistoryComment {
    #[serde(default, rename = "createdDate")]
    pub created_date: Option<String>,
    #[serde(default, rename = "createdBy")]
    pub created_by: Option<V1CreatedBy>,
}

#[derive(Debug, Deserialize)]
pub struct V1CreatedBy {
    #[serde(default, rename = "displayName")]
    pub display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct V1Property {
    pub id: Option<String>,
    pub key: String,
    pub value: Value,
    pub version: Option<V1Version>,
}

pub fn parse_datetime(value: Option<&str>) -> Option<DateTime<Utc>> {
    value.and_then(|value| {
        DateTime::parse_from_rfc3339(value)
            .ok()
            .map(|value| value.with_timezone(&Utc))
    })
}

pub fn combine_url(base: &str, path: Option<&str>) -> Option<String> {
    let path = path?;
    let base_url = Url::parse(base).ok()?;
    if let Ok(url) = base_url.join(path) {
        Some(url.to_string())
    } else {
        None
    }
}

pub fn v1_content_to_item(
    base_url: &str,
    item: V1Content,
    labels: Vec<String>,
    properties: BTreeMap<String, Value>,
) -> ContentItem {
    let kind = match item.content_type.as_str() {
        "blogpost" => ContentKind::BlogPost,
        _ => ContentKind::Page,
    };
    ContentItem {
        id: item.id,
        kind,
        title: item.title,
        status: if item.status.is_empty() {
            "current".to_string()
        } else {
            item.status
        },
        space_id: item
            .space
            .as_ref()
            .and_then(|space| space.id.as_ref().map(value_to_string)),
        space_key: item.space.as_ref().map(|space| space.key.clone()),
        parent_id: item.ancestors.last().map(|ancestor| ancestor.id.clone()),
        version: item.version.as_ref().map(|version| version.number),
        body_storage: item
            .body
            .and_then(|body| body.storage.map(|storage| storage.value)),
        labels,
        properties,
        web_url: combine_url(base_url, item._links.webui.as_deref()),
        created_at: item
            .history
            .as_ref()
            .and_then(|history| parse_datetime(history.created_date.as_deref())),
        updated_at: item
            .version
            .as_ref()
            .and_then(|version| parse_datetime(version.when.as_deref()))
            .or_else(|| {
                item.history
                    .as_ref()
                    .and_then(|history| history.last_updated.as_ref())
                    .and_then(|update| parse_datetime(update.when.as_deref()))
            }),
    }
}

pub fn v1_search_result(base_url: &str, item: V1Content) -> Option<SearchResult> {
    let kind = match item.content_type.as_str() {
        "page" => ContentKind::Page,
        "blogpost" => ContentKind::BlogPost,
        _ => return None,
    };

    Some(SearchResult {
        id: item.id,
        title: item.title,
        excerpt: None,
        kind,
        space_key: item.space.map(|space| space.key),
        web_url: combine_url(base_url, item._links.webui.as_deref()),
    })
}

pub fn v2_page_to_item(
    profile: &ResolvedProfile,
    item: V2Page,
    labels: Vec<String>,
    properties: BTreeMap<String, Value>,
) -> ContentItem {
    ContentItem {
        id: value_to_string(&item.id),
        kind: ContentKind::Page,
        title: item.title,
        status: item.status,
        space_id: item.space_id.as_ref().map(value_to_string),
        space_key: None,
        parent_id: item.parent_id.as_ref().map(value_to_string),
        version: item.version.as_ref().map(|version| version.number),
        body_storage: item
            .body
            .and_then(|body| body.storage.map(|storage| storage.value)),
        labels,
        properties,
        web_url: combine_url(&profile.base_url, item._links.webui.as_deref()),
        created_at: item
            .version
            .as_ref()
            .and_then(|version| parse_datetime(version.created_at.as_deref())),
        updated_at: item
            .version
            .as_ref()
            .and_then(|version| parse_datetime(version.created_at.as_deref())),
    }
}

pub fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Number(value) => value.to_string(),
        _ => value.to_string(),
    }
}

pub async fn fetch_all_v1<T>(client: &HttpClient, path: &str) -> Result<Vec<T>>
where
    T: for<'de> Deserialize<'de>,
{
    let mut start = 0;
    let mut combined = Vec::new();
    loop {
        let url = if path.contains('?') {
            format!("{}&start={start}", client.v1_url(path))
        } else {
            format!("{}?start={start}", client.v1_url(path))
        };
        let page: Results<T> = client.json(Method::GET, url, None).await?;
        let count = page.results.len();
        combined.extend(page.results);
        let page_limit = page.limit.unwrap_or(count);
        if count == 0 || count < page_limit {
            break;
        }
        start += page_limit;
    }
    Ok(combined)
}

pub async fn resolve_reference_via_url_or_search(
    client: &HttpClient,
    reference: &str,
) -> Result<String> {
    if reference.chars().all(|c| c.is_ascii_digit()) {
        return Ok(reference.to_string());
    }

    if reference.starts_with("http://") || reference.starts_with("https://") {
        let url = Url::parse(reference)?;
        if let Some((_, page_id)) = url.path_segments().and_then(|segments| {
            let segments: Vec<_> = segments.collect();
            segments
                .windows(2)
                .find(|window| {
                    window[0] == "pages" && window[1].chars().all(|c| c.is_ascii_digit())
                })
                .map(|window| ("path", window[1].to_string()))
        }) {
            return Ok(page_id);
        }
        for key in ["pageId", "contentId"] {
            if let Some(value) = url.query_pairs().find_map(|(query_key, value)| {
                if query_key == key {
                    Some(value.to_string())
                } else {
                    None
                }
            }) {
                return Ok(value);
            }
        }
        bail!("could not extract a Confluence page ID from {reference}");
    }

    if let Some((space_key, title)) = reference.split_once(':') {
        let query = format!(
            "/content?title={}&spaceKey={}&type=page&limit=2",
            urlencoding::encode(title),
            urlencoding::encode(space_key)
        );
        let results: Results<V1Content> = client
            .json(Method::GET, client.v1_url(&query), None)
            .await?;
        match results.results.len() {
            0 => bail!("no page found for {reference}"),
            1 => Ok(results.results[0].id.clone()),
            _ => bail!("multiple pages matched {reference}"),
        }
    } else {
        bail!("unsupported page reference `{reference}`; use an ID, URL, or SPACE:Title")
    }
}

pub fn build_search_cql(query: &str, cql: bool) -> String {
    if cql {
        query.to_string()
    } else {
        let escaped = query.replace('"', "\\\"");
        format!("text ~ \"{escaped}\" order by lastmodified desc")
    }
}

pub fn normalize_properties(properties: Vec<ContentProperty>) -> BTreeMap<String, Value> {
    properties
        .into_iter()
        .map(|property| (property.key, property.value))
        .collect()
}

pub fn property_payload(key: &str, value: Value, version: Option<u64>) -> Value {
    let mut body = json!({
        "key": key,
        "value": value,
    });
    if let Some(version) = version {
        body["version"] = json!({ "number": version + 1 });
    }
    body
}

pub fn ensure_writable(profile: &ResolvedProfile) -> Result<()> {
    if profile.read_only {
        bail!(
            "profile `{}` is read-only; refusing to perform a write operation",
            profile.name
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn search_content(content_type: &str) -> V1Content {
        V1Content {
            id: "123".to_string(),
            content_type: content_type.to_string(),
            title: "Example".to_string(),
            status: "current".to_string(),
            space: Some(V1SpaceRef {
                id: None,
                key: "MFS".to_string(),
                name: None,
            }),
            version: None,
            ancestors: Vec::new(),
            body: None,
            metadata: None,
            _links: Links {
                webui: Some("/spaces/MFS/pages/123/Example".to_string()),
                ..Links::default()
            },
            history: None,
        }
    }

    #[test]
    fn search_filters_out_attachment_hits() {
        assert!(
            v1_search_result(
                "https://example.atlassian.net",
                search_content("attachment")
            )
            .is_none()
        );
        let page = v1_search_result("https://example.atlassian.net", search_content("page"))
            .expect("page result should be preserved");
        assert_eq!(page.kind, ContentKind::Page);
    }
}
