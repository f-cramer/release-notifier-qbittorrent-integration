use anyhow::{Result, anyhow, bail};
use log::{LevelFilter, debug, error, trace};
use reqwest::{Client, RequestBuilder, Response};
use serde::Deserialize;
use soup::{NodeExt, QueryBuilderExt, Soup};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, SystemTime};
use tokio::fs::{create_dir_all, read_dir, read_to_string, remove_file, rename};
use tokio::task::yield_now;
use tokio::time::sleep;

#[tokio::main]
async fn main() -> Result<()> {
    let config = init_config()?;

    let logging_level = config
        .logging
        .as_ref()
        .and_then(|logging| {
            logging
                .level
                .as_ref()
                .map(|level| LevelFilter::from_str(level))
        })
        .transpose()?
        .unwrap_or(LevelFilter::Info);
    init_log4rs(logging_level);

    debug!("{:?}", config);

    let read_path = &config.path;
    let read_path = Path::new(read_path);
    ensure_dir_exists(read_path).await?;

    let archive_path = &config.archive.path;
    let archive_path = Path::new(archive_path);
    ensure_dir_exists(archive_path).await?;

    let mut client = QbittorrentClient {
        client: Client::new(),
        sid: "".to_string(),
        configuration: config.qbittorrent,
    };

    loop {
        if let Err(e) = process_directory(read_path, archive_path, &mut client, &config.filters).await {
            error!("{}", e);
        }

        if let Some(retention_period) = config.archive.retention_period
            && let Err(e) = delete_old_files(archive_path, retention_period).await
        {
            error!("{}", e);
        }

        sleep(Duration::from_mins(1)).await;
    }
}

async fn process_directory(
    directory: &Path,
    archive_directory: &Path,
    client: &mut QbittorrentClient,
    filters: &[Filter],
) -> Result<()> {
    let mut entries = read_dir(directory).await?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        process_file(&path, archive_directory, client, filters).await?;
    }

    Ok(())
}

async fn process_file(
    path: &Path,
    archive_directory: &Path,
    client: &mut QbittorrentClient,
    filters: &[Filter],
) -> Result<()> {
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("No file name found in {}", path.display()))?;

    let content = read_to_string(path).await?;
    yield_now().await;
    let magnet_links = extract_magnet_links(&content, filters);

    debug!(
        "found {} magnet links in {}",
        magnet_links.len(),
        path.display()
    );

    if !magnet_links.is_empty() {
        let links = magnet_links.join("\r\n");

        client
            .intercepting_send(|client| {
                client
                    .post("api/v2/torrents/add")
                    .multipart(reqwest::multipart::Form::new().text("urls", links.clone()))
            })
            .await?
            .error_for_status()?;
    }

    let mut archive_path = PathBuf::from(archive_directory);
    archive_path.push(file_name);
    rename(&path, archive_path).await?;

    Ok(())
}

async fn delete_old_files(directory: &Path, retention_period: Duration) -> Result<()> {
    let mut entries = read_dir(directory).await?;
    let now = SystemTime::now();

    while let Some(entry) = entries.next_entry().await? {
        trace!("checking {:?}", entry.path());
        let metadata = entry.metadata().await?;
        if metadata.is_file() {
            let modified = metadata.modified()?;
            if now.duration_since(modified).unwrap_or(Duration::ZERO) > retention_period {
                let path = entry.path();
                debug!("deleting {:?}", path);
                remove_file(path).await?;
            }
        }
    }

    Ok(())
}

async fn get_sid(client: &Client, config: &QbittorrentConfiguration) -> Result<String> {
    let response = client
        .post(format!("{}api/v2/auth/login", config.url))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Referer", &config.url)
        .body(format!(
            "username={}&password={}",
            config.username, config.password
        ))
        .send()
        .await?
        .error_for_status()?;

    let sid = response
        .cookies()
        .find(|cookie| cookie.name() == "SID")
        .ok_or_else(|| anyhow!("No SID cookie found"))?
        .value()
        .to_string();
    Ok(sid)
}

async fn ensure_dir_exists(read_path: &Path) -> Result<()> {
    if read_path.is_file() {
        bail!("{} is not a directory", read_path.display());
    } else if !read_path.exists() {
        create_dir_all(read_path).await?;
    }
    Ok(())
}

fn init_log4rs(level: LevelFilter) {
    let config = log4rs::config::Config::builder()
        .appender(log4rs::config::Appender::builder().build(
            "stdout",
            Box::new(log4rs::append::console::ConsoleAppender::builder().build()),
        ))
        .build(
            log4rs::config::Root::builder()
                .appender("stdout")
                .build(level),
        )
        .unwrap();
    log4rs::init_config(config).unwrap();
}

fn init_config() -> Result<Configuration> {
    let config = config::Config::builder()
        .add_source(config::File::with_name("config"))
        .add_source(config::Environment::with_prefix("APP"))
        .build()?;
    Ok(config.try_deserialize::<Configuration>()?)
}

#[derive(Debug, Deserialize)]
struct Filter {
    terms: Vec<String>,
    #[serde(default)]
    case_sensitive: bool,
}

#[derive(Debug, Deserialize)]
struct Configuration {
    path: String,
    archive: ArchiveConfiguration,
    qbittorrent: QbittorrentConfiguration,
    #[serde(default)]
    logging: Option<LoggingConfiguration>,
    #[serde(default)]
    filters: Vec<Filter>,
}

fn extract_magnet_links(content: &str, filters: &[Filter]) -> Vec<String> {
    let soup = Soup::new(content);

    let body = match soup.tag("body").find() {
        Some(body) => body,
        None => return Vec::new(),
    };

    body.tag("ul")
        .find_all()
        .flat_map(|ul| ul.tag("li").find_all())
        .filter(|li| {
            let full_text = li.text();
            filters.iter().any(|filter| {
                filter.terms.iter().all(|term| {
                    if filter.case_sensitive {
                        full_text.contains(term)
                    } else {
                        full_text.to_lowercase().contains(&term.to_lowercase())
                    }
                })
            })
        })
        .flat_map(|li| li.tag("a").find_all())
        .map(|a| a.text())
        .filter(|text| text.starts_with("magnet:"))
        .collect::<Vec<_>>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_logic() {
        let html = r#"
            <html>
                <body>
                    <div>
                        <ul>
                            <li>
                                <div><span>Hello c376</span></div>
                                <div><div><div><a href="magnet:?xt=urn:btih:1">magnet:?xt=urn:btih:1</a></div></div></div>
                            </li>
                            <li>
                                <div><span>hello C376</span></div>
                                <div><div><div><a href="magnet:?xt=urn:btih:2">magnet:?xt=urn:btih:2</a></div></div></div>
                            </li>
                            <li>
                                <div><span>World BO0</span></div>
                                <div><div><div><a href="magnet:?xt=urn:btih:3">magnet:?xt=urn:btih:3</a></div></div></div>
                            </li>
                            <li>
                                <div><span>world bo0</span></div>
                                <div><div><div><a href="magnet:?xt=urn:btih:4">magnet:?xt=urn:btih:4</a></div></div></div>
                            </li>
                        </ul>
                    </div>
                </body>
            </html>
        "#;

        let filters = vec![
            Filter {
                terms: vec!["Hello".to_string(), "c376".to_string()],
                case_sensitive: false,
            },
            Filter {
                terms: vec!["World".to_string(), "BO0".to_string()],
                case_sensitive: true,
            },
        ];

        let magnet_links = extract_magnet_links(html, &filters);

        assert_eq!(magnet_links.len(), 3);
        assert!(magnet_links.contains(&"magnet:?xt=urn:btih:1".to_string()));
        assert!(magnet_links.contains(&"magnet:?xt=urn:btih:2".to_string()));
        assert!(magnet_links.contains(&"magnet:?xt=urn:btih:3".to_string()));
        assert!(!magnet_links.contains(&"magnet:?xt=urn:btih:4".to_string()));
    }
}

#[derive(Debug, Deserialize)]
struct ArchiveConfiguration {
    path: String,
    #[serde(rename = "retentionPeriod", with = "humantime_serde::option", default)]
    retention_period: Option<Duration>,
}

#[derive(Debug, Deserialize)]
struct QbittorrentConfiguration {
    url: String,
    username: String,
    password: String,
}

#[derive(Debug, Deserialize)]
struct LoggingConfiguration {
    #[serde(default)]
    level: Option<String>,
}

struct QbittorrentClient {
    client: Client,
    sid: String,
    configuration: QbittorrentConfiguration,
}

impl QbittorrentClient {
    fn post(&self, path: &str) -> RequestBuilder {
        self.append_data(self.client.post(self.to_url(path)))
    }

    fn append_data(&self, request_builder: RequestBuilder) -> RequestBuilder {
        request_builder
            .header("Cookie", format!("SID={}", self.sid))
            .header("Referer", &self.configuration.url)
    }

    fn to_url(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.configuration
                .url
                .strip_suffix("/")
                .unwrap_or(&self.configuration.url),
            path.strip_prefix("/").unwrap_or(path)
        )
    }
}

trait InterceptingClient {
    async fn intercepting_send<F>(&mut self, request_builder: F) -> Result<Response>
    where
        F: Fn(&Self) -> RequestBuilder;
}

impl InterceptingClient for QbittorrentClient {
    async fn intercepting_send<F>(&mut self, request_builder: F) -> Result<Response>
    where
        F: Fn(&Self) -> RequestBuilder,
    {
        let request = request_builder(self);
        let response = request.send().await?;
        let status_code = response.status();
        if status_code.is_success() || status_code.as_u16() != 403 {
            return Ok(response);
        }

        debug!("got 403 response, refreshing SID");
        let sid = get_sid(&self.client, &self.configuration).await?;
        self.sid = sid;

        Ok(request_builder(self).send().await?)
    }
}
