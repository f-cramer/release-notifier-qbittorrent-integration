# Todo

Ideas for further improvements.

## 1. Retry a failed download in a later run

The retries of `downloaders.retries` all happen within a few minutes and block the run while
they wait. A download that fails because the source is unavailable for longer needs a new
attempt across runs, half an hour or a few hours later.

Before building it: raising `downloaders.retries` and `downloaders.retry_delay` (e.g. five
attempts ten minutes apart) covers almost an hour without any new code. The price is that the
run is blocked for that time, so no notification file is processed and no torrent is handed to
qBittorrent, and that a restart loses everything. Only worth the rebuild if that hurts.

### Plan

A `retry/` directory holding a reduced notification file. When the download of an entry fails
for good, write a new HTML file containing only the `<div>` containers of the failed entries,
serialized straight from the original with `ElementRef::html()` — no own format and no
serialization code. The next run treats `retry/` like the input directory, but only for files
whose modification time is older than `retry.after`.

The state then lives in the file system instead of a database: the timestamp is the
modification time, the format is the one of the input directory, the existing parsing and
downloading is reused unchanged, and it can be corrected by hand — deleting a file gives up,
touching it retries right away.

```yaml
videos:
  retry:
    path: /home/user/retry
    after: 30m       # minimum age before a new attempt
    attempts: 5      # give up afterwards
```

### Decisions it needs

- **The attempt counter** has to go into the file, otherwise entries for permanently dead
  sources pile up. As an attribute on the `<body>` (`<body data-attempt="3">`) it stays inside
  the file and does not disturb the parsing.
- **When to report.** A failed download with attempts left only goes to the log, only the last
  attempt creates a `Problem` and with it the email. Otherwise every run sends one.
- **Magnet links must not be added again** on a retry. With the current files that happens to
  be harmless, because the video links do not start with `magnet:`, but the retry path should
  be limited to the video handling explicitly instead of relying on that.
- **Thumbnail and video are not tracked separately.** Only the video download is repeated; if
  just the thumbnail fails it stays a reported problem. Anything else means tracking which part
  of an entry is still missing, which costs more than it is worth.
- **What does not work here:** copying the original file and skipping every entry whose target
  file already exists. The target directory is cleaned up regularly, so an already downloaded
  file would be gone and would be downloaded again.

### Effort

Roughly 150 to 200 lines plus tests. The manageable part is writing and reading the `retry/`
directory. The fiddly part is that `process_videos` has to tell "failed, can be repeated" from
"failed for good" (no downloader for the link, no usable name) and treat them differently.
