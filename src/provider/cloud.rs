use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use bytes::Bytes;
use reqwest::Method;
use reqwest::multipart::{Form, Part};
use serde_json::{Value, json};
use tokio::fs;

use crate::config::ResolvedProfile;
use crate::model::{
    AttachmentInfo, CommentInfo, ContentItem, ContentKind, ContentProperty, CreateContentRequest,
    ProviderKind, SearchResult, SpaceSummary, UpdateContentRequest,
};
use crate::provider::{
    ConfluenceProvider, HttpClient, Results, V1Attachment, V1Comment, V1Content, V1Label,
    V1Property, V1Space, V1SpaceRef, V2Page, build_search_cql, ensure_writable, fetch_all_v1,
    normalize_properties, parse_datetime, property_payload, resolve_reference_via_url_or_search,
    v1_content_to_item, v1_search_result, v2_page_to_item, value_to_string,
};

pub struct CloudProvider {
    http: HttpClient,
}

impl CloudProvider {
    pub fn new(profile: ResolvedProfile) -> Self {
        Self {
            http: HttpClient::new(profile).expect("http client initialization failed"),
        }
    }

    async fn space_by_key_or_id(&self, key_or_id: &str) -> Result<SpaceSummary> {
        let spaces = self.list_spaces(500).await?;
        spaces
            .into_iter()
            .find(|space| space.key == key_or_id || space.id == key_or_id)
            .ok_or_else(|| anyhow!("space `{key_or_id}` not found"))
    }

    async fn page_v2(&self, id: &str, include_body: bool) -> Result<V2Page> {
        let suffix = if include_body {
            "?body-format=storage"
        } else {
            ""
        };
        self.http
            .json(
                Method::GET,
                self.http.v2_url(&format!("/pages/{id}{suffix}")),
                None,
            )
            .await
    }

    async fn content_v1(&self, id: &str, include_body: bool, status: &str) -> Result<V1Content> {
        let expand = if include_body {
            "version,space,ancestors,body.storage,metadata.labels,history"
        } else {
            "version,space,ancestors,metadata.labels,history"
        };
        let mut path = format!("/content/{id}?expand={expand}");
        if status != "current" {
            path.push_str(&format!("&status={}", urlencoding::encode(status)));
        }
        self.http
            .json(Method::GET, self.http.v1_url(&path), None)
            .await
    }

    async fn labels_for(&self, content_id: &str) -> Result<Vec<String>> {
        let response: Results<V1Label> = self
            .http
            .json(
                Method::GET,
                self.http
                    .v1_url(&format!("/content/{content_id}/label?limit=200")),
                None,
            )
            .await?;
        Ok(response
            .results
            .into_iter()
            .map(|label| label.name)
            .collect())
    }

    async fn properties_for(&self, content_id: &str) -> Result<Vec<ContentProperty>> {
        let response: Results<V1Property> = self
            .http
            .json(
                Method::GET,
                self.http
                    .v1_url(&format!("/content/{content_id}/property?limit=200")),
                None,
            )
            .await?;
        Ok(response
            .results
            .into_iter()
            .map(|property| ContentProperty {
                id: property.id,
                key: property.key,
                value: property.value,
                version: property.version.map(|version| version.number),
            })
            .collect())
    }

    async fn attachment_by_name(
        &self,
        content_id: &str,
        file_name: &str,
    ) -> Result<Option<AttachmentInfo>> {
        let attachments = self.list_attachments(content_id).await?;
        Ok(attachments.into_iter().find(|item| item.title == file_name))
    }

    fn map_space(&self, space: V1Space) -> SpaceSummary {
        SpaceSummary {
            id: space
                .id
                .map(|value| value_to_string(&value))
                .unwrap_or_default(),
            key: space.key,
            name: space.name,
            space_type: space.space_type,
            homepage_id: space.homepage.map(|homepage| value_to_string(&homepage.id)),
            web_url: crate::provider::combine_url(
                &self.http.profile.base_url,
                space._links.webui.as_deref(),
            ),
        }
    }

    fn map_attachment(&self, attachment: V1Attachment) -> AttachmentInfo {
        AttachmentInfo {
            id: attachment.id,
            title: attachment.title,
            media_type: attachment
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.media_type.clone()),
            file_size: attachment
                .extensions
                .as_ref()
                .and_then(|extensions| extensions.file_size),
            download_url: crate::provider::combine_url(
                &self.http.profile.base_url,
                attachment._links.download.as_deref(),
            ),
            comment: attachment.metadata.and_then(|metadata| metadata.comment),
        }
    }

    fn map_comment(&self, comment: V1Comment) -> CommentInfo {
        CommentInfo {
            id: comment.id,
            author: comment
                .history
                .as_ref()
                .and_then(|history| history.created_by.as_ref())
                .and_then(|author| author.display_name.clone()),
            body_storage: comment
                .body
                .and_then(|body| body.storage.map(|storage| storage.value))
                .unwrap_or_default(),
            created_at: comment
                .history
                .as_ref()
                .and_then(|history| parse_datetime(history.created_date.as_deref())),
            version: comment.version.map(|v| v.number),
        }
    }
}

#[async_trait]
impl ConfluenceProvider for CloudProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Cloud
    }

    fn web_path_prefix(&self) -> String {
        self.http.profile.web_path_prefix()
    }

    async fn ping(&self) -> Result<()> {
        let _: Results<V1SpaceRef> = self
            .http
            .json(Method::GET, self.http.v1_url("/space?limit=1"), None)
            .await?;
        Ok(())
    }

    async fn resolve_page_ref(&self, reference: &str) -> Result<String> {
        resolve_reference_via_url_or_search(&self.http, reference).await
    }

    async fn list_spaces(&self, limit: usize) -> Result<Vec<SpaceSummary>> {
        let mut spaces: Vec<SpaceSummary> =
            fetch_all_v1::<V1Space>(&self.http, "/space?limit=200&expand=homepage")
                .await?
                .into_iter()
                .map(|space| self.map_space(space))
                .collect();
        spaces.truncate(limit);
        Ok(spaces)
    }

    async fn get_space(&self, key_or_id: &str) -> Result<SpaceSummary> {
        self.space_by_key_or_id(key_or_id).await
    }

    async fn search(
        &self,
        query: &str,
        cql: bool,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<SearchResult>> {
        let cql = build_search_cql(query, cql);
        let path = format!(
            "/content/search?cql={}&limit={limit}&start={offset}&expand=space",
            urlencoding::encode(&cql)
        );
        let response: Results<V1Content> = self
            .http
            .json(Method::GET, self.http.v1_url(&path), None)
            .await?;
        Ok(response
            .results
            .into_iter()
            .filter_map(|item| v1_search_result(&self.http.profile.base_url, item))
            .collect())
    }

    async fn get_content(
        &self,
        kind: ContentKind,
        id: &str,
        include_body: bool,
    ) -> Result<ContentItem> {
        let labels = self.labels_for(id).await.unwrap_or_default();
        let properties = normalize_properties(self.properties_for(id).await.unwrap_or_default());
        match kind {
            ContentKind::Page => {
                let page = self.page_v2(id, include_body).await?;
                let mut item = v2_page_to_item(&self.http.profile, page, labels, properties);
                let enriched = self.content_v1(id, include_body, "current").await?;
                item.space_key = enriched.space.as_ref().map(|space| space.key.clone());
                item.parent_id = enriched
                    .ancestors
                    .last()
                    .map(|ancestor| ancestor.id.clone());
                if include_body && item.body_storage.is_none() {
                    item.body_storage = enriched
                        .body
                        .and_then(|body| body.storage.map(|storage| storage.value));
                }
                Ok(item)
            }
            ContentKind::BlogPost => {
                let item = self.content_v1(id, include_body, "current").await?;
                Ok(v1_content_to_item(
                    &self.http.profile.base_url,
                    item,
                    labels,
                    properties,
                ))
            }
        }
    }

    async fn list_children(&self, parent_id: &str, recursive: bool) -> Result<Vec<ContentItem>> {
        let mut all_items = Vec::new();
        let mut stack = vec![parent_id.to_string()];
        while let Some(current) = stack.pop() {
            let response: Results<V1Content> = self
                .http
                .json(
                    Method::GET,
                    self.http.v1_url(&format!(
                        "/content/{current}/child/page?limit=200&expand=version,space,ancestors,body.storage,metadata.labels,history"
                    )),
                    None,
                )
                .await?;
            for child in response.results {
                let child_id = child.id.clone();
                all_items.push(v1_content_to_item(
                    &self.http.profile.base_url,
                    child,
                    Vec::new(),
                    BTreeMap::new(),
                ));
                if recursive {
                    stack.push(child_id);
                }
            }
            if !recursive {
                break;
            }
        }
        Ok(all_items)
    }

    async fn list_space_content(
        &self,
        kind: ContentKind,
        space_key_or_id: &str,
        _recursive: bool,
    ) -> Result<Vec<ContentItem>> {
        let space = self.space_by_key_or_id(space_key_or_id).await?;
        let path = format!(
            "/content?spaceKey={}&type={}&limit=200&expand=version,space,ancestors,body.storage,metadata.labels,history",
            urlencoding::encode(&space.key),
            kind.as_str()
        );
        let response: Vec<V1Content> = fetch_all_v1(&self.http, &path).await?;
        Ok(response
            .into_iter()
            .map(|item| {
                v1_content_to_item(
                    &self.http.profile.base_url,
                    item,
                    Vec::new(),
                    BTreeMap::new(),
                )
            })
            .collect())
    }

    async fn create_content(&self, request: &CreateContentRequest) -> Result<ContentItem> {
        ensure_writable(&self.http.profile)?;
        match request.kind {
            ContentKind::Page => {
                let space = self.space_by_key_or_id(&request.space).await?;
                let created: V2Page = self
                    .http
                    .json(
                        Method::POST,
                        self.http.v2_url("/pages"),
                        Some(json!({
                            "spaceId": space.id,
                            "status": request.status,
                            "title": request.title,
                            "parentId": request.parent_id,
                            "body": {
                                "representation": "storage",
                                "value": request.body_storage,
                            }
                        })),
                    )
                    .await?;
                let content_id = value_to_string(&created.id);
                for label in &request.labels {
                    let _ = self.add_label(&content_id, label).await;
                }
                for (key, value) in &request.properties {
                    let _ = self.set_property(&content_id, key, value.clone()).await;
                }
                let labels = self.labels_for(&content_id).await.unwrap_or_default();
                let properties = normalize_properties(
                    self.properties_for(&content_id).await.unwrap_or_default(),
                );
                let page = self.page_v2(&content_id, true).await?;
                let mut item = v2_page_to_item(&self.http.profile, page, labels, properties);
                let enriched = self.content_v1(&content_id, true, &request.status).await?;
                item.space_key = enriched.space.as_ref().map(|space| space.key.clone());
                item.parent_id = enriched
                    .ancestors
                    .last()
                    .map(|ancestor| ancestor.id.clone());
                if item.body_storage.is_none() {
                    item.body_storage = enriched
                        .body
                        .and_then(|body| body.storage.map(|storage| storage.value));
                }
                Ok(item)
            }
            ContentKind::BlogPost => {
                let body = json!({
                    "type": request.kind.as_str(),
                    "title": request.title,
                    "space": { "key": request.space },
                    "status": request.status,
                    "body": {
                        "storage": {
                            "value": request.body_storage,
                            "representation": "storage"
                        }
                    }
                });
                let created: V1Content = self
                    .http
                    .json(Method::POST, self.http.v1_url("/content"), Some(body))
                    .await?;
                let content_id = created.id.clone();
                for label in &request.labels {
                    let _ = self.add_label(&content_id, label).await;
                }
                for (key, value) in &request.properties {
                    let _ = self.set_property(&content_id, key, value.clone()).await;
                }
                let labels = self.labels_for(&content_id).await.unwrap_or_default();
                let properties = normalize_properties(
                    self.properties_for(&content_id).await.unwrap_or_default(),
                );
                let item = self.content_v1(&content_id, true, &request.status).await?;
                Ok(v1_content_to_item(
                    &self.http.profile.base_url,
                    item,
                    labels,
                    properties,
                ))
            }
        }
    }

    async fn update_content(&self, request: &UpdateContentRequest) -> Result<ContentItem> {
        ensure_writable(&self.http.profile)?;
        match request.kind {
            ContentKind::Page => {
                let _: V2Page = self
                    .http
                    .json(
                        Method::PUT,
                        self.http.v2_url(&format!("/pages/{}", request.id)),
                        Some(json!({
                            "id": request.id,
                            "status": request.status,
                            "title": request.title,
                            "parentId": request.parent_id,
                            "body": {
                                "representation": "storage",
                                "value": request.body_storage,
                            },
                            "version": {
                                "number": request.version + 1,
                                "message": request.message,
                            }
                        })),
                    )
                    .await?;
            }
            ContentKind::BlogPost => {
                let mut body = json!({
                    "id": request.id,
                    "type": request.kind.as_str(),
                    "title": request.title,
                    "status": request.status,
                    "version": {
                        "number": request.version + 1,
                        "message": request.message
                    },
                    "body": {
                        "storage": {
                            "value": request.body_storage,
                            "representation": "storage"
                        }
                    }
                });
                if let Some(parent_id) = &request.parent_id {
                    body["ancestors"] = json!([{ "id": parent_id }]);
                }
                let _: V1Content = self
                    .http
                    .json(
                        Method::PUT,
                        self.http.v1_url(&format!("/content/{}", request.id)),
                        Some(body),
                    )
                    .await?;
            }
        }

        let current_labels = self.list_labels(&request.id).await.unwrap_or_default();
        for label in current_labels
            .iter()
            .filter(|label| !request.labels.contains(*label))
        {
            let _ = self.remove_label(&request.id, label).await;
        }
        for label in request
            .labels
            .iter()
            .filter(|label| !current_labels.contains(*label))
        {
            let _ = self.add_label(&request.id, label).await;
        }

        let current_properties = self.list_properties(&request.id).await.unwrap_or_default();
        let current_map: BTreeMap<_, _> = current_properties
            .into_iter()
            .map(|property| (property.key, property.value))
            .collect();
        for (key, value) in &request.properties {
            if current_map.get(key) != Some(value) {
                let _ = self.set_property(&request.id, key, value.clone()).await;
            }
        }
        for key in current_map
            .keys()
            .filter(|key| !request.properties.contains_key(*key))
        {
            let _ = self.delete_property(&request.id, key).await;
        }

        self.get_content(request.kind, &request.id, true).await
    }

    async fn delete_content(&self, kind: ContentKind, id: &str) -> Result<()> {
        ensure_writable(&self.http.profile)?;
        match kind {
            ContentKind::Page => {
                self.http
                    .empty(
                        Method::DELETE,
                        self.http.v2_url(&format!("/pages/{id}")),
                        None,
                    )
                    .await
            }
            ContentKind::BlogPost => {
                self.http
                    .empty(
                        Method::DELETE,
                        self.http.v1_url(&format!("/content/{id}")),
                        None,
                    )
                    .await
            }
        }
    }

    async fn list_attachments(&self, content_id: &str) -> Result<Vec<AttachmentInfo>> {
        let response: Results<V1Attachment> = self
            .http
            .json(
                Method::GET,
                self.http.v1_url(&format!(
                    "/content/{content_id}/child/attachment?limit=200&expand=metadata,extensions"
                )),
                None,
            )
            .await?;
        Ok(response
            .results
            .into_iter()
            .map(|attachment| self.map_attachment(attachment))
            .collect())
    }

    async fn download_attachment(&self, content_id: &str, attachment_id: &str) -> Result<Bytes> {
        self.http
            .bytes(
                Method::GET,
                self.http.v1_url(&format!(
                    "/content/{content_id}/child/attachment/{attachment_id}/download"
                )),
            )
            .await
    }

    async fn upload_attachment(
        &self,
        content_id: &str,
        path: &Path,
        comment: Option<&str>,
        replace: bool,
        minor_edit: bool,
    ) -> Result<AttachmentInfo> {
        ensure_writable(&self.http.profile)?;
        let bytes = fs::read(path).await?;
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow!("attachment path must contain a file name"))?;

        let existing = if replace {
            self.attachment_by_name(content_id, file_name).await?
        } else {
            None
        };
        let replacing_existing = existing.is_some();
        let endpoint = if let Some(existing) = &existing {
            self.http.v1_url(&format!(
                "/content/{content_id}/child/attachment/{}/data",
                existing.id
            ))
        } else {
            self.http
                .v1_url(&format!("/content/{content_id}/child/attachment"))
        };

        let part = Part::bytes(bytes)
            .file_name(file_name.to_string())
            .mime_str(
                mime_guess::from_path(path)
                    .first_or_octet_stream()
                    .essence_str(),
            )?;
        let mut form = Form::new().part("file", part);
        if let Some(comment) = comment {
            form = form.text("comment", comment.to_string());
        }
        form = form.text("minorEdit", minor_edit.to_string());

        let response = self
            .http
            .send(
                &endpoint,
                self.http.auth(
                    self.http
                        .raw_client()
                        .post(endpoint.clone())
                        .header("X-Atlassian-Token", "nocheck")
                        .multipart(form),
                ),
            )
            .await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("attachment upload failed with {status}: {body}");
        }
        if replacing_existing {
            let attachment: V1Attachment = response.json().await?;
            Ok(self.map_attachment(attachment))
        } else {
            let payload: Results<V1Attachment> = response.json().await?;
            payload
                .results
                .into_iter()
                .next()
                .map(|attachment| self.map_attachment(attachment))
                .ok_or_else(|| anyhow!("attachment upload returned no attachment metadata"))
        }
    }

    async fn delete_attachment(&self, _content_id: &str, attachment_id: &str) -> Result<()> {
        ensure_writable(&self.http.profile)?;
        self.http
            .empty(
                Method::DELETE,
                self.http.v2_url(&format!("/attachments/{attachment_id}")),
                None,
            )
            .await
    }

    async fn list_labels(&self, content_id: &str) -> Result<Vec<String>> {
        self.labels_for(content_id).await
    }

    async fn add_label(&self, content_id: &str, label: &str) -> Result<()> {
        ensure_writable(&self.http.profile)?;
        self.http
            .empty(
                Method::POST,
                self.http.v1_url(&format!("/content/{content_id}/label")),
                Some(json!([{ "prefix": "global", "name": label }])),
            )
            .await
    }

    async fn remove_label(&self, content_id: &str, label: &str) -> Result<()> {
        ensure_writable(&self.http.profile)?;
        self.http
            .empty(
                Method::DELETE,
                self.http.v1_url(&format!(
                    "/content/{content_id}/label?name={}",
                    urlencoding::encode(label)
                )),
                None,
            )
            .await
    }

    async fn list_comments(&self, content_id: &str) -> Result<Vec<CommentInfo>> {
        let response: Results<V1Comment> = self
            .http
            .json(
                Method::GET,
                self.http.v1_url(&format!(
                    "/content/{content_id}/child/comment?limit=200&expand=body.storage,history"
                )),
                None,
            )
            .await?;
        Ok(response
            .results
            .into_iter()
            .map(|comment| self.map_comment(comment))
            .collect())
    }

    async fn add_comment(&self, content_id: &str, text: &str) -> Result<CommentInfo> {
        ensure_writable(&self.http.profile)?;
        let comment: V1Comment = self
            .http
            .json(
                Method::POST,
                self.http.v1_url("/content"),
                Some(json!({
                    "type": "comment",
                    "container": { "type": "page", "id": content_id },
                    "body": { "storage": { "value": text, "representation": "storage" } }
                })),
            )
            .await?;
        Ok(self.map_comment(comment))
    }

    async fn update_comment(&self, comment_id: &str, text: &str) -> Result<CommentInfo> {
        ensure_writable(&self.http.profile)?;
        let current: V1Comment = self
            .http
            .json(
                Method::GET,
                self.http.v1_url(&format!(
                    "/content/{comment_id}?expand=body.storage,version"
                )),
                None,
            )
            .await?;
        let version = current
            .version
            .as_ref()
            .map(|v| v.number)
            .ok_or_else(|| anyhow!("comment version unavailable"))?;
        let updated: V1Comment = self
            .http
            .json(
                Method::PUT,
                self.http.v1_url(&format!("/content/{comment_id}")),
                Some(json!({
                    "type": "comment",
                    "version": { "number": version + 1 },
                    "body": { "storage": { "value": text, "representation": "storage" } }
                })),
            )
            .await?;
        Ok(self.map_comment(updated))
    }

    async fn delete_comment(&self, comment_id: &str) -> Result<()> {
        ensure_writable(&self.http.profile)?;
        self.http
            .empty(
                Method::DELETE,
                self.http.v1_url(&format!("/content/{comment_id}")),
                None,
            )
            .await
    }

    async fn list_properties(&self, content_id: &str) -> Result<Vec<ContentProperty>> {
        self.properties_for(content_id).await
    }

    async fn get_property(&self, content_id: &str, key: &str) -> Result<Option<ContentProperty>> {
        let response = self
            .http
            .send(
                &self
                    .http
                    .v1_url(&format!("/content/{content_id}/property/{key}")),
                self.http.auth(
                    self.http.raw_client().get(
                        self.http
                            .v1_url(&format!("/content/{content_id}/property/{key}")),
                    ),
                ),
            )
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let response = response.error_for_status()?;
        let property: V1Property = response.json().await?;
        Ok(Some(ContentProperty {
            id: property.id,
            key: property.key,
            value: property.value,
            version: property.version.map(|version| version.number),
        }))
    }

    async fn set_property(
        &self,
        content_id: &str,
        key: &str,
        value: Value,
    ) -> Result<ContentProperty> {
        ensure_writable(&self.http.profile)?;
        let existing = self.get_property(content_id, key).await?;
        let (method, url, body) = if let Some(existing) = existing.as_ref() {
            (
                Method::PUT,
                self.http
                    .v1_url(&format!("/content/{content_id}/property/{key}")),
                property_payload(key, value.clone(), existing.version),
            )
        } else {
            (
                Method::POST,
                self.http.v1_url(&format!("/content/{content_id}/property")),
                property_payload(key, value.clone(), None),
            )
        };
        let property: V1Property = self.http.json(method, url, Some(body)).await?;
        Ok(ContentProperty {
            id: property.id,
            key: property.key,
            value: property.value,
            version: property.version.map(|version| version.number),
        })
    }

    async fn delete_property(&self, content_id: &str, key: &str) -> Result<()> {
        ensure_writable(&self.http.profile)?;
        self.http
            .empty(
                Method::DELETE,
                self.http
                    .v1_url(&format!("/content/{content_id}/property/{key}")),
                None,
            )
            .await
    }
}
