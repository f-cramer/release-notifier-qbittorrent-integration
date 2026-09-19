# Todo

Ideas for further improvements, roughly in the order in which they seem worth doing.

## 1. Do not let one broken file block the whole run (done)

`process_directory` collects the error of each file, reports the errors of the run together and
continues with the next file. A file that cannot be processed stays where it is and is retried
by the next run.

Open: a file that keeps failing is reported again on every run. If that turns out to be
annoying, remember the already reported failures or move them aside after a few attempts.

## 2. Report errors outside the video handling (done)

The main loop reports a run that failed and a failed cleanup of the archive through the
`Notifier` instead of only logging them. The only remaining `error!` is the fallback of the
`Notifier` itself, which cannot report that it could not report.

## 3. Retry failed downloads (done)

A failed video download is repeated `downloaders.retries` times with `downloaders.retry_delay`
in between, which covers temporary failures like a network hiccup or throttling. Only the video
download is repeated, not the thumbnail and not the update of yt-dlp.

Note that the retries of point 1 do not apply here: `process_videos` collects a failed download
as a `Problem` instead of returning an error, so the notification file is archived either way.
Deliberately so, because repeating the whole file would download the entries that already
succeeded a second time. See point 6 for the retry across runs.

## 4. Dry run for developing filters (done)

`--dry-run <file>` prints the magnet links the filters find, the recognized video entries with
the file name each one would get and the command that would download it. Nothing is downloaded,
archived or sent to qBittorrent, and no directory is created.

## 5. Make the polling interval configurable (done)

The top level `interval` sets what is waited between two runs, defaulting to the one minute that
used to be hard coded.

## 6. Retry a failed download in a later run

The retries of point 3 all happen within a few minutes. A download that fails because the
source is temporarily unavailable needs a longer break — a new attempt a few minutes or hours
later, across runs. Needs state beyond a single run: which entry of which notification file
still has to be downloaded, and since when. A `failed/` directory holding a notification file
reduced to the missing entries would keep that state in the file system instead of in a
database, and would be picked up by a run once it is old enough.
