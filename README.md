# pod

A self-hosted, Audacity-style podcast editor that does two things well: **silence** a
range on the selected tracks, and **cut** a range out. You can stop on one machine
(or phone) and continue on another, where it opens with the same edits, cursor and zoom.

All the audio work happens in the browser, in Rust compiled to WebAssembly
(egui UI, WAV decoding, waveform analysis, playback mixing, export rendering). The server
only stores files and the edit history.

## How it works

- **Projects are lists of edits.** Each track points at its uploaded WAV, which is never
  modified. Edits (`silence`, `cut`, `undo`, …) are small JSON ops appended to
  `ops.jsonl`; the timeline is computed from them. The same Rust code (`core/`)
  applies ops in the browser and on the server, so the two always agree.
- **Sync** sends ops as you make them (within about a second). Every 15 s, and whenever
  the window regains focus, the app pulls in edits made elsewhere. If a stale tab tries
  to save over newer work, its edits are discarded and you're told; nothing is overwritten.
  The playhead, zoom and selection are saved too.
- **Big files**: each WAV is uploaded once in resumable 8 MiB chunks. A small waveform
  summary (~1.3 MB per hour) is uploaded with it, so another device draws the waveform
  right away. The audio itself is downloaded once per device into the browser's
  Cache Storage, and HTTP Range requests stream it until then. Playback and export work
  in ~1–10 s chunks and never hold a whole file in memory, which matters on phones.
- **Export** renders a 24-bit WAV in the browser, streams it to the server and gives
  you a download link that works from any device.

## Using it

| | Desktop | Touch |
|---|---|---|
| Select | drag across the tracks you want | one-finger drag |
| Select / unselect a track | click its name | tap its name |
| Silence selection on selected tracks | `S` or `Ctrl+L` | 🔇 |
| Cut selection | `Delete`, `X` or `Ctrl+K` | ✂ |
| Undo / redo | `Ctrl+Z` / `Ctrl+Shift+Z` | ⟲ / ⟳ |
| Play / pause | `Space` | ▶ |
| Zoom | `Ctrl`+wheel, `Ctrl+1`/`Ctrl+3`, `Ctrl+F` fit | pinch |
| Scroll | wheel / trackpad, scrollbar | two-finger drag |

By default, Cut removes the range from **every** track so the speakers stay in
sync. To cut only the selected tracks, turn on "Cut only selected tracks" in the ☰ menu.
Right-click (or long-press) a track name to rename or remove the track. All
WAVs in a project must share one sample rate (8/16/24/32-bit PCM or 32/64-bit float).

## Build

```sh
rustup target add wasm32-unknown-unknown x86_64-unknown-linux-musl
cargo install trunk            # or download a release binary
cd client && trunk build --release && cd ..
cargo build --release -p pod-server --target x86_64-unknown-linux-musl
```

The server binary embeds `client/dist`, so it's the only file to deploy. Tests:
`cargo test -p pod-core`.

For local development, run `trunk build` (in `client/`), then run
`cargo run -p pod-server -- config.json`. A debug server reads `client/dist`
from disk, so you only need to rebuild the client.

## Deploy (pod.pco2699.xyz)

```sh
# on the server
mkdir -p ~/apps/pod && cd ~/apps/pod
scp …/pod-server .                      # from your machine
printf '{"port":8094,"token":"%s"}\n' "$(openssl rand -hex 24)" > config.json
chmod 600 config.json
cp deploy/pod.service ~/.config/systemd/user/ && systemctl --user daemon-reload && systemctl --user enable --now pod
sudo cp deploy/pod.pco2699.xyz.nginx /etc/nginx/sites-available/pod.pco2699.xyz
sudo ln -s /etc/nginx/sites-available/pod.pco2699.xyz /etc/nginx/sites-enabled/
sudo nginx -t && sudo systemctl reload nginx && sudo certbot --nginx -d pod.pco2699.xyz
```

Open `https://pod.pco2699.xyz/?key=<token>` once on each device. The key is then
remembered and removed from the address bar.

### Behind a login proxy

If nginx already signs you in (oauth2-proxy via `auth_request`), the key is a second
password for the same person. Add `"proxy_auth_header": "X-Auth-Request-Email"` to
`config.json` and have nginx always set that header from the proxy's answer
(`proxy_set_header X-Auth-Request-Email $email;`, which also replaces any copy a client
sent). A request carrying it needs no key, so the app opens without asking for one. A
key that is still stored is tried first and dropped if refused. If the login lapses
while editing, the app says so and keeps unsynced edits until you sign in again.

Only set it when nginx always overwrites the header and nothing else can reach the
port: anyone who can send the header directly skips the check. The token keeps working
for scripts.

Data lives in `~/apps/pod/data` (`blobs/`, `projects/`, `exports/`). Back up that folder.
