use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
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

#[derive(Debug)]
pub struct SearchPage {
    pub items: Vec<SearchResult>,
    /// Exact total reported by Confluence, when the deployment exposes it.
    pub total: Option<usize>,
}

#[async_trait]
pub trait ConfluenceProvider: Send + Sync {
    fn kind(&self) -> ProviderKind;
    fn web_path_prefix(&self) -> String;

    async fn ping(&self) -> Result<()>;
    async fn resolve_page_ref(&self, reference: &str) -> Result<String>;
    async fn list_spaces(&self, limit: usize) -> Result<Vec<SpaceSummary>>;
    async fn get_space(&self, key_or_id: &str) -> Result<SpaceSummary>;
    async fn search(
        &self,
        query: &str,
        cql: bool,
        limit: usize,
        offset: usize,
    ) -> Result<SearchPage>;
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
    async fn update_comment(&self, comment_id: &str, text: &str) -> Result<CommentInfo>;
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

    fn api_base_url(&self) -> String {
        if self.profile.provider == ProviderKind::Cloud && self.profile.token_kind == "scoped" {
            format!(
                "https://api.atlassian.com/ex/confluence/{}",
                self.profile.cloud_id.as_deref().unwrap_or_default()
            )
        } else {
            self.profile.base_url.trim_end_matches('/').to_string()
        }
    }

    pub fn v1_url(&self, path: &str) -> String {
        format!(
            "{}{}{}",
            self.api_base_url(),
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
            self.api_base_url(),
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
            let raw = response.text().await.unwrap_or_default();
            let message = extract_error_message(&raw);
            return Err(crate::output::http_error(
                status,
                format!("request to {url} failed with {status}: {message}"),
            ));
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
            let raw = response.text().await.unwrap_or_default();
            let message = extract_error_message(&raw);
            return Err(crate::output::http_error(
                status,
                format!("request to {url} failed with {status}: {message}"),
            ));
        }
        Ok(())
    }

    pub async fn bytes(&self, method: Method, url: String) -> Result<Bytes> {
        let response = self
            .send_with_retry(self.auth(self.client.request(method, &url)), &url)
            .await?;
        let status = response.status();
        if !status.is_success() {
            let raw = response.text().await.unwrap_or_default();
            let message = extract_error_message(&raw);
            return Err(crate::output::http_error(
                status,
                format!("request to {url} failed with {status}: {message}"),
            ));
        }
        Ok(response.bytes().await?)
    }

    pub async fn send(&self, url: &str, request: RequestBuilder) -> Result<Response> {
        self.send_with_retry(request, url)
            .await
            .with_context(|| format!("request failed for {url}"))
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
    matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
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

#[derive(Debug, Deserialize)]
pub struct Results<T> {
    pub results: Vec<T>,
    pub limit: Option<usize>,
    #[serde(default, rename = "totalSize")]
    pub total_size: Option<usize>,
    #[serde(default)]
    pub _links: Links,
}

#[derive(Debug, Deserialize, Default)]
pub struct Links {
    pub webui: Option<String>,
    pub download: Option<String>,
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

#[derive(Debug, Deserialize)]
pub struct V1SpaceRef {
    pub id: Option<Value>,
    pub key: String,
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
        return Err(crate::output::typed_error(
            crate::output::ErrorKind::InvalidInput,
            format!("could not extract a Confluence page ID from {reference}"),
        ));
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
            0 => Err(crate::output::typed_error(
                crate::output::ErrorKind::NotFound,
                format!("no page found for {reference}"),
            )),
            1 => Ok(results.results[0].id.clone()),
            _ => Err(crate::output::typed_error(
                crate::output::ErrorKind::Conflict,
                format!("multiple pages matched {reference}"),
            )),
        }
    } else {
        Err(crate::output::typed_error_with_hint(
            crate::output::ErrorKind::InvalidInput,
            format!("unsupported page reference `{reference}`"),
            "use a numeric ID, Confluence URL, or SPACE:Title",
        ))
    }
}

pub fn build_search_cql(query: &str, cql: bool) -> String {
    let content_filter = "(type = page OR type = blogpost)";
    if cql {
        if let Some(order_index) = cql_order_by_index(query) {
            format!(
                "{content_filter} AND ({}){}",
                query[..order_index].trim(),
                &query[order_index..]
            )
        } else {
            format!("{content_filter} AND ({query})")
        }
    } else {
        let escaped = escape_cql_literal(query);
        format!("{content_filter} AND text ~ \"{escaped}\" order by lastmodified desc")
    }
}

fn cql_order_by_index(query: &str) -> Option<usize> {
    let lowercase = query.to_ascii_lowercase();
    let mut in_quotes = false;
    let mut escaped = false;
    for (index, character) in query.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if in_quotes && character == '\\' {
            escaped = true;
            continue;
        }
        if character == '"' {
            in_quotes = !in_quotes;
            continue;
        }
        if !in_quotes && lowercase[index..].starts_with(" order by ") {
            return Some(index);
        }
    }
    None
}

pub fn escape_cql_literal(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

pub fn normalize_properties(properties: Vec<ContentProperty>) -> BTreeMap<String, Value> {
    properties
        .into_iter()
        .map(|property| (property.key, property.value))
        .collect()
}

pub async fn add_content_metadata<P>(
    provider: &P,
    content_id: &str,
    labels: &[String],
    properties: &BTreeMap<String, Value>,
) -> Result<()>
where
    P: ConfluenceProvider + ?Sized,
{
    for label in labels {
        provider
            .add_label(content_id, label)
            .await
            .with_context(|| format!("failed to add label `{label}` to content {content_id}"))?;
    }
    for (key, value) in properties {
        provider
            .set_property(content_id, key, value.clone())
            .await
            .with_context(|| format!("failed to set property `{key}` on content {content_id}"))?;
    }
    Ok(())
}

pub async fn sync_content_metadata<P>(
    provider: &P,
    content_id: &str,
    labels: &[String],
    properties: &BTreeMap<String, Value>,
) -> Result<()>
where
    P: ConfluenceProvider + ?Sized,
{
    let current_labels = provider
        .list_labels(content_id)
        .await
        .with_context(|| format!("failed to list labels on content {content_id}"))?;
    for label in current_labels
        .iter()
        .filter(|label| !labels.contains(*label))
    {
        provider
            .remove_label(content_id, label)
            .await
            .with_context(|| {
                format!("failed to remove label `{label}` from content {content_id}")
            })?;
    }
    for label in labels
        .iter()
        .filter(|label| !current_labels.contains(*label))
    {
        provider
            .add_label(content_id, label)
            .await
            .with_context(|| format!("failed to add label `{label}` to content {content_id}"))?;
    }

    let current_properties = provider
        .list_properties(content_id)
        .await
        .with_context(|| format!("failed to list properties on content {content_id}"))?;
    let current_map: BTreeMap<_, _> = current_properties
        .into_iter()
        .map(|property| (property.key, property.value))
        .collect();
    for (key, value) in properties {
        if current_map.get(key) != Some(value) {
            provider
                .set_property(content_id, key, value.clone())
                .await
                .with_context(|| {
                    format!("failed to set property `{key}` on content {content_id}")
                })?;
        }
    }
    for key in current_map
        .keys()
        .filter(|key| !properties.contains_key(*key))
    {
        provider
            .delete_property(content_id, key)
            .await
            .with_context(|| {
                format!("failed to delete property `{key}` from content {content_id}")
            })?;
    }
    Ok(())
}

pub fn partial_remote_mutation_error(
    error: anyhow::Error,
    operation: &str,
    content_id: &str,
    completed_stage: &str,
    failed_stage: &str,
) -> anyhow::Error {
    let (kind, hint) = crate::output::classify_anyhow(&error);
    crate::output::typed_error_with_details(
        kind,
        format!(
            "{operation} partially completed for content {content_id}; {failed_stage} failed after {completed_stage}: {error:#}"
        ),
        hint,
        json!({
            "operation": operation,
            "content_id": content_id,
            "partial_success": true,
            "completed_stage": completed_stage,
            "failed_stage": failed_stage,
        }),
    )
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

fn extract_error_message(raw: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(raw)
        && let Some(msg) = value.get("message").and_then(|m| m.as_str())
    {
        return msg.to_string();
    }
    raw.to_string()
}

pub fn ensure_writable(profile: &ResolvedProfile) -> Result<()> {
    if profile.read_only {
        return Err(crate::output::typed_error_with_hint(
            crate::output::ErrorKind::ReadOnly,
            format!(
                "profile `{}` is read-only; refusing to perform a write operation",
                profile.name
            ),
            "disable read-only mode for the active profile",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    // ── Fixtures ──────────────────────────────────────────────────────────────

    fn search_content(content_type: &str) -> V1Content {
        V1Content {
            id: "123".to_string(),
            content_type: content_type.to_string(),
            title: "Example".to_string(),
            status: "current".to_string(),
            space: Some(V1SpaceRef {
                id: None,
                key: "MFS".to_string(),
            }),
            version: None,
            ancestors: Vec::new(),
            body: None,
            _links: Links {
                webui: Some("/spaces/MFS/pages/123/Example".to_string()),
                ..Links::default()
            },
            history: None,
        }
    }

    fn test_profile(base_url: &str) -> ResolvedProfile {
        ResolvedProfile {
            name: "test".to_string(),
            provider: crate::model::ProviderKind::DataCenter,
            base_url: base_url.to_string(),
            api_path: "/rest/api".to_string(),
            auth: crate::config::AuthConfig::Bearer {
                token: "test-token".to_string(),
            },
            credential_store: "session".to_string(),
            cloud_id: None,
            token_kind: "classic".to_string(),
            expires_at: None,
            read_only: false,
        }
    }

    #[test]
    fn scoped_cloud_token_uses_gateway_but_keeps_site_url() {
        let profile = ResolvedProfile {
            name: "cloud".to_string(),
            provider: crate::model::ProviderKind::Cloud,
            base_url: "https://acme.atlassian.net".to_string(),
            api_path: "/wiki/rest/api".to_string(),
            auth: crate::config::AuthConfig::Basic {
                username: "me@example.com".to_string(),
                token: "token".to_string(),
            },
            credential_store: "session".to_string(),
            cloud_id: Some("cloud-123".to_string()),
            token_kind: "scoped".to_string(),
            expires_at: None,
            read_only: false,
        };
        let client = HttpClient::new(profile).unwrap();

        assert_eq!(
            client.v1_url("/space"),
            "https://api.atlassian.com/ex/confluence/cloud-123/wiki/rest/api/space"
        );
        assert_eq!(
            client.v2_url("/pages"),
            "https://api.atlassian.com/ex/confluence/cloud-123/wiki/api/v2/pages"
        );
        assert_eq!(client.profile.base_url, "https://acme.atlassian.net");
    }

    fn space_result(key: &str, name: &str, id: &str) -> serde_json::Value {
        json!({ "key": key, "name": name, "id": id, "_links": {} })
    }

    fn paginated_response(
        results: serde_json::Value,
        limit: usize,
        start: usize,
    ) -> serde_json::Value {
        json!({
            "results": results,
            "limit": limit,
            "size": results.as_array().map(|a| a.len()).unwrap_or(0),
            "start": start,
            "_links": {}
        })
    }

    // ── build_search_cql ──────────────────────────────────────────────────────

    #[test]
    fn search_cql_plain_text_wraps_in_text_match() {
        let cql = build_search_cql("hello world", false);
        assert_eq!(
            cql,
            r#"(type = page OR type = blogpost) AND text ~ "hello world" order by lastmodified desc"#
        );
    }

    #[test]
    fn search_cql_plain_text_escapes_quotes() {
        let cql = build_search_cql(r#"say "hi""#, false);
        assert_eq!(
            cql,
            r#"(type = page OR type = blogpost) AND text ~ "say \"hi\"" order by lastmodified desc"#
        );
    }

    #[test]
    fn search_cql_plain_text_escapes_backslashes() {
        let cql = build_search_cql(r"docs\draft", false);
        assert_eq!(
            cql,
            r#"(type = page OR type = blogpost) AND text ~ "docs\\draft" order by lastmodified desc"#
        );
    }

    #[test]
    fn search_cql_scopes_raw_cql_to_supported_content_types() {
        let query = r#"space = "PROJ" AND type = page"#;
        assert_eq!(
            build_search_cql(query, true),
            r#"(type = page OR type = blogpost) AND (space = "PROJ" AND type = page)"#
        );
    }

    #[test]
    fn search_cql_preserves_raw_order_by_clause() {
        let query = r#"space = "PROJ" order by title asc"#;
        assert_eq!(
            build_search_cql(query, true),
            r#"(type = page OR type = blogpost) AND (space = "PROJ") order by title asc"#
        );
    }

    #[test]
    fn search_cql_does_not_split_order_by_inside_a_literal() {
        let query = r#"text ~ "order by migration" order by title asc"#;
        assert_eq!(
            build_search_cql(query, true),
            r#"(type = page OR type = blogpost) AND (text ~ "order by migration") order by title asc"#
        );
    }

    #[test]
    fn automatic_retries_are_limited_to_safe_read_methods() {
        for method in [Method::GET, Method::HEAD, Method::OPTIONS] {
            assert!(request_supports_retry(&method), "{method} should retry");
        }
        for method in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
            assert!(
                !request_supports_retry(&method),
                "{method} could duplicate a mutation"
            );
        }
    }

    // ── extract_error_message ─────────────────────────────────────────────────

    #[test]
    fn extract_error_message_pulls_message_field() {
        assert_eq!(
            extract_error_message(r#"{"message":"Not authorized","statusCode":401}"#),
            "Not authorized"
        );
    }

    #[test]
    fn extract_error_message_falls_back_to_raw_on_missing_field() {
        assert_eq!(
            extract_error_message(r#"{"error":"oops"}"#),
            r#"{"error":"oops"}"#
        );
    }

    #[test]
    fn extract_error_message_falls_back_to_raw_on_invalid_json() {
        assert_eq!(
            extract_error_message("plain text error"),
            "plain text error"
        );
    }

    // ── v1_search_result ──────────────────────────────────────────────────────

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

    #[test]
    fn search_preserves_blogpost_hits() {
        let post = v1_search_result("https://example.atlassian.net", search_content("blogpost"))
            .expect("blogpost result should be preserved");
        assert_eq!(post.kind, ContentKind::BlogPost);
    }

    // ── property_payload ──────────────────────────────────────────────────────

    #[test]
    fn property_payload_without_version_omits_version_field() {
        let payload = property_payload("my-key", json!("value"), None);
        assert_eq!(payload["key"], "my-key");
        assert_eq!(payload["value"], "value");
        assert!(payload.get("version").is_none());
    }

    #[test]
    fn property_payload_with_version_increments_by_one() {
        let payload = property_payload("k", json!(42), Some(3));
        assert_eq!(payload["version"]["number"], 4);
    }

    // ── ensure_writable ───────────────────────────────────────────────────────

    #[test]
    fn ensure_writable_allows_non_readonly_profile() {
        let profile = test_profile("https://example.com");
        assert!(ensure_writable(&profile).is_ok());
    }

    #[test]
    fn ensure_writable_rejects_readonly_profile() {
        let mut profile = test_profile("https://example.com");
        profile.read_only = true;
        let err = ensure_writable(&profile).unwrap_err();
        assert!(err.to_string().contains("read-only"));
    }

    // ── fetch_all_v1 ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_all_v1_single_page_returns_all_results() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/space"))
            .and(query_param("start", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(paginated_response(
                json!([
                    space_result("A", "Alpha", "1"),
                    space_result("B", "Beta", "2")
                ]),
                50,
                0,
            )))
            .mount(&server)
            .await;

        let client = HttpClient::new(test_profile(&server.uri())).unwrap();
        let spaces: Vec<V1Space> = fetch_all_v1(&client, "/space?limit=50&expand=homepage")
            .await
            .unwrap();
        assert_eq!(spaces.len(), 2);
        assert_eq!(spaces[0].key, "A");
        assert_eq!(spaces[1].key, "B");
    }

    #[tokio::test]
    async fn fetch_all_v1_follows_pagination_across_multiple_pages() {
        let server = MockServer::start().await;

        // Page 1: full page of 2 — signals more results exist
        Mock::given(method("GET"))
            .and(path("/rest/api/space"))
            .and(query_param("start", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(paginated_response(
                json!([
                    space_result("A", "Alpha", "1"),
                    space_result("B", "Beta", "2")
                ]),
                2,
                0,
            )))
            .mount(&server)
            .await;

        // Page 2: partial page — signals end of results
        Mock::given(method("GET"))
            .and(path("/rest/api/space"))
            .and(query_param("start", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(paginated_response(
                json!([space_result("C", "Gamma", "3")]),
                2,
                2,
            )))
            .mount(&server)
            .await;

        let client = HttpClient::new(test_profile(&server.uri())).unwrap();
        let spaces: Vec<V1Space> = fetch_all_v1(&client, "/space?limit=2&expand=homepage")
            .await
            .unwrap();
        assert_eq!(spaces.len(), 3);
        assert_eq!(spaces[2].key, "C");
    }

    #[tokio::test]
    async fn fetch_all_v1_stops_on_empty_page() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/rest/api/space"))
            .and(query_param("start", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({ "results": [], "limit": 50, "size": 0, "start": 0, "_links": {} }),
            ))
            .mount(&server)
            .await;

        let client = HttpClient::new(test_profile(&server.uri())).unwrap();
        let spaces: Vec<V1Space> = fetch_all_v1(&client, "/space?limit=50&expand=homepage")
            .await
            .unwrap();
        assert!(spaces.is_empty());
    }

    #[tokio::test]
    async fn fetch_all_v1_three_full_pages_then_empty_collects_all() {
        let server = MockServer::start().await;

        for page in 0..3usize {
            let start = page * 2;
            Mock::given(method("GET"))
                .and(path("/rest/api/space"))
                .and(query_param("start", start.to_string()))
                .respond_with(ResponseTemplate::new(200).set_body_json(paginated_response(
                    json!([
                        space_result(&format!("K{start}"), "Space", &format!("{start}")),
                        space_result(
                            &format!("K{}", start + 1),
                            "Space",
                            &format!("{}", start + 1)
                        )
                    ]),
                    2,
                    start,
                )))
                .mount(&server)
                .await;
        }

        // Final page: empty
        Mock::given(method("GET"))
            .and(path("/rest/api/space"))
            .and(query_param("start", "6"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({ "results": [], "limit": 2, "size": 0, "start": 6, "_links": {} }),
            ))
            .mount(&server)
            .await;

        let client = HttpClient::new(test_profile(&server.uri())).unwrap();
        let spaces: Vec<V1Space> = fetch_all_v1(&client, "/space?limit=2&expand=homepage")
            .await
            .unwrap();
        assert_eq!(spaces.len(), 6);
    }
}
