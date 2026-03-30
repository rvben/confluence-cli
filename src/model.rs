use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Cloud,
    DataCenter,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cloud => "cloud",
            Self::DataCenter => "data_center",
        }
    }
}

impl Display for ProviderKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ContentKind {
    Page,
    BlogPost,
}

impl ContentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Page => "page",
            Self::BlogPost => "blogpost",
        }
    }

    pub fn file_type(self) -> &'static str {
        match self {
            Self::Page => "page",
            Self::BlogPost => "blog",
        }
    }
}

impl Display for ContentKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpaceSummary {
    pub id: String,
    pub key: String,
    pub name: String,
    pub space_type: Option<String>,
    pub homepage_id: Option<String>,
    pub web_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentItem {
    pub id: String,
    pub kind: ContentKind,
    pub title: String,
    pub status: String,
    pub space_id: Option<String>,
    pub space_key: Option<String>,
    pub parent_id: Option<String>,
    pub version: Option<u64>,
    pub body_storage: Option<String>,
    pub labels: Vec<String>,
    pub properties: BTreeMap<String, Value>,
    pub web_url: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub id: String,
    pub title: String,
    pub excerpt: Option<String>,
    pub kind: ContentKind,
    pub space_key: Option<String>,
    pub web_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentInfo {
    pub id: String,
    pub title: String,
    pub media_type: Option<String>,
    pub file_size: Option<u64>,
    pub download_url: Option<String>,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommentInfo {
    pub id: String,
    pub author: Option<String>,
    pub body_storage: String,
    pub created_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentProperty {
    pub id: Option<String>,
    pub key: String,
    pub value: Value,
    pub version: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateContentRequest {
    pub kind: ContentKind,
    pub title: String,
    pub space: String,
    pub parent_id: Option<String>,
    pub body_storage: String,
    pub status: String,
    pub labels: Vec<String>,
    pub properties: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateContentRequest {
    pub id: String,
    pub kind: ContentKind,
    pub title: String,
    pub parent_id: Option<String>,
    pub body_storage: String,
    pub version: u64,
    pub message: Option<String>,
    pub status: String,
    pub labels: Vec<String>,
    pub properties: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AttachmentState {
    pub id: String,
    pub file_name: String,
    pub media_type: Option<String>,
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalContentIndex {
    pub directory: PathBuf,
    pub markdown_path: PathBuf,
    pub sidecar_path: PathBuf,
    pub title: String,
    pub kind: ContentKind,
    pub parent_directory: Option<PathBuf>,
    pub content_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanActionKind {
    CreateContent,
    UpdateContent,
    MoveContent,
    UploadAttachment,
    DeleteAttachment,
    UpdateLabels,
    UpdateProperties,
    DeleteRemote,
    Noop,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanItem {
    pub action: PlanActionKind,
    pub title: String,
    pub content_id: Option<String>,
    pub path: PathBuf,
    pub details: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SyncPlan {
    pub items: Vec<PlanItem>,
}
