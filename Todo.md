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

## 3. Retry failed downloads

yt-dlp fails temporarily every now and then (network, throttling). Currently there is one
attempt, the problem is reported and the notification file is archived, so the download is lost
and the file has to be copied back by hand. Add one or two retries with a delay, or a `failed/`
directory that is picked up by the next run.

## 4. Dry run for developing filters

A `--dry-run <file>` that reads a notification file and only prints which entries are found and
which file names they would get, without downloading or archiving anything. Useful whenever
`names.strip` and `names.affixes` are adjusted.

## 5. Do not overwrite existing files silently

`download_image` writes unconditionally, and the downloaders overwrite as well. If the same
notification arrives twice, or two entries end up with the same generated name, they overwrite
each other. Skip with a note, or add a counter suffix.

## 6. Make the polling interval configurable

The one minute in the main loop is the only value that is hard coded while everything else comes
from the configuration.
