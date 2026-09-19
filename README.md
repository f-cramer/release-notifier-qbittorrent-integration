# Release Notifier qBittorrent Integration

This tool monitors a directory for HTML files (e.g., from release notifications), extracts magnet links based on configurable filters, and automatically adds them to qBittorrent. It can also download videos (m3u8 playlists and YouTube) and their thumbnails from notification files.

## Features

- **Directory Monitoring**: Monitors a folder for new files.
- **HTML Parsing**: Uses `scraper` (based on `html5ever`) to extract magnet links from `<li>` elements.
- **Flexible Filters**: Filtering by multiple terms (AND-linked within a filter) with optional case sensitivity.
- **qBittorrent Integration**: Automatic login and adding of torrents via the Web API.
- **Video Downloads**: For files whose name matches `videos.files`, the links are identified by their link text (see `videos.links`). YouTube links are downloaded via [yt-dlp](https://github.com/yt-dlp/yt-dlp) and m3u8 playlists via [N_m3u8DL-RE](https://github.com/nilaoda/N_m3u8DL-RE). The thumbnail is only downloaded if the video link can be handled by one of them; entries with other video links are skipped. Both are stored in the configured video directory and named after the `<h4>` heading of the entry (without a leading date `YYYY-MM-DD` or `YYYY-MM-DD - `, without everything from the first `|` on and without characters that are invalid in file names). Failed downloads are logged.
- **Name Generation**: `videos.names` removes terms from the file name and adds prefixes and suffixes to it based on filters on the full heading, so that information behind the `|` is not lost.
- **Email Notifications**: Problems are optionally reported by email (SMTP) instead of the log.
- **Dry Run**: `--dry-run <file>` shows what a file would result in, without downloading or archiving anything.
- **Archiving**: Processed files are moved to an archive directory.
- **Cleanup**: Automatic deletion of old files from the archive after a configurable retention period.

## Configuration (`config.yml`)

Create a `config.yml` in the project directory:

```yaml
path: /home/user/notifier    # Directory containing the files to process
archive:
  path: /home/user/archive   # Directory for processed files
  retentionPeriod: 7d          # Retention period (e.g., 7d, 24h)

qbittorrent:
  url: http://localhost:8080/
  username: admin
  password: your_password

logging:
  level: info                  # trace, debug, info, warn, error

filters:
  - terms: ["Hello", "c376"] # Matches entries containing BOTH terms
    case_sensitive: false
  - terms: ["World", "BO0"]
    case_sensitive: true

videos:                        # optional
  path: /home/user/videos      # Target directory for thumbnails and videos
  files:                       # Applied to the file name, not to the file content
    - terms: ["TabletopTactics"]
  links:                       # Applied to the link texts below each <h4> heading
    thumbnail:                 # First matching link is downloaded as thumbnail
      - terms: ["Thumbnail"]
    video:                     # First matching link is passed to a downloader
      - terms: ["Video"]
  names:                       # optional, builds the file name from the <h4> heading
    strip:                     # Removed from the name (all occurrences, ASCII case is ignored)
      - "**NEW CODEX!!**"
    affixes:                   # Applied to the full heading, including the part behind the "|"
      - terms: ["Codex Review"]
        suffix: " - Codex Review"
      - terms: ["Battle Report"]
        prefix: "Batrep - "
  downloaders:                 # optional, all entries have defaults
    hls:                       # Used for video links whose path ends with .m3u8
      executable: N_m3u8DL-RE  # default: N_m3u8DL-RE from PATH
      arguments: ["-M", "format=mp4"]  # default: none
    youtube:
      executable: yt-dlp       # default: yt-dlp from PATH
      arguments: ["--merge-output-format", "mp4"]  # default: none
      hosts: ["youtube.com", "youtu.be", "youtube-nocookie.com"]  # default, subdomains included
      update_interval: 1d      # optional, runs "yt-dlp --update" at startup and then after each interval (default: no updates)
    retries: 2                 # default, additional attempts if a video download fails
    retry_delay: 30s           # default, waited before each additional attempt

email:                         # optional, without it problems are only logged
  host: smtp.example.com       # SMTP server
  port: 465                    # 587 uses STARTTLS, all other ports use TLS from the start
  username: user               # SMTP login
  password: your_password
  from: notifier@example.com   # optional, sender address (default: username)
  to: you@example.com          # Recipient address
```

The file name of an entry is built in this order: the leading date and everything from the first `|` on are removed from the `<h4>` heading, then every term of `names.strip` is removed from the rest, and finally the affixes of all `names.affixes` whose filter matches the **full** heading are added — the prefixes in front and the suffixes behind, both in the order of the configuration and without a separator, so that the separator is part of the configured value. Characters that are invalid in file names are removed at the end. If nothing is left of the heading itself, the entry is reported as a problem and the affixes are dropped with it.

With the configuration above, `2026-09-19 - Space Marines | Warhammer 40k Codex Review` becomes `Space Marines - Codex Review` and `2026-09-19 - **NEW CODEX!!** Space Marines vs Orks | Warhammer 40k Battle Report` becomes `Batrep - Space Marines vs Orks`.

A failed video download is repeated `downloaders.retries` times, with `downloaders.retry_delay` between the attempts, which covers temporary failures like a network hiccup or throttling. This applies to the video download only, not to the thumbnail and not to the update of yt-dlp. A download that still fails afterwards is reported as a problem; the notification file is archived either way, so it is not retried in a later run.

N_m3u8DL-RE is always called with `--save-dir`, `--save-name` and `--auto-select`.

yt-dlp is always called with `--no-playlist`, `--no-progress`, `--paths` and `--output`. It needs [ffmpeg](https://ffmpeg.org/) to merge video and audio. Members-only videos or a "Sign in to confirm you're not a bot" error require cookies (e.g. `arguments: ["--cookies", "/path/to/cookies.txt"]`). Keep yt-dlp up to date, as YouTube changes frequently, e.g. with `update_interval`. The update runs between two processing runs, never during a download, and a failed update is reported like other problems. `--update` only works for the standalone binaries from the yt-dlp releases; if yt-dlp was installed via pip or a package manager, update it there instead.

If `email` is configured, problems (e.g. a video entry without a usable video link, a failed download, a file that could not be processed or an unreachable qBittorrent) are sent by email instead of being logged; the problems of one notification file are combined into one email, as are the files of one run that could not be processed. If sending fails, the error and the problems are logged.

## Installation & Execution

1. **Install Rust**: Ensure Rust and Cargo are installed.
2. **Clone & Build**:
   ```bash
   cargo build --release
   ```
3. **Run**:
   ```bash
   cargo run
   ```

## Dry Run

```bash
release-notifier-qbittorrent-integration --dry-run "2026-09-19 - 2 new videos"
```

Reads the given file with the current `config.yml` and prints which magnet links the filters
find, which video entries are recognized, which file name each entry would get and which
command would download it. Nothing is downloaded, archived or sent to qBittorrent, and no
directory is created — useful for developing `filters`, `videos.links` and `videos.names`.

The `videos.files` filter is reported but not obeyed, so that the entries of a renamed or
copied file can be checked as well.

## Development

### Run Tests
```bash
cargo test
```
