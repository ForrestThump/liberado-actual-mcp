use reqwest::Client;
use std::time::Duration;

use crate::models::{ApiResponse, LoginData, UserFile};

pub struct ActualClient {
    client: Client,
    pub server_url: String,
}

impl ActualClient {
    pub fn new(server_url: String) -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("failed to build HTTP client"),
            server_url,
        }
    }

    pub async fn login(&self, password: &str) -> anyhow::Result<String> {
        let url = format!("{}/account/login", self.server_url);
        let resp: ApiResponse<LoginData> = self
            .client
            .post(&url)
            .json(&serde_json::json!({ "password": password }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp.data.token)
    }

    pub async fn list_files(&self, token: &str) -> anyhow::Result<Vec<UserFile>> {
        let url = format!("{}/list-user-files", self.server_url);
        let resp: ApiResponse<Vec<UserFile>> = self
            .client
            .get(&url)
            .header("x-actual-token", token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp.data.into_iter().filter(|f| !f.deleted).collect())
    }

    pub async fn download_file(&self, token: &str, file_id: &str) -> anyhow::Result<Vec<u8>> {
        let url = format!("{}/download-user-file", self.server_url);
        let bytes = self
            .client
            .get(&url)
            .header("x-actual-token", token)
            .header("x-actual-file-id", file_id)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        Ok(bytes.to_vec())
    }
}

pub fn find_budget_file<'a>(files: &'a [UserFile], budget_id: Option<&str>) -> Option<&'a UserFile> {
    if let Some(id) = budget_id {
        files.iter().find(|f| f.file_id == id || f.name == id)
    } else {
        files.first()
    }
}

pub const SQLITE_MAGIC: &[u8] = b"SQLite format 3\x00";

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_files(ids: &[(&str, &str, bool)]) -> Vec<UserFile> {
        ids.iter()
            .map(|(id, name, deleted)| UserFile {
                file_id: id.to_string(),
                name: name.to_string(),
                deleted: *deleted,
                encrypt_key_id: None,
            })
            .collect()
    }

    #[test]
    fn find_by_file_id() {
        let files = make_files(&[("abc", "Budget A", false), ("def", "Budget B", false)]);
        let f = find_budget_file(&files, Some("abc")).unwrap();
        assert_eq!(f.file_id, "abc");
    }

    #[test]
    fn find_by_name() {
        let files = make_files(&[("abc", "My Budget", false)]);
        let f = find_budget_file(&files, Some("My Budget")).unwrap();
        assert_eq!(f.name, "My Budget");
    }

    #[test]
    fn find_first_when_no_id_given() {
        let files = make_files(&[("first", "First", false), ("second", "Second", false)]);
        let f = find_budget_file(&files, None).unwrap();
        assert_eq!(f.file_id, "first");
    }

    #[test]
    fn returns_none_on_empty_list() {
        let files: Vec<UserFile> = vec![];
        assert!(find_budget_file(&files, None).is_none());
    }

    #[test]
    fn sqlite_magic_matches_real_header() {
        let header = b"SQLite format 3\x00some more bytes";
        assert!(header.starts_with(SQLITE_MAGIC));
    }

    #[test]
    fn sqlite_magic_rejects_encrypted() {
        let encrypted = b"\xde\xad\xbe\xef\x00\x01\x02\x03other bytes";
        assert!(!encrypted.starts_with(SQLITE_MAGIC));
    }
}
