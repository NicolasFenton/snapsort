# snapsort

Sort a folder of photos into category folders using the
[SubQ Decisions API](https://docs.subq.ai/endpoints/decisions-api/).

You choose the folders (or use the built-in list). For each image, snapsort builds a
small JPEG preview in memory and asks SubQ which folder fits best. It then
moves (or copies) the **original, untouched** file into that folder. Images the
model isn't sure about go to `needs_review`. Optionally, blurry, badly exposed
or accidental shots go to an `unusable` folder instead (see
[Unusable photos](#unusable-photos)).

```
$ snapsort -i ~/Pictures/inbox -o ~/Pictures/sorted -c categories.json
snapsort 0.1.0 · 1,204 images · 31 categories · move → /Users/me/Pictures/sorted
⠼ 00:01:12 ━━━━━━━━━━━━━━━━━━━━━━━━╸─────────────── 734/1204 10.2/s eta 46s
  sorted 688 · needs review 41 · failed 5 · in flight 32
```

## Install

```bash
npm install -g snapsort
```

npm installs a prebuilt native binary for your platform: macOS (Apple Silicon
and Intel), Linux (x64 and arm64, static musl builds that run on any distro)
and Windows (x64 and arm64). It needs no Rust toolchain and runs no
postinstall scripts.

To build from source instead (Rust 1.88+):

```bash
cargo install --path .
```

## Quick start

```bash
npm install -g snapsort
snapsort
```

snapsort asks for what it needs, one question at a time. Press Enter to accept
the answer in brackets:

```
No SubQ API key found. Get one at https://console.subq.ai
Paste your SubQ API key (input is hidden):
Saved to ~/.config/snapsort/credentials.json (delete it to change the key).
Folder to sort [~]: ~/Pictures/field-day
  Found 412 images.

Which folders should images be sorted into?
  Enter        Bulls, Cows, Rams, Livestock, People, Products, Machinery, …
  a, b, c      your own folder names, separated by commas
  file.json    a categories file with descriptions
Folders [defaults]:

Put the sorted folders in [~/Pictures/field-day]:
Move the images, or copy them and keep the originals? (move/copy) [move]:
Put blurry, badly exposed or accidental photos in an unusable folder? [y/N]: y

Move images from ~/Pictures/field-day into 31 folders in ~/Pictures/field-day, and blurry or bad photos into unusable
Start? [Y/n]:
```

- The API key is asked for only on the first run; see [API key](#api-key).
- To enter a folder, you can drag it into the terminal window.
- Pressing Enter at the folders question uses the built-in list in
  [examples/categories.json](examples/categories.json): livestock, people,
  machinery, infrastructure, drone shots, and a `Crop-` folder per crop type
  (wheat, barley, canola, cotton, pasture, vineyards, orchards and more).

### Running with flags

Every question has a flag, and passing `-i`, `-o` and `-c` together skips the
questions entirely. This is handy for scripts and repeat runs:

1. Write a categories file. Keys become folder names. Values describe the
   folder for the model, or are `null` when the name says enough. You need
   between 2 and 255 entries:

   ```json
   {
     "Livestock": "Ground-level photos where farm animals are the main subject",
     "Machinery": "Ground-level photos of tractors, harvesters, trucks or excavators",
     "Drone-Fields": "Aerial or drone photos of farmland, crops or paddocks",
     "People": null
   }
   ```

   The built-in list in [examples/categories.json](examples/categories.json)
   is a good starting point to copy.

2. Preview the result first, then run it for real:

   ```bash
   snapsort -i ./inbox -o ./sorted -c categories.json --dry-run
   snapsort -i ./inbox -o ./sorted -c categories.json
   ```

## Unusable photos

Add `--unusable` (or answer `y` to the setup question) to pull bad shots out
of your category folders:

```bash
snapsort -i ./inbox -o ./sorted -c categories.json --unusable --dry-run
snapsort -i ./inbox -o ./sorted -c categories.json --unusable
```

- **No extra requests.** The same request that picks the folder also asks
  SubQ a yes/no question: is this photo badly blurred, out of focus, far too
  dark or bright, or an accidental shot (the ground, a pocket, a finger over
  the lens)? The extra question adds a few input tokens per image.
- **Where photos go.** A photo SubQ rates at least 0.5 likely to be unusable
  goes to `<output>/unusable/` *instead of* its category folder, even if it
  would otherwise have gone to `needs_review`. Everything else is sorted as
  usual. Nothing is deleted: look through `unusable` before you empty it.
- **Tuning.** `--unusable-threshold` sets the cutoff. Raise it if good photos
  land in `unusable`; lower it if bad ones slip through. A `--dry-run` with
  `--unusable` adds an UNUSABLE column with every photo's score, so you can
  pick a threshold before anything moves. In a test on Canon R5 C farm photos,
  good shots scored 0.03–0.10 and slightly soft ones about 0.2, while heavily
  blurred, very dark and accidental shots scored 0.87–0.98. Overexposed shots
  were the hardest to catch, at about 0.6–0.85.
- **What it can judge.** SubQ sees the same downscaled preview as for
  sorting (`--max-dim`, 1280 px by default). It catches obvious problems, not
  pixel-level softness, and a slightly soft or noisy photo counts as usable.
- A category named `unusable` (any case) can't be used together with
  `--unusable`, since both would fill the same folder.

## API key

snapsort looks for a key in this order:

1. `--api-key <KEY>`
2. The `SUBQ_API_KEY` environment variable
3. The key saved by an earlier run
4. If you're at a terminal, a prompt (input is hidden). The key you enter is
   saved for next time.

Where the key is saved:

| OS | File |
| --- | --- |
| macOS / Linux | `~/.config/snapsort/credentials.json` (or `$XDG_CONFIG_HOME/snapsort/`) |
| Windows | `%APPDATA%\snapsort\credentials.json` |

- On macOS and Linux, only your user can read the file (`0600`).
- To change the key, delete the file; snapsort will ask again on the next run.
- If SubQ rejects a saved key (a typo, or a revoked key), snapsort deletes the
  file for you.
- snapsort sends the key only in the `Authorization` header to the API
  endpoint. It never writes the key to reports or logs.

**Scripts and CI:** with no terminal there is no prompt; snapsort exits with
an error instead of hanging. Set `SUBQ_API_KEY` from your secret store.
Prefer the environment variable to `--api-key`, which is visible in shell
history and `ps`.

## Supported formats

| Kind | Extensions |
| --- | --- |
| Images | `.jpg`, `.jpeg`, `.png`, `.webp` |
| Camera raw | `.cr2`, `.cr3` (Canon), `.nef` (Nikon), `.arw` (Sony), `.raf` (Fujifilm), `.rw2` (Panasonic), `.dng` |

Extensions match case-insensitively, so `IMG_0001.CR3` is found.

snapsort doesn't develop raw sensor data. Every raw file also embeds JPEGs the
camera rendered for its own screen, and snapsort classifies from one of those.
It reads only that JPEG and the metadata that locates it, never the sensor
data, so a folder of 40 MB raws sorts about as fast as a folder of JPEGs. A
raw with no embedded JPEG (some DNG converters leave it out) fails with "no
embedded JPEG preview found" and stays where it is.

Not supported yet: `.orf` (Olympus/OM System) and `.pef` (Pentax), which keep
their previews inside maker notes, and `.heic`/`.avif`, which would need C
decoders that the static builds avoid.

## Options

| Flag | Default | Description |
| --- | --- | --- |
| `-i, --input <PATH>` | asked | Directory of unsorted images; see [Supported formats](#supported-formats) |
| `-o, --output <PATH>` | asked | Root folder for category subfolders (may equal `--input`) |
| `-c, --categories <PATH>` | asked | JSON file of folder names → descriptions (`string` or `null`) |
| `-j, --concurrency <N>` | `32` | Images processed in parallel (1–256) |
| `--max-dim <PX>` | `1280` | Longest side of the in-memory preview sent to the API |
| `--quality <1-100>` | `80` | JPEG quality of the preview |
| `--min-confidence <0-1>` | `0.55` | Below this, files go to `needs_review` |
| `--unusable` | off | Put blurry, badly exposed or accidental photos in `unusable`; see [Unusable photos](#unusable-photos) |
| `--unusable-threshold <0-1>` | `0.5` | At or above this likelihood of being unusable, files go to `unusable` (needs `--unusable`) |
| `--copy` | off | Copy instead of move |
| `--dry-run` | off | Classify and print a plan table; change nothing on disk |
| `--recursive` | off | Include subfolders of `--input` |
| `--api-key <KEY>` | `$SUBQ_API_KEY` | SubQ API key; see [API key](#api-key) |
| `--report <PATH>` | none | Write a JSON record of every decision |
| `-v, --verbose` | off | Print one line per file as it's sorted |
| `--api-url <URL>` | `https://api.subq.ai/v1/systemone` | Endpoint (`$SUBQ_API_URL`) |
| `--model <MODEL>` | `subq-snap-preview` | Decision model (`$SUBQ_MODEL`) |
| `--retries <N>` | `4` | Retries per image on 429/5xx/network errors |
| `--timeout <SECS>` | `120` | Per-request timeout |

### Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Every image was sorted (including any sent to `needs_review`) |
| `1` | Fatal error: bad config, or the API rejected the key, credits or model |
| `2` | Invalid command-line arguments |
| `3` | Finished, but some files failed and were left in place |
| `130` | Interrupted with Ctrl-C |

## How it works

1. **Scan.** snapsort finds supported images (matching extensions
   case-insensitively). It skips hidden files, such as macOS `._*` files, and
   symlinks. With `--recursive` it never descends into the destination
   folders (including `needs_review` and `unusable`), so sorting a folder into
   itself (`-o` equal to `-i`) is safe and can be re-run.
2. **Preview** (CPU threadpool via `spawn_blocking`). It decodes the image by
   content rather than extension and applies EXIF rotation. For a camera raw
   it decodes the smallest embedded JPEG that still fills `--max-dim` (or the
   largest, if none does) and rotates it by the raw's own orientation tag;
   for a 24 MP CR3 at the default size, that is Canon's 1620×1080 preview
   rather than the full-size JPEG. It then shrinks
   the image to fit `--max-dim` with a triangle filter, keeping the aspect
   ratio, and flattens any transparency onto white. Finally it encodes a JPEG
   at `--quality`. If that JPEG is not strictly under SubQ's 4 MiB per-image
   limit, quality and then size are stepped down until it is. The original
   file is only ever read.
3. **Classify.** It sends one request per image to `POST /v1/systemone`. The
   request carries the preview as raw base64 (no `data:` prefix) and a
   `target_folder` *choice* question built from your categories. With
   `--unusable` it also carries an `unusable` yes/no (*noul*) question.
   - 429, 5xx and network errors are retried with jittered exponential
     backoff. A `Retry-After` header is honored when present.
   - 401, 402 and 403 (bad key, no credits, model not allowed) stop the run
     at once instead of failing every remaining file.
   - A file the API rejects (for example, a 422 on a bad image) is reported
     and left where it is.
4. **Place.** The original goes to `<output>/<choice>/`. It goes to
   `<output>/needs_review/` instead when the confidence is below
   `--min-confidence` or the answer is not one of your folders. With
   `--unusable`, a photo at or above `--unusable-threshold` goes to
   `<output>/unusable/` before either of those checks.
   - Existing files are never overwritten: a name collision becomes
     `photo (1).jpg`.
   - Moves are atomic renames. Across filesystems, snapsort copies to a hidden
     temp file, renames it into place, and deletes the source only after a
     complete copy.
   - Copies keep the original modification time.

Network concurrency is set by `--concurrency`. CPU-heavy decoding is capped
separately at the number of cores, so memory stays bounded however high you
set `-j`. Above about 32, upload bandwidth rather than SubQ is usually the
limit (each image uploads a 0.2–0.3 MB preview), so on a slow connection a
lower `--max-dim` speeds things up more than a higher `-j`. Press Ctrl-C once to finish the images in flight and get a summary;
press it again to quit immediately.

### JSON report

`--report decisions.json` writes run metadata, totals, and one entry per
file: source, status (`sorted` / `needs_review` / `unusable` / `failed`),
folder, destination, chosen label, confidence, every category's probability,
the likelihood of being unusable (`null` without `--unusable`), and the error
for failures. Use it for auditing, or to undo a run.

## Development

```bash
cargo test                               # unit + end-to-end tests (mock SubQ server)
cargo clippy --all-targets -- -D warnings
cargo build --release
```

The end-to-end tests in [tests/cli.rs](tests/cli.rs) run the real binary
against a local mock server. The mock checks every request against the
documented SubQ schema.

Raw-format tests use small synthetic files built by
[tests/support/raw.rs](tests/support/raw.rs). To check real camera files too
(CC0 samples for most cameras are at [raw.pixls.us](https://raw.pixls.us)),
point an opt-in test at a folder of them. It prints each file's embedded
JPEGs and saves the previews it would send:

```bash
SNAPSORT_RAW_SAMPLES=~/raw-samples SNAPSORT_RAW_PREVIEWS=/tmp/previews \
  cargo test --release real_raw_samples -- --ignored --nocapture
```

### npm packaging

The npm distribution follows the esbuild/biome layout:

- [`npm/snapsort`](npm/snapsort) is the `snapsort` package: a small Node
  launcher ([bin/snapsort.js](npm/snapsort/bin/snapsort.js)) plus
  `optionalDependencies` on one package per platform.
- Each platform package (`@snapsort/darwin-arm64`, `@snapsort/linux-x64`, …)
  declares `os`/`cpu`, so npm installs only the one that matches the machine.
- [`npm/snapsort/platforms.json`](npm/snapsort/platforms.json) maps platforms
  to package names and Rust targets. The launcher and the assemble script
  both read it.
- [`scripts/npm/assemble.mjs`](scripts/npm/assemble.mjs) builds every package
  from compiled binaries. It takes the version from `Cargo.toml`, the single
  source of truth.
- `SNAPSORT_BINARY=/path/to/snapsort` makes the launcher use a specific binary.

Try the npm install locally without publishing:

```bash
cargo build --release
node scripts/npm/assemble.mjs --host --pack
npm install -g dist/npm/*.tgz
```

### Releasing

One-time setup:

1. Create the `snapsort` organization on npmjs.com; it owns the `@snapsort/*`
   platform packages.
2. Add an npm automation token as the `NPM_TOKEN` repository secret.
3. Optionally, once the packages exist, configure npm trusted publishing for
   this repository.

Then for each release:

1. Bump `version` in `Cargo.toml` and run `cargo check` to update
   `Cargo.lock`.
2. Commit, then tag and push:

   ```bash
   git tag v0.1.0
   git push origin v0.1.0
   ```

The [release workflow](.github/workflows/release.yml) builds all six targets
on native runners and checks that the tag matches `Cargo.toml`. It then
publishes the platform packages followed by the launcher, with npm provenance
on public repos. It skips versions that are already published, so a failed
release can be re-run. Finally it creates a GitHub release with archives and
`SHA256SUMS`. Versions with a prerelease suffix (`0.2.0-beta.1`) are
published under the `next` dist-tag.

To rehearse a release without publishing anything, run the workflow manually
from the Actions tab with **dry run** checked.

The arm64 Linux and Windows builds use GitHub's `ubuntu-24.04-arm` and
`windows-11-arm` runners. If those aren't available to your repository, remove
those matrix entries and the matching lines in `platforms.json`.

## License

MIT
