# Release Notifier qBittorrent Integration

This tool monitors a directory for HTML files (e.g., from release notifications), extracts magnet links based on configurable filters, and automatically adds them to qBittorrent.

## Features

- **Directory Monitoring**: Monitors a folder for new files.
- **HTML Parsing**: Uses `scraper` (based on `html5ever`) to extract magnet links from `<li>` elements.
- **Flexible Filters**: Filtering by multiple terms (AND-linked within a filter) with optional case sensitivity.
- **qBittorrent Integration**: Automatic login and adding of torrents via the Web API.
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
```

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

## Development

### Run Tests
```bash
cargo test
```
