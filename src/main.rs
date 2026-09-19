use anyhow::{Context, Result, anyhow, bail};
use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use log::{Level, LevelFilter, debug, error, info, log, trace, warn};
use reqwest::header::CONTENT_TYPE;
use reqwest::{Client, RequestBuilder, Response, Url};
use scraper::{ElementRef, Html, Selector};
use serde::Deserialize;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::str::FromStr;
use std::time::{Duration, Instant, SystemTime};
use tokio::fs::{create_dir_all, read_dir, read_to_string, remove_file, rename, write};
use tokio::process::Command;
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

    if let Some(path) = dry_run_path(std::env::args().skip(1))? {
        return dry_run(&path, &config).await;
    }

    let notifier = Notifier::new(config.email.as_ref())?;

    // Child processes like N_m3u8DL-RE inherit the limit and may open many segment files at once.
    #[cfg(unix)]
    match rlimit::increase_nofile_limit(u64::MAX) {
        Ok(limit) => debug!("open file limit: {}", limit),
        Err(e) => {
            notifier
                .report(
                    "Could not increase open file limit",
                    &[Problem::warning(format!(
                        "could not increase open file limit: {}",
                        e
                    ))],
                )
                .await
        }
    }

    let read_path = &config.path;
    let read_path = Path::new(read_path);
    ensure_dir_exists(read_path).await?;

    let archive_path = &config.archive.path;
    let archive_path = Path::new(archive_path);
    ensure_dir_exists(archive_path).await?;

    if let Some(videos) = &config.videos {
        ensure_dir_exists(Path::new(&videos.path)).await?;
    }

    let mut client = QbittorrentClient {
        client: Client::new(),
        sid: "".to_string(),
        configuration: config.qbittorrent,
    };
    let http_client = Client::new();
    let mut last_youtube_update: Option<Instant> = None;

    loop {
        if let Some(videos) = &config.videos {
            update_youtube_downloader(
                &videos.downloaders.youtube,
                &mut last_youtube_update,
                &notifier,
            )
            .await;
        }

        if let Err(e) = process_directory(
            read_path,
            archive_path,
            &mut client,
            &config.filters,
            &http_client,
            config.videos.as_ref(),
            &notifier,
        )
        .await
        {
            notifier
                .report(
                    &format!("Could not process {}", read_path.display()),
                    &[Problem::error(format!(
                        "could not process {}: {:#}",
                        read_path.display(),
                        e
                    ))],
                )
                .await;
        }

        if let Some(retention_period) = config.archive.retention_period
            && let Err(e) = delete_old_files(archive_path, retention_period).await
        {
            notifier
                .report(
                    &format!("Could not clean up {}", archive_path.display()),
                    &[Problem::error(format!(
                        "could not delete old files in {}: {:#}",
                        archive_path.display(),
                        e
                    ))],
                )
                .await;
        }

        sleep(config.interval).await;
    }
}

/// Returns the file of a "--dry-run <file>" invocation.
fn dry_run_path<I>(arguments: I) -> Result<Option<PathBuf>>
where
    I: IntoIterator<Item = String>,
{
    let mut arguments = arguments.into_iter();
    let Some(argument) = arguments.next() else {
        return Ok(None);
    };
    if argument != "--dry-run" {
        bail!(
            "unknown argument '{}', expected: --dry-run <file>",
            argument
        );
    }

    let path = arguments
        .next()
        .ok_or_else(|| anyhow!("--dry-run needs the file to read"))?;
    if let Some(argument) = arguments.next() {
        bail!("unexpected argument '{}' after the file", argument);
    }

    Ok(Some(PathBuf::from(path)))
}

/// Prints what the file would result in, without downloading or archiving anything.
async fn dry_run(path: &Path, config: &Configuration) -> Result<()> {
    let content = read_to_string(path)
        .await
        .with_context(|| format!("could not read {}", path.display()))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("No file name found in {}", path.display()))?;

    println!(
        "{}",
        dry_run_report(&content, &file_name.to_string_lossy(), config)
    );
    Ok(())
}

/// Describes the entries of the file and what they would be named, for developing the filters.
fn dry_run_report(content: &str, file_name: &str, config: &Configuration) -> String {
    let mut lines = vec![format!("file: {}", file_name)];

    let magnet_links = extract_magnet_links(content, &config.filters);
    lines.push(format!(
        "
magnet links: {}",
        magnet_links.len()
    ));
    lines.extend(magnet_links.iter().map(|link| format!("  {}", link)));

    let Some(videos) = &config.videos else {
        lines.push(
            "
no video configuration"
                .to_string(),
        );
        return lines.join(
            "
",
        );
    };

    if !videos.files.iter().any(|filter| filter.matches(file_name)) {
        // Shown but not obeyed, so that the entries of a renamed or copied file can be checked.
        lines.push(
            "
the file name does not match videos.files, a real run would skip it"
                .to_string(),
        );
    }

    let entries = extract_videos(content, &videos.links);
    lines.push(format!(
        "
video entries: {}",
        entries.len()
    ));
    for entry in entries {
        let name = file_name_from_title(&entry.title, &videos.names);
        lines.push(format!(
            "
  title:     {}",
            entry.title
        ));
        lines.push(match name.is_empty() {
            true => "  name:      <none, the entry would be reported as a problem>".to_string(),
            false => format!("  name:      {}", name),
        });
        lines.push(match &entry.thumbnail_url {
            Some(url) => format!("  thumbnail: {}", url),
            None => "  thumbnail: <none>".to_string(),
        });
        match &entry.video_url {
            Some(url) => {
                lines.push(format!("  video:     {}", url));
                lines.push(match videos.downloaders.command(url, &videos.path, &name) {
                    Some((executable, arguments)) => {
                        // Quoted, so that an argument containing spaces stays recognizable as one.
                        let arguments: Vec<String> = arguments
                            .iter()
                            .map(|argument| match argument.contains(' ') {
                                true => format!("\"{}\"", argument),
                                false => argument.clone(),
                            })
                            .collect();
                        format!("  command:   {} {}", executable, arguments.join(" "))
                    }
                    None => "  command:   <no downloader handles this link>".to_string(),
                });
            }
            None => lines.push("  video:     <none>".to_string()),
        }
    }

    lines.join(
        "
",
    )
}

async fn process_directory(
    directory: &Path,
    archive_directory: &Path,
    client: &mut QbittorrentClient,
    filters: &[Filter],
    http_client: &Client,
    videos: Option<&VideoConfiguration>,
    notifier: &Notifier,
) -> Result<()> {
    let mut entries = read_dir(directory).await?;
    let mut problems = Vec::new();
    loop {
        // A file that cannot be processed stays where it is and does not stop the run, so that
        // the remaining files still get their chance and the next run retries the failed one.
        let entry = match entries.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(e) => {
                problems.push(Problem::error(format!(
                    "could not read the entries of {}: {}",
                    directory.display(),
                    e
                )));
                break;
            }
        };

        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        if let Err(e) = process_file(
            &path,
            archive_directory,
            client,
            filters,
            http_client,
            videos,
            notifier,
        )
        .await
        {
            problems.push(Problem::error(format!(
                "could not process {}: {:#}",
                path.display(),
                e
            )));
        }
    }

    notifier
        .report(&format!("Problems in {}", directory.display()), &problems)
        .await;

    Ok(())
}

async fn process_file(
    path: &Path,
    archive_directory: &Path,
    client: &mut QbittorrentClient,
    filters: &[Filter],
    http_client: &Client,
    videos: Option<&VideoConfiguration>,
    notifier: &Notifier,
) -> Result<()> {
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("No file name found in {}", path.display()))?;

    let content = match read_to_string(path).await {
        Ok(content) => content,
        Err(e) if e.kind() == ErrorKind::InvalidData => {
            notifier
                .report(
                    &format!("Could not read {}", file_name.to_string_lossy()),
                    &[Problem::warning(format!(
                        "could not read {} as text: {}",
                        path.display(),
                        e
                    ))],
                )
                .await;
            String::new()
        }
        Err(e) => return Err(e.into()),
    };
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

    if let Some(videos) = videos
        && videos
            .files
            .iter()
            .any(|filter| filter.matches(&file_name.to_string_lossy()))
    {
        let problems = process_videos(&content, path, http_client, videos).await;
        notifier
            .report(
                &format!("Video problems in {}", file_name.to_string_lossy()),
                &problems,
            )
            .await;
    }

    let mut archive_path = PathBuf::from(archive_directory);
    archive_path.push(file_name);
    rename(&path, archive_path).await?;

    Ok(())
}

async fn process_videos(
    content: &str,
    path: &Path,
    http_client: &Client,
    config: &VideoConfiguration,
) -> Vec<Problem> {
    let entries = extract_videos(content, &config.links);
    debug!("found {} videos in {}", entries.len(), path.display());

    let mut problems = Vec::new();
    for entry in entries {
        let name = file_name_from_title(&entry.title, &config.names);
        if name.is_empty() {
            problems.push(Problem::warning(format!(
                "no usable file name for '{}'",
                entry.title
            )));
            continue;
        }

        let Some(video_url) = &entry.video_url else {
            problems.push(Problem::warning(format!(
                "no video link found for '{}'",
                entry.title
            )));
            continue;
        };
        let Some(command) = config.downloaders.command(video_url, &config.path, &name) else {
            problems.push(Problem::warning(format!(
                "no downloader for video {} of '{}'",
                video_url, entry.title
            )));
            continue;
        };

        if let Some(url) = &entry.thumbnail_url
            && let Err(e) = download_image(http_client, url, Path::new(&config.path), &name).await
        {
            problems.push(Problem::error(format!(
                "could not download thumbnail {} for '{}': {:#}",
                url, entry.title, e
            )));
        }

        let (executable, arguments) = command;
        info!(
            "downloading {} as '{}' with {}",
            video_url, name, executable
        );
        if let Err(e) = run_download(executable, &arguments, &config.downloaders).await {
            problems.push(Problem::error(format!(
                "could not download video {} for '{}': {:#}",
                video_url, entry.title, e
            )));
        } else {
            info!("downloaded {} to {}", video_url, config.path);
        }
    }

    problems
}

async fn download_image(client: &Client, url: &Url, directory: &Path, name: &str) -> Result<()> {
    let response = client.get(url.clone()).send().await?.error_for_status()?;
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok());
    let extension = url_extension(url)
        .or_else(|| content_type.and_then(extension_from_content_type))
        .ok_or_else(|| anyhow!("No file extension found for {}", url))?;
    let path = directory.join(format!("{}.{}", name, extension));

    let bytes = response.bytes().await?;
    write(&path, &bytes).await?;

    info!("downloaded {} to {}", url, path.display());
    Ok(())
}

/// Updates yt-dlp if an update interval is configured and it has elapsed since the last update.
async fn update_youtube_downloader(
    youtube: &YoutubeDownloader,
    last_update: &mut Option<Instant>,
    notifier: &Notifier,
) {
    let Some(interval) = youtube.update_interval else {
        return;
    };
    if last_update.is_some_and(|last_update| last_update.elapsed() < interval) {
        return;
    }
    *last_update = Some(Instant::now());

    info!("updating {}", youtube.executable);
    match run_command(&youtube.executable, &["--update".to_string()]).await {
        Ok(output) => info!("{}", last_lines(output.as_bytes(), 1)),
        Err(e) => {
            notifier
                .report(
                    &format!("Could not update {}", youtube.executable),
                    &[Problem::error(format!(
                        "could not update {}: {:#}",
                        youtube.executable, e
                    ))],
                )
                .await
        }
    }
}

/// Runs the download and repeats it after the configured delay if it fails.
async fn run_download(
    executable: &str,
    arguments: &[String],
    downloaders: &Downloaders,
) -> Result<String> {
    for attempt in 1..=downloaders.retries {
        match run_command(executable, arguments).await {
            Ok(output) => return Ok(output),
            Err(e) => warn!(
                "attempt {} of {} failed, retrying in {:?}: {:#}",
                attempt,
                downloaders.retries + 1,
                downloaders.retry_delay,
                e
            ),
        }
        sleep(downloaders.retry_delay).await;
    }

    run_command(executable, arguments).await
}

/// Runs the command and returns its standard output.
async fn run_command(executable: &str, arguments: &[String]) -> Result<String> {
    let output = Command::new(executable)
        .args(arguments)
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("could not start {}", executable))?;

    trace!("{}", String::from_utf8_lossy(&output.stdout));
    if !output.status.success() {
        bail!(
            "{} exited with {}: {}",
            executable,
            output.status,
            last_lines(&[&output.stdout[..], &output.stderr[..]].concat(), 10)
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn last_lines(output: &[u8], count: usize) -> String {
    let output = String::from_utf8_lossy(output);
    let lines: Vec<&str> = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    lines[lines.len().saturating_sub(count)..].join("\n")
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
#[serde(untagged)]
enum FilterTerm {
    Simple(String),
    Full {
        #[serde(deserialize_with = "deserialize_string_or_number")]
        term: String,
        #[serde(default)]
        exclude: bool,
    },
}

fn deserialize_string_or_number<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let value = config::Value::deserialize(deserializer)?;
    value
        .into_string()
        .map_err(|e| D::Error::custom(format!("could not convert to string: {}", e)))
}

#[derive(Debug, Deserialize)]
struct Filter {
    terms: Vec<FilterTerm>,
    #[serde(default)]
    case_sensitive: bool,
}

impl Filter {
    fn matches(&self, text: &str) -> bool {
        self.terms.iter().all(|term_config| {
            let (term, exclude) = match term_config {
                FilterTerm::Simple(s) => (s, false),
                FilterTerm::Full { term, exclude } => (term, *exclude),
            };

            let contains = if self.case_sensitive {
                text.contains(term)
            } else {
                text.to_lowercase().contains(&term.to_lowercase())
            };

            if exclude { !contains } else { contains }
        })
    }
}

fn default_interval() -> Duration {
    Duration::from_mins(1)
}

#[derive(Debug, Deserialize)]
struct Configuration {
    path: String,
    /// Waited between two runs.
    #[serde(with = "humantime_serde", default = "default_interval")]
    interval: Duration,
    archive: ArchiveConfiguration,
    qbittorrent: QbittorrentConfiguration,
    #[serde(default)]
    logging: Option<LoggingConfiguration>,
    #[serde(default)]
    filters: Vec<Filter>,
    #[serde(default)]
    videos: Option<VideoConfiguration>,
    #[serde(default)]
    email: Option<EmailConfiguration>,
}

#[derive(Deserialize)]
struct EmailConfiguration {
    host: String,
    port: u16,
    username: String,
    password: String,
    /// Sender, defaults to the username.
    #[serde(default)]
    from: Option<String>,
    to: String,
}

impl std::fmt::Debug for EmailConfiguration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmailConfiguration")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &"***")
            .field("from", &self.from)
            .field("to", &self.to)
            .finish()
    }
}

struct Problem {
    level: Level,
    message: String,
}

impl Problem {
    fn warning(message: String) -> Self {
        Self {
            level: Level::Warn,
            message,
        }
    }

    fn error(message: String) -> Self {
        Self {
            level: Level::Error,
            message,
        }
    }
}

/// Reports problems by email if configured, otherwise (or if sending fails) in the log.
struct Notifier {
    email: Option<EmailNotifier>,
}

struct EmailNotifier {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
    to: Mailbox,
}

impl Notifier {
    fn new(config: Option<&EmailConfiguration>) -> Result<Self> {
        let email = config.map(EmailNotifier::new).transpose()?;
        Ok(Self { email })
    }

    async fn report(&self, subject: &str, problems: &[Problem]) {
        if problems.is_empty() {
            return;
        }

        if let Some(email) = &self.email {
            let body = problems
                .iter()
                .map(|problem| format!("{}: {}", problem.level, problem.message))
                .collect::<Vec<_>>()
                .join("\n");
            match email.send(subject, &body).await {
                Ok(()) => {
                    info!("sent email '{}' to {}", subject, email.to);
                    return;
                }
                Err(e) => error!("could not send email '{}': {:#}", subject, e),
            }
        }

        for problem in problems {
            log!(problem.level, "{}", problem.message);
        }
    }
}

impl EmailNotifier {
    fn new(config: &EmailConfiguration) -> Result<Self> {
        let from = config.from.as_ref().unwrap_or(&config.username);
        let from = from
            .parse()
            .with_context(|| format!("invalid email sender '{}'", from))?;
        let to = config
            .to
            .parse()
            .with_context(|| format!("invalid email recipient '{}'", config.to))?;

        // Port 587 uses STARTTLS, all other ports use TLS from the start.
        let builder = if config.port == 587 {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&config.host)?
        } else {
            AsyncSmtpTransport::<Tokio1Executor>::relay(&config.host)?
        };
        let transport = builder
            .port(config.port)
            .credentials(Credentials::new(
                config.username.clone(),
                config.password.clone(),
            ))
            .build();

        Ok(Self {
            transport,
            from,
            to,
        })
    }

    async fn send(&self, subject: &str, body: &str) -> Result<()> {
        let message = Message::builder()
            .from(self.from.clone())
            .to(self.to.clone())
            .subject(subject)
            .body(body.to_string())?;
        self.transport.send(message).await?;
        Ok(())
    }
}

#[derive(Debug, PartialEq)]
struct VideoEntry {
    title: String,
    thumbnail_url: Option<Url>,
    video_url: Option<Url>,
}

fn extract_videos(content: &str, link_filters: &LinkFilters) -> Vec<VideoEntry> {
    let document = Html::parse_document(content);
    let h4_selector = Selector::parse("h4").unwrap();
    let a_selector = Selector::parse("a[href]").unwrap();

    document
        .select(&h4_selector)
        .filter_map(|h4| {
            let title = h4.text().collect::<String>().trim().to_string();
            let container = h4.parent().and_then(ElementRef::wrap)?;
            let links: Vec<(String, Url)> = container
                .select(&a_selector)
                .filter_map(|a| {
                    let url = Url::parse(a.value().attr("href")?.trim()).ok()?;
                    Some((a.text().collect::<String>().trim().to_string(), url))
                })
                .collect();

            let find_url = |filters: &[Filter]| {
                links
                    .iter()
                    .find(|(text, _)| filters.iter().any(|filter| filter.matches(text)))
                    .map(|(_, url)| url.clone())
            };

            Some(VideoEntry {
                title,
                thumbnail_url: find_url(&link_filters.thumbnail),
                video_url: find_url(&link_filters.video),
            })
        })
        .filter(|entry| !entry.title.is_empty())
        .collect()
}

fn url_extension(url: &Url) -> Option<String> {
    Path::new(url.path())
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
}

fn extension_from_content_type(content_type: &str) -> Option<String> {
    let mime_type = content_type.split(';').next()?.trim().to_lowercase();
    let subtype = mime_type.strip_prefix("image/")?;
    let extension = match subtype {
        "jpeg" | "pjpeg" => "jpg",
        "svg+xml" => "svg",
        "x-icon" | "vnd.microsoft.icon" => "ico",
        subtype => subtype,
    };
    (!extension.is_empty()).then(|| extension.to_string())
}

/// Builds the file name from the entry title: removes a leading "YYYY-MM-DD" (optionally followed
/// by " - ") and everything from the first "|" on, then removes the configured terms and adds the
/// affixes of all rules matching the full title.
fn file_name_from_title(title: &str, names: &NameConfiguration) -> String {
    let title = title.trim();
    let base = strip_date_prefix(title);
    let base = base.split('|').next().unwrap_or_default();
    let base = names
        .strip
        .iter()
        .fold(base.to_string(), |base, term| remove_all(&base, term));
    let base = sanitize_file_name(&base);
    if base.is_empty() {
        return base;
    }

    let matching: Vec<&Affix> = names
        .affixes
        .iter()
        .filter(|affix| affix.filter.matches(title))
        .collect();

    let mut name = String::new();
    for affix in &matching {
        name.push_str(&affix.prefix);
    }
    name.push_str(&base);
    for affix in &matching {
        name.push_str(&affix.suffix);
    }

    sanitize_file_name(&name)
}

/// Removes all occurrences of the term, ignoring ASCII case.
fn remove_all(text: &str, term: &str) -> String {
    if term.is_empty() {
        return text.to_string();
    }

    let haystack = text.to_ascii_lowercase();
    let needle = term.to_ascii_lowercase();
    let mut result = String::with_capacity(text.len());
    let mut end = 0;
    for (start, _) in haystack.match_indices(&needle) {
        result.push_str(&text[end..start]);
        end = start + needle.len();
    }
    result.push_str(&text[end..]);
    result
}

fn strip_date_prefix(title: &str) -> &str {
    let bytes = title.as_bytes();
    let is_date = bytes.len() >= 10
        && bytes[..10].iter().enumerate().all(|(i, b)| match i {
            4 | 7 => *b == b'-',
            _ => b.is_ascii_digit(),
        });
    if !is_date || !title[10..].starts_with(char::is_whitespace) {
        return title;
    }

    let rest = title[10..].trim_start();
    rest.strip_prefix('-').map_or(rest, str::trim_start)
}

fn sanitize_file_name(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .filter(|c| !matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'))
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    sanitized
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end_matches('.')
        .trim_end()
        .to_string()
}

fn extract_magnet_links(content: &str, filters: &[Filter]) -> Vec<String> {
    let document = Html::parse_document(content);
    let li_selector = Selector::parse("li").unwrap();
    let a_selector = Selector::parse("a").unwrap();

    document
        .select(&li_selector)
        .filter(|li| {
            let full_text = li.text().collect::<String>();
            filters.iter().any(|filter| filter.matches(&full_text))
        })
        .flat_map(|li| {
            li.select(&a_selector)
                .map(|a| a.text().collect::<String>())
                .filter(|text| text.starts_with("magnet:"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::FileFormat;

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
                terms: vec![
                    FilterTerm::Simple("Hello".to_string()),
                    FilterTerm::Simple("c376".to_string()),
                ],
                case_sensitive: false,
            },
            Filter {
                terms: vec![
                    FilterTerm::Simple("World".to_string()),
                    FilterTerm::Simple("BO0".to_string()),
                ],
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

    #[test]
    fn test_filter_negation() {
        let html = r#"
            <li><span>Movie 2024 x264</span><a href="magnet:?1">magnet:?1</a></li>
            <li><span>Movie 2024 x265</span><a href="magnet:?2">magnet:?2</a></li>
            <li><span>Other 2024 x264</span><a href="magnet:?3">magnet:?3</a></li>
        "#;

        let filters = vec![Filter {
            terms: vec![
                FilterTerm::Simple("Movie".to_string()),
                FilterTerm::Full {
                    term: "x265".to_string(),
                    exclude: true,
                },
            ],
            case_sensitive: false,
        }];

        let magnet_links = extract_magnet_links(html, &filters);
        assert_eq!(magnet_links.len(), 1);
        assert!(magnet_links.contains(&"magnet:?1".to_string()));
        assert!(!magnet_links.contains(&"magnet:?2".to_string()));
    }

    #[test]
    fn test_filter_deserialization() {
        let yaml = r#"
            terms:
              - simple_term
              - term: excluded_term
                exclude: true
              - term: included_term
                exclude: false
              - term: 1234
                exclude: true
            case_sensitive: true
        "#;

        let config = config::Config::builder()
            .add_source(config::File::from_str(yaml, FileFormat::Yaml))
            .build()
            .expect("could not create config");
        let filter: Filter = config
            .try_deserialize()
            .expect("could not deserialize filter");

        assert_eq!(filter.terms.len(), 4);
        assert!(filter.case_sensitive);

        match &filter.terms[0] {
            FilterTerm::Simple(s) => assert_eq!(s, "simple_term"),
            _ => panic!("Expected Simple term"),
        }

        match &filter.terms[1] {
            FilterTerm::Full { term, exclude } => {
                assert_eq!(term, "excluded_term");
                assert!(exclude);
            }
            _ => panic!("Expected Full term"),
        }

        match &filter.terms[2] {
            FilterTerm::Full { term, exclude } => {
                assert_eq!(term, "included_term");
                assert!(!exclude);
            }
            _ => panic!("Expected Full term"),
        }

        match &filter.terms[3] {
            FilterTerm::Full { term, exclude } => {
                assert_eq!(term, "1234");
                assert!(exclude);
            }
            _ => panic!("Expected Full term"),
        }
    }

    #[test]
    fn test_extract_videos() {
        let html = r#"
            <html>
              <body>
                <div>
                  <h4>2026-09-15 - Aeldari vs Emperor’s Children | Warhammer 40k Battle Report</h4>
                  <ul>
                    <li><a href="https://tabletoptactics.tv/wp-content/uploads/2026/09/Batrep-44-Emperors-Children-vs-Aeldari-1-670x377.jpg">Thumbnail</a></li>
                    <li><a href="https://content.uplynk.com/cfe58e01a8ad4d3a9f2feb90f8b2d61e.m3u8">Video</a></li>
                  </ul>
                </div>
                <div>
                  <h4>Second</h4>
                  <ul>
                    <li><a href="https://example.com/other.jpg">Other</a></li>
                    <li><a href="https://example.com/playlist?id=1"> video </a></li>
                    <li><a href="https://example.com/second">Video</a></li>
                    <li><a href="not a url">Thumbnail</a></li>
                  </ul>
                </div>
              </body>
            </html>
        "#;

        let videos = extract_videos(html, &link_filters());

        assert_eq!(
            videos,
            vec![
                VideoEntry {
                    title: "2026-09-15 - Aeldari vs Emperor’s Children | Warhammer 40k Battle Report"
                        .to_string(),
                    thumbnail_url: Some(Url::parse("https://tabletoptactics.tv/wp-content/uploads/2026/09/Batrep-44-Emperors-Children-vs-Aeldari-1-670x377.jpg").unwrap()),
                    video_url: Some(Url::parse("https://content.uplynk.com/cfe58e01a8ad4d3a9f2feb90f8b2d61e.m3u8").unwrap()),
                },
                VideoEntry {
                    title: "Second".to_string(),
                    thumbnail_url: None,
                    video_url: Some(Url::parse("https://example.com/playlist?id=1").unwrap()),
                },
            ]
        );
    }

    #[test]
    fn test_extract_videos_without_html() {
        assert!(extract_videos("no html at all", &link_filters()).is_empty());
    }

    fn link_filters() -> LinkFilters {
        let filter = |term: &str| Filter {
            terms: vec![FilterTerm::Simple(term.to_string())],
            case_sensitive: false,
        };
        LinkFilters {
            thumbnail: vec![filter("Thumbnail")],
            video: vec![filter("Video")],
        }
    }

    #[test]
    fn test_extension_from_content_type() {
        assert_eq!(
            extension_from_content_type("image/jpeg"),
            Some("jpg".to_string())
        );
        assert_eq!(
            extension_from_content_type("Image/PNG; charset=binary"),
            Some("png".to_string())
        );
        assert_eq!(
            extension_from_content_type("image/svg+xml"),
            Some("svg".to_string())
        );
        assert_eq!(extension_from_content_type("text/html"), None);
        assert_eq!(extension_from_content_type("image/"), None);
    }

    #[test]
    fn test_file_name_from_title() {
        let names = NameConfiguration::default();
        let file_name = |title: &str| file_name_from_title(title, &names);

        assert_eq!(
            file_name("2026-09-15 - Aeldari vs Emperor’s Children | Warhammer 40k Battle Report"),
            "Aeldari vs Emperor’s Children"
        );
        assert_eq!(
            file_name("Aeldari vs Emperor’s Children | Battle Report"),
            "Aeldari vs Emperor’s Children"
        );
        assert_eq!(
            file_name("2026-09-15 - Aeldari: Codex Review"),
            "Aeldari Codex Review"
        );
        assert_eq!(file_name("2026-09-15 Review"), "Review");
        assert_eq!(file_name("2026-09-15Review"), "2026-09-15Review");
        assert_eq!(file_name("2026-09-15"), "2026-09-15");
        assert_eq!(file_name("2026-9-15 - Review"), "2026-9-15 - Review");
        assert_eq!(file_name("2026-09-15 - | Review"), "");
    }

    #[test]
    fn test_file_name_affixes() {
        let affix = |term: &str, prefix: &str, suffix: &str| Affix {
            filter: Filter {
                terms: vec![FilterTerm::Simple(term.to_string())],
                case_sensitive: false,
            },
            prefix: prefix.to_string(),
            suffix: suffix.to_string(),
        };
        let names = NameConfiguration {
            strip: vec!["**NEW CODEX!!**".to_string()],
            affixes: vec![
                affix("Warhammer 40k", "40k - ", ""),
                affix("Codex Review", "", " - Codex Review"),
                affix("Battle Report", "Batrep - ", ""),
            ],
        };
        let file_name = |title: &str| file_name_from_title(title, &names);

        assert_eq!(
            file_name("2026-09-19 - Space Marines | Warhammer 40k Codex Review"),
            "40k - Space Marines - Codex Review"
        );
        assert_eq!(
            file_name(
                "2026-09-19 - **NEW CODEX!!** Space Marines vs Orks | Warhammer 40k Battle Report"
            ),
            "40k - Batrep - Space Marines vs Orks"
        );
        // Without a matching filter the name stays as it was.
        assert_eq!(
            file_name("2026-09-19 - Space Marines | Unboxing"),
            "Space Marines"
        );
        // The affixes are dropped along with an empty name.
        assert_eq!(file_name("2026-09-19 - | Warhammer 40k Codex Review"), "");
    }

    #[tokio::test]
    async fn test_process_directory_continues_after_a_failing_file() {
        let base = std::env::temp_dir().join(format!(
            "release-notifier-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let directory = base.join("notifications");
        let archive = base.join("archive");
        create_dir_all(&directory).await.unwrap();
        create_dir_all(&archive).await.unwrap();

        for name in ["a.html", "b.html", "c.html"] {
            write(directory.join(name), "<html></html>").await.unwrap();
        }
        // Archiving b.html fails because a non-empty directory of that name is in the way.
        create_dir_all(archive.join("b.html").join("blocker"))
            .await
            .unwrap();

        let mut client = QbittorrentClient {
            client: Client::new(),
            sid: String::new(),
            configuration: QbittorrentConfiguration {
                url: "http://localhost:1/".to_string(),
                username: String::new(),
                password: String::new(),
            },
        };

        // Without filters no magnet link is found and without a video configuration no download
        // is started, so the run does not talk to anything.
        process_directory(
            &directory,
            &archive,
            &mut client,
            &[],
            &Client::new(),
            None,
            &Notifier::new(None).unwrap(),
        )
        .await
        .expect("the run itself must not fail");

        assert!(
            directory.join("b.html").is_file(),
            "the failing file stays where it is"
        );
        assert!(
            !directory.join("a.html").exists() && !directory.join("c.html").exists(),
            "the files before and after it are processed"
        );
        assert!(archive.join("a.html").is_file() && archive.join("c.html").is_file());

        tokio::fs::remove_dir_all(&base).await.unwrap();
    }

    #[test]
    fn test_dry_run_path() {
        let path = |arguments: &[&str]| {
            dry_run_path(arguments.iter().map(|a| a.to_string()).collect::<Vec<_>>())
        };

        assert_eq!(path(&[]).unwrap(), None);
        assert_eq!(
            path(&["--dry-run", "a file"]).unwrap(),
            Some(PathBuf::from("a file"))
        );
        assert!(path(&["--dry-run"]).is_err());
        assert!(path(&["--dry-run", "a file", "another"]).is_err());
        assert!(path(&["file"]).is_err());
    }

    #[test]
    fn test_dry_run_report() {
        let yaml = r#"
            path: /notifications
            archive:
              path: /archive
            qbittorrent:
              url: http://localhost:8080/
              username: admin
              password: secret
            videos:
              path: /videos
              files:
                - terms: ["TabletopTactics"]
              links:
                thumbnail:
                  - terms: ["Thumbnail"]
                video:
                  - terms: ["Video"]
              names:
                strip: ["**NEW CODEX!!**"]
                affixes:
                  - terms: ["Codex Review"]
                    prefix: "Codex Review - "
        "#;
        let config: Configuration = config::Config::builder()
            .add_source(config::File::from_str(yaml, FileFormat::Yaml))
            .build()
            .expect("could not create config")
            .try_deserialize()
            .expect("could not deserialize configuration");

        let html = r#"
            <html><body>
              <div>
                <h4>2026-09-19 - Space Marines | Warhammer 40k Codex Review</h4>
                <ul>
                  <li><a href="https://example.com/thumb.jpg">Thumbnail</a></li>
                  <li><a href="https://content.uplynk.com/abc.m3u8">Video</a></li>
                </ul>
              </div>
              <div>
                <h4>2026-09-19 - **NEW CODEX!!** Space Marines vs Orks | Warhammer 40k Battle Report</h4>
                <ul>
                  <li><a href="https://www.youtube.com/embed/u9d7RlroH1w">Video</a></li>
                </ul>
              </div>
            </body></html>
        "#;

        let report = dry_run_report(html, "1 new video from TabletopTactics", &config);

        assert!(
            report.contains("name:      Codex Review - Space Marines"),
            "{}",
            report
        );
        assert!(
            report.contains("name:      Space Marines vs Orks"),
            "{}",
            report
        );
        assert!(report.contains("thumbnail: <none>"), "{}", report);
        assert!(
            report.contains(r#"--save-name "Codex Review - Space Marines""#),
            "{}",
            report
        );
        assert!(report.contains("N_m3u8DL-RE"), "{}", report);
        assert!(report.contains("yt-dlp"), "{}", report);
        assert!(report.contains("video entries: 2"), "{}", report);
        assert!(
            !report.contains("does not match videos.files"),
            "{}",
            report
        );

        // A file name the real run would skip is still reported, with a note.
        let report = dry_run_report(html, "1 new video from OtherChannel", &config);
        assert!(report.contains("does not match videos.files"), "{}", report);
        assert!(report.contains("video entries: 2"), "{}", report);
    }

    #[test]
    fn test_interval_deserialization() {
        let configuration = |yaml: &str| {
            config::Config::builder()
                .add_source(config::File::from_str(yaml, FileFormat::Yaml))
                .build()
                .expect("could not create config")
                .try_deserialize::<Configuration>()
                .expect("could not deserialize configuration")
        };
        let base = r#"
            path: /notifications
            archive:
              path: /archive
            qbittorrent:
              url: http://localhost:8080/
              username: admin
              password: secret
        "#;

        let with_interval = r#"
            path: /notifications
            interval: 15m
            archive:
              path: /archive
            qbittorrent:
              url: http://localhost:8080/
              username: admin
              password: secret
        "#;

        assert_eq!(configuration(base).interval, Duration::from_mins(1));
        assert_eq!(
            configuration(with_interval).interval,
            Duration::from_mins(15)
        );
    }

    #[test]
    fn test_remove_all() {
        assert_eq!(remove_all("a **NEW** b **new** c", "**new**"), "a  b  c");
        assert_eq!(remove_all("abc", ""), "abc");
        assert_eq!(remove_all("aaa", "aa"), "a");
        assert_eq!(remove_all("über Über", "über"), " Über");
    }

    #[test]
    fn test_sanitize_file_name() {
        assert_eq!(
            sanitize_file_name("Aeldari | Emperor’s Children"),
            "Aeldari Emperor’s Children"
        );
        assert_eq!(sanitize_file_name(" a/b\\c:d*e?f\"g<h>i. "), "abcdefghi");
        assert_eq!(sanitize_file_name("a\tb  c"), "a b c");
        assert_eq!(sanitize_file_name(" ... "), "");
    }

    #[test]
    fn test_file_name_filter() {
        let filter = Filter {
            terms: vec![
                FilterTerm::Simple("tabletoptactics".to_string()),
                FilterTerm::Full {
                    term: "Shorts".to_string(),
                    exclude: true,
                },
            ],
            case_sensitive: false,
        };

        assert!(filter.matches(
            "2026-09-15T09%3A05%3A13.632996888Z - 1 new video available from TabletopTactics"
        ));
        assert!(!filter.matches("1 new video available from TabletopTactics Shorts"));
        assert!(!filter.matches("1 new video available from OtherChannel"));
    }

    #[test]
    fn test_video_configuration_deserialization() {
        let yaml = r#"
            path: /videos
            files:
              - terms: ["TabletopTactics"]
            links:
              thumbnail:
                - terms: ["Thumbnail"]
              video:
                - terms: ["Video"]
                  case_sensitive: true
        "#;

        let config = config::Config::builder()
            .add_source(config::File::from_str(yaml, FileFormat::Yaml))
            .build()
            .expect("could not create config");
        let videos: VideoConfiguration = config
            .try_deserialize()
            .expect("could not deserialize video configuration");

        assert_eq!(videos.path, "/videos");
        assert_eq!(videos.files.len(), 1);
        assert_eq!(videos.links.thumbnail.len(), 1);
        assert!(!videos.links.thumbnail[0].case_sensitive);
        assert_eq!(videos.links.video.len(), 1);
        assert!(videos.links.video[0].case_sensitive);
        assert_eq!(videos.downloaders.hls.executable, "N_m3u8DL-RE");
        assert!(videos.downloaders.hls.arguments.is_empty());
        assert_eq!(videos.downloaders.youtube.executable, "yt-dlp");
        assert!(videos.downloaders.youtube.arguments.is_empty());
        assert_eq!(videos.downloaders.youtube.hosts.len(), 3);
        assert_eq!(videos.downloaders.youtube.update_interval, None);
        assert_eq!(videos.downloaders.retries, 2);
        assert_eq!(videos.downloaders.retry_delay, Duration::from_secs(30));
        assert!(videos.names.strip.is_empty());
        assert!(videos.names.affixes.is_empty());
    }

    #[test]
    fn test_name_configuration_deserialization() {
        let yaml = r#"
            path: /videos
            links: {}
            names:
              strip: ["**NEW CODEX!!**"]
              affixes:
                - terms: ["Codex Review"]
                  suffix: " - Codex Review"
                - terms: ["Battle Report"]
                  case_sensitive: true
                  prefix: "Batrep - "
        "#;

        let config = config::Config::builder()
            .add_source(config::File::from_str(yaml, FileFormat::Yaml))
            .build()
            .expect("could not create config");
        let videos: VideoConfiguration = config
            .try_deserialize()
            .expect("could not deserialize video configuration");
        let names = videos.names;

        assert_eq!(names.strip, ["**NEW CODEX!!**"]);
        assert_eq!(names.affixes.len(), 2);
        assert!(
            names.affixes[0]
                .filter
                .matches("Warhammer 40k Codex Review")
        );
        assert!(!names.affixes[0].filter.case_sensitive);
        assert_eq!(names.affixes[0].prefix, "");
        assert_eq!(names.affixes[0].suffix, " - Codex Review");
        assert!(names.affixes[1].filter.case_sensitive);
        assert_eq!(names.affixes[1].prefix, "Batrep - ");
        assert_eq!(names.affixes[1].suffix, "");
    }

    #[test]
    fn test_downloaders_deserialization() {
        let yaml = r#"
            path: /videos
            links: {}
            downloaders:
              hls:
                arguments: ["-M", "format=mp4"]
              youtube:
                executable: /usr/local/bin/yt-dlp
                hosts: ["example.com"]
                update_interval: 1d
              retries: 0
              retry_delay: 5m
        "#;

        let config = config::Config::builder()
            .add_source(config::File::from_str(yaml, FileFormat::Yaml))
            .build()
            .expect("could not create config");
        let videos: VideoConfiguration = config
            .try_deserialize()
            .expect("could not deserialize video configuration");
        let downloaders = videos.downloaders;

        assert_eq!(downloaders.hls.executable, "N_m3u8DL-RE");
        assert_eq!(downloaders.hls.arguments, ["-M", "format=mp4"]);
        assert_eq!(downloaders.youtube.executable, "/usr/local/bin/yt-dlp");
        assert!(downloaders.youtube.arguments.is_empty());
        assert_eq!(downloaders.youtube.hosts, ["example.com"]);
        assert_eq!(
            downloaders.youtube.update_interval,
            Some(Duration::from_hours(24))
        );
        assert_eq!(downloaders.retries, 0);
        assert_eq!(downloaders.retry_delay, Duration::from_mins(5));
    }

    #[tokio::test]
    async fn test_run_download_retries() {
        let downloaders = Downloaders {
            retries: 2,
            retry_delay: Duration::from_millis(50),
            ..Downloaders::default()
        };

        let start = Instant::now();
        let result = run_download("no-such-executable-3e7f1a", &[], &downloaders).await;

        assert!(result.is_err());
        assert!(
            start.elapsed() >= Duration::from_millis(100),
            "three attempts wait twice, but only took {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn test_run_download_without_retries() {
        let downloaders = Downloaders {
            retries: 0,
            retry_delay: Duration::from_secs(60),
            ..Downloaders::default()
        };

        let start = Instant::now();
        let result = run_download("no-such-executable-3e7f1a", &[], &downloaders).await;

        assert!(result.is_err());
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "a single attempt does not wait"
        );
    }

    #[test]
    fn test_youtube_downloader_handles() {
        let youtube = YoutubeDownloader::default();
        let handles = |url: &str| youtube.handles(&Url::parse(url).unwrap());

        assert!(handles("https://www.youtube.com/watch?v=abc"));
        assert!(handles("https://m.youtube.com/watch?v=abc"));
        assert!(handles("https://youtu.be/abc"));
        assert!(handles("https://WWW.YouTube.com/watch?v=abc"));
        assert!(handles("https://www.youtube-nocookie.com/embed/abc"));
        assert!(!handles("https://notyoutube.com/watch?v=abc"));
        assert!(!handles("https://youtube.com.example.com/watch?v=abc"));
        assert!(!handles("https://content.uplynk.com/abc.m3u8"));
    }

    #[test]
    fn test_email_configuration_deserialization() {
        let yaml = r#"
            host: smtp.example.com
            port: 465
            username: me
            password: secret
            from: me@example.com
            to: you@example.com
        "#;

        let config = config::Config::builder()
            .add_source(config::File::from_str(yaml, FileFormat::Yaml))
            .build()
            .expect("could not create config");
        let email: EmailConfiguration = config
            .try_deserialize()
            .expect("could not deserialize email configuration");

        assert_eq!(email.host, "smtp.example.com");
        assert_eq!(email.port, 465);
        assert_eq!(email.username, "me");
        assert_eq!(email.password, "secret");
        assert_eq!(email.from.as_deref(), Some("me@example.com"));
        assert_eq!(email.to, "you@example.com");
        assert!(!format!("{:?}", email).contains("secret"));

        let notifier = EmailNotifier::new(&email).expect("could not create email notifier");
        assert_eq!(notifier.from.to_string(), "me@example.com");
        assert_eq!(notifier.to.to_string(), "you@example.com");
    }

    #[test]
    fn test_email_sender_defaults_to_username() {
        let email = EmailConfiguration {
            host: "smtp.example.com".to_string(),
            port: 465,
            username: "me@example.com".to_string(),
            password: "secret".to_string(),
            from: None,
            to: "you@example.com".to_string(),
        };

        let notifier = EmailNotifier::new(&email).expect("could not create email notifier");
        assert_eq!(notifier.from.to_string(), "me@example.com");

        let email = EmailConfiguration {
            username: "me".to_string(),
            ..email
        };
        assert!(EmailNotifier::new(&email).is_err());
    }

    #[test]
    fn test_email_configuration_has_no_defaults() {
        let yaml = r#"
            username: me
            password: secret
            from: me@example.com
            to: you@example.com
        "#;

        let config = config::Config::builder()
            .add_source(config::File::from_str(yaml, FileFormat::Yaml))
            .build()
            .expect("could not create config");

        assert!(config.try_deserialize::<EmailConfiguration>().is_err());
    }

    #[test]
    fn test_email_notifier_rejects_invalid_addresses() {
        let email = EmailConfiguration {
            host: "smtp.example.com".to_string(),
            port: 587,
            username: "me".to_string(),
            password: "secret".to_string(),
            from: Some("me@example.com".to_string()),
            to: "not an address".to_string(),
        };
        assert!(EmailNotifier::new(&email).is_err());
    }

    #[test]
    fn test_downloader_selection() {
        let downloaders = Downloaders::default();
        let executable = |url: &str| {
            downloaders
                .command(&Url::parse(url).unwrap(), "/videos", "name")
                .map(|(executable, _)| executable.to_string())
        };

        assert_eq!(
            executable("https://www.youtube.com/watch?v=abc").as_deref(),
            Some("yt-dlp")
        );
        assert_eq!(
            executable("https://content.uplynk.com/abc.m3u8").as_deref(),
            Some("N_m3u8DL-RE")
        );
        assert_eq!(
            executable("https://content.uplynk.com/abc.M3U8?token=1").as_deref(),
            Some("N_m3u8DL-RE")
        );
        assert_eq!(executable("https://example.com/playlist?id=1"), None);
        assert_eq!(executable("https://example.com/video.mp4"), None);
    }

    #[test]
    fn test_downloader_arguments() {
        let url = Url::parse("https://youtu.be/abc").unwrap();

        let hls = HlsDownloader {
            arguments: vec!["-M".to_string(), "format=mp4".to_string()],
            ..HlsDownloader::default()
        };
        assert_eq!(
            hls.command_arguments(&url, "/videos", "100% Review"),
            [
                "https://youtu.be/abc",
                "--save-dir",
                "/videos",
                "--save-name",
                "100% Review",
                "--auto-select",
                "-M",
                "format=mp4",
            ]
        );

        let youtube = YoutubeDownloader {
            arguments: vec!["--cookies".to_string(), "cookies.txt".to_string()],
            ..YoutubeDownloader::default()
        };
        assert_eq!(
            youtube.command_arguments(&url, "/videos", "100% Review"),
            [
                "https://youtu.be/abc",
                "--no-playlist",
                "--no-progress",
                "--paths",
                "/videos",
                "--output",
                "100%% Review.%(ext)s",
                "--cookies",
                "cookies.txt",
            ]
        );
    }

    #[test]
    fn test_video_configuration_requires_links() {
        let yaml = "path: /videos";

        let config = config::Config::builder()
            .add_source(config::File::from_str(yaml, FileFormat::Yaml))
            .build()
            .expect("could not create config");

        assert!(config.try_deserialize::<VideoConfiguration>().is_err());
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
struct VideoConfiguration {
    path: String,
    /// Filters on the file name to identify the files containing videos.
    #[serde(default)]
    files: Vec<Filter>,
    links: LinkFilters,
    /// Builds the file name of an entry from its title.
    #[serde(default)]
    names: NameConfiguration,
    #[serde(default)]
    downloaders: Downloaders,
}

/// Terms removed from the file name and rules adding a prefix and/or suffix to it.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct NameConfiguration {
    /// Removed from the file name (all occurrences, ignoring ASCII case) before the affixes are added.
    strip: Vec<String>,
    /// All rules whose filter matches the full title contribute their affixes, in this order.
    affixes: Vec<Affix>,
}

/// A prefix and/or suffix added to the file name if the filter matches the full title.
#[derive(Debug, Deserialize)]
struct Affix {
    #[serde(flatten)]
    filter: Filter,
    /// Added in front of the file name, without a separator.
    #[serde(default)]
    prefix: String,
    /// Added after the file name, without a separator.
    #[serde(default)]
    suffix: String,
}

/// Filters on the link text to identify the thumbnail and the video link of an entry.
#[derive(Debug, Deserialize)]
struct LinkFilters {
    #[serde(default)]
    thumbnail: Vec<Filter>,
    #[serde(default)]
    video: Vec<Filter>,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct Downloaders {
    hls: HlsDownloader,
    youtube: YoutubeDownloader,
    /// Number of additional attempts if a download fails.
    retries: u32,
    /// Waited before each additional attempt.
    #[serde(with = "humantime_serde")]
    retry_delay: Duration,
}

impl Default for Downloaders {
    fn default() -> Self {
        Self {
            hls: HlsDownloader::default(),
            youtube: YoutubeDownloader::default(),
            retries: 2,
            retry_delay: Duration::from_secs(30),
        }
    }
}

impl Downloaders {
    /// Returns the executable and its arguments for the downloader handling the url, if any.
    fn command(&self, url: &Url, directory: &str, name: &str) -> Option<(&str, Vec<String>)> {
        if self.youtube.handles(url) {
            Some((
                &self.youtube.executable,
                self.youtube.command_arguments(url, directory, name),
            ))
        } else if self.hls.handles(url) {
            Some((
                &self.hls.executable,
                self.hls.command_arguments(url, directory, name),
            ))
        } else {
            None
        }
    }
}

/// Downloads HLS playlists (urls whose path ends with ".m3u8") with N_m3u8DL-RE.
#[derive(Debug, Deserialize)]
#[serde(default)]
struct HlsDownloader {
    executable: String,
    arguments: Vec<String>,
}

impl Default for HlsDownloader {
    fn default() -> Self {
        Self {
            executable: "N_m3u8DL-RE".to_string(),
            arguments: Vec::new(),
        }
    }
}

impl HlsDownloader {
    fn handles(&self, url: &Url) -> bool {
        url_extension(url).as_deref() == Some("m3u8")
    }

    fn command_arguments(&self, url: &Url, directory: &str, name: &str) -> Vec<String> {
        let mut arguments: Vec<String> = [
            url.as_str(),
            "--save-dir",
            directory,
            "--save-name",
            name,
            "--auto-select",
        ]
        .map(String::from)
        .into();
        arguments.extend(self.arguments.iter().cloned());
        arguments
    }
}

/// Downloads links to the configured hosts (and their subdomains) with yt-dlp.
#[derive(Debug, Deserialize)]
#[serde(default)]
struct YoutubeDownloader {
    executable: String,
    arguments: Vec<String>,
    hosts: Vec<String>,
    /// Runs "yt-dlp --update" at startup and then after each interval; no updates if not set.
    #[serde(with = "humantime_serde::option")]
    update_interval: Option<Duration>,
}

impl Default for YoutubeDownloader {
    fn default() -> Self {
        Self {
            executable: "yt-dlp".to_string(),
            arguments: Vec::new(),
            hosts: ["youtube.com", "youtu.be", "youtube-nocookie.com"]
                .map(String::from)
                .into(),
            update_interval: None,
        }
    }
}

impl YoutubeDownloader {
    fn handles(&self, url: &Url) -> bool {
        let Some(host) = url.host_str() else {
            return false;
        };
        let host = host.to_lowercase();
        self.hosts.iter().any(|allowed| {
            let allowed = allowed.to_lowercase();
            host == allowed
                || host
                    .strip_suffix(allowed.as_str())
                    .is_some_and(|subdomain| subdomain.ends_with('.'))
        })
    }

    fn command_arguments(&self, url: &Url, directory: &str, name: &str) -> Vec<String> {
        // The output is a template, so a literal "%" has to be escaped.
        let output = format!("{}.%(ext)s", name.replace('%', "%%"));
        let mut arguments: Vec<String> = [
            url.as_str(),
            "--no-playlist",
            "--no-progress",
            "--paths",
            directory,
            "--output",
            &output,
        ]
        .map(String::from)
        .into();
        arguments.extend(self.arguments.iter().cloned());
        arguments
    }
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
