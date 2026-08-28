use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const SPACE_ID: &str = "1";
const SPACE_KEY: &str = "TEST";
const CLOUD_AUTHORIZATION: &str = "Basic c2ltdWxhdG9yQGV4YW1wbGUudGVzdDpzaW11bGF0ZWQtdG9rZW4=";
const DATA_CENTER_AUTHORIZATION: &str = "Bearer simulated-token";

pub struct ConfluenceSimulator {
    server: MockServer,
}

impl ConfluenceSimulator {
    pub async fn start() -> Self {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(SimulatorResponder::default())
            .mount(&server)
            .await;
        Self { server }
    }

    pub fn base_url(&self) -> String {
        self.server.uri()
    }
}

#[derive(Clone, Default)]
struct SimulatorResponder {
    state: Arc<Mutex<SimulatorState>>,
}

struct SimulatorState {
    next_id: u64,
    content: BTreeMap<String, SimulatedContent>,
    comments: BTreeMap<String, SimulatedComment>,
    attachments: BTreeMap<String, SimulatedAttachment>,
}

impl Default for SimulatorState {
    fn default() -> Self {
        let homepage = SimulatedContent {
            id: "100".to_string(),
            content_type: "page".to_string(),
            title: "Overview".to_string(),
            status: "current".to_string(),
            parent_id: None,
            version: 1,
            body_storage: "<p>Simulator homepage</p>".to_string(),
            labels: BTreeSet::new(),
            properties: BTreeMap::new(),
        };
        Self {
            next_id: 0,
            content: BTreeMap::from([(homepage.id.clone(), homepage)]),
            comments: BTreeMap::new(),
            attachments: BTreeMap::new(),
        }
    }
}

#[derive(Clone)]
struct SimulatedContent {
    id: String,
    content_type: String,
    title: String,
    status: String,
    parent_id: Option<String>,
    version: u64,
    body_storage: String,
    labels: BTreeSet<String>,
    properties: BTreeMap<String, SimulatedProperty>,
}

#[derive(Clone)]
struct SimulatedProperty {
    value: Value,
    version: u64,
}

#[derive(Clone)]
struct SimulatedComment {
    id: String,
    container_id: String,
    body_storage: String,
    version: u64,
}

#[derive(Clone)]
struct SimulatedAttachment {
    id: String,
    content_id: String,
    title: String,
    version: u64,
    media_type: String,
    bytes: Vec<u8>,
    comment: Option<String>,
}

impl Respond for SimulatorResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let method = request.method.as_str();
        let path = request.url.path();
        let authorized = request
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| matches!(value, CLOUD_AUTHORIZATION | DATA_CENTER_AUTHORIZATION));
        if !authorized {
            return ResponseTemplate::new(401).set_body_json(json!({
                "statusCode": 401,
                "message": "Authentication failed"
            }));
        }
        let Some((api, resource)) = split_api_path(path) else {
            return not_implemented(method, path);
        };
        let mut state = self.state.lock().expect("simulator state lock");

        match (method, api, resource) {
            ("GET", "v1", "/user/current") => ResponseTemplate::new(200).set_body_json(json!({
                "accountId": "simulator-account-id",
                "userKey": "simulator-user-key",
                "username": "simulator",
                "displayName": "Simulator User"
            })),
            ("GET", "v1", "/space") => results_response(vec![json!({
                "id": SPACE_ID,
                "key": SPACE_KEY,
                "name": "Simulator",
                "type": "global",
                "homepage": { "id": "100" },
                "_links": { "webui": "/spaces/TEST" }
            })]),
            ("GET", "v1", "/content/search") => results_response(
                state
                    .content
                    .values()
                    .map(v1_content_json)
                    .collect::<Vec<_>>(),
            ),
            ("POST", "v1", "/content") => create_v1_content(&mut state, request),
            ("PUT", "v1", resource)
                if content_resource(resource).is_some_and(|(_, tail)| tail.is_empty()) =>
            {
                let Some((id, "")) = content_resource(resource) else {
                    return not_found();
                };
                update_v1_content(&mut state, id, request)
            }
            ("POST", "v2", "/pages") => create_page(&mut state, request),
            ("GET", "v2", resource) if resource.starts_with("/pages/") => {
                content_id(resource, "/pages/")
                    .and_then(|id| state.content.get(id))
                    .map(v2_page_response)
                    .unwrap_or_else(not_found)
            }
            ("PUT", "v2", resource) if resource.starts_with("/pages/") => {
                let Some(id) = content_id(resource, "/pages/") else {
                    return not_found();
                };
                update_page(&mut state, id, request)
            }
            ("DELETE", "v2", resource) if resource.starts_with("/pages/") => {
                let Some(id) = content_id(resource, "/pages/") else {
                    return not_found();
                };
                if state.content.remove(id).is_some() {
                    ResponseTemplate::new(204)
                } else {
                    not_found()
                }
            }
            ("DELETE", "v2", resource) if resource.starts_with("/attachments/") => {
                let Some(id) = content_id(resource, "/attachments/") else {
                    return not_found();
                };
                if state.attachments.remove(id).is_some() {
                    ResponseTemplate::new(204)
                } else {
                    not_found()
                }
            }
            ("DELETE", "v1", resource) if resource.contains("/child/attachment/") => {
                let Some((content_id, tail)) = content_resource(resource) else {
                    return not_found();
                };
                let Some(attachment_id) = tail.strip_prefix("/child/attachment/") else {
                    return not_found();
                };
                if state
                    .attachments
                    .get(attachment_id)
                    .is_some_and(|attachment| attachment.content_id == content_id)
                {
                    state.attachments.remove(attachment_id);
                    ResponseTemplate::new(204)
                } else {
                    not_found()
                }
            }
            ("DELETE", "v1", resource)
                if content_resource(resource).is_some_and(|(_, tail)| tail.is_empty()) =>
            {
                let Some((id, "")) = content_resource(resource) else {
                    return not_found();
                };
                if state.comments.remove(id).is_some() || state.content.remove(id).is_some() {
                    ResponseTemplate::new(204)
                } else {
                    not_found()
                }
            }
            ("POST", "v1", resource) if resource.ends_with("/label") => {
                let Some((id, "/label")) = content_resource(resource) else {
                    return not_found();
                };
                add_labels(&mut state, id, request)
            }
            ("DELETE", "v1", resource) if resource.ends_with("/label") => {
                let Some((id, "/label")) = content_resource(resource) else {
                    return not_found();
                };
                let Some(content) = state.content.get_mut(id) else {
                    return not_found();
                };
                if let Some(label) = request
                    .url
                    .query_pairs()
                    .find_map(|(key, value)| (key == "name").then(|| value.into_owned()))
                {
                    content.labels.remove(&label);
                }
                ResponseTemplate::new(204)
            }
            ("POST", "v1", resource) if resource.ends_with("/child/attachment") => {
                let Some((id, "/child/attachment")) = content_resource(resource) else {
                    return not_found();
                };
                upload_attachment(&mut state, id, None, request)
            }
            ("POST", "v1", resource) if resource.ends_with("/data") => {
                let Some((content_id, tail)) = content_resource(resource) else {
                    return not_found();
                };
                let Some(attachment_id) = tail
                    .strip_prefix("/child/attachment/")
                    .and_then(|tail| tail.strip_suffix("/data"))
                else {
                    return not_found();
                };
                upload_attachment(&mut state, content_id, Some(attachment_id), request)
            }
            ("POST", "v1", resource) if resource.ends_with("/property") => {
                let Some((id, "/property")) = content_resource(resource) else {
                    return not_found();
                };
                set_property(&mut state, id, None, request)
            }
            ("PUT", "v1", resource) if resource.contains("/property/") => {
                let Some((id, key)) = property_resource(resource) else {
                    return not_found();
                };
                set_property(&mut state, id, Some(key), request)
            }
            ("DELETE", "v1", resource) if resource.contains("/property/") => {
                let Some((id, key)) = property_resource(resource) else {
                    return not_found();
                };
                let Some(content) = state.content.get_mut(id) else {
                    return not_found();
                };
                content.properties.remove(key);
                ResponseTemplate::new(204)
            }
            ("GET", "v1", resource) if resource.starts_with("/content/") => {
                let Some((id, tail)) = content_resource(resource) else {
                    return not_found();
                };
                if tail == "/child/comment" {
                    return results_response(
                        state
                            .comments
                            .values()
                            .filter(|comment| comment.container_id == id)
                            .map(comment_json)
                            .collect(),
                    );
                }
                if tail == "/child/page" {
                    return results_response(
                        state
                            .content
                            .values()
                            .filter(|content| {
                                content.content_type == "page"
                                    && content.parent_id.as_deref() == Some(id)
                            })
                            .map(v1_content_json)
                            .collect(),
                    );
                }
                if tail == "/child/attachment" {
                    return results_response(
                        state
                            .attachments
                            .values()
                            .filter(|attachment| attachment.content_id == id)
                            .map(attachment_json)
                            .collect(),
                    );
                }
                if let Some(attachment_id) = tail
                    .strip_prefix("/child/attachment/")
                    .and_then(|tail| tail.strip_suffix("/download"))
                {
                    return state
                        .attachments
                        .get(attachment_id)
                        .filter(|attachment| attachment.content_id == id)
                        .map(|attachment| {
                            ResponseTemplate::new(200)
                                .insert_header("content-type", attachment.media_type.as_str())
                                .set_body_bytes(attachment.bytes.clone())
                        })
                        .unwrap_or_else(not_found);
                }
                let Some(content) = state.content.get(id) else {
                    return not_found();
                };
                match tail {
                    "/label" => results_response(
                        content
                            .labels
                            .iter()
                            .map(|name| json!({ "name": name }))
                            .collect(),
                    ),
                    "/property" => results_response(
                        content
                            .properties
                            .iter()
                            .map(|(key, property)| property_json(id, key, property))
                            .collect(),
                    ),
                    tail if tail.starts_with("/property/") => {
                        let key = tail.trim_start_matches("/property/");
                        content
                            .properties
                            .get(key)
                            .map(|property| {
                                ResponseTemplate::new(200)
                                    .set_body_json(property_json(id, key, property))
                            })
                            .unwrap_or_else(not_found)
                    }
                    "" => v1_content_response(content),
                    _ => not_implemented(method, path),
                }
            }
            _ => not_implemented(method, path),
        }
    }
}

fn split_api_path(path: &str) -> Option<(&'static str, &str)> {
    for (prefix, api) in [
        ("/wiki/rest/api", "v1"),
        ("/wiki/api/v2", "v2"),
        ("/rest/api", "v1"),
    ] {
        if let Some(resource) = path.strip_prefix(prefix) {
            return Some((api, resource));
        }
    }
    None
}

fn content_id<'a>(resource: &'a str, prefix: &str) -> Option<&'a str> {
    resource
        .strip_prefix(prefix)
        .and_then(|rest| rest.split('/').next())
        .filter(|id| !id.is_empty())
}

fn content_resource(resource: &str) -> Option<(&str, &str)> {
    let rest = resource.strip_prefix("/content/")?;
    let split = rest.find('/').unwrap_or(rest.len());
    Some((&rest[..split], &rest[split..]))
}

fn create_page(state: &mut SimulatorState, request: &Request) -> ResponseTemplate {
    let body: Value = match request.body_json() {
        Ok(body) => body,
        Err(error) => return bad_request(&format!("invalid JSON body: {error}")),
    };
    state.next_id += 1;
    let id = (1000 + state.next_id).to_string();
    let content = SimulatedContent {
        id: id.clone(),
        content_type: "page".to_string(),
        title: json_string(&body, "title"),
        status: json_string(&body, "status"),
        parent_id: body
            .get("parentId")
            .and_then(Value::as_str)
            .map(str::to_string),
        version: 1,
        body_storage: body
            .pointer("/body/value")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        labels: BTreeSet::new(),
        properties: BTreeMap::new(),
    };
    let response = v2_page_response(&content);
    state.content.insert(id, content);
    response
}

fn create_v1_content(state: &mut SimulatorState, request: &Request) -> ResponseTemplate {
    let body: Value = match request.body_json() {
        Ok(body) => body,
        Err(error) => return bad_request(&format!("invalid content body: {error}")),
    };
    match body.get("type").and_then(Value::as_str) {
        Some("comment") => create_comment(state, &body),
        Some("page" | "blogpost") => create_v1_page_or_blog(state, &body),
        Some(other) => bad_request(&format!("unsupported v1 content type: {other}")),
        None => bad_request("content type is required"),
    }
}

fn create_v1_page_or_blog(state: &mut SimulatorState, body: &Value) -> ResponseTemplate {
    state.next_id += 1;
    let id = (1000 + state.next_id).to_string();
    let content = SimulatedContent {
        id: id.clone(),
        content_type: json_string(body, "type"),
        title: json_string(body, "title"),
        status: json_string(body, "status"),
        parent_id: body
            .get("ancestors")
            .and_then(Value::as_array)
            .and_then(|ancestors| ancestors.last())
            .and_then(|ancestor| ancestor.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string),
        version: 1,
        body_storage: body
            .pointer("/body/storage/value")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        labels: BTreeSet::new(),
        properties: BTreeMap::new(),
    };
    let response = v1_content_response(&content);
    state.content.insert(id, content);
    response
}

fn update_v1_content(state: &mut SimulatorState, id: &str, request: &Request) -> ResponseTemplate {
    let body: Value = match request.body_json() {
        Ok(body) => body,
        Err(error) => return bad_request(&format!("invalid content body: {error}")),
    };
    let Some(content) = state.content.get_mut(id) else {
        return not_found();
    };
    content.content_type = json_string(&body, "type");
    content.title = json_string(&body, "title");
    content.status = json_string(&body, "status");
    content.parent_id = body
        .get("ancestors")
        .and_then(Value::as_array)
        .and_then(|ancestors| ancestors.last())
        .and_then(|ancestor| ancestor.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string);
    content.version = body
        .pointer("/version/number")
        .and_then(Value::as_u64)
        .unwrap_or(content.version + 1);
    content.body_storage = body
        .pointer("/body/storage/value")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    v1_content_response(content)
}

fn create_comment(state: &mut SimulatorState, body: &Value) -> ResponseTemplate {
    state.next_id += 1;
    let id = (1000 + state.next_id).to_string();
    let comment = SimulatedComment {
        id: id.clone(),
        container_id: body
            .pointer("/container/id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        body_storage: body
            .pointer("/body/storage/value")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        version: 1,
    };
    let response = ResponseTemplate::new(200).set_body_json(comment_json(&comment));
    state.comments.insert(id, comment);
    response
}

fn comment_json(comment: &SimulatedComment) -> Value {
    json!({
        "id": comment.id,
        "body": { "storage": { "value": comment.body_storage } },
        "version": { "number": comment.version },
        "history": {
            "createdDate": "2026-01-01T00:00:00Z",
            "createdBy": { "displayName": "Simulator User" }
        }
    })
}

fn upload_attachment(
    state: &mut SimulatorState,
    content_id: &str,
    existing_id: Option<&str>,
    request: &Request,
) -> ResponseTemplate {
    if !state.content.contains_key(content_id) {
        return not_found();
    }
    let Some(upload) = parse_multipart_upload(request) else {
        return bad_request("multipart request did not contain a file");
    };
    let replacing = existing_id.is_some();
    let id = if let Some(existing_id) = existing_id {
        existing_id.to_string()
    } else {
        state.next_id += 1;
        (1000 + state.next_id).to_string()
    };
    let version = state
        .attachments
        .get(&id)
        .map(|attachment| attachment.version + 1)
        .unwrap_or(1);
    let attachment = SimulatedAttachment {
        id: id.clone(),
        content_id: content_id.to_string(),
        title: upload.file_name,
        version,
        media_type: upload.media_type,
        bytes: upload.bytes,
        comment: upload.comment,
    };
    let payload = attachment_json(&attachment);
    state.attachments.insert(id, attachment);
    if replacing {
        ResponseTemplate::new(200).set_body_json(payload)
    } else {
        results_response(vec![payload])
    }
}

struct MultipartUpload {
    file_name: String,
    media_type: String,
    bytes: Vec<u8>,
    comment: Option<String>,
}

fn parse_multipart_upload(request: &Request) -> Option<MultipartUpload> {
    let content_type = request.headers.get("content-type")?.to_str().ok()?;
    let boundary = content_type.split("boundary=").nth(1)?.trim_matches('"');
    let delimiter = format!("--{boundary}").into_bytes();
    let mut file_name = None;
    let mut media_type = "application/octet-stream".to_string();
    let mut bytes = None;
    let mut comment = None;

    for part in split_bytes(&request.body, &delimiter) {
        let Some(header_end) = find_bytes(part, b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&part[..header_end]);
        let mut body = &part[header_end + 4..];
        body = body.strip_suffix(b"\r\n").unwrap_or(body);
        if headers.contains("name=\"file\"") {
            file_name = header_parameter(&headers, "filename");
            if let Some(value) = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-type: "))
            {
                media_type = value.trim().to_string();
            }
            bytes = Some(body.to_vec());
        } else if headers.contains("name=\"comment\"") {
            comment = Some(String::from_utf8_lossy(body).into_owned());
        }
    }

    Some(MultipartUpload {
        file_name: file_name?,
        media_type,
        bytes: bytes?,
        comment,
    })
}

fn split_bytes<'a>(bytes: &'a [u8], delimiter: &[u8]) -> Vec<&'a [u8]> {
    let mut parts = Vec::new();
    let mut start = 0;
    while let Some(offset) = find_bytes(&bytes[start..], delimiter) {
        let end = start + offset;
        if end > start {
            parts.push(&bytes[start..end]);
        }
        start = end + delimiter.len();
    }
    if start < bytes.len() {
        parts.push(&bytes[start..]);
    }
    parts
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn header_parameter(headers: &str, name: &str) -> Option<String> {
    let marker = format!("{name}=\"");
    let value = headers.split(&marker).nth(1)?;
    Some(value.split('"').next()?.to_string())
}

fn attachment_json(attachment: &SimulatedAttachment) -> Value {
    json!({
        "id": attachment.id,
        "title": attachment.title,
        "version": { "number": attachment.version },
        "metadata": {
            "mediaType": attachment.media_type,
            "comment": attachment.comment
        },
        "extensions": { "fileSize": attachment.bytes.len() },
        "_links": {
            "download": format!(
                "/wiki/rest/api/content/{}/child/attachment/{}/download",
                attachment.content_id, attachment.id
            )
        }
    })
}

fn property_resource(resource: &str) -> Option<(&str, &str)> {
    let (id, tail) = content_resource(resource)?;
    tail.strip_prefix("/property/").map(|key| (id, key))
}

fn set_property(
    state: &mut SimulatorState,
    id: &str,
    path_key: Option<&str>,
    request: &Request,
) -> ResponseTemplate {
    let body: Value = match request.body_json() {
        Ok(body) => body,
        Err(error) => return bad_request(&format!("invalid property body: {error}")),
    };
    let Some(content) = state.content.get_mut(id) else {
        return not_found();
    };
    let key = path_key
        .or_else(|| body.get("key").and_then(Value::as_str))
        .unwrap_or_default()
        .to_string();
    let version = body
        .pointer("/version/number")
        .and_then(Value::as_u64)
        .unwrap_or(1);
    let property = SimulatedProperty {
        value: body.get("value").cloned().unwrap_or(Value::Null),
        version,
    };
    let response = ResponseTemplate::new(200).set_body_json(property_json(id, &key, &property));
    content.properties.insert(key, property);
    response
}

fn property_json(id: &str, key: &str, property: &SimulatedProperty) -> Value {
    json!({
        "id": format!("{id}:{key}"),
        "key": key,
        "value": property.value,
        "version": { "number": property.version }
    })
}

fn add_labels(state: &mut SimulatorState, id: &str, request: &Request) -> ResponseTemplate {
    let labels: Vec<Value> = match request.body_json() {
        Ok(labels) => labels,
        Err(error) => return bad_request(&format!("invalid labels body: {error}")),
    };
    let Some(content) = state.content.get_mut(id) else {
        return not_found();
    };
    for label in labels {
        if let Some(name) = label.get("name").and_then(Value::as_str) {
            content.labels.insert(name.to_string());
        }
    }
    ResponseTemplate::new(200).set_body_json(json!({}))
}

fn update_page(state: &mut SimulatorState, id: &str, request: &Request) -> ResponseTemplate {
    let body: Value = match request.body_json() {
        Ok(body) => body,
        Err(error) => return bad_request(&format!("invalid JSON body: {error}")),
    };
    let Some(content) = state.content.get_mut(id) else {
        return not_found();
    };
    content.title = json_string(&body, "title");
    content.status = json_string(&body, "status");
    content.parent_id = body
        .get("parentId")
        .and_then(Value::as_str)
        .map(str::to_string);
    content.version = body
        .pointer("/version/number")
        .and_then(Value::as_u64)
        .unwrap_or(content.version + 1);
    content.body_storage = body
        .pointer("/body/value")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    v2_page_response(content)
}

fn json_string(body: &Value, key: &str) -> String {
    body.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn v2_page_response(content: &SimulatedContent) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": content.id,
        "status": content.status,
        "title": content.title,
        "spaceId": SPACE_ID,
        "parentId": content.parent_id,
        "version": {
            "number": content.version,
            "createdAt": "2026-01-01T00:00:00Z"
        },
        "body": { "storage": { "value": content.body_storage } },
        "_links": { "webui": format!("/spaces/TEST/pages/{}", content.id) }
    }))
}

fn v1_content_response(content: &SimulatedContent) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(v1_content_json(content))
}

fn v1_content_json(content: &SimulatedContent) -> Value {
    json!({
        "id": content.id,
        "type": content.content_type,
        "title": content.title,
        "status": content.status,
        "space": { "id": SPACE_ID, "key": SPACE_KEY },
        "version": { "number": content.version, "when": "2026-01-01T00:00:00Z" },
        "ancestors": content.parent_id.iter().map(|id| json!({ "id": id })).collect::<Vec<_>>(),
        "body": { "storage": { "value": content.body_storage } },
        "history": {
            "createdDate": "2026-01-01T00:00:00Z",
            "lastUpdated": { "when": "2026-01-01T00:00:00Z" }
        },
        "_links": { "webui": format!("/spaces/TEST/pages/{}", content.id) }
    })
}

fn results_response(results: Vec<Value>) -> ResponseTemplate {
    let total = results.len();
    ResponseTemplate::new(200).set_body_json(json!({
        "results": results,
        "limit": 200,
        "totalSize": total,
        "_links": {}
    }))
}

fn bad_request(message: &str) -> ResponseTemplate {
    ResponseTemplate::new(400).set_body_json(json!({ "message": message }))
}

fn not_found() -> ResponseTemplate {
    ResponseTemplate::new(404).set_body_json(json!({ "message": "not found" }))
}

fn not_implemented(method: &str, path: &str) -> ResponseTemplate {
    ResponseTemplate::new(501).set_body_json(json!({
        "message": format!("simulator route not implemented: {method} {path}")
    }))
}
