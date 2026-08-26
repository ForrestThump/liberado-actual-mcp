use anyhow::bail;
use reqwest::Client;
use std::time::Duration;

use crate::models::{ApiResponse, LoginData, UserFile};

fn check_response<T>(resp: ApiResponse<T>, context: &str) -> anyhow::Result<T> {
    if resp.status != "ok" {
        bail!("{context}: server returned status '{}'", resp.status);
    }
    resp.data.ok_or_else(|| anyhow::anyhow!("{context}: response data was null"))
}

pub struct ActualClient {
    client: Client,
    server_url: String,
}

impl ActualClient {
    pub fn new(server_url: String) -> anyhow::Result<Self> {
        let mut builder = Client::builder().timeout(Duration::from_secs(30));

        // Self-hosted Actual servers routinely sit behind a private CA or a self-signed cert, which
        // this client rejects by default with an opaque "error sending request". Two escape hatches,
        // in order of preference:
        //
        // ACTUAL_CA_CERT — path to a PEM cert/chain to trust. The correct fix: verification stays
        // on, we just teach the client about the private CA.
        //
        // ACTUAL_TLS_INSECURE — skip verification entirely. Opt-in and OFF by default because it
        // drops MITM protection; only reasonable for a private-LAN server you control. It is loud
        // in the logs on purpose, so a temporary workaround cannot quietly become permanent.
        if let Ok(path) = std::env::var("ACTUAL_CA_CERT") {
            let pem = std::fs::read(&path)
                .map_err(|e| anyhow::anyhow!("ACTUAL_CA_CERT: cannot read {path}: {e}"))?;
            let cert = reqwest::Certificate::from_pem(&pem)
                .map_err(|e| anyhow::anyhow!("ACTUAL_CA_CERT: {path} is not valid PEM: {e}"))?;
            tracing::info!(ca_cert = %path, "trusting private CA for the Actual server");
            builder = builder.add_root_certificate(cert);
        }

        let insecure = std::env::var("ACTUAL_TLS_INSECURE")
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);
        if insecure {
            tracing::warn!(
                "ACTUAL_TLS_INSECURE is set: TLS certificate verification is DISABLED for the \
                 Actual server. Traffic is encrypted but not authenticated, so this is only safe \
                 on a private network you control. Prefer ACTUAL_CA_CERT."
            );
            builder = builder.danger_accept_invalid_certs(true);
        }

        Ok(Self {
            client: builder.build()?,
            server_url: server_url.trim_end_matches('/').to_string(),
        })
    }

    pub async fn login(&self, password: &str) -> anyhow::Result<String> {
        let url = format!("{}/account/login", self.server_url);
        let response = self
            .client
            .post(&url)
            .json(&serde_json::json!({ "password": password }))
            .send()
            .await?;

        // Deliberately no `error_for_status()` here. Actual answers a failed login with HTTP 400
        // *and* a JSON body carrying the real cause (e.g. {"status":"error","reason":
        // "invalid-password"}). error_for_status() discards that body, so a simple wrong password
        // surfaced as an opaque "HTTP status client error (400 Bad Request)" and looked like a
        // protocol bug. Read the body first and report the server's own reason.
        let status = response.status();
        let body = response.text().await?;

        // The failure body parses cleanly as ApiResponse (status="error", data=null), so checking
        // only the parse result would report a generic "status 'error'" and still bury the cause.
        // Pull `reason` out of the raw body before anything else.
        if let Some(reason) = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("reason").and_then(|r| r.as_str()).map(str::to_owned))
        {
            bail!(
                "login rejected by the Actual server: {reason} (HTTP {status}). \
                 Check ACTUAL_PASSWORD."
            );
        }

        match serde_json::from_str::<ApiResponse<LoginData>>(&body) {
            Ok(parsed) => Ok(check_response(parsed, "login")?.token),
            Err(parse_err) => {
                bail!("login failed: HTTP {status}, unparseable response ({parse_err}): {body}")
            }
        }
    }

    pub async fn list_files(&self, token: &str) -> anyhow::Result<Vec<UserFile>> {
        let url = format!("{}/sync/list-user-files", self.server_url);
        let resp: ApiResponse<Vec<UserFile>> = self
            .client
            .get(&url)
            .header("x-actual-token", token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(check_response(resp, "list files")?
            .into_iter()
            .filter(|f| !f.deleted)
            .collect())
    }

    pub async fn download_file(&self, token: &str, file_id: &str) -> anyhow::Result<Vec<u8>> {
        let url = format!("{}/sync/download-user-file", self.server_url);
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

    #[test]
    fn new_trims_trailing_slash() {
        let client = ActualClient::new("http://localhost:5006/".to_string()).unwrap();
        assert_eq!(client.server_url, "http://localhost:5006");
    }

    #[test]
    fn new_trims_multiple_trailing_slashes() {
        let client = ActualClient::new("http://localhost:5006///".to_string()).unwrap();
        assert_eq!(client.server_url, "http://localhost:5006");
    }
}
